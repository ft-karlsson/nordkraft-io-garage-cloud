// src/services/pfsense_client.rs
//
// REST API client for pfSense firewall management.
// Compatible with pfSense REST API v2.x
//
// API Documentation: https://pfrest.org/api-docs/
//
// IMPORTANT API v2 differences from v1:
// - IDs are NOT persistent across reboots/config changes
// - Use 'tracker' field for persistent identification
// - DELETE uses query parameter: /api/v2/firewall/rule?id=X
// - Must query rules to find current ID from tracker
//
// This client manages:
// - Per-container IPv6 firewall rules
// - Static routes for IPv4 ingress (container IP → node gateway)
//
// Static routes enable HAProxy to reach container IPs (172.21.x.x)
// by routing through the node's LAN IP (e.g., 10.0.0.36).

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Firewall rule info returned after creation
#[derive(Debug, Clone)]
pub struct FirewallRuleInfo {
    pub rule_id: String, // Current ID (may change!)
}

/// Static route info returned after creation
#[derive(Debug, Clone)]
pub struct StaticRouteInfo {
    pub route_id: String, // Route ID for deletion
}

/// Trait for pfSense client implementations (real and dummy)
#[async_trait]
pub trait PfSenseClientTrait: Send + Sync {
    async fn add_container_rule(
        &self,
        ipv6_address: &str,
        ports: &[i32],
        container_name: &str,
        user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>>;

    async fn remove_rule(
        &self,
        rule_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    async fn update_rule_ports(
        &self,
        rule_id: &str,
        ipv6_address: &str,
        new_ports: &[i32],
        container_name: &str,
        user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>>;

    // Static route methods for IPv4 ingress
    async fn add_static_route(
        &self,
        destination: &str,
        gateway: &str,
        description: &str,
    ) -> Result<StaticRouteInfo, Box<dyn std::error::Error + Send + Sync>>;

    async fn remove_static_route(
        &self,
        route_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;

    async fn remove_static_route_by_destination(
        &self,
        destination: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

/// pfSense API v2 client
#[derive(Clone)]
pub struct PfSenseClient {
    client: Client,
    base_url: String,
    api_key: String,
    wan_interface: String,
}

/// Request to create a firewall rule (API v2 format)
#[derive(Debug, Serialize)]
struct CreateRuleRequest {
    #[serde(rename = "type")]
    rule_type: String,
    interface: Vec<String>,
    ipprotocol: String,
    protocol: String,
    source: String,
    destination: String,      // Address only - NO port here!
    destination_port: String, // Port(s) go here separately
    descr: String,
    disabled: bool,
    log: bool,
    top: bool,
    apply: bool,
}

/// Request to create a static route (API v2 format)
#[derive(Debug, Serialize)]
struct CreateStaticRouteRequest {
    network: String, // Destination network (e.g., "172.21.1.5/32")
    gateway: String, // Gateway NAME (e.g., "OPTIPLEX_GW") - NOT IP address!
    descr: String,   // Description
    disabled: bool,
}

/// Response from pfSense API v2
#[derive(Debug, Deserialize)]
struct PfSenseResponse {
    code: i32,
    response_id: String,
    message: String,
    data: Option<serde_json::Value>,
}

impl PfSenseClient {
    /// Create a new pfSense API v2 client
    pub fn new(
        base_url: String,
        api_key: String,
        wan_interface: String,
        verify_ssl: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new_with_gateway(base_url, api_key, wan_interface, verify_ssl)
    }

    /// Create a new pfSense API v2 client with custom LAN gateway
    pub fn new_with_gateway(
        base_url: String,
        api_key: String,
        wan_interface: String,
        verify_ssl: bool,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let client = Client::builder()
            .danger_accept_invalid_certs(!verify_ssl)
            .timeout(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            wan_interface,
        })
    }

    /// Check if the client is configured (has credentials)
    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Test API connectivity
    #[allow(dead_code)]
    pub async fn test_connection(&self) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/v2/system/version", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if response.status().is_success() {
            let body: PfSenseResponse = response.json().await?;
            info!("✅ pfSense API connected: {:?}", body.data);
            Ok(true)
        } else {
            warn!("⚠️ pfSense API connection failed: {}", response.status());
            Ok(false)
        }
    }

    /// Add a firewall rule for container IPv6 access
    /// Creates one rule per port (pfSense doesn't accept comma-separated ports)
    pub async fn add_container_rule(
        &self,
        ipv6_address: &str,
        ports: &[i32],
        container_name: &str,
        user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        // pfSense doesn't accept comma-separated ports in destination_port
        // Create one rule per port and track all IDs/trackers
        let mut rule_ids: Vec<String> = Vec::new();
        let mut trackers: Vec<String> = Vec::new();
        let url = format!("{}/api/v2/firewall/rule", self.base_url);

        for port in ports {
            let description = format!(
                "NordKraft: {} (user: {}) - port {}",
                container_name,
                &user_id[..8.min(user_id.len())],
                port
            );

            let rule = CreateRuleRequest {
                rule_type: "pass".to_string(),
                interface: vec![self.wan_interface.clone()],
                ipprotocol: "inet6".to_string(),
                protocol: "tcp".to_string(),
                source: "any".to_string(),
                destination: ipv6_address.to_string(),
                destination_port: port.to_string(), // Single port only!
                descr: description.clone(),
                disabled: false,
                log: false,
                top: false,
                apply: false, // Don't apply until all rules created
            };

            debug!("Creating pfSense rule for port {}: {:?}", port, rule);

            let response = self
                .client
                .post(&url)
                .header("X-API-Key", &self.api_key)
                .header("Content-Type", "application/json")
                .json(&rule)
                .send()
                .await?;

            let status = response.status();
            let response_text = response.text().await?;

            debug!("pfSense response status: {}", status);
            debug!("pfSense response body: {}", response_text);

            let body: PfSenseResponse = match serde_json::from_str(&response_text) {
                Ok(parsed) => parsed,
                Err(e) => {
                    error!("Failed to parse pfSense response: {}", e);
                    return Err(format!("Invalid JSON response: {}", e).into());
                }
            };

            if body.code != 200 {
                return Err(format!(
                    "Failed to create pfSense rule for port {}: {} - {} ({})",
                    port, body.code, body.message, body.response_id
                )
                .into());
            }

            // Extract rule ID and tracker
            if let Some(data) = &body.data {
                if let Some(id) = data.get("id").and_then(|t| t.as_i64()) {
                    rule_ids.push(id.to_string());
                }
                if let Some(tracker) = data.get("tracker").and_then(|t| {
                    t.as_str()
                        .map(|s| s.to_string())
                        .or_else(|| t.as_i64().map(|i| i.to_string()))
                }) {
                    trackers.push(tracker);
                }
            }
        }

        // Apply all changes at once
        self.apply_changes().await?;

        let port_str = ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");

        info!(
            "✅ Created {} pfSense rule(s) for {} → {} ports [{}]",
            rule_ids.len(),
            container_name,
            ipv6_address,
            port_str
        );

        Ok(FirewallRuleInfo {
            rule_id: rule_ids.join(","),
        })
    }

    /// Find current rule ID by tracker (trackers are persistent, IDs are not)
    async fn find_rule_id_by_tracker(
        &self,
        tracker: &str,
    ) -> Result<Option<i64>, Box<dyn std::error::Error + Send + Sync>> {
        // API v2: GET /api/v2/firewall/rules (plural with 's')
        let url = format!("{}/api/v2/firewall/rules", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Failed to list rules: {} - {}", status, text).into());
        }

        let body: PfSenseResponse = response.json().await?;

        if let Some(data) = body.data {
            if let Some(rules) = data.as_array() {
                for rule in rules {
                    let rule_tracker = rule.get("tracker").and_then(|t| {
                        t.as_str()
                            .map(|s| s.to_string())
                            .or_else(|| t.as_i64().map(|i| i.to_string()))
                    });

                    if rule_tracker.as_deref() == Some(tracker) {
                        if let Some(id) = rule.get("id").and_then(|i| i.as_i64()) {
                            debug!("Found rule with tracker {}: id={}", tracker, id);
                            return Ok(Some(id));
                        }
                    }
                }
            }
        }

        debug!("No rule found with tracker: {}", tracker);
        Ok(None)
    }

    /// Find gateway name by IP address
    /// pfSense static routes require gateway NAME, not IP
    async fn find_gateway_name_by_ip(
        &self,
        ip: &str,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let url = format!("{}/api/v2/routing/gateways", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Failed to list gateways: {} - {}", status, text).into());
        }

        let body: PfSenseResponse = response.json().await?;

        if let Some(data) = body.data {
            if let Some(gateways) = data.as_array() {
                for gw in gateways {
                    let gw_ip = gw.get("gateway").and_then(|v| v.as_str()).unwrap_or("");
                    if gw_ip == ip {
                        if let Some(name) = gw.get("name").and_then(|v| v.as_str()) {
                            debug!("Found gateway '{}' for IP {}", name, ip);
                            return Ok(Some(name.to_string()));
                        }
                    }
                }
            }
        }

        debug!("No gateway found for IP: {}", ip);
        Ok(None)
    }

    /// Remove firewall rule(s) by ID
    /// Handles comma-separated IDs (from multi-port rule creation)
    /// IMPORTANT: Deletes in reverse order (highest ID first) to prevent ID shifting issues
    /// NOTE: In API v2, IDs can change! Use remove_rule_by_tracker for reliability.
    pub async fn remove_rule(
        &self,
        rule_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        // Handle comma-separated IDs (from multi-port creation)
        let mut ids: Vec<i64> = rule_id
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .filter_map(|s| s.parse::<i64>().ok())
            .collect();

        if ids.is_empty() {
            warn!("⚠️ No valid rule IDs provided to remove");
            return Ok(());
        }

        // Sort in REVERSE order (highest first) to prevent ID shifting issues
        // When you delete ID 5, rule ID 6 becomes ID 5, so delete from highest first
        ids.sort_by(|a, b| b.cmp(a));

        debug!("Deleting rules in order: {:?}", ids);

        let url = format!("{}/api/v2/firewall/rule", self.base_url);
        let mut deleted_count = 0;

        for id in &ids {
            let response = self
                .client
                .delete(&url)
                .header("X-API-Key", &self.api_key)
                .query(&[("id", id.to_string().as_str()), ("apply", "false")]) // Don't apply until all deleted
                .send()
                .await?;

            let status = response.status();
            let response_text = response.text().await?;

            debug!(
                "pfSense delete response for id {}: {} - {}",
                id, status, response_text
            );

            if status.is_success() {
                deleted_count += 1;
            } else if status.as_u16() == 404 {
                warn!("⚠️ pfSense rule {} not found (already deleted?)", id);
                // Continue with other rules
            } else {
                error!("Failed to delete rule {}: {}", id, response_text);
                // Continue trying other rules
            }
        }

        // Apply all changes at once
        if deleted_count > 0 {
            self.apply_changes().await?;
            info!("🗑️ Removed {} pfSense rule(s)", deleted_count);
        }

        Ok(())
    }

    /// Remove firewall rule(s) by tracker (RECOMMENDED - trackers are persistent)
    /// Handles comma-separated trackers (from multi-port rule creation)
    pub async fn remove_rule_by_tracker(
        &self,
        tracker: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        // Handle comma-separated trackers (from multi-port creation)
        let trackers: Vec<&str> = tracker
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

        if trackers.is_empty() {
            warn!("⚠️ No trackers provided to remove");
            return Ok(());
        }

        let mut deleted_count = 0;

        for t in &trackers {
            debug!("Looking up current ID for tracker: {}", t);

            match self.find_rule_id_by_tracker(t).await? {
                Some(id) => {
                    info!("Found rule id {} for tracker {}, deleting...", id, t);
                    // Delete without applying (we'll apply once at the end)
                    let url = format!("{}/api/v2/firewall/rule", self.base_url);
                    let response = self
                        .client
                        .delete(&url)
                        .header("X-API-Key", &self.api_key)
                        .query(&[("id", id.to_string().as_str()), ("apply", "false")])
                        .send()
                        .await?;

                    if response.status().is_success() {
                        deleted_count += 1;
                    }
                }
                None => {
                    warn!("⚠️ No rule found with tracker {} (already deleted?)", t);
                }
            }
        }

        // Apply all changes at once
        if deleted_count > 0 {
            self.apply_changes().await?;
            info!("🗑️ Removed {} pfSense rule(s) by tracker", deleted_count);
        }

        Ok(())
    }

    /// Update ports for an existing rule (delete + recreate)
    pub async fn update_rule_ports(
        &self,
        tracker: &str,
        ipv6_address: &str,
        new_ports: &[i32],
        container_name: &str,
        user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>> {
        // Remove existing rule by tracker (more reliable than ID)
        let _ = self.remove_rule_by_tracker(tracker).await;

        // Create new rule with updated ports
        self.add_container_rule(ipv6_address, new_ports, container_name, user_id)
            .await
    }

    // ============= STATIC ROUTE METHODS =============

    /// Add a static route for IPv4 container ingress
    /// Routes container IP (e.g., 172.21.1.5/32) via node's LAN IP (e.g., 10.0.0.36)
    /// NOTE: gateway param can be IP or name - if IP, we'll lookup the gateway name
    pub async fn add_static_route(
        &self,
        destination: &str,
        gateway: &str,
        description: &str,
    ) -> Result<StaticRouteInfo, Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        // Check if route already exists
        if let Ok(Some(existing)) = self.find_static_route_by_destination(destination).await {
            info!(
                "Static route for {} already exists (id: {})",
                destination, existing.route_id
            );
            return Ok(existing);
        }

        // Resolve gateway IP to gateway NAME if needed
        // pfSense API requires gateway NAME, not IP address
        let gateway_name = if gateway
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false)
        {
            // Looks like an IP address, need to lookup gateway name
            match self.find_gateway_name_by_ip(gateway).await? {
                Some(name) => {
                    debug!("Resolved gateway IP {} to name '{}'", gateway, name);
                    name
                }
                None => {
                    return Err(format!(
                        "No gateway found for IP {}. Create a gateway in pfSense first.",
                        gateway
                    )
                    .into());
                }
            }
        } else {
            // Already a gateway name
            gateway.to_string()
        };

        let url = format!("{}/api/v2/routing/static_route", self.base_url);

        let route = CreateStaticRouteRequest {
            network: destination.to_string(),
            gateway: gateway_name.clone(),
            descr: format!("NordKraft Ingress: {}", description),
            disabled: false,
        };

        debug!("Creating pfSense static route: {:?}", route);

        let response = self
            .client
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .header("Content-Type", "application/json")
            .json(&route)
            .send()
            .await?;

        let status = response.status();
        let response_text = response.text().await?;

        debug!(
            "pfSense static route response: {} - {}",
            status, response_text
        );

        let body: PfSenseResponse = match serde_json::from_str(&response_text) {
            Ok(parsed) => parsed,
            Err(e) => {
                error!("Failed to parse pfSense response: {}", e);
                return Err(format!("Invalid JSON response: {}", e).into());
            }
        };

        if body.code != 200 {
            return Err(format!(
                "Failed to create static route: {} - {} ({})",
                body.code, body.message, body.response_id
            )
            .into());
        }

        // Extract route ID from response
        let route_id = body
            .data
            .as_ref()
            .and_then(|d| d.get("id"))
            .and_then(|id| {
                id.as_i64()
                    .map(|i| i.to_string())
                    .or_else(|| id.as_str().map(|s| s.to_string()))
            })
            .unwrap_or_else(|| format!("route-{}", uuid::Uuid::new_v4()));

        info!(
            "✅ Created static route: {} → {} (id: {})",
            destination, gateway_name, route_id
        );

        // Apply routing changes
        self.apply_routing().await?;

        Ok(StaticRouteInfo { route_id })
    }

    /// Remove a static route by ID
    pub async fn remove_static_route(
        &self,
        route_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        let url = format!("{}/api/v2/routing/static_route", self.base_url);

        let response = self
            .client
            .delete(&url)
            .header("X-API-Key", &self.api_key)
            .query(&[("id", route_id), ("apply", "true")])
            .send()
            .await?;

        let status = response.status();
        let response_text = response.text().await?;

        debug!(
            "pfSense delete static route response: {} - {}",
            status, response_text
        );

        if status.is_success() {
            info!("🗑️ Removed static route: {}", route_id);
            Ok(())
        } else if status.as_u16() == 404 {
            warn!("⚠️ Static route {} not found (already deleted?)", route_id);
            Ok(())
        } else {
            Err(format!(
                "Failed to remove static route: {} - {}",
                status, response_text
            )
            .into())
        }
    }

    /// Remove a static route by destination network (safe across reboots)
    ///
    /// pfSense API v2 IDs shift on reboot/config changes, so stored IDs go stale.
    /// This method looks up the current ID by destination before deleting.
    pub async fn remove_static_route_by_destination(
        &self,
        destination: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        match self.find_static_route_by_destination(destination).await? {
            Some(route_info) => {
                info!(
                    "Found static route for {} with current id {}, removing...",
                    destination, route_info.route_id
                );
                self.remove_static_route(&route_info.route_id).await
            }
            None => {
                warn!(
                    "⚠️ No static route found for {} (already deleted?)",
                    destination
                );
                Ok(())
            }
        }
    }

    /// Find a static route by destination network
    pub async fn find_static_route_by_destination(
        &self,
        destination: &str,
    ) -> Result<Option<StaticRouteInfo>, Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Ok(None);
        }

        let url = format!("{}/api/v2/routing/static_routes", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Failed to list static routes: {} - {}", status, text).into());
        }

        let body: PfSenseResponse = response.json().await?;

        if let Some(data) = body.data {
            if let Some(routes) = data.as_array() {
                for route in routes {
                    let network = route.get("network").and_then(|n| n.as_str());

                    if network == Some(destination) {
                        let route_id = route
                            .get("id")
                            .and_then(|id| {
                                id.as_i64()
                                    .map(|i| i.to_string())
                                    .or_else(|| id.as_str().map(|s| s.to_string()))
                            })
                            .unwrap_or_default();

                        return Ok(Some(StaticRouteInfo { route_id }));
                    }
                }
            }
        }

        Ok(None)
    }

