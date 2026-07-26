// src/services/acme_manager.rs
//
// Controller-managed ACME (Let's Encrypt) issuance for custom domains.
//
// Why controller-managed instead of the pfSense ACME package:
// ONE state machine, in our database, observable and testable. The pfSense
// package splits state between our DB and pfSense's cron, and its per-domain
// lifecycle is not fully drivable via the REST API.
//
// Challenge flow (HTTP-01):
//   1. Let's Encrypt requests http://<domain>/.well-known/acme-challenge/<token>
//   2. That hits pfSense HAProxy port 80/443, where a bootstrap path-ACL
//      (created at startup, see haproxy_client::ensure_acme_challenge_route)
//      routes it to container-api's own Rocket route.
//   3. The route answers from the in-memory ChallengeStore below.
//
// The private key is generated here per order (rcgen), never reused, and is
// handed to pfSense's cert store together with the issued chain. We keep only
// the pfSense cert refid + expiry in our database — never key material.

use instant_acme::{
    Account, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub type AcmeError = Box<dyn std::error::Error + Send + Sync>;

/// Shared token → key-authorization map, served by the Rocket challenge route.
#[derive(Clone, Default)]
pub struct ChallengeStore {
    inner: Arc<RwLock<HashMap<String, String>>>,
}

impl ChallengeStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn put(&self, token: &str, key_auth: &str) {
        self.inner
            .write()
            .await
            .insert(token.to_string(), key_auth.to_string());
    }

    pub async fn get(&self, token: &str) -> Option<String> {
        self.inner.read().await.get(token).cloned()
    }

    pub async fn remove(&self, token: &str) {
        self.inner.write().await.remove(token);
    }
}

/// Result of a successful issuance.
pub struct IssuedCertificate {
    /// Full PEM chain (leaf first) as returned by the CA.
    pub cert_chain_pem: String,
    /// PKCS#8 PEM private key generated for this order.
    pub private_key_pem: String,
    /// notAfter of the leaf certificate.
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

pub struct AcmeManager {
    contact_email: String,
    staging: bool,
    challenge_store: ChallengeStore,
}

impl AcmeManager {
    pub fn new(contact_email: String, staging: bool, challenge_store: ChallengeStore) -> Self {
        Self {
            contact_email,
            staging,
            challenge_store,
        }
    }

    pub fn directory_url(&self) -> &'static str {
        if self.staging {
            LetsEncrypt::Staging.url()
        } else {
            LetsEncrypt::Production.url()
        }
    }

