// src/routes/domains.rs
//
// Custom-domain (BYO domain) API. The endpoints are intentionally thin:
// they validate, write DB rows, and answer questions — every slow or
// failure-prone interaction with DNS, ACME, and pfSense happens in the
// background domain_reconciler, never inline in a request.
//
// Endpoints (all WireGuard-authenticated except the ACME challenge):
//   POST   /api/domains/add                — register a domain, get DNS instructions
//   GET    /api/domains/<domain>/status    — state machine position + what to do next
//   POST   /api/domains/<domain>/verify    — nudge: clear backoff so the next pass checks now
//   GET    /api/domains/list               — all of the user's domains
//   DELETE /api/domains/<domain>           — teardown + delete
//
// Public (reached via HAProxy path-ACL from the internet):
//   GET /.well-known/acme-challenge/<token>  (mounted at /, not /api)

use crate::guards::AuthenticatedUser;
use crate::services::acme_manager::ChallengeStore;
use crate::services::dns_verifier::DnsVerifierTrait;
use crate::services::domain_reconciler::teardown_custom_domain;
use crate::services::domain_validation::{
    challenge_record_name, challenge_record_value, normalize_and_validate,
};
use crate::services::haproxy_client::HAProxyClientTrait;
use crate::services::pfsense_client::PfSenseClientTrait;
use crate::storage::{self, CustomDomainDb};

use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::State;
use std::sync::Arc;
use tracing::{info, warn};

// ============= CONFIG STATE =============

/// Custom-domain settings resolved once at startup (main.rs) and managed by
/// Rocket. Kept separate from AppConfig to avoid touching every AppConfig
/// construction site.
pub struct CustomDomainsConfig {
    pub enabled: bool,
    pub max_domains_per_user: i64,
    pub public_ip: String,
    pub base_domain: String,
}

// ============= REQUEST/RESPONSE TYPES =============

#[derive(Debug, Deserialize)]
pub struct AddDomainRequest {
    /// The customer's own hostname, e.g. "customer1.net" or "www.customer1.net"
    pub domain: String,
    pub container_id: String,
    /// Container port HAProxy forwards decrypted HTTP to (default 80)
    pub target_port: Option<u16>,
}

#[derive(Debug, Serialize)]
pub struct DnsInstructions {
    pub txt_record_name: String,
    pub txt_record_value: String,
    pub a_record_name: String,
    pub a_record_value: String,
    pub cname_alternative: String,
    pub note: String,
}

fn dns_instructions(
    domain: &str,
    token: &str,
    public_ip: &str,
    base_domain: &str,
) -> DnsInstructions {
    DnsInstructions {
        txt_record_name: challenge_record_name(domain),
        txt_record_value: challenge_record_value(token),
        a_record_name: domain.to_string(),
        a_record_value: public_ip.to_string(),
        cname_alternative: format!(
            "Subdomains may use a CNAME to any *.{} hostname instead of the A record",
            base_domain
        ),
        note: "Add BOTH records at your DNS provider. Verification runs automatically \
               (typically live 5-30 minutes after the records propagate). \
               Check progress with: nordkraft domain status <domain>"
            .to_string(),
    }
}

/// Human-readable "what happens next" per state — the CLI prints this.
fn state_explanation(d: &CustomDomainDb) -> &'static str {
    match d.status.as_str() {
        "pending_dns" => "Waiting for DNS records. Add the TXT and A/CNAME records at your DNS provider; checks run every minute against your domain's authoritative nameservers.",
        "dns_verified" => "DNS verified. Requesting a TLS certificate from Let's Encrypt next.",
        "issuing_cert" => "Requesting a TLS certificate from Let's Encrypt (HTTP-01).",
        "cert_ready" => "Certificate issued. Configuring routing on the edge next.",
        "activating" => "Configuring routing (backend, host rule, certificate binding) and verifying every piece.",
        "active" => "Live. Traffic to this domain reaches your container over HTTPS.",
        "degraded" => "Your domain stopped resolving to the platform's ingress IP. Restore the DNS records — otherwise routing is removed automatically after the grace period.",
        "error" => "A step failed; it will be retried automatically with backoff. See last_error.",
        "disabled" => "Disabled. Remove the domain and add it again to start over.",
        _ => "Unknown state.",
    }
}

fn domain_json(d: &CustomDomainDb, cfg: &CustomDomainsConfig) -> serde_json::Value {
    serde_json::json!({
        "domain": d.domain,
        "container_id": d.container_id,
        "target_port": d.target_port,
        "status": d.status,
        "explanation": state_explanation(d),
        "url": format!("https://{}", d.domain),
        "dns_instructions": dns_instructions(&d.domain, &d.verification_token, &cfg.public_ip, &cfg.base_domain),
        "verified_at": d.verified_at.map(|t| t.to_rfc3339()),
        "cert_expires_at": d.cert_expires_at.map(|t| t.to_rfc3339()),
        "degraded_since": d.degraded_since.map(|t| t.to_rfc3339()),
        "last_checked_at": d.last_checked_at.map(|t| t.to_rfc3339()),
        "last_error": d.last_error,
        "created_at": d.created_at.to_rfc3339(),
    })
}