    /// Apply pending firewall changes
    #[allow(dead_code)]
    pub async fn apply_changes(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        let url = format!("{}/api/v2/firewall/apply", self.base_url);

        let response = self
            .client
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if response.status().is_success() {
            info!("✅ Applied firewall changes");
            Ok(())
        } else {
            let text = response.text().await.unwrap_or_default();
            Err(format!("Failed to apply firewall changes: {}", text).into())
        }
    }

    /// Apply pending routing changes (static routes, gateways)
    pub async fn apply_routing(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Err("pfSense API not configured".into());
        }

        let url = format!("{}/api/v2/routing/apply", self.base_url);

        let response = self
            .client
            .post(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if response.status().is_success() {
            info!("✅ Applied routing changes");
            Ok(())
        } else {
            let text = response.text().await.unwrap_or_default();
            Err(format!("Failed to apply routing changes: {}", text).into())
        }
    }

    /// List all NordKraft-managed firewall rules
    #[allow(dead_code)]
    pub async fn list_rules(
        &self,
    ) -> Result<Vec<FirewallRuleInfo>, Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Ok(vec![]);
        }

        // API v2: GET /api/v2/firewall/rules (plural)
        let url = format!("{}/api/v2/firewall/rules", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err("Failed to list pfSense rules".into());
        }

        let body: PfSenseResponse = response.json().await?;

        let rules = body
            .data
            .and_then(|d| d.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|rule| {
                let descr = rule.get("descr")?.as_str()?;

                // Only return NordKraft-managed rules
                if descr.starts_with("NordKraft:") {
                    let rule_id = rule
                        .get("id")
                        .and_then(|v| v.as_i64())
                        .map(|i| i.to_string())
                        .unwrap_or_default();

                    Some(FirewallRuleInfo { rule_id })
                } else {
                    None
                }
            })
            .collect();

        Ok(rules)
    }

    /// List all NordKraft-managed static routes
    #[allow(dead_code)]
    pub async fn list_static_routes(
        &self,
    ) -> Result<Vec<StaticRouteInfo>, Box<dyn std::error::Error + Send + Sync>> {
        if !self.is_configured() {
            return Ok(vec![]);
        }

        let url = format!("{}/api/v2/routing/static_routes", self.base_url);

        let response = self
            .client
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .send()
            .await?;

        if !response.status().is_success() {
            return Err("Failed to list static routes".into());
        }

        let body: PfSenseResponse = response.json().await?;

        let routes = body
            .data
            .and_then(|d| d.as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|route| {
                let descr = route.get("descr")?.as_str()?;

                // Only return NordKraft-managed routes
                if descr.starts_with("NordKraft") {
                    let route_id = route
                        .get("id")
                        .and_then(|v| {
                            v.as_i64()
                                .map(|i| i.to_string())
                                .or_else(|| v.as_str().map(|s| s.to_string()))
                        })
                        .unwrap_or_default();

                    Some(StaticRouteInfo { route_id })
                } else {
                    None
                }
            })
            .collect();

        Ok(routes)
    }
}

