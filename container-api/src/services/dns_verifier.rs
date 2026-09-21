// src/services/dns_verifier.rs
//
// DNS verification for custom domains.
//
// SECURITY MODEL:
// - The ownership proof (TXT token at _nordkraft-challenge.<domain>) is
//   checked against the domain's AUTHORITATIVE nameservers, discovered by
//   walking up the name until NS records are found and resolving those NS
//   hosts via a public recursive resolver. Caches are disabled. A recursive
//   cache is never trusted for the ownership decision.
// - The routing check (domain resolves to INGRESS_PUBLIC_IP) uses a public
//   recursive resolver, because it must follow CNAME chains across zones
//   (e.g. www.customer1.net → myapp.nordkraft.cloud → A). This check is
//   operational, not an ownership proof — ownership rests on the random TXT
//   token plus Let's Encrypt's independent HTTP-01 validation.
//
// The reconciler additionally requires 2 consecutive successful verifications
// before advancing a domain, and re-checks active domains daily.

use async_trait::async_trait;
use hickory_resolver::config::{NameServerConfigGroup, ResolverConfig, ResolverOpts};
use hickory_resolver::TokioAsyncResolver;
use std::net::IpAddr;
use std::time::Duration;
use tracing::{debug, info, warn};

pub type DnsError = Box<dyn std::error::Error + Send + Sync>;

/// Outcome of a full verification pass. `txt_ok` is the ownership proof;
/// `a_ok` is the routing check. Both must hold to advance the state machine.
#[derive(Debug, Clone)]
pub struct DnsCheckResult {
    pub txt_ok: bool,
    pub a_ok: bool,
    /// Human-readable detail for `last_error` / CLI status output.
    pub detail: String,
}

impl DnsCheckResult {
    pub fn passed(&self) -> bool {
        self.txt_ok && self.a_ok
    }
}

#[async_trait]
pub trait DnsVerifierTrait: Send + Sync {
    /// Verify the ownership TXT token (authoritative) and the A/CNAME routing
    /// (recursive) for `domain`. Never returns Err for "record not found" —
    /// that is a normal not-yet-propagated result carried in DnsCheckResult.
    /// Err means the check itself could not be performed (network failure).
    async fn verify_domain(
        &self,
        domain: &str,
        token: &str,
        expected_ip: &str,
    ) -> Result<DnsCheckResult, DnsError>;

    /// Routing-only check used for steady-state monitoring of active domains.
    async fn resolves_to(&self, domain: &str, expected_ip: &str) -> Result<bool, DnsError>;
}

// ============= REAL IMPLEMENTATION =============

pub struct DnsVerifier {
    /// Public recursive resolver (Cloudflare + Google) for NS discovery and
    /// the routing check.
    recursive: TokioAsyncResolver,
}

impl DnsVerifier {
    pub fn new() -> Self {
        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_secs(10);
        opts.attempts = 2;
        // No caching — every reconciler pass must observe live DNS.
        opts.cache_size = 0;

        let mut group = NameServerConfigGroup::cloudflare();
        group.merge(NameServerConfigGroup::google());
        let config = ResolverConfig::from_parts(None, vec![], group);

        Self {
            recursive: TokioAsyncResolver::tokio(config, opts),
        }
    }

    /// Discover the authoritative nameserver IPs for `domain` by walking up
    /// the name until NS records are found.
    async fn authoritative_ips(&self, domain: &str) -> Result<Vec<IpAddr>, DnsError> {
        let mut candidate = domain.to_string();

        loop {
            match self.recursive.ns_lookup(candidate.as_str()).await {
                Ok(ns) => {
                    let mut ips = Vec::new();
                    for record in ns.iter() {
                        let host = record.0.to_utf8();
                        match self.recursive.lookup_ip(host.as_str()).await {
                            Ok(addrs) => ips.extend(addrs.iter()),
                            Err(e) => {
                                debug!("Could not resolve NS host {}: {}", host, e);
                            }
                        }
                    }
                    if !ips.is_empty() {
                        debug!(
                            "Authoritative NS for {} (zone {}): {:?}",
                            domain, candidate, ips
                        );
                        return Ok(ips);
                    }
                    // NS records existed but none resolved — treat as failure.
                    return Err(format!(
                        "NS records for zone '{}' did not resolve to any IP",
                        candidate
                    )
                    .into());
                }
                Err(_) => {
                    // Walk up one label; stop before we hit a bare suffix.
                    match candidate.split_once('.') {
                        Some((_, parent)) if parent.contains('.') => {
                            candidate = parent.to_string();
                        }
                        _ => {
                            return Err(format!(
                                "No authoritative nameservers found for {}",
                                domain
                            )
                            .into())
                        }
                    }
                }
            }
        }
    }