// ============= ROUTES =============

/// Register a custom domain. Returns immediately with DNS instructions;
/// the reconciler takes it from there.
#[post("/domains/add", data = "<request>")]
pub async fn add_domain(
    request: Json<AddDomainRequest>,
    user: AuthenticatedUser,
    cfg: &State<CustomDomainsConfig>,
    pool: &State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    if !cfg.enabled {
        return Json(serde_json::json!({
            "error": "Custom domains are not enabled on this installation (set CUSTOM_DOMAINS_ENABLED=true)"
        }));
    }

    // 1. Normalize + validate (lowercase, punycode, PSL, platform-namespace
    //    rejection, label rules). NOTHING downstream ever sees the raw input.
    let domain = match normalize_and_validate(&request.domain, &cfg.base_domain) {
        Ok(d) => d,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };

    let target_port = request.target_port.unwrap_or(80);
    if target_port == 0 {
        return Json(serde_json::json!({ "error": "target_port must be 1-65535" }));
    }

    // 2. Per-user quota
    match storage::count_custom_domains_for_user(pool.inner(), &user.0.id).await {
        Ok(count) if count >= cfg.max_domains_per_user => {
            return Json(serde_json::json!({
                "error": format!(
                    "Custom domain limit reached ({}). Remove one first or contact support.",
                    cfg.max_domains_per_user
                )
            }));
        }
        Ok(_) => {}
        Err(e) => {
            return Json(serde_json::json!({ "error": format!("Database error: {}", e) }));
        }
    }

    // 3. Global uniqueness (friendly precheck; the DB UNIQUE constraint is
    //    the real guarantee against a race)
    match storage::custom_domain_exists(pool.inner(), &domain).await {
        Ok(true) => {
            return Json(serde_json::json!({
                "error": format!("'{}' is already registered on this platform", domain)
            }));
        }
        Ok(false) => {}
        Err(e) => {
            return Json(serde_json::json!({ "error": format!("Database error: {}", e) }));
        }
    }

    // 4. Container must exist and belong to this user
    let container =
        match storage::get_container_info(pool.inner(), &request.container_id, &user.0.id).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                return Json(serde_json::json!({
                    "error": "Container not found or access denied"
                }));
            }
            Err(e) => {
                return Json(serde_json::json!({ "error": format!("Database error: {}", e) }));
            }
        };

    // 5. Fresh random verification token per (user, domain) registration
    let token = uuid::Uuid::new_v4().simple().to_string();

    let id = match storage::insert_custom_domain(
        pool.inner(),
        &user.0.id,
        &request.container_id,
        &domain,
        target_port as i32,
        &token,
    )
    .await
    {
        Ok(id) => id,
        Err(e) => {
            // Unique-violation race lands here
            return Json(serde_json::json!({
                "error": format!("Could not register domain (already taken?): {}", e)
            }));
        }
    };

    info!(
        "🌍 Custom domain registered: {} → container {} (row {}, user {})",
        domain, container.container_name, id, user.0.id
    );

    Json(serde_json::json!({
        "status": "pending_dns",
        "domain": domain,
        "container_id": request.container_id,
        "target_port": target_port,
        "dns_instructions": dns_instructions(&domain, &token, &cfg.public_ip, &cfg.base_domain),
        "explanation": "Add the DNS records above. Verification and TLS issuance run automatically; \
                        this deliberately takes minutes, not seconds — every step is verified before \
                        any traffic is routed."
    }))
}

/// State-machine position + guidance for one domain.
#[get("/domains/<domain>/status")]
pub async fn domain_status(
    domain: String,
    user: AuthenticatedUser,
    cfg: &State<CustomDomainsConfig>,
    pool: &State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let normalized = match normalize_and_validate(&domain, &cfg.base_domain) {
        Ok(d) => d,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };

    match storage::get_custom_domain_with_owner(pool.inner(), &normalized, &user.0.id).await {
        Ok(Some(d)) => Json(domain_json(&d, cfg)),
        Ok(None) => Json(serde_json::json!({
            "error": "Domain not found or access denied"
        })),
        Err(e) => Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
    }
}