/// Dummy client for when pfSense API is disabled
pub struct DummyPfSenseClient;

impl DummyPfSenseClient {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl PfSenseClientTrait for DummyPfSenseClient {
    async fn add_container_rule(
        &self,
        ipv6_address: &str,
        ports: &[i32],
        container_name: &str,
        _user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ pfSense API disabled - rule not created for {} → {} ports {:?}",
            ipv6_address, container_name, ports
        );
        warn!("   Manual firewall configuration required!");

        Ok(FirewallRuleInfo {
            rule_id: format!("manual-{}", uuid::Uuid::new_v4()),
        })
    }

    async fn remove_rule(
        &self,
        _rule_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!("⚠️ pfSense API disabled - manual cleanup required!");
        Ok(())
    }

    async fn update_rule_ports(
        &self,
        _rule_id: &str,
        ipv6_address: &str,
        new_ports: &[i32],
        container_name: &str,
        user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>> {
        self.add_container_rule(ipv6_address, new_ports, container_name, user_id)
            .await
    }

    async fn add_static_route(
        &self,
        destination: &str,
        gateway: &str,
        description: &str,
    ) -> Result<StaticRouteInfo, Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ pfSense API disabled - static route not created: {} → {} ({})",
            destination, gateway, description
        );
        warn!("   Manual route configuration required!");
        warn!("   Run: pfctl add route {} via {}", destination, gateway);