    /// Build a resolver that queries ONLY the given (authoritative) servers.
    fn resolver_for(&self, ips: &[IpAddr]) -> TokioAsyncResolver {
        let mut opts = ResolverOpts::default();
        opts.timeout = Duration::from_secs(10);
        opts.attempts = 2;
        opts.cache_size = 0;
        // Authoritative servers answer for their zones without recursion.
        opts.recursion_desired = false;

        let group = NameServerConfigGroup::from_ips_clear(ips, 53, true);
        let config = ResolverConfig::from_parts(None, vec![], group);
        TokioAsyncResolver::tokio(config, opts)
    }

    /// Check the TXT ownership token against the authoritative servers.
    async fn check_txt_authoritative(
        &self,
        domain: &str,
        token: &str,
    ) -> Result<(bool, String), DnsError> {
        let record_name = super::domain_validation::challenge_record_name(domain);
        let expected = super::domain_validation::challenge_record_value(token);

        let auth_ips = match self.authoritative_ips(domain).await {
            Ok(ips) => ips,
            Err(e) => return Ok((false, format!("NS discovery failed: {}", e))),
        };

        let auth_resolver = self.resolver_for(&auth_ips);

        match auth_resolver.txt_lookup(record_name.as_str()).await {
            Ok(txts) => {
                for txt in txts.iter() {
                    let value: String = txt
                        .iter()
                        .map(|part| String::from_utf8_lossy(part).to_string())
                        .collect();
                    if value.trim().trim_matches('"') == expected {
                        return Ok((true, "TXT token verified (authoritative)".to_string()));
                    }
                }
                Ok((
                    false,
                    format!(
                        "TXT record at {} exists but no value matches the verification token",
                        record_name
                    ),
                ))
            }
            Err(e) => Ok((
                false,
                format!("TXT record not found at {} ({})", record_name, e),
            )),
        }
    }
}

impl Default for DnsVerifier {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DnsVerifierTrait for DnsVerifier {
    async fn verify_domain(
        &self,
        domain: &str,
        token: &str,
        expected_ip: &str,
    ) -> Result<DnsCheckResult, DnsError> {
        let (txt_ok, txt_detail) = self.check_txt_authoritative(domain, token).await?;

        let (a_ok, a_detail) = match self.resolves_to(domain, expected_ip).await {
            Ok(true) => (true, format!("{} resolves to {}", domain, expected_ip)),
            Ok(false) => (
                false,
                format!(
                    "{} does not resolve to the ingress IP {} — add an A record (apex) or CNAME (subdomain)",
                    domain, expected_ip
                ),
            ),
            Err(e) => (false, format!("A/CNAME lookup failed: {}", e)),
        };

        let result = DnsCheckResult {
            txt_ok,
            a_ok,
            detail: format!("{}; {}", txt_detail, a_detail),
        };

        if result.passed() {
            info!("✅ DNS verification passed for {}", domain);
        } else {
            debug!(
                "DNS verification incomplete for {}: {}",
                domain, result.detail
            );
        }

        Ok(result)
    }

    async fn resolves_to(&self, domain: &str, expected_ip: &str) -> Result<bool, DnsError> {
        let expected: IpAddr = expected_ip
            .parse()
            .map_err(|e| format!("Invalid expected IP '{}': {}", expected_ip, e))?;

        match self.recursive.lookup_ip(domain).await {
            Ok(addrs) => Ok(addrs.iter().any(|ip| ip == expected)),
            Err(e) => {
                debug!("lookup_ip failed for {}: {}", domain, e);
                Ok(false)
            }
        }
    }
}

// ============= DUMMY (DEV_MODE / tests) =============

/// Always-verified stub so the full state machine can be exercised in
/// development without real DNS. Logs loudly so it can never be mistaken
/// for the real thing in production logs.
pub struct DummyDnsVerifier;

#[async_trait]
impl DnsVerifierTrait for DummyDnsVerifier {
    async fn verify_domain(
        &self,
        domain: &str,
        _token: &str,
        _expected_ip: &str,
    ) -> Result<DnsCheckResult, DnsError> {
        warn!("⚠️ DEV MODE: DNS verification bypassed for {}", domain);
        Ok(DnsCheckResult {
            txt_ok: true,
            a_ok: true,
            detail: "dev mode bypass".to_string(),
        })
    }

    async fn resolves_to(&self, domain: &str, _expected_ip: &str) -> Result<bool, DnsError> {
        warn!("⚠️ DEV MODE: routing check bypassed for {}", domain);
        Ok(true)
    }
}
