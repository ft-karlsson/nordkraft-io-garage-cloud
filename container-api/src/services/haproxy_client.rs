// src/services/haproxy_client.rs
//
// HAProxy management via pfSense REST API v2.
// Manages backends, servers, ACLs, and actions for ingress routing.
//
// SIMPLIFIED FOR WILDCARD CERT:
// =============================
// The wildcard certificate *.example.dk is ALREADY bound to the HTTPS frontend.
// No per-subdomain certificate operations needed!
//
// For HTTPS ingress, we only:
//   1. Create a backend pointing to container IP:port
//   2. Add a Host header ACL matching subdomain.example.dk
//   3. Add a use_backend action routing ACL → backend
//
// CRITICAL: Deletion order must be:
//   1. Delete ACTION (use_backend) → Apply
//   2. Delete ACL → Apply
//   3. Delete backend → Apply
//
// API Structure (pfSense REST API v2):
//   - POST /api/v2/services/haproxy/backend - Create backend
//   - POST /api/v2/services/haproxy/backend/server - Add server (with parent_id in body!)
//   - POST /api/v2/services/haproxy/frontend/acl - Add ACL (with parent_id in body!)
//   - POST /api/v2/services/haproxy/frontend/action - Add action (with parent_id in body!)
//   - DELETE endpoints use query params: ?parent_id=X&id=Y

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, info, warn};

// ============= RESULT TYPES =============

#[derive(Debug, Clone)]
pub struct HttpIngressResult {
    pub backend_name: String,
    pub server_name: String,
    pub acl_name: String,
}

#[derive(Debug, Clone)]
pub struct HttpsIngressResult {
    pub backend_name: String,
    pub server_name: String,
    pub acl_name: String,
}

#[derive(Debug, Clone)]
pub struct TcpIngressResult {
    pub backend_name: String,
    pub server_name: String,
    pub frontend_name: String,
}

/// Result of creating custom-domain routing objects (BYO domain).
#[derive(Debug, Clone)]
pub struct CustomDomainIngressResult {
    pub backend_name: String,
    pub server_name: String,
    pub acl_name: String,
}

/// Shared names for the ACME HTTP-01 challenge plumbing, created once at
/// startup by ensure_acme_challenge_route(). The path ACL routes
/// /.well-known/acme-challenge/* on BOTH frontends to container-api.
pub const ACME_CHALLENGE_BACKEND: &str = "nk_acme_challenge";
pub const ACME_CHALLENGE_SERVER: &str = "nk_acme_srv";
pub const ACME_CHALLENGE_ACL: &str = "nk_acme_path";
pub const ACME_CHALLENGE_PATH: &str = "/.well-known/acme-challenge/";

// ============= TRAIT =============

#[async_trait]
pub trait HAProxyClientTrait: Send + Sync {
    fn get_base_domain(&self) -> &str;
    fn get_public_ip(&self) -> &str;