        Ok(StaticRouteInfo {
            route_id: format!("manual-{}", uuid::Uuid::new_v4()),
        })
    }

    async fn remove_static_route(
        &self,
        _route_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!("⚠️ pfSense API disabled - manual route cleanup required!");
        Ok(())
    }

    async fn remove_static_route_by_destination(
        &self,
        destination: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        warn!(
            "⚠️ pfSense API disabled - manual route cleanup required for {}!",
            destination
        );
        Ok(())
    }
}

#[async_trait]
impl PfSenseClientTrait for PfSenseClient {
    async fn add_container_rule(
        &self,
        ipv6_address: &str,
        ports: &[i32],
        container_name: &str,
        user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>> {
        PfSenseClient::add_container_rule(self, ipv6_address, ports, container_name, user_id).await
    }

    async fn remove_rule(
        &self,
        rule_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        PfSenseClient::remove_rule(self, rule_id).await
    }

    async fn update_rule_ports(
        &self,
        tracker: &str,
        ipv6_address: &str,
        new_ports: &[i32],
        container_name: &str,
        user_id: &str,
    ) -> Result<FirewallRuleInfo, Box<dyn std::error::Error + Send + Sync>> {
        PfSenseClient::update_rule_ports(
            self,
            tracker,
            ipv6_address,
            new_ports,
            container_name,
            user_id,
        )
        .await
    }

    async fn add_static_route(
        &self,
        destination: &str,
        gateway: &str,
        description: &str,
    ) -> Result<StaticRouteInfo, Box<dyn std::error::Error + Send + Sync>> {
        PfSenseClient::add_static_route(self, destination, gateway, description).await
    }

    async fn remove_static_route(
        &self,
        route_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        PfSenseClient::remove_static_route(self, route_id).await
    }

    async fn remove_static_route_by_destination(
        &self,
        destination: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        PfSenseClient::remove_static_route_by_destination(self, destination).await
    }
}

impl Default for DummyPfSenseClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn test_port_formatting() {
        let single_port = vec![80];
        let port_str = if single_port.len() == 1 {
            single_port[0].to_string()
        } else {
            single_port
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(port_str, "80");

        let multi_ports = vec![80, 443, 8080];
        let port_str = multi_ports
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(port_str, "80,443,8080");
    }

    #[test]
    fn test_destination_format() {
        // Container IP with /32 for single host
        let dest = format!("{}/32", "172.21.1.5");
        assert_eq!(dest, "172.21.1.5/32");
    }
}
