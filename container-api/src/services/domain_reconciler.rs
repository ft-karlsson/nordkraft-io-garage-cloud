// src/services/domain_reconciler.rs
//
// Background state machine that drives every custom domain toward 'active'
// and keeps it healthy afterwards. Runs on the controller only.
//
//   pending_dns  → dns_verified → issuing_cert → cert_ready → activating → active
//                                                                    ↑         │
//                                                                    └─repair──┤
//         degraded ←──────────── DNS moved away ───────────────────────────────┘
//             │ grace period (CUSTOM_DOMAINS_GRACE_DAYS) expires
//             ▼
//         disabled (full teardown — new domain owner can never receive
//                   the old tenant's traffic)
//
// DESIGN RULES (deliberately slow, never wrong):
// - Every transition is idempotent and re-runnable after a crash.
// - 'active' is only set after READING BACK from pfSense that the backend,
//   ACL, action, and cert binding all exist.
// - DNS ownership needs 2 consecutive authoritative successes.
// - Domains are processed sequentially — the pfSense API is slow and this
//   avoids concurrent config writes entirely.
// - Failures schedule an exponential-backoff retry; nothing is ever retried
//   in a tight loop (Let's Encrypt rate limits are strict).

use crate::services::acme_manager::AcmeManager;
use crate::services::dns_verifier::DnsVerifierTrait;
use crate::services::domain_validation::object_name_component;
use crate::services::haproxy_client::HAProxyClientTrait;
use crate::services::pfsense_client::PfSenseClientTrait;
use crate::storage::{self, CustomDomainDb};

use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

type ReconcileError = Box<dyn std::error::Error + Send + Sync>;

/// How many consecutive successful DNS checks are required before a domain
/// counts as verified.
const REQUIRED_DNS_SUCCESSES: i32 = 2;

/// Generic retry backoff: 60s * 2^retry, capped at 1 hour.
fn generic_backoff(retry_count: i32) -> i64 {
    (60i64 << retry_count.clamp(0, 6)).min(3600)
}

/// ACME retry backoff: 15 min * (retry+1), capped at 6 hours. Let's Encrypt
/// allows 5 validation failures per account/hostname/hour — stay well under.
fn acme_backoff(retry_count: i32) -> i64 {
    (900i64 * (retry_count as i64 + 1)).min(21600)
}

#[derive(Clone)]
pub struct DomainReconcilerSettings {
    /// Seconds between reconciler passes.
    pub interval_seconds: u64,
    /// The public ingress IP customers must point their domains at.
    pub public_ip: String,
    /// Days a degraded (DNS moved away) domain keeps its routing before
    /// automatic teardown.
    pub degraded_grace_days: i64,
    /// Renew certificates this many days before expiry.
    pub renew_before_days: i64,
    /// Steady-state re-check interval for active domains, in seconds.
    pub active_recheck_seconds: i64,
}

impl Default for DomainReconcilerSettings {
    fn default() -> Self {
        Self {
            interval_seconds: 60,
            public_ip: String::new(),
            degraded_grace_days: 7,
            renew_before_days: 30,
            active_recheck_seconds: 86_400,
        }
    }
}

pub struct DomainReconciler {
    pool: PgPool,
    haproxy: Arc<dyn HAProxyClientTrait>,
    pfsense: Arc<dyn PfSenseClientTrait>,
    dns: Arc<dyn DnsVerifierTrait>,
    acme: Arc<AcmeManager>,
    settings: DomainReconcilerSettings,
}

impl DomainReconciler {
    pub fn new(
        pool: PgPool,
        haproxy: Arc<dyn HAProxyClientTrait>,
        pfsense: Arc<dyn PfSenseClientTrait>,
        dns: Arc<dyn DnsVerifierTrait>,
        acme: Arc<AcmeManager>,
        settings: DomainReconcilerSettings,
    ) -> Self {
        Self {
            pool,
            haproxy,
            pfsense,
            dns,
            acme,
            settings,
        }
    }