    /// Create HTTP ingress (port 80, no TLS)
    async fn create_http_ingress(
        &self,
        subdomain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<HttpIngressResult, Box<dyn std::error::Error + Send + Sync>>;

    async fn remove_http_ingress(
        &self,
        backend_name: &str,
        acl_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Create HTTPS ingress with TLS offload via wildcard cert
    ///
    /// The wildcard cert *.example.dk is already bound to the HTTPS frontend.
    /// This just creates backend + ACL + action. No certificate operations!
    async fn create_https_ingress(
        &self,
        subdomain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<HttpsIngressResult, Box<dyn std::error::Error + Send + Sync>>;

    async fn remove_https_ingress(
        &self,
        backend_name: &str,
        acl_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Create TCP ingress with dedicated port
    async fn create_tcp_ingress(
        &self,
        subdomain: &str,
        public_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<TcpIngressResult, Box<dyn std::error::Error + Send + Sync>>;

    async fn remove_tcp_ingress(
        &self,
        frontend_name: &str,
        backend_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    async fn apply(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    // ============= CUSTOM DOMAINS (BYO domain) =============

    /// Idempotently create the shared ACME challenge backend + path ACL +
    /// action on BOTH frontends. Called at startup. `challenge_addr` is
    /// "ip:port" of container-api's own challenge endpoint.
    async fn ensure_acme_challenge_route(
        &self,
        challenge_addr: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Upload a PEM cert chain + private key to the pfSense certificate
    /// store. Returns the pfSense cert refid.
    async fn upload_certificate(
        &self,
        name: &str,
        cert_chain_pem: &str,
        private_key_pem: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>>;

    /// Remove a certificate from the pfSense store by refid. Idempotent.
    async fn delete_certificate(
        &self,
        refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Attach a certificate to the HTTPS frontend's additional-certificates
    /// list (SNI selects it at runtime). Idempotent.
    async fn bind_certificate_to_https_frontend(
        &self,
        cert_refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Detach a certificate from the HTTPS frontend. Idempotent.
    async fn unbind_certificate_from_https_frontend(
        &self,
        cert_refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    /// Create routing for a custom domain on the HTTPS frontend: dedicated
    /// backend + EXACT host-match ACL + use_backend action. `name_prefix`
    /// must be derived from the DB row id (e.g. "cd_17_customer1_net") —
    /// never from raw user input.
    async fn create_custom_domain_ingress(
        &self,
        name_prefix: &str,
        full_domain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<CustomDomainIngressResult, Box<dyn std::error::Error + Send + Sync>>;

    /// Read-back verification: do the backend, ACL, and action for this
    /// custom domain all currently exist on pfSense? Used before a domain is
    /// ever marked 'active', and for steady-state drift detection.
    async fn verify_custom_domain_ingress(
        &self,
        backend_name: &str,
        acl_name: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;

    /// Read-back verification for cert binding on the HTTPS frontend.
    async fn https_frontend_has_certificate(
        &self,
        cert_refid: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>>;
}

// ============= PFSENSE HAPROXY CLIENT =============

#[derive(Clone)]
pub struct HAProxyClient {
    client: Client,
    base_url: String,
    api_key: String,
    base_domain: String,
    public_ip: String,
    http_frontend: String,
    https_frontend: String,
}

#[derive(Debug, Serialize)]
struct CreateBackendRequest {
    name: String,
    mode: String,
    balance: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    httpcheck_method: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateServerRequest {
    parent_id: i64,
    name: String,
    address: String,
    port: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weight: Option<i32>,
}

#[derive(Debug, Serialize)]
struct CreateFrontendRequest {
    name: String,
    mode: String,
    bind: String,
    default_backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[derive(Debug, Serialize)]
struct CreateAclRequest {
    parent_id: i64,
    name: String,
    expression: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    casesensitive: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    not: Option<bool>,
}

#[derive(Debug, Serialize)]
struct CreateActionRequest {
    parent_id: i64,
    action: String,
    acl: String,
    backend: String,
}

#[derive(Debug, Deserialize)]
struct PfSenseResponse {
    #[allow(dead_code)]
    code: Option<i32>,
    #[allow(dead_code)]
    status: Option<String>,
    #[allow(dead_code)]
    response_id: Option<String>,
    #[allow(dead_code)]
    message: Option<String>,
    data: Option<serde_json::Value>,
}

impl HAProxyClient {
    pub fn new(
        base_url: String,
        api_key: String,
        base_domain: String,
        public_ip: String,
        http_frontend: String,
        https_frontend: String,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            base_domain,
            public_ip,
            http_frontend,
            https_frontend,
        })
    }

    async fn api_request<T: Serialize>(
        &self,
        method: &str,
        endpoint: &str,
        body: Option<&T>,
    ) -> Result<PfSenseResponse, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}{}", self.base_url, endpoint);
        debug!("HAProxy API {} {}", method, url);

        let mut request = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            "PATCH" => self.client.patch(&url),
            _ => return Err(format!("Unsupported method: {}", method).into()),
        };

        request = request.header("X-API-Key", &self.api_key);

        if let Some(b) = body {
            let body_json = serde_json::to_string(b).unwrap_or_default();
            debug!("Request body: {}", body_json);
            request = request.header("Content-Type", "application/json").json(b);
        }

        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;

        debug!("HAProxy API response: {} - {}", status, text);

        if !status.is_success() {
            return Err(format!("HAProxy API error {}: {}", status, text).into());
        }

        let parsed: PfSenseResponse = serde_json::from_str(&text).unwrap_or(PfSenseResponse {
            code: Some(status.as_u16() as i32),
            status: Some(status.to_string()),
            response_id: None,
            message: Some(text),
            data: None,
        });

        Ok(parsed)
    }

    // ============= BACKEND OPERATIONS =============

    async fn create_backend(
        &self,
        name: &str,
        mode: &str,
    ) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        let request = CreateBackendRequest {
            name: name.to_string(),
            mode: mode.to_string(),
            balance: "roundrobin".to_string(),
            check_type: if mode == "http" {
                Some("HTTP".to_string())
            } else {
                None
            },
            httpcheck_method: if mode == "http" {
                Some("GET".to_string())
            } else {
                None
            },
        };

        let response = self
            .api_request("POST", "/api/v2/services/haproxy/backend", Some(&request))
            .await?;

        if let Some(data) = &response.data {
            if let Some(id) = data.get("id").and_then(|v| v.as_i64()) {
                info!("✅ Created backend: {} (id: {})", name, id);
                return Ok(id);
            }
        }

        Err("Failed to get backend ID from response".into())
    }

    async fn add_server(
        &self,
        backend_id: i64,
        server_name: &str,
        address: &str,
        port: u16,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = CreateServerRequest {
            parent_id: backend_id,
            name: server_name.to_string(),
            address: address.to_string(),
            port: port.to_string(),
            status: Some("active".to_string()),
            weight: Some(1),
        };

        self.api_request(
            "POST",
            "/api/v2/services/haproxy/backend/server",
            Some(&request),
        )
        .await?;
        info!(
            "✅ Added server {} to backend id {}",
            server_name, backend_id
        );
        Ok(())
    }

    async fn delete_backend(
        &self,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .api_request::<()>("GET", "/api/v2/services/haproxy/backends", None)
            .await?;

        if let Some(data) = response.data {
            if let Some(backends) = data.as_array() {
                for backend in backends {
                    if backend.get("name").and_then(|v| v.as_str()) == Some(name) {
                        if let Some(id) = backend.get("id").and_then(|v| v.as_i64()) {
                            let endpoint = format!("/api/v2/services/haproxy/backend?id={}", id);
                            self.api_request::<()>("DELETE", &endpoint, None).await?;
                            info!("✅ Deleted backend: {}", name);
                            return Ok(());
                        }
                    }
                }
            }
        }

        warn!("Backend not found for deletion: {}", name);
        Ok(())
    }

    // ============= FRONTEND OPERATIONS =============

    async fn create_tcp_frontend(
        &self,
        name: &str,
        port: u16,
        backend: &str,
        description: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = CreateFrontendRequest {
            name: name.to_string(),
            mode: "tcp".to_string(),
            bind: format!("0.0.0.0:{}", port),
            default_backend: backend.to_string(),
            description: Some(description.to_string()),
        };

        self.api_request("POST", "/api/v2/services/haproxy/frontend", Some(&request))
            .await?;
        info!("✅ Created TCP frontend: {} on port {}", name, port);
        Ok(())
    }

    async fn delete_frontend(
        &self,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let frontends = self.get_frontends().await?;

        for frontend in frontends {
            if frontend.get("name").and_then(|v| v.as_str()) == Some(name) {
                if let Some(id) = frontend.get("id").and_then(|v| v.as_i64()) {
                    let endpoint = format!("/api/v2/services/haproxy/frontend?id={}", id);
                    self.api_request::<()>("DELETE", &endpoint, None).await?;
                    info!("✅ Deleted frontend: {}", name);
                    return Ok(());
                }
            }
        }

        warn!("Frontend not found for deletion: {}", name);
        Ok(())
    }

    async fn get_frontends(
        &self,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .api_request::<()>("GET", "/api/v2/services/haproxy/frontends", None)
            .await?;

        if let Some(data) = response.data {
            if let Some(frontends) = data.as_array() {
                return Ok(frontends.clone());
            }
        }

        Ok(vec![])
    }

    async fn get_frontend_id(
        &self,
        name: &str,
    ) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
        let frontends = self.get_frontends().await?;

        for frontend in frontends {
            if frontend.get("name").and_then(|v| v.as_str()) == Some(name) {
                if let Some(id) = frontend.get("id").and_then(|v| v.as_i64()) {
                    debug!("Found frontend '{}' with id {}", name, id);
                    return Ok(id);
                }
            }
        }

        Err(format!("Frontend not found: {}", name).into())
    }

    // ============= ACL OPERATIONS =============

    async fn add_http_acl(
        &self,
        frontend_id: i64,
        acl_name: &str,
        full_domain: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = CreateAclRequest {
            parent_id: frontend_id,
            name: acl_name.to_string(),
            expression: "host_matches".to_string(),
            value: full_domain.to_string(),
            casesensitive: Some(false),
            not: Some(false),
        };

        self.api_request(
            "POST",
            "/api/v2/services/haproxy/frontend/acl",
            Some(&request),
        )
        .await?;
        info!("✅ Added Host ACL {} for {}", acl_name, full_domain);
        Ok(())
    }

    async fn delete_acl_from_frontend(
        &self,
        frontend_name: &str,
        acl_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let frontends = self.get_frontends().await?;

        for frontend in frontends {
            if frontend.get("name").and_then(|v| v.as_str()) == Some(frontend_name) {
                if let Some(acls) = frontend.get("ha_acls").and_then(|v| v.as_array()) {
                    for acl in acls {
                        if acl.get("name").and_then(|v| v.as_str()) == Some(acl_name) {
                            if let (Some(parent_id), Some(acl_id)) = (
                                frontend.get("id").and_then(|v| v.as_i64()),
                                acl.get("id").and_then(|v| v.as_i64()),
                            ) {
                                let endpoint = format!(
                                    "/api/v2/services/haproxy/frontend/acl?parent_id={}&id={}",
                                    parent_id, acl_id
                                );
                                self.api_request::<()>("DELETE", &endpoint, None).await?;
                                info!(
                                    "✅ Deleted ACL {} from frontend {}",
                                    acl_name, frontend_name
                                );
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        warn!("ACL {} not found in frontend {}", acl_name, frontend_name);
        Ok(())
    }

    // ============= ACTION OPERATIONS =============

    async fn add_backend_action(
        &self,
        frontend_id: i64,
        acl_name: &str,
        backend: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = CreateActionRequest {
            parent_id: frontend_id,
            acl: acl_name.to_string(),
            action: "use_backend".to_string(),
            backend: backend.to_string(),
        };

        self.api_request(
            "POST",
            "/api/v2/services/haproxy/frontend/action",
            Some(&request),
        )
        .await?;
        info!("✅ Added action: {} → {}", acl_name, backend);
        Ok(())
    }

    // ============= CUSTOM DOMAIN HELPERS =============
    //
    // Endpoint/field names for frontend certificate binding are CONFIRMED
    // against the pfSense REST API v2 OpenAPI spec (pfrest.org):
    //   POST/DELETE /api/v2/services/haproxy/frontend/certificate
    //   HAProxyFrontendCertificate.ssl_certificate = cert refid
    //   HAProxyFrontend.ha_certificates = additional-certs array
    // If a future package version renames them, adjust the two constants
    // below — nothing else references them.
    const FRONTEND_CERT_ENDPOINT: &'static str = "/api/v2/services/haproxy/frontend/certificate";
    const FRONTEND_CERT_FIELD: &'static str = "ssl_certificate";

    async fn get_backends(
        &self,
    ) -> Result<Vec<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
        let response = self
            .api_request::<()>("GET", "/api/v2/services/haproxy/backends", None)
            .await?;

        if let Some(data) = response.data {
            if let Some(backends) = data.as_array() {
                return Ok(backends.clone());
            }
        }
        Ok(vec![])
    }

    async fn find_backend_id(
        &self,
        name: &str,
    ) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>> {
        for backend in self.get_backends().await? {
            if backend.get("name").and_then(|v| v.as_str()) == Some(name) {
                return Ok(backend.get("id").and_then(|v| v.as_i64()));
            }
        }
        Ok(None)
    }

    /// Does `frontend_name` currently carry both the named ACL and a
    /// use_backend action pointing at `backend_name`?
    async fn frontend_has_acl_and_action(
        &self,
        frontend_name: &str,
        acl_name: &str,
        backend_name: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        for frontend in self.get_frontends().await? {
            if frontend.get("name").and_then(|v| v.as_str()) != Some(frontend_name) {
                continue;
            }
            let has_acl = frontend
                .get("ha_acls")
                .and_then(|v| v.as_array())
                .map(|acls| {
                    acls.iter()
                        .any(|a| a.get("name").and_then(|v| v.as_str()) == Some(acl_name))
                })
                .unwrap_or(false);
            let has_action = frontend
                .get("a_actionitems")
                .and_then(|v| v.as_array())
                .map(|actions| {
                    actions
                        .iter()
                        .any(|a| a.get("backend").and_then(|v| v.as_str()) == Some(backend_name))
                })
                .unwrap_or(false);
            return Ok(has_acl && has_action);
        }
        Ok(false)
    }

    async fn add_path_acl(
        &self,
        frontend_id: i64,
        acl_name: &str,
        path_prefix: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let request = CreateAclRequest {
            parent_id: frontend_id,
            name: acl_name.to_string(),
            expression: "path_starts_with".to_string(),
            value: path_prefix.to_string(),
            casesensitive: Some(true),
            not: Some(false),
        };

        self.api_request(
            "POST",
            "/api/v2/services/haproxy/frontend/acl",
            Some(&request),
        )
        .await?;
        info!("✅ Added path ACL {} for {}", acl_name, path_prefix);
        Ok(())
    }

    /// Idempotently ensure the ACME challenge backend + ACL + action exist on
    /// the given frontend.
    async fn ensure_acme_route_on_frontend(
        &self,
        frontend_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self
            .frontend_has_acl_and_action(frontend_name, ACME_CHALLENGE_ACL, ACME_CHALLENGE_BACKEND)
            .await?
        {
            debug!(
                "ACME challenge route already present on frontend {}",
                frontend_name
            );
            return Ok(());
        }

        let frontend_id = self.get_frontend_id(frontend_name).await?;
        self.add_path_acl(frontend_id, ACME_CHALLENGE_ACL, ACME_CHALLENGE_PATH)
            .await?;
        self.add_backend_action(frontend_id, ACME_CHALLENGE_ACL, ACME_CHALLENGE_BACKEND)
            .await?;
        info!(
            "✅ ACME challenge route created on frontend {}",
            frontend_name
        );
        Ok(())
    }

    async fn delete_action_from_frontend(
        &self,
        frontend_name: &str,
        backend_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let frontends = self.get_frontends().await?;

        for frontend in frontends {
            if frontend.get("name").and_then(|v| v.as_str()) == Some(frontend_name) {
                if let Some(actions) = frontend.get("a_actionitems").and_then(|v| v.as_array()) {
                    for action in actions {
                        if action.get("backend").and_then(|v| v.as_str()) == Some(backend_name) {
                            if let (Some(parent_id), Some(action_id)) = (
                                frontend.get("id").and_then(|v| v.as_i64()),
                                action.get("id").and_then(|v| v.as_i64()),
                            ) {
                                let endpoint = format!(
                                    "/api/v2/services/haproxy/frontend/action?parent_id={}&id={}",
                                    parent_id, action_id
                                );
                                self.api_request::<()>("DELETE", &endpoint, None).await?;
                                info!("✅ Deleted action for backend: {}", backend_name);
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        warn!(
            "Action not found for backend {} in frontend {}",
            backend_name, frontend_name
        );
        Ok(())
    }
}

// ============= TRAIT IMPLEMENTATION =============

#[async_trait]
impl HAProxyClientTrait for HAProxyClient {
    fn get_base_domain(&self) -> &str {
        &self.base_domain
    }
    fn get_public_ip(&self) -> &str {
        &self.public_ip
    }

    async fn create_http_ingress(
        &self,
        subdomain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<HttpIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        let subdomain = subdomain.to_lowercase();
        let full_domain = format!("{}.{}", subdomain, self.base_domain);
        let backend_name = format!("ingress_http_{}", subdomain);
        let server_name = format!("srv_{}", subdomain);
        let acl_name = format!("acl_host_{}", subdomain);

        info!(
            "🌐 Creating HTTP ingress: {} → {}:{}",
            full_domain, target_ip, target_port
        );

        let backend_id = self.create_backend(&backend_name, "http").await?;
        self.add_server(backend_id, &server_name, target_ip, target_port)
            .await?;
        let frontend_id = self.get_frontend_id(&self.http_frontend).await?;
        self.add_http_acl(frontend_id, &acl_name, &full_domain)
            .await?;
        self.add_backend_action(frontend_id, &acl_name, &backend_name)
            .await?;
        self.apply().await?;

        Ok(HttpIngressResult {
            backend_name,
            server_name,
            acl_name,
        })
    }

    async fn remove_http_ingress(
        &self,
        backend_name: &str,
        acl_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("🗑️ Removing HTTP ingress: {}", backend_name);

        // CRITICAL ORDER: Action → Apply → ACL → Apply → Backend → Apply
        self.delete_action_from_frontend(&self.http_frontend, backend_name)
            .await?;
        self.apply().await?;

        self.delete_acl_from_frontend(&self.http_frontend, acl_name)
            .await?;
        self.apply().await?;

        self.delete_backend(backend_name).await?;
        self.apply().await?;

        info!("✅ HTTP ingress removed: {}", backend_name);
        Ok(())
    }

    async fn create_https_ingress(
        &self,
        subdomain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<HttpsIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        let subdomain = subdomain.to_lowercase();
        let full_domain = format!("{}.{}", subdomain, self.base_domain);
        let backend_name = format!("ingress_https_{}", subdomain);
        let server_name = format!("srv_tls_{}", subdomain);
        let acl_name = format!("acl_host_{}", subdomain);

        info!(
            "🔒 Creating HTTPS ingress: {} → {}:{}",
            full_domain, target_ip, target_port
        );
        info!("   TLS handled by wildcard cert *.{}", self.base_domain);

        // Create backend (HTTP mode - HAProxy terminates TLS, forwards plain HTTP)
        let backend_id = self.create_backend(&backend_name, "http").await?;
        self.add_server(backend_id, &server_name, target_ip, target_port)
            .await?;

        // Add ACL + action to HTTPS frontend
        let frontend_id = self.get_frontend_id(&self.https_frontend).await?;
        self.add_http_acl(frontend_id, &acl_name, &full_domain)
            .await?;
        self.add_backend_action(frontend_id, &acl_name, &backend_name)
            .await?;

        // Apply config
        self.apply().await?;

        info!("✅ HTTPS ingress created: https://{}", full_domain);

        Ok(HttpsIngressResult {
            backend_name,
            server_name,
            acl_name,
        })
    }

    async fn remove_https_ingress(
        &self,
        backend_name: &str,
        acl_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("🗑️ Removing HTTPS ingress: {}", backend_name);

        // CRITICAL ORDER: Action → Apply → ACL → Apply → Backend → Apply
        self.delete_action_from_frontend(&self.https_frontend, backend_name)
            .await?;
        self.apply().await?;

        self.delete_acl_from_frontend(&self.https_frontend, acl_name)
            .await?;
        self.apply().await?;

        self.delete_backend(backend_name).await?;
        self.apply().await?;

        info!("✅ HTTPS ingress removed: {}", backend_name);
        Ok(())
    }

    async fn create_tcp_ingress(
        &self,
        subdomain: &str,
        public_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<TcpIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        let subdomain = subdomain.to_lowercase();
        let full_domain = format!("{}.{}", subdomain, self.base_domain);
        let backend_name = format!("ingress_tcp_{}", subdomain);
        let server_name = format!("srv_{}", subdomain);
        let frontend_name = format!("fe_tcp_{}", subdomain);

        info!(
            "🔌 Creating TCP ingress: {}:{} → {}:{}",
            full_domain, public_port, target_ip, target_port
        );

        let backend_id = self.create_backend(&backend_name, "tcp").await?;
        self.add_server(backend_id, &server_name, target_ip, target_port)
            .await?;
        let description = format!("NordKraft TCP: {}", full_domain);
        self.create_tcp_frontend(&frontend_name, public_port, &backend_name, &description)
            .await?;
        self.apply().await?;

        Ok(TcpIngressResult {
            backend_name,
            server_name,
            frontend_name,
        })
    }

    async fn remove_tcp_ingress(
        &self,
        frontend_name: &str,
        backend_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("🗑️ Removing TCP ingress: {}", frontend_name);

        self.delete_frontend(frontend_name).await?;
        self.apply().await?;

        self.delete_backend(backend_name).await?;
        self.apply().await?;

        info!("✅ TCP ingress removed: {}", frontend_name);
        Ok(())
    }

    async fn apply(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.api_request::<()>("POST", "/api/v2/services/haproxy/apply", None)
            .await?;
        info!("✅ HAProxy configuration applied");
        Ok(())
    }

    // ============= CUSTOM DOMAINS =============

    async fn ensure_acme_challenge_route(
        &self,
        challenge_addr: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let (addr, port_str) = challenge_addr
            .rsplit_once(':')
            .ok_or("CUSTOM_DOMAINS_CHALLENGE_ADDR must be ip:port")?;
        let port: u16 = port_str
            .parse()
            .map_err(|e| format!("Invalid challenge port '{}': {}", port_str, e))?;

        // 1. Shared backend pointing at container-api's challenge endpoint
        if self
            .find_backend_id(ACME_CHALLENGE_BACKEND)
            .await?
            .is_none()
        {
            let backend_id = self.create_backend(ACME_CHALLENGE_BACKEND, "http").await?;
            self.add_server(backend_id, ACME_CHALLENGE_SERVER, addr, port)
                .await?;
            info!("✅ ACME challenge backend created → {}:{}", addr, port);
        } else {
            debug!("ACME challenge backend already exists");
        }

        // 2. Path ACL + action on BOTH frontends. Both are needed: if the
        //    HTTP frontend redirects to HTTPS, Let's Encrypt follows the
        //    redirect and the challenge arrives on 443 instead.
        let http_frontend = self.http_frontend.clone();
        let https_frontend = self.https_frontend.clone();
        self.ensure_acme_route_on_frontend(&http_frontend).await?;
        self.ensure_acme_route_on_frontend(&https_frontend).await?;

        self.apply().await?;
        info!("✅ ACME challenge routing verified on both frontends");
        Ok(())
    }

    async fn upload_certificate(
        &self,
        name: &str,
        cert_chain_pem: &str,
        private_key_pem: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;

        // Fields per pfSense REST API v2 Certificate model: crt/prv are
        // Base64Fields (base64-encoded PEM); type "server" marks the cert as
        // usable by services (it is also the API default — set explicitly).
        let request = serde_json::json!({
            "descr": name,
            "type": "server",
            "crt": engine.encode(cert_chain_pem),
            "prv": engine.encode(private_key_pem),
        });

        let response = self
            .api_request("POST", "/api/v2/system/certificate", Some(&request))
            .await?;

        let refid = response
            .data
            .as_ref()
            .and_then(|d| d.get("refid"))
            .and_then(|v| v.as_str())
            .ok_or("pfSense did not return a certificate refid")?
            .to_string();

        info!("✅ Uploaded certificate '{}' (refid: {})", name, refid);
        Ok(refid)
    }

    async fn delete_certificate(
        &self,
        refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Look up the current numeric id by refid (ids shift, refids don't).
        let response = self
            .api_request::<()>("GET", "/api/v2/system/certificates", None)
            .await?;

        if let Some(data) = response.data {
            if let Some(certs) = data.as_array() {
                for cert in certs {
                    if cert.get("refid").and_then(|v| v.as_str()) == Some(refid) {
                        if let Some(id) = cert.get("id").and_then(|v| v.as_i64()) {
                            let endpoint = format!("/api/v2/system/certificate?id={}", id);
                            self.api_request::<()>("DELETE", &endpoint, None).await?;
                            info!("🗑️ Deleted certificate refid {}", refid);
                            return Ok(());
                        }
                    }
                }
            }
        }

        warn!("Certificate refid {} not found (already deleted?)", refid);
        Ok(())
    }

    async fn bind_certificate_to_https_frontend(
        &self,
        cert_refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.https_frontend_has_certificate(cert_refid).await? {
            debug!("Certificate {} already bound to HTTPS frontend", cert_refid);
            return Ok(());
        }

        let frontend_id = self.get_frontend_id(&self.https_frontend).await?;
        let request = serde_json::json!({
            "parent_id": frontend_id,
            Self::FRONTEND_CERT_FIELD: cert_refid,
        });

        self.api_request("POST", Self::FRONTEND_CERT_ENDPOINT, Some(&request))
            .await?;
        self.apply().await?;
        info!(
            "✅ Certificate {} bound to HTTPS frontend (SNI)",
            cert_refid
        );
        Ok(())
    }

    async fn unbind_certificate_from_https_frontend(
        &self,
        cert_refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for frontend in self.get_frontends().await? {
            if frontend.get("name").and_then(|v| v.as_str()) != Some(self.https_frontend.as_str()) {
                continue;
            }
            if let Some(certs) = frontend.get("ha_certificates").and_then(|v| v.as_array()) {
                for cert in certs {
                    if cert.get(Self::FRONTEND_CERT_FIELD).and_then(|v| v.as_str())
                        == Some(cert_refid)
                    {
                        if let (Some(parent_id), Some(cert_id)) = (
                            frontend.get("id").and_then(|v| v.as_i64()),
                            cert.get("id").and_then(|v| v.as_i64()),
                        ) {
                            let endpoint = format!(
                                "{}?parent_id={}&id={}",
                                Self::FRONTEND_CERT_ENDPOINT,
                                parent_id,
                                cert_id
                            );
                            self.api_request::<()>("DELETE", &endpoint, None).await?;
                            self.apply().await?;
                            info!("🗑️ Certificate {} unbound from HTTPS frontend", cert_refid);
                            return Ok(());
                        }
                    }
                }
            }
        }

        warn!(
            "Certificate {} not bound to HTTPS frontend (already unbound?)",
            cert_refid
        );
        Ok(())
    }

    async fn create_custom_domain_ingress(
        &self,
        name_prefix: &str,
        full_domain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<CustomDomainIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        let backend_name = format!("{}_be", name_prefix);
        let server_name = format!("{}_srv", name_prefix);
        let acl_name = format!("{}_acl", name_prefix);

        info!(
            "🌍 Creating custom-domain ingress: {} → {}:{}",
            full_domain, target_ip, target_port
        );

        // Idempotent: a leftover backend from a previous partial attempt is
        // reused rather than failing the whole activation.
        match self.find_backend_id(&backend_name).await? {
            Some(id) => {
                debug!("Backend {} already exists (id {})", backend_name, id);
            }
            None => {
                let id = self.create_backend(&backend_name, "http").await?;
                self.add_server(id, &server_name, target_ip, target_port)
                    .await?;
            }
        }

        if !self
            .frontend_has_acl_and_action(&self.https_frontend, &acl_name, &backend_name)
            .await?
        {
            let frontend_id = self.get_frontend_id(&self.https_frontend).await?;
            // EXACT host match — never a wildcard. Isolation by construction.
            self.add_http_acl(frontend_id, &acl_name, full_domain)
                .await?;
            self.add_backend_action(frontend_id, &acl_name, &backend_name)
                .await?;
        }

        self.apply().await?;

        info!("✅ Custom-domain ingress created: https://{}", full_domain);

        Ok(CustomDomainIngressResult {
            backend_name,
            server_name,
            acl_name,
        })
    }

    async fn verify_custom_domain_ingress(
        &self,
        backend_name: &str,
        acl_name: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let backend_exists = self.find_backend_id(backend_name).await?.is_some();
        let routing_exists = self
            .frontend_has_acl_and_action(&self.https_frontend, acl_name, backend_name)
            .await?;
        Ok(backend_exists && routing_exists)
    }

    async fn https_frontend_has_certificate(
        &self,
        cert_refid: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        for frontend in self.get_frontends().await? {
            if frontend.get("name").and_then(|v| v.as_str()) != Some(self.https_frontend.as_str()) {
                continue;
            }
            if let Some(certs) = frontend.get("ha_certificates").and_then(|v| v.as_array()) {
                return Ok(certs.iter().any(|c| {
                    c.get(Self::FRONTEND_CERT_FIELD).and_then(|v| v.as_str()) == Some(cert_refid)
                }));
            }
            return Ok(false);
        }
        Ok(false)
    }
}

// ============= DUMMY CLIENT =============

pub struct DummyHAProxyClient {
    base_domain: String,
    public_ip: String,
}

impl DummyHAProxyClient {
    pub fn new(base_domain: String, public_ip: String) -> Self {
        Self {
            base_domain,
            public_ip,
        }
    }
}

impl Default for DummyHAProxyClient {
    fn default() -> Self {
        Self {
            base_domain: "example.dk".to_string(),
            public_ip: "203.0.113.1".to_string(),
        }
    }
}

#[async_trait]
impl HAProxyClientTrait for DummyHAProxyClient {
    fn get_base_domain(&self) -> &str {
        &self.base_domain
    }
    fn get_public_ip(&self) -> &str {
        &self.public_ip
    }

    async fn create_http_ingress(
        &self,
        subdomain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<HttpIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        let full_domain = format!("{}.{}", subdomain, self.base_domain);
        warn!(
            "⚠️ HAProxy API disabled - manual config required: {} → {}:{}",
            full_domain, target_ip, target_port
        );
        Ok(HttpIngressResult {
            backend_name: format!("manual_http_{}", subdomain),
            server_name: format!("manual_srv_{}", subdomain),
            acl_name: format!("manual_acl_{}", subdomain),
        })
    }

    async fn remove_http_ingress(
        &self,
        backend_name: &str,
        _acl_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - manual cleanup required: {}",
            backend_name
        );
        Ok(())
    }

    async fn create_https_ingress(
        &self,
        subdomain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<HttpsIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        let full_domain = format!("{}.{}", subdomain, self.base_domain);
        warn!(
            "⚠️ HAProxy API disabled - manual config required: {} → {}:{}",
            full_domain, target_ip, target_port
        );
        Ok(HttpsIngressResult {
            backend_name: format!("manual_https_{}", subdomain),
            server_name: format!("manual_srv_tls_{}", subdomain),
            acl_name: format!("manual_acl_host_{}", subdomain),
        })
    }

    async fn remove_https_ingress(
        &self,
        backend_name: &str,
        _acl_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - manual cleanup required: {}",
            backend_name
        );
        Ok(())
    }

    async fn create_tcp_ingress(
        &self,
        subdomain: &str,
        public_port: u16,
        target_ip: &str,
        target_port: u16,
    ) -> Result<TcpIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        let full_domain = format!("{}.{}", subdomain, self.base_domain);
        warn!(
            "⚠️ HAProxy API disabled - manual config required: {}:{} → {}:{}",
            full_domain, public_port, target_ip, target_port
        );
        Ok(TcpIngressResult {
            backend_name: format!("manual_tcp_{}", subdomain),
            server_name: format!("manual_srv_{}", subdomain),
            frontend_name: format!("manual_fe_{}", subdomain),
        })
    }

    async fn remove_tcp_ingress(
        &self,
        frontend_name: &str,
        _backend_name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - manual cleanup required: {}",
            frontend_name
        );
        Ok(())
    }

    async fn apply(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!("⚠️ HAProxy API disabled - manual apply required");
        Ok(())
    }

    // ============= CUSTOM DOMAINS (dummy) =============

    async fn ensure_acme_challenge_route(
        &self,
        challenge_addr: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - ACME challenge route not created (would target {})",
            challenge_addr
        );
        Ok(())
    }

    async fn upload_certificate(
        &self,
        name: &str,
        _cert_chain_pem: &str,
        _private_key_pem: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - certificate '{}' not uploaded",
            name
        );
        Ok(format!("manual-cert-{}", uuid::Uuid::new_v4()))
    }

    async fn delete_certificate(
        &self,
        refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - certificate {} not deleted",
            refid
        );
        Ok(())
    }

    async fn bind_certificate_to_https_frontend(
        &self,
        cert_refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - certificate {} not bound to frontend",
            cert_refid
        );
        Ok(())
    }

    async fn unbind_certificate_from_https_frontend(
        &self,
        cert_refid: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - certificate {} not unbound from frontend",
            cert_refid
        );
        Ok(())
    }

    async fn create_custom_domain_ingress(
        &self,
        name_prefix: &str,
        full_domain: &str,
        target_ip: &str,
        target_port: u16,
    ) -> Result<CustomDomainIngressResult, Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ HAProxy API disabled - manual config required: {} → {}:{}",
            full_domain, target_ip, target_port
        );
        Ok(CustomDomainIngressResult {
            backend_name: format!("{}_be", name_prefix),
            server_name: format!("{}_srv", name_prefix),
            acl_name: format!("{}_acl", name_prefix),
        })
    }

    async fn verify_custom_domain_ingress(
        &self,
        _backend_name: &str,
        _acl_name: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        // Dummy mode: pretend verified so the dev-mode state machine can
        // reach 'active'.
        Ok(true)
    }

    async fn https_frontend_has_certificate(
        &self,
        _cert_refid: &str,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        Ok(true)
    }
}