    /// Issue a certificate for exactly one hostname via HTTP-01.
    ///
    /// Deliberately slow-and-careful: generous poll intervals, bounded
    /// retries, and challenge tokens are always cleaned from the store —
    /// success or failure. A fresh ACME account per issuance keeps this
    /// stateless; Let's Encrypt allows this and it avoids persisting account
    /// keys (rate limits are per-domain and per-IP, not per-account-creation
    /// in any way that affects our volumes).
    pub async fn issue_certificate(&self, domain: &str) -> Result<IssuedCertificate, AcmeError> {
        info!(
            "🔏 ACME: starting order for {} ({})",
            domain,
            if self.staging {
                "STAGING"
            } else {
                "production"
            }
        );

        let contact = format!("mailto:{}", self.contact_email);
        let (account, _credentials) = Account::create(
            &NewAccount {
                contact: &[contact.as_str()],
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            self.directory_url(),
            None,
        )
        .await?;

        let identifier = Identifier::Dns(domain.to_string());
        let mut order = account
            .new_order(&NewOrder {
                identifiers: &[identifier],
            })
            .await?;

        let authorizations = order.authorizations().await?;
        let mut tokens: Vec<String> = Vec::new();

        for authz in &authorizations {
            match authz.status {
                AuthorizationStatus::Pending => {}
                AuthorizationStatus::Valid => continue,
                status => {
                    return Err(format!(
                        "ACME authorization for {} in unexpected state: {:?}",
                        domain, status
                    )
                    .into());
                }
            }

            let challenge = authz
                .challenges
                .iter()
                .find(|c| c.r#type == ChallengeType::Http01)
                .ok_or("CA offered no HTTP-01 challenge")?;

            let key_auth = order.key_authorization(challenge);
            self.challenge_store
                .put(&challenge.token, key_auth.as_str())
                .await;
            tokens.push(challenge.token.clone());

            info!(
                "🔏 ACME: challenge staged for {} (token {}…)",
                domain,
                &challenge.token[..8.min(challenge.token.len())]
            );

            order.set_challenge_ready(&challenge.url).await?;
        }

        // Poll the order until it leaves the validation phase.
        // Slow is fine — bulletproof beats fast.
        let result = self.poll_and_finalize(&mut order, domain).await;

        // Always clean up challenge tokens.
        for token in &tokens {
            self.challenge_store.remove(token).await;
        }

        result
    }

    async fn poll_and_finalize(
        &self,
        order: &mut instant_acme::Order,
        domain: &str,
    ) -> Result<IssuedCertificate, AcmeError> {
        let mut delay = Duration::from_secs(2);
        let mut attempts = 0u32;

        let state = loop {
            tokio::time::sleep(delay).await;
            let state = order.refresh().await?;
            match state.status {
                OrderStatus::Ready | OrderStatus::Invalid | OrderStatus::Valid => break state,
                _ => {
                    attempts += 1;
                    if attempts >= 15 {
                        return Err(format!(
                            "ACME order for {} did not become ready after {} polls",
                            domain, attempts
                        )
                        .into());
                    }
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        };

        if state.status == OrderStatus::Invalid {
            return Err(format!(
                "ACME order for {} became invalid — the CA could not validate the HTTP-01 challenge. \
                 Check that the domain points at the ingress IP and port 80 reaches HAProxy.",
                domain
            )
            .into());
        }

        // Generate key + CSR for exactly this hostname.
        let mut params = rcgen::CertificateParams::new(vec![domain.to_string()])?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        let key_pair = rcgen::KeyPair::generate()?;
        let csr = params.serialize_request(&key_pair)?;

        order.finalize(csr.der()).await?;

        // Fetch the certificate (may need a few polls).
        let cert_chain_pem = {
            let mut attempts = 0u32;
            loop {
                match order.certificate().await? {
                    Some(chain) => break chain,
                    None => {
                        attempts += 1;
                        if attempts >= 10 {
                            return Err(format!(
                                "ACME finalize succeeded but certificate for {} never appeared",
                                domain
                            )
                            .into());
                        }
                        tokio::time::sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        };

        let expires_at = leaf_not_after(&cert_chain_pem).unwrap_or_else(|e| {
            warn!(
                "Could not parse notAfter from issued cert for {} ({}); assuming 60 days",
                domain, e
            );
            chrono::Utc::now() + chrono::Duration::days(60)
        });

        info!(
            "✅ ACME: certificate issued for {} (expires {})",
            domain, expires_at
        );

        Ok(IssuedCertificate {
            cert_chain_pem,
            private_key_pem: key_pair.serialize_pem(),
            expires_at,
        })
    }
}

/// Parse notAfter from the first (leaf) certificate in a PEM chain.
fn leaf_not_after(chain_pem: &str) -> Result<chrono::DateTime<chrono::Utc>, AcmeError> {
    let (_, pem) = x509_parser::pem::parse_x509_pem(chain_pem.as_bytes())
        .map_err(|e| format!("PEM parse error: {}", e))?;
    let cert = pem
        .parse_x509()
        .map_err(|e| format!("X509 parse error: {}", e))?;
    let ts = cert.validity().not_after.timestamp();
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .ok_or_else(|| "Invalid notAfter timestamp".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn challenge_store_roundtrip() {
        let store = ChallengeStore::new();
        store.put("tok", "tok.keyauth").await;
        assert_eq!(store.get("tok").await.as_deref(), Some("tok.keyauth"));
        store.remove("tok").await;
        assert_eq!(store.get("tok").await, None);
    }

    #[test]
    fn staging_flag_selects_directory() {
        let store = ChallengeStore::new();
        let staging = AcmeManager::new("a@b.dk".into(), true, store.clone());
        let production = AcmeManager::new("a@b.dk".into(), false, store);
        assert!(staging.directory_url().contains("staging"));
        assert!(!production.directory_url().contains("staging"));
    }
}