/// "Check now": clears the retry backoff so the next reconciler pass (≤ one
/// interval away) picks the domain up immediately, and runs a quick
/// synchronous DNS pre-check purely as user feedback.
#[post("/domains/<domain>/verify")]
pub async fn verify_domain_now(
    domain: String,
    user: AuthenticatedUser,
    cfg: &State<CustomDomainsConfig>,
    dns: &State<Arc<dyn DnsVerifierTrait>>,
    pool: &State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let normalized = match normalize_and_validate(&domain, &cfg.base_domain) {
        Ok(d) => d,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };

    let d = match storage::get_custom_domain_with_owner(pool.inner(), &normalized, &user.0.id).await
    {
        Ok(Some(d)) => d,
        Ok(None) => {
            return Json(serde_json::json!({ "error": "Domain not found or access denied" }))
        }
        Err(e) => return Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
    };

    // Clear backoff so the reconciler acts on the next pass.
    if let Err(e) = storage::schedule_custom_domain_retry(
        pool.inner(),
        d.id,
        d.retry_count,
        0,
        "manual verify requested",
    )
    .await
    {
        warn!("verify_domain_now: could not clear backoff: {}", e);
    }

    // Informational pre-check (does NOT advance the state machine — the
    // reconciler's own 2-consecutive-checks rule does that).
    let precheck = dns
        .verify_domain(&normalized, &d.verification_token, &cfg.public_ip)
        .await;

    match precheck {
        Ok(result) => Json(serde_json::json!({
            "domain": normalized,
            "status": d.status,
            "txt_record_found": result.txt_ok,
            "a_record_points_here": result.a_ok,
            "detail": result.detail,
            "note": "Verification is confirmed by the background reconciler (two consecutive \
                     authoritative checks) — this pre-check is informational."
        })),
        Err(e) => Json(serde_json::json!({
            "domain": normalized,
            "status": d.status,
            "error": format!("DNS check could not run: {}", e)
        })),
    }
}

/// List all of the user's custom domains.
#[get("/domains/list")]
pub async fn list_domains(
    user: AuthenticatedUser,
    cfg: &State<CustomDomainsConfig>,
    pool: &State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    match storage::list_custom_domains_for_user(pool.inner(), &user.0.id).await {
        Ok(domains) => {
            let list: Vec<serde_json::Value> =
                domains.iter().map(|d| domain_json(d, cfg)).collect();
            Json(serde_json::json!({ "domains": list, "count": list.len() }))
        }
        Err(e) => Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
    }
}

/// Remove a domain: full reverse-order teardown of pfSense artifacts, then
/// delete the row. Runs inline (slow is fine) so the user gets a definitive
/// answer; every cleanup step is tolerant of already-missing pieces.
#[delete("/domains/<domain>")]
pub async fn remove_domain(
    domain: String,
    user: AuthenticatedUser,
    cfg: &State<CustomDomainsConfig>,
    haproxy: &State<Arc<dyn HAProxyClientTrait>>,
    pfsense: &State<Arc<dyn PfSenseClientTrait>>,
    pool: &State<sqlx::PgPool>,
) -> Json<serde_json::Value> {
    let normalized = match normalize_and_validate(&domain, &cfg.base_domain) {
        Ok(d) => d,
        Err(e) => return Json(serde_json::json!({ "error": e.to_string() })),
    };

    let d = match storage::get_custom_domain_with_owner(pool.inner(), &normalized, &user.0.id).await
    {
        Ok(Some(d)) => d,
        Ok(None) => {
            return Json(serde_json::json!({ "error": "Domain not found or access denied" }))
        }
        Err(e) => return Json(serde_json::json!({ "error": format!("Database error: {}", e) })),
    };

    let warnings = teardown_custom_domain(pool.inner(), haproxy.inner(), pfsense.inner(), &d).await;

    if let Err(e) = storage::delete_custom_domain(pool.inner(), d.id).await {
        return Json(serde_json::json!({
            "error": format!("Teardown ran but the record could not be deleted: {}", e),
            "warnings": warnings
        }));
    }

    info!(
        "🗑️ Custom domain removed: {} (user {})",
        normalized, user.0.id
    );

    if warnings.is_empty() {
        Json(serde_json::json!({ "status": "removed", "domain": normalized }))
    } else {
        Json(serde_json::json!({
            "status": "removed",
            "domain": normalized,
            "warnings": warnings
        }))
    }
}

// ============= ACME HTTP-01 CHALLENGE (public, mounted at /) =============

/// Served to Let's Encrypt validators via the HAProxy path ACL. Anything not
/// in the in-memory store 404s — there is nothing to enumerate and tokens
/// live only for the seconds an order is in flight.
#[get("/.well-known/acme-challenge/<token>")]
pub async fn acme_challenge(
    token: String,
    store: &State<ChallengeStore>,
) -> Result<String, rocket::http::Status> {
    match store.get(&token).await {
        Some(key_auth) => {
            info!(
                "🔏 ACME challenge served for token {}…",
                &token[..8.min(token.len())]
            );
            Ok(key_auth)
        }
        None => Err(rocket::http::Status::NotFound),
    }
}