    /// Spawn the reconciler loop.
    pub fn start(self: Arc<Self>) {
        let interval = self.settings.interval_seconds;
        info!(
            "🌍 Domain reconciler started (interval {}s, grace {}d, renew {}d before expiry)",
            interval, self.settings.degraded_grace_days, self.settings.renew_before_days
        );
        tokio::spawn(async move {
            loop {
                if let Err(e) = self.tick().await {
                    error!("Domain reconciler pass failed: {}", e);
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        });
    }

    async fn tick(&self) -> Result<(), ReconcileError> {
        let domains = storage::get_custom_domains_needing_work(
            &self.pool,
            self.settings.active_recheck_seconds,
        )
        .await?;

        for domain in domains {
            let id = domain.id;
            let name = domain.domain.clone();
            let status = domain.status.clone();
            let retry_count = domain.retry_count;

            if let Err(e) = self.process(domain).await {
                warn!("Domain {} ({}, state {}): {}", name, id, status, e);
                let backoff = if status == "dns_verified" || status == "issuing_cert" {
                    acme_backoff(retry_count)
                } else {
                    generic_backoff(retry_count)
                };
                if let Err(db_err) = storage::schedule_custom_domain_retry(
                    &self.pool,
                    id,
                    retry_count + 1,
                    backoff,
                    &e.to_string(),
                )
                .await
                {
                    error!("Failed to record retry for domain {}: {}", id, db_err);
                }
            }
        }

        Ok(())
    }

    async fn process(&self, d: CustomDomainDb) -> Result<(), ReconcileError> {
        match d.status.as_str() {
            "pending_dns" => self.check_dns(&d).await,
            // 'issuing_cert' is a crash-recovery marker: an order was in
            // flight when we died. Orders are cheap — start a fresh one.
            "dns_verified" | "issuing_cert" => self.issue_certificate(&d).await,
            "cert_ready" | "activating" => self.activate(&d).await,
            "active" => self.steady_state(&d).await,
            "degraded" => self.handle_degraded(&d).await,
            "error" => self.recover_from_error(&d).await,
            other => {
                warn!("Domain {} in unknown state '{}', ignoring", d.domain, other);
                Ok(())
            }
        }
    }

    // ============= pending_dns =============

    async fn check_dns(&self, d: &CustomDomainDb) -> Result<(), ReconcileError> {
        let result = self
            .dns
            .verify_domain(&d.domain, &d.verification_token, &self.settings.public_ip)
            .await?;

        if result.passed() {
            let successes = d.dns_check_successes + 1;
            let verified = successes >= REQUIRED_DNS_SUCCESSES;
            storage::update_custom_domain_dns_progress(
                &self.pool,
                d.id,
                successes,
                verified,
                &result.detail,
            )
            .await?;
            if verified {
                info!(
                    "✅ Domain {} DNS-verified ({} consecutive checks)",
                    d.domain, successes
                );
            } else {
                info!(
                    "Domain {} DNS check {}/{} passed — confirming on next pass",
                    d.domain, successes, REQUIRED_DNS_SUCCESSES
                );
            }
        } else {
            // Not an error — the customer just hasn't (correctly) published
            // the records yet. Reset the consecutive counter.
            storage::update_custom_domain_dns_progress(&self.pool, d.id, 0, false, &result.detail)
                .await?;
        }

        Ok(())
    }

    // ============= dns_verified → cert_ready =============

    async fn issue_certificate(&self, d: &CustomDomainDb) -> Result<(), ReconcileError> {
        // Re-verify ownership right before issuance — DNS may have changed
        // between the verification pass and now.
        let dns = self
            .dns
            .verify_domain(&d.domain, &d.verification_token, &self.settings.public_ip)
            .await?;
        if !dns.passed() {
            storage::update_custom_domain_dns_progress(&self.pool, d.id, 0, false, &dns.detail)
                .await?;
            storage::update_custom_domain_status(
                &self.pool,
                d.id,
                "pending_dns",
                Some("DNS records changed before certificate issuance — re-verifying"),
            )
            .await?;
            return Ok(());
        }

        storage::update_custom_domain_status(&self.pool, d.id, "issuing_cert", None).await?;

        let issued = self.acme.issue_certificate(&d.domain).await?;

        let cert_name = format!("nk_cd_{}_{}", d.id, object_name_component(&d.domain));
        let refid = self
            .haproxy
            .upload_certificate(&cert_name, &issued.cert_chain_pem, &issued.private_key_pem)
            .await?;

        storage::set_custom_domain_cert(&self.pool, d.id, &refid, issued.expires_at).await?;
        info!(
            "✅ Domain {}: certificate issued and uploaded (refid {}, expires {})",
            d.domain, refid, issued.expires_at
        );
        Ok(())
    }

    // ============= cert_ready / activating → active =============

    /// Create (or repair) every pfSense object for this domain, then read
    /// everything back before marking active. Fully idempotent.
    async fn activate(&self, d: &CustomDomainDb) -> Result<(), ReconcileError> {
        storage::update_custom_domain_status(&self.pool, d.id, "activating", None).await?;

        let cert_refid = d
            .cert_refid
            .as_deref()
            .ok_or("activate called without a certificate refid")?;

        // 1. Bind cert (SNI) — idempotent.
        self.haproxy
            .bind_certificate_to_https_frontend(cert_refid)
            .await?;

        // 2. Resolve the container's current IP + node (ownership enforced
        //    by user_id in the query).
        let target_ip = storage::get_container_ipv4(&self.pool, &d.container_id, &d.user_id)
            .await?
            .ok_or("Container has no routable IPv4 address (was it deleted?)")?;

        let node_lan_ip = storage::get_container_node_info(&self.pool, &d.container_id, &d.user_id)
            .await?
            .map(|(_, lan_ip)| lan_ip)
            .ok_or("Could not determine the container's host node")?;

        // 3. Static route so pfSense/HAProxy can reach the container.
        //    add_static_route is idempotent (checks by destination first).
        let static_route_ok = match self
            .pfsense
            .add_static_route(
                &format!("{}/32", target_ip),
                &node_lan_ip,
                &format!("custom-domain {}", d.domain),
            )
            .await
        {
            Ok(_) => true,
            Err(e) => {
                warn!(
                    "Domain {}: static route creation failed ({}); will retry",
                    d.domain, e
                );
                false
            }
        };
        if !static_route_ok {
            return Err("Static route creation failed".into());
        }

        // 4. Backend + exact-host ACL + action on the HTTPS frontend.
        let name_prefix = format!("cd_{}_{}", d.id, object_name_component(&d.domain));
        let result = self
            .haproxy
            .create_custom_domain_ingress(&name_prefix, &d.domain, &target_ip, d.target_port as u16)
            .await?;

        storage::set_custom_domain_routing(
            &self.pool,
            d.id,
            &result.backend_name,
            &result.acl_name,
            &result.server_name,
            &target_ip,
            true,
        )
        .await?;

        // 5. READ BACK — never mark active optimistically.
        let routing_ok = self
            .haproxy
            .verify_custom_domain_ingress(&result.backend_name, &result.acl_name)
            .await?;
        let cert_ok = self
            .haproxy
            .https_frontend_has_certificate(cert_refid)
            .await?;

        if routing_ok && cert_ok {
            storage::mark_custom_domain_active(&self.pool, d.id).await?;
            info!("🎉 Custom domain ACTIVE: https://{}", d.domain);
            Ok(())
        } else {
            Err(format!(
                "Read-back verification failed (routing: {}, cert bound: {}) — will retry",
                routing_ok, cert_ok
            )
            .into())
        }
    }

    // ============= active (steady state) =============

    async fn steady_state(&self, d: &CustomDomainDb) -> Result<(), ReconcileError> {
        // a) Does the domain still point at us? If not → degraded.
        let still_ours = self
            .dns
            .resolves_to(&d.domain, &self.settings.public_ip)
            .await?;
        if !still_ours {
            warn!(
                "⚠️ Domain {} no longer resolves to {} — marking degraded (grace {} days)",
                d.domain, self.settings.public_ip, self.settings.degraded_grace_days
            );
            storage::mark_custom_domain_degraded(
                &self.pool,
                d.id,
                &format!(
                    "Domain no longer resolves to ingress IP {} — routing will be removed after {} days unless DNS is restored",
                    self.settings.public_ip, self.settings.degraded_grace_days
                ),
            )
            .await?;
            return Ok(());
        }

        // b) Certificate renewal.
        if let Some(expires_at) = d.cert_expires_at {
            let renew_at = expires_at - chrono::Duration::days(self.settings.renew_before_days);
            if chrono::Utc::now() >= renew_at {
                info!(
                    "🔄 Domain {}: certificate expires {}, renewing now",
                    d.domain, expires_at
                );
                self.renew_certificate(d).await?;
            }
        }

        // c) Container IP drift (redeploys change IPs).
        let current_ip = storage::get_container_ipv4(&self.pool, &d.container_id, &d.user_id)
            .await?
            .ok_or("Container has no IPv4 address (was it deleted?)")?;
        let ip_changed = d.target_ip.as_deref() != Some(current_ip.as_str());

        // d) pfSense object drift (reboots, manual edits).
        let routing_ok = match (&d.haproxy_backend_name, &d.haproxy_acl_name) {
            (Some(backend), Some(acl)) => {
                self.haproxy
                    .verify_custom_domain_ingress(backend, acl)
                    .await?
            }
            _ => false,
        };

        if ip_changed || !routing_ok {
            warn!(
                "🔧 Domain {}: repairing routing (ip_changed: {}, objects present: {})",
                d.domain, ip_changed, routing_ok
            );
            if ip_changed {
                // Tear down stale objects (old IP) before recreating.
                if let (Some(backend), Some(acl)) = (&d.haproxy_backend_name, &d.haproxy_acl_name) {
                    if let Err(e) = self.haproxy.remove_https_ingress(backend, acl).await {
                        warn!("Domain {}: stale routing cleanup: {}", d.domain, e);
                    }
                }
                if let Some(old_ip) = &d.target_ip {
                    if let Err(e) = self
                        .pfsense
                        .remove_static_route_by_destination(&format!("{}/32", old_ip))
                        .await
                    {
                        warn!("Domain {}: stale static route cleanup: {}", d.domain, e);
                    }
                }
            }
            // activate() recreates everything idempotently and re-verifies.
            self.activate(d).await?;
            return Ok(());
        }

        // All healthy — just stamp the check time.
        storage::update_custom_domain_status(&self.pool, d.id, "active", None).await?;
        Ok(())
    }

    async fn renew_certificate(&self, d: &CustomDomainDb) -> Result<(), ReconcileError> {
        let issued = self.acme.issue_certificate(&d.domain).await?;

        let cert_name = format!(
            "nk_cd_{}_{}_r{}",
            d.id,
            object_name_component(&d.domain),
            chrono::Utc::now().format("%Y%m%d")
        );
        let new_refid = self
            .haproxy
            .upload_certificate(&cert_name, &issued.cert_chain_pem, &issued.private_key_pem)
            .await?;

        // Bind new before unbinding old — no TLS gap.
        self.haproxy
            .bind_certificate_to_https_frontend(&new_refid)
            .await?;

        if let Some(old_refid) = &d.cert_refid {
            if let Err(e) = self
                .haproxy
                .unbind_certificate_from_https_frontend(old_refid)
                .await
            {
                warn!("Domain {}: unbinding old cert: {}", d.domain, e);
            }
            if let Err(e) = self.haproxy.delete_certificate(old_refid).await {
                warn!("Domain {}: deleting old cert: {}", d.domain, e);
            }
        }

        storage::set_custom_domain_cert(&self.pool, d.id, &new_refid, issued.expires_at).await?;
        // set_custom_domain_cert sets status cert_ready; restore active —
        // routing was untouched.
        storage::mark_custom_domain_active(&self.pool, d.id).await?;
        info!(
            "✅ Domain {}: certificate renewed (expires {})",
            d.domain, issued.expires_at
        );
        Ok(())
    }

    // ============= degraded =============

    async fn handle_degraded(&self, d: &CustomDomainDb) -> Result<(), ReconcileError> {
        // DNS restored? Verify routing and go back to active.
        if self
            .dns
            .resolves_to(&d.domain, &self.settings.public_ip)
            .await?
        {
            info!("✅ Domain {}: DNS restored, re-activating", d.domain);
            self.activate(d).await?;
            return Ok(());
        }

        let since = d.degraded_since.unwrap_or(d.created_at);
        let deadline = since + chrono::Duration::days(self.settings.degraded_grace_days);
        if chrono::Utc::now() >= deadline {
            warn!(
                "🗑️ Domain {}: degraded since {}, grace expired — tearing down",
                d.domain, since
            );
            teardown_custom_domain(&self.pool, &self.haproxy, &self.pfsense, d).await;
            storage::update_custom_domain_status(
                &self.pool,
                d.id,
                "disabled",
                Some("Auto-disabled: DNS stopped pointing at the platform and the grace period expired. Remove and re-add the domain to start over."),
            )
            .await?;
        } else {
            storage::mark_custom_domain_degraded(
                &self.pool,
                d.id,
                &format!(
                    "Domain does not resolve to ingress IP; teardown at {}",
                    deadline
                ),
            )
            .await?;
        }
        Ok(())
    }

    // ============= error recovery =============

    async fn recover_from_error(&self, d: &CustomDomainDb) -> Result<(), ReconcileError> {
        // Resume from the furthest state the row's artifacts support.
        let resume = if d.cert_refid.is_some() {
            "cert_ready"
        } else if d.verified_at.is_some() {
            "dns_verified"
        } else {
            "pending_dns"
        };
        info!(
            "Domain {}: retrying from state '{}' after error",
            d.domain, resume
        );
        storage::update_custom_domain_status(&self.pool, d.id, resume, d.last_error.as_deref())
            .await?;
        Ok(())
    }
}

/// Reverse-order teardown of every pfSense artifact for a domain. Tolerant of
/// already-missing pieces — used by auto-disable and by user-requested
/// removal. Never fails: each step logs, records a warning, and continues.
pub async fn teardown_custom_domain(
    pool: &PgPool,
    haproxy: &Arc<dyn HAProxyClientTrait>,
    pfsense: &Arc<dyn PfSenseClientTrait>,
    d: &CustomDomainDb,
) -> Vec<String> {
    let mut warnings = Vec::new();

    // 1. Routing: action → ACL → backend (remove_https_ingress handles order),
    //    plus the HTTP→HTTPS redirect objects on the HTTP frontend.
    if let (Some(backend), Some(acl)) = (&d.haproxy_backend_name, &d.haproxy_acl_name) {
        if let Err(e) = haproxy.remove_https_ingress(backend, acl).await {
            warnings.push(format!("HAProxy cleanup: {}", e));
        }
        if let Err(e) = haproxy.remove_custom_domain_http_redirect(acl).await {
            warnings.push(format!("HTTP redirect cleanup: {}", e));
        }
    }

    // 2. Certificate: unbind from frontend, then delete from store
    if let Some(refid) = &d.cert_refid {
        if let Err(e) = haproxy.unbind_certificate_from_https_frontend(refid).await {
            warnings.push(format!("Cert unbind: {}", e));
        }
        if let Err(e) = haproxy.delete_certificate(refid).await {
            warnings.push(format!("Cert delete: {}", e));
        }
    }

    // 3. Static route (by destination — IDs shift on pfSense reboots).
    //    The /32 route is per-container-IP and SHARED with platform ingress
    //    and sibling custom domains, so only remove it when nothing else
    //    still references the same IP.
    if d.static_route_created {
        if let Some(ip) = &d.target_ip {
            match storage::ip_still_routed(pool, ip, d.id).await {
                Ok(true) => {
                    info!(
                        "Teardown {}: keeping static route for {} (still used by another route)",
                        d.domain, ip
                    );
                }
                Ok(false) => {
                    if let Err(e) = pfsense
                        .remove_static_route_by_destination(&format!("{}/32", ip))
                        .await
                    {
                        warnings.push(format!("Static route cleanup: {}", e));
                    }
                }
                Err(e) => {
                    // Fail safe: keep the route rather than risk breaking a
                    // sibling — an orphaned /32 is harmless.
                    warnings.push(format!(
                        "Could not check static-route sharing ({}); route kept",
                        e
                    ));
                }
            }
        }
    }

    for w in &warnings {
        warn!("Teardown {} : {}", d.domain, w);
    }
    warnings
}
