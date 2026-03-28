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
}
