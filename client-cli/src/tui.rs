// tui.rs — nordkraft ui
// k9s-style terminal dashboard for NordKraft.io
//
// DEPS (Cargo.toml):
//   ratatui  = "0.28"
//   crossterm = "0.28"
//
// WIRE UP (cli.rs):
//   Commands::Ui => crate::tui::run_tui().await,

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use reqwest::Client;
use serde::Deserialize;
use std::{
    io,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

// ─── Palette ─────────────────────────────────────────────────────────────────

const CYAN: Color = Color::Rgb(0, 210, 210);
const EMERALD: Color = Color::Rgb(52, 211, 153);
const AMBER: Color = Color::Rgb(251, 191, 36);
const ROSE: Color = Color::Rgb(251, 82, 82);
const INDIGO: Color = Color::Rgb(129, 140, 248);
const MUTED: Color = Color::Rgb(100, 116, 139);
const PANEL_BG: Color = Color::Rgb(10, 15, 25);
const SEL_BG: Color = Color::Rgb(22, 38, 60);
const HEADER_BG: Color = Color::Rgb(5, 10, 20);

// ─── Constants ───────────────────────────────────────────────────────────────

use super::API_BASE_URL;

const POLL_INTERVAL_SECS: u64 = 5;

// ─── API Types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct ContainerInfo {
    container_id: String,
    name: String,
    image: String,
    status: String,
    container_ip: Option<String>,
    ipv6_address: Option<String>,
    #[serde(default)]
    ipv6_enabled: bool,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct ContainerListResponse {
    containers: Vec<ContainerInfo>,
}

#[derive(Debug, Deserialize)]
struct LogsResponse {
    logs: String,
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    // status: Option<String>,
    error: Option<String>,
}

// ─── Ingress Types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct IngressRoute {
    #[serde(default)]
    container_id: String,
    subdomain: String,
    url: String,
    target_port: u16,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    target_ip: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IngressListResponse {
    routes: Vec<IngressRoute>,
}

// ─── Node Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct NodeView {
    id: String,
    address: String,
    #[serde(default)]
    port: u16,
    status: String,
    #[serde(default)]
    last_heartbeat: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NodeListResponse {
    nodes: Vec<NodeView>,
}

// ─── Spec Types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct SpecEntry {
    container_name: String, // raw name on disk (app-…)
    alias: Option<String>,  // resolved from ~/.nordkraft/aliases.json
    /// Image declared in the `.nk` spec's `[deployment]` table.
    /// None if the TOML is unparseable or the field is missing.
    image: Option<String>,
}

/// Tri-state for a spec on the Specs tab.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SpecState {
    /// Spec matches the live container (same image).
    Deployed,
    /// Spec and container both exist, but the spec's image differs — user
    /// edited the `.nk` file and needs to run `nordkraft upgrade`.
    Updated,
    /// No live container matching this spec's name.
    NotDeployed,
}

// ─── Describe (rich container inspect) ──────────────────────────────────────

/// Matches `ContainerInspectResponse` in main.rs — the rich shape returned
/// from `GET /api/containers/{name}`. We only pull fields we actually render.
#[derive(Debug, Deserialize, Clone, Default)]
struct ContainerDescribe {
    #[serde(default)]
    container_id: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    image: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    finished_at: Option<String>,
    #[serde(default)]
    exit_code: Option<i64>,
    #[serde(default)]
    restart_count: Option<i64>,
    #[serde(default)]
    container_ip: Option<String>,
    #[serde(default)]
    ipv6_address: Option<String>,
    #[serde(default)]
    ipv6_enabled: bool,
    #[serde(default)]
    ports: Vec<serde_json::Value>,
    #[serde(default)]
    env_vars: Vec<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    node_id: String,
    #[serde(default)]
    runtime: String,
    #[serde(default)]
    cpu_limit: Option<f64>,
    #[serde(default)]
    memory_limit: Option<i64>,
    #[serde(default)]
    volume_mounts: Vec<String>,
}

// ─── Background Task Messages ────────────────────────────────────────────────
// All network I/O runs in a spawned task and sends results here.
// The event loop never awaits a network call directly → always responsive.

enum BgMsg {
    Containers(Vec<ContainerInfo>),
    ContainersFailed(String),
    Ingress(Vec<IngressRoute>),
    IngressFailed(String),
    Nodes(Vec<NodeView>),
    NodesFailed(String),
    Describe(ContainerDescribe),
    DescribeFailed(String),
    Connection(bool),
    Logs {
        #[allow(dead_code)]
        name: String,
        lines: Vec<String>,
    },
    LogsFailed(String),
    ActionDone {
        verb: String,
        name: String,
    },
    ActionFailed(String),
    Usage {
        cpu_ratio: f64,
        ram_ratio: f64,
        disk_ratio: f64,
        cpu_label: String,
        ram_label: String,
        disk_label: String,
    },
}

// ─── Cluster Snapshot ────────────────────────────────────────────────────────

/// Snapshot of cluster-wide state shown in the TUI header/footer.
/// Some fields are populated but not yet surfaced in the UI — they're
/// reserved for upcoming metrics work (MARK II observability + Energinet).
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct ClusterSnapshot {
    garage: String,          // e.g. "ry" from connection.json assigned_garage
    server_endpoint: String, // e.g. "cloud.nordkraft.io:51820"
    /// End-to-end connection state: true if /api/auth/verify succeeds
    /// through the WireGuard tunnel. Updated asynchronously on each poll.
    connected: bool,
    node_count: usize,
    nodes_online: usize,
    cpu_used: f64,      // ratio 0.0-1.0 from /api/usage
    ram_used: f64,      // ratio 0.0-1.0 from /api/usage
    disk_used: f64,     // ratio 0.0-1.0 from /api/usage
    cpu_label: String,  // e.g. "1.0/2.0 vCPU"
    ram_label: String,  // e.g. "1536/4096 MB"
    disk_label: String, // e.g. "1.0/100.0GB"
    traffic_gb: f64,    // TODO: /api/usage
    co2_g_kwh: f64,     // TODO: Energinet API (MARK III)
}

impl ClusterSnapshot {
    fn from_connection() -> Self {
        // Pull garage + endpoint from ~/.nordkraft/connection.json.
        // If no config is present (self-hosted controller, dev mode), fall
        // back to a quiet "unconfigured" label instead of a marketing stub.
        let (garage, server_endpoint) = match super::load_connection_config() {
            Some(cfg) => (cfg.assigned_garage, cfg.server_endpoint),
            None => ("unconfigured".to_string(), String::new()),
        };
        Self {
            garage,
            server_endpoint,
            connected: false, // updated asynchronously via spawn_connection_check
            node_count: 0,
            nodes_online: 0,
            cpu_used: 0.0,
            ram_used: 0.0,
            disk_used: 0.0,
            cpu_label: "loading...".into(),
            ram_label: "loading...".into(),
            disk_label: "loading...".into(),
            traffic_gb: 0.0,
            co2_g_kwh: 82.0,
        }
    }
}

// ─── Connection Status ───────────────────────────────────────────────────────
// Rather than checking whether the WireGuard interface exists (which only
// confirms the config is loaded, not that the tunnel is routable), we verify
// end-to-end connectivity by hitting `/api/auth/verify` through the tunnel —
// the same check `nordkraft auth login` runs. Success means wg is up AND the
// controller is reachable AND our session is valid.

async fn check_connection(client: &Client) -> bool {
    let url = format!("{}/auth/verify", *API_BASE_URL);
    match client
        .get(&url)
        .timeout(Duration::from_secs(3))
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

// ─── App State ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tab {
    Containers,
    Ingress,
    Nodes,
    Specs,
}

impl Tab {
    fn next(&self) -> Tab {
        match self {
            Tab::Containers => Tab::Ingress,
            Tab::Ingress => Tab::Nodes,
            Tab::Nodes => Tab::Specs,
            Tab::Specs => Tab::Containers,
        }
    }
    fn prev(&self) -> Tab {
        match self {
            Tab::Containers => Tab::Specs,
            Tab::Ingress => Tab::Containers,
            Tab::Nodes => Tab::Ingress,
            Tab::Specs => Tab::Nodes,
        }
    }
    fn title(&self) -> &'static str {
        match self {
            Tab::Containers => "Containers",
            Tab::Ingress => "Ingress",
            Tab::Nodes => "Nodes",
            Tab::Specs => "Deployment Specs",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum View {
    ContainerList,
    Logs,
    Describe,
    Help,
}

#[derive(Debug, Clone, PartialEq)]
enum ActionResult {
    None,
    Success(String),
    Error(String),
    Pending(String),
}

#[derive(Debug, Clone)]
enum ConfirmAction {
    Stop(String),
    Remove(String),
    Restart(String),
    /// Delete an ingress route. (container_id, subdomain_for_display)
    DeleteIngress(String, String),
    /// Delete a local .nk spec file. (container_name, display_label)
    DeleteSpec(String, String),
}

struct App {
    client: Client,
    containers: Vec<ContainerInfo>,
    list_state: ListState,
    view: View,
    tab: Tab,
    ingress_routes: Vec<IngressRoute>,
    ingress_list_state: ListState,
    ingress_poll_in_flight: bool,
    ingress_last_poll: Instant,
    nodes: Vec<NodeView>,
    nodes_list_state: ListState,
    nodes_poll_in_flight: bool,
    nodes_last_poll: Instant,
    specs: Vec<SpecEntry>,
    specs_list_state: ListState,
    /// Rich inspect data for the currently-viewed container in Describe view.
    /// Populated on-demand via `spawn_describe_container`.
    describe_data: Option<ContainerDescribe>,
    describe_loading: bool,
    log_lines: Vec<String>,
    log_scroll: u16,
    log_lines_count: usize,
    last_poll: Instant,
    is_loading: bool,
    poll_in_flight: bool,
    action_result: ActionResult,
    action_result_at: Option<Instant>,
    confirm_action: Option<ConfirmAction>,
    cluster: ClusterSnapshot,
    tick: u64,
    tx: mpsc::Sender<BgMsg>,
    rx: mpsc::Receiver<BgMsg>,
}

impl App {
    fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        let (tx, rx) = mpsc::channel::<BgMsg>(32);
        Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
            containers: Vec::new(),
            list_state,
            view: View::ContainerList,
            tab: Tab::Containers,
            ingress_routes: Vec::new(),
            ingress_list_state: ListState::default(),
            ingress_poll_in_flight: false,
            ingress_last_poll: Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1),
            nodes: Vec::new(),
            nodes_list_state: ListState::default(),
            nodes_poll_in_flight: false,
            nodes_last_poll: Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1),
            specs: Vec::new(),
            specs_list_state: ListState::default(),
            describe_data: None,
            describe_loading: false,
            log_lines: Vec::new(),
            log_scroll: 0,
            log_lines_count: 200,
            last_poll: Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1),
            is_loading: false,
            poll_in_flight: false,
            action_result: ActionResult::None,
            action_result_at: None,
            confirm_action: None,
            cluster: ClusterSnapshot::from_connection(),
            tick: 0,
            tx,
            rx,
        }
    }

    fn selected_container(&self) -> Option<&ContainerInfo> {
        self.list_state
            .selected()
            .and_then(|i| self.containers.get(i))
    }
    fn selected_name(&self) -> Option<String> {
        self.selected_container().map(|c| c.name.clone())
    }
    fn next(&mut self) {
        if self.containers.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        self.list_state
            .select(Some((i + 1) % self.containers.len()));
    }
    fn prev(&mut self) {
        if self.containers.is_empty() {
            return;
        }
        let i = self.list_state.selected().unwrap_or(0);
        self.list_state.select(Some(if i == 0 {
            self.containers.len() - 1
        } else {
            i - 1
        }));
    }
    fn set_result(&mut self, r: ActionResult) {
        self.action_result = r;
        self.action_result_at = Some(Instant::now());
    }
    fn maybe_clear_result(&mut self) {
        if let Some(t) = self.action_result_at {
            if t.elapsed() > Duration::from_secs(4) {
                self.action_result = ActionResult::None;
                self.action_result_at = None;
            }
        }
    }
    fn should_poll(&self) -> bool {
        !self.poll_in_flight && self.last_poll.elapsed() >= Duration::from_secs(POLL_INTERVAL_SECS)
    }
    fn should_poll_ingress(&self) -> bool {
        !self.ingress_poll_in_flight
            && self.ingress_last_poll.elapsed() >= Duration::from_secs(POLL_INTERVAL_SECS)
    }
    fn should_poll_nodes(&self) -> bool {
        !self.nodes_poll_in_flight
            && self.nodes_last_poll.elapsed() >= Duration::from_secs(POLL_INTERVAL_SECS)
    }
    fn nodes_online(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.status.to_lowercase() == "online")
            .count()
    }
    /// Rebuild the specs list from disk. Cheap — called on tab switch + refresh.
    fn reload_specs(&mut self) {
        let aliases = super::load_aliases();
        // reverse map: container_name → alias
        let mut reverse: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for (alias, name) in aliases.iter() {
            reverse.insert(name.clone(), alias.clone());
        }
        let specs = super::list_deployment_specs();
        self.specs = specs
            .into_iter()
            .map(|container_name| {
                let alias = reverse.get(&container_name).cloned();
                let image = parse_spec_image(&container_name);
                SpecEntry {
                    container_name,
                    alias,
                    image,
                }
            })
            .collect();
        if self.specs_list_state.selected().is_none() && !self.specs.is_empty() {
            self.specs_list_state.select(Some(0));
        }
        if let Some(i) = self.specs_list_state.selected() {
            if i >= self.specs.len() && !self.specs.is_empty() {
                self.specs_list_state.select(Some(self.specs.len() - 1));
            }
        }
    }

    /// Determine the tri-state for a spec by cross-referencing against the
    /// live container list. "Updated" is detected by image mismatch — env/port
    /// edits will still show as "deployed" (cheap approximation; the real
    /// diff lives in `nordkraft diff`).
    ///
    /// Images are normalized before comparison so `registry://foo:v1` and
    /// its resolved form `172.21.1.3:5001/foo:v1` compare equal.
    fn spec_state(&self, entry: &SpecEntry) -> SpecState {
        let live = self
            .containers
            .iter()
            .find(|c| c.name == entry.container_name);
        match (live, &entry.image) {
            (None, _) => SpecState::NotDeployed,
            (Some(_), None) => SpecState::Deployed, // can't compare, assume ok
            (Some(c), Some(spec_img)) => {
                let spec_norm = super::normalize_image(spec_img);
                let live_norm = super::normalize_image(&c.image);
                if spec_norm == live_norm {
                    SpecState::Deployed
                } else {
                    SpecState::Updated
                }
            }
        }
    }
    fn running_count(&self) -> usize {
        self.containers
            .iter()
            .filter(|c| {
                let s = c.status.to_lowercase();
                s == "running" || s == "up"
            })
            .count()
    }

    fn spawn_poll(&mut self) {
        self.poll_in_flight = true;
        self.is_loading = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match fetch_containers(&client).await {
                Ok(c) => {
                    let _ = tx.send(BgMsg::Containers(c)).await;
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::ContainersFailed(e)).await;
                }
            }
        });
    }

    fn spawn_logs(&mut self, name: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        let count = self.log_lines_count;
        tokio::spawn(async move {
            match fetch_logs(&client, &name, count).await {
                Ok(raw) => {
                    let lines = raw.lines().map(|s| s.to_string()).collect();
                    let _ = tx.send(BgMsg::Logs { name, lines }).await;
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::LogsFailed(e)).await;
                }
            }
        });
    }

    fn spawn_action(&mut self, name: String, verb: &'static str) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match container_action(&client, &name, verb).await {
                Ok(_) => {
                    let _ = tx
                        .send(BgMsg::ActionDone {
                            verb: verb.to_string(),
                            name,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::ActionFailed(e)).await;
                }
            }
        });
    }

    fn spawn_ingress_poll(&mut self) {
        self.ingress_poll_in_flight = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match fetch_ingress(&client).await {
                Ok(routes) => {
                    let _ = tx.send(BgMsg::Ingress(routes)).await;
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::IngressFailed(e)).await;
                }
            }
        });
    }

    fn spawn_nodes_poll(&mut self) {
        self.nodes_poll_in_flight = true;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match fetch_nodes(&client).await {
                Ok(nodes) => {
                    let _ = tx.send(BgMsg::Nodes(nodes)).await;
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::NodesFailed(e)).await;
                }
            }
        });
    }

    fn spawn_ingress_delete(&mut self, container_id: String) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match delete_ingress(&client, &container_id).await {
                Ok(_) => {
                    let _ = tx
                        .send(BgMsg::ActionDone {
                            verb: "disable ingress".to_string(),
                            name: container_id,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::ActionFailed(e)).await;
                }
            }
        });
    }

    fn spawn_describe_container(&mut self, name: String) {
        self.describe_loading = true;
        self.describe_data = None;
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match fetch_describe(&client, &name).await {
                Ok(d) => {
                    let _ = tx.send(BgMsg::Describe(d)).await;
                }
                Err(e) => {
                    let _ = tx.send(BgMsg::DescribeFailed(e)).await;
                }
            }
        });
    }

    fn spawn_connection_check(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let ok = check_connection(&client).await;
            let _ = tx.send(BgMsg::Connection(ok)).await;
        });
    }

    fn spawn_usage_poll(&mut self) {
        let client = self.client.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(data) = fetch_usage(&client).await {
                let cpu_ratio = data["ratios"]["cpu"].as_f64().unwrap_or(0.0);
                let ram_ratio = data["ratios"]["memory"].as_f64().unwrap_or(0.0);
                let disk_ratio = data["ratios"]["disk"].as_f64().unwrap_or(0.0);
                let cpu_used = data["usage"]["cpu"].as_f64().unwrap_or(0.0);
                let cpu_max = data["plan"]["limits"]["cpu"].as_f64().unwrap_or(1.0);
                let mem_used = data["usage"]["memory_mb"].as_i64().unwrap_or(0);
                let mem_max = data["plan"]["limits"]["memory_mb"].as_i64().unwrap_or(512);
                let disk_used = data["usage"]["disk_mb"].as_i64().unwrap_or(0);
                let disk_max = data["plan"]["limits"]["storage_mb"]
                    .as_i64()
                    .unwrap_or(102400);

                let disk_label = if disk_max >= 1024 {
                    format!(
                        "{:.1}/{:.0}GB",
                        disk_used as f64 / 1024.0,
                        disk_max as f64 / 1024.0
                    )
                } else {
                    format!("{}/{}MB", disk_used, disk_max)
                };

                let _ = tx
                    .send(BgMsg::Usage {
                        cpu_ratio,
                        ram_ratio,
                        disk_ratio,
                        cpu_label: format!("{:.1}/{:.1} vCPU", cpu_used, cpu_max),
                        ram_label: format!("{}/{}MB", mem_used, mem_max),
                        disk_label,
                    })
                    .await;
            }
        });
    }

    fn apply_bg_messages(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                BgMsg::Containers(new) => {
                    if !new.is_empty() {
                        if let Some(i) = self.list_state.selected() {
                            if i >= new.len() {
                                self.list_state.select(Some(new.len() - 1));
                            }
                        }
                        if self.list_state.selected().is_none() {
                            self.list_state.select(Some(0));
                        }
                        self.containers = new;
                    }
                    self.poll_in_flight = false;
                    self.is_loading = false;
                    self.last_poll = Instant::now();
                }
                BgMsg::ContainersFailed(e) => {
                    self.set_result(ActionResult::Error(e));
                    self.poll_in_flight = false;
                    self.is_loading = false;
                    self.last_poll = Instant::now();
                }
                BgMsg::Ingress(routes) => {
                    if self.ingress_list_state.selected().is_none() && !routes.is_empty() {
                        self.ingress_list_state.select(Some(0));
                    }
                    self.ingress_routes = routes;
                    self.ingress_poll_in_flight = false;
                    self.ingress_last_poll = Instant::now();
                }
                BgMsg::IngressFailed(e) => {
                    self.set_result(ActionResult::Error(format!("ingress: {e}")));
                    self.ingress_poll_in_flight = false;
                    self.ingress_last_poll = Instant::now();
                }
                BgMsg::Nodes(nodes) => {
                    if self.nodes_list_state.selected().is_none() && !nodes.is_empty() {
                        self.nodes_list_state.select(Some(0));
                    }
                    self.nodes = nodes;
                    self.cluster.node_count = self.nodes.len();
                    self.cluster.nodes_online = self.nodes_online();
                    self.nodes_poll_in_flight = false;
                    self.nodes_last_poll = Instant::now();
                }
                BgMsg::NodesFailed(e) => {
                    self.set_result(ActionResult::Error(format!("nodes: {e}")));
                    self.nodes_poll_in_flight = false;
                    self.nodes_last_poll = Instant::now();
                }
                BgMsg::Describe(d) => {
                    self.describe_data = Some(d);
                    self.describe_loading = false;
                    self.action_result = ActionResult::None;
                }
                BgMsg::DescribeFailed(e) => {
                    self.describe_loading = false;
                    self.set_result(ActionResult::Error(format!("describe: {e}")));
                }
                BgMsg::Connection(ok) => {
                    self.cluster.connected = ok;
                }
                BgMsg::Logs { name: _, lines } => {
                    self.log_lines = lines;
                    self.log_scroll = self.log_lines.len().saturating_sub(1) as u16;
                    self.view = View::Logs;
                    self.action_result = ActionResult::None;
                }
                BgMsg::LogsFailed(e) => {
                    self.set_result(ActionResult::Error(e));
                }
                BgMsg::ActionDone { verb, name } => {
                    self.set_result(ActionResult::Success(format!("{verb} · {name}")));
                    // Force an immediate re-poll of both containers and ingress,
                    // since actions can affect either.
                    self.last_poll = Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
                    self.ingress_last_poll =
                        Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
                }
                BgMsg::ActionFailed(e) => {
                    self.set_result(ActionResult::Error(e));
                }
                BgMsg::Usage {
                    cpu_ratio,
                    ram_ratio,
                    disk_ratio,
                    cpu_label,
                    ram_label,
                    disk_label,
                } => {
                    self.cluster.cpu_used = cpu_ratio;
                    self.cluster.ram_used = ram_ratio;
                    self.cluster.disk_used = disk_ratio;
                    self.cluster.cpu_label = cpu_label;
                    self.cluster.ram_label = ram_label;
                    self.cluster.disk_label = disk_label;
                }
            }
        }
    }
}

// ─── API Calls ───────────────────────────────────────────────────────────────

async fn fetch_containers(client: &Client) -> Result<Vec<ContainerInfo>, String> {
    let resp = client
        .get(format!("{}/containers", *API_BASE_URL))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let data: ContainerListResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(data.containers)
}

async fn fetch_usage(client: &Client) -> Result<serde_json::Value, String> {
    let resp = client
        .get(format!("{}/usage", *API_BASE_URL))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    resp.json().await.map_err(|e| e.to_string())
}

async fn fetch_ingress(client: &Client) -> Result<Vec<IngressRoute>, String> {
    // FIX: endpoint is /api/ingress/list, not /api/ingress
    let resp = client
        .get(format!("{}/ingress/list", *API_BASE_URL))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let data: IngressListResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(data.routes)
}

async fn fetch_nodes(client: &Client) -> Result<Vec<NodeView>, String> {
    let resp = client
        .get(format!("{}/nodes", *API_BASE_URL))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let data: NodeListResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(data.nodes)
}

async fn fetch_describe(client: &Client, name: &str) -> Result<ContainerDescribe, String> {
    let resp = client
        .get(format!("{}/containers/{}", *API_BASE_URL, name))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("{status}: {body}"));
    }
    serde_json::from_str::<ContainerDescribe>(&body).map_err(|e| format!("parse: {e}"))
}

async fn delete_ingress(client: &Client, container_id: &str) -> Result<(), String> {
    let url = format!("{}/ingress/{}/disable", *API_BASE_URL, container_id);
    let resp = client.delete(&url).send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(format!("{status}: {body}"));
    }
    // Best-effort error parse — endpoint may return {error: "..."} even on 200
    if let Ok(data) = serde_json::from_str::<ApiResponse>(&body) {
        if let Some(err) = data.error {
            return Err(err);
        }
    }
    Ok(())
}

async fn fetch_logs(client: &Client, name: &str, lines: usize) -> Result<String, String> {
    let resp = client
        .get(format!(
            "{}/containers/{}/logs?lines={}",
            *API_BASE_URL, name, lines
        ))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let data: LogsResponse = resp.json().await.map_err(|e| e.to_string())?;
    Ok(data.logs)
}

async fn container_action(client: &Client, name: &str, action: &str) -> Result<(), String> {
    let url = match action {
        "stop" => format!("{}/containers/{}/stop", *API_BASE_URL, name),
        "start" => format!("{}/containers/{}/start", *API_BASE_URL, name),
        "restart" => format!("{}/containers/{}/restart", *API_BASE_URL, name),
        "remove" => format!("{}/containers/{}", *API_BASE_URL, name),
        _ => return Err(format!("Unknown action: {action}")),
    };
    let req = if action == "remove" {
        client.delete(&url)
    } else {
        client.post(&url)
    };
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let data: ApiResponse = resp.json().await.map_err(|e| e.to_string())?;
    if let Some(err) = data.error {
        return Err(err);
    }
    Ok(())
}

// ─── Entry Point ─────────────────────────────────────────────────────────────

pub async fn run_tui() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // NOTE: No EnableMouseCapture — we deliberately let the terminal keep
    // control of mouse events so users can select and copy text natively
    // (drag to select, Cmd/Ctrl+C to copy). The TUI is keyboard-only so
    // we lose nothing by not capturing mouse.
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new();
    let result = run_loop(&mut terminal, &mut app).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    if let Err(e) = result {
        eprintln!("TUI error: {e}");
    }
    Ok(())
}

// ─── Event Loop ──────────────────────────────────────────────────────────────

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        app.apply_bg_messages();

        if app.tab == Tab::Containers && app.view == View::ContainerList && app.should_poll() {
            app.spawn_poll();
            app.spawn_usage_poll(); // piggyback on same interval
        }
        if app.tab == Tab::Ingress && app.should_poll_ingress() {
            app.spawn_ingress_poll();
        }
        // Poll nodes always — the header summary (N/M online) depends on it,
        // not just the Nodes tab. Piggyback the connection check on the same
        // cadence so the wg-up badge stays fresh.
        if app.should_poll_nodes() {
            app.spawn_nodes_poll();
            app.spawn_connection_check();
        }

        app.maybe_clear_result();
        app.tick = app.tick.wrapping_add(1);

        terminal.draw(|f| ui(f, app))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Global quit
                if key.code == KeyCode::Char('q')
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL))
                {
                    return Ok(());
                }

                // Confirm dialog steals all input
                if app.confirm_action.is_some() {
                    handle_confirm_key(app, key.code);
                    continue;
                }

                // ── Tab switching ─────────────────────────────────────────
                // Tab / Shift+Tab / ← / → all switch tabs from the main list view
                if matches!(app.view, View::ContainerList) {
                    let switch = match key.code {
                        KeyCode::Tab => Some(true),      // Tab → next
                        KeyCode::BackTab => Some(false), // Shift+Tab → prev
                        KeyCode::Right => Some(true),
                        KeyCode::Left => Some(false),
                        _ => Option::None,
                    };
                    if let Some(forward) = switch {
                        app.tab = if forward {
                            app.tab.next()
                        } else {
                            app.tab.prev()
                        };
                        if app.tab == Tab::Specs {
                            app.reload_specs();
                        }
                        continue;
                    }
                }

                // ── Per-view routing ──────────────────────────────────────
                match app.view {
                    View::ContainerList if app.tab == Tab::Containers => {
                        // `e` on containers → edit the backing .nk spec if
                        // one exists. Shortcut to the Specs-tab edit flow
                        // without forcing a tab switch.
                        if key.code == KeyCode::Char('e') {
                            if let Some(name) = app.selected_name() {
                                let path = super::nk_path(&name);
                                if !path.exists() {
                                    app.set_result(ActionResult::Error(format!(
                                        "no spec · run 'nordkraft init {}'",
                                        name
                                    )));
                                } else if let Err(e) = edit_spec_in_editor(terminal, &path) {
                                    app.set_result(ActionResult::Error(format!(
                                        "editor: {e}"
                                    )));
                                } else {
                                    app.set_result(ActionResult::Success(format!(
                                        "edited · spec for {}",
                                        name
                                    )));
                                    app.reload_specs();
                                }
                            }
                        } else {
                            handle_list_key(app, key.code)
                        }
                    }
                    View::ContainerList if app.tab == Tab::Ingress => {
                        handle_ingress_key(app, key.code)
                    }
                    View::ContainerList if app.tab == Tab::Nodes => {
                        handle_nodes_key(app, key.code)
                    }
                    View::ContainerList if app.tab == Tab::Specs => {
                        // `e` and `u` both shell out to subprocesses and
                        // therefore need &mut Terminal — handled inline.
                        if key.code == KeyCode::Char('e') {
                            if let Some(path) = selected_spec_path(app) {
                                if let Err(e) = edit_spec_in_editor(terminal, &path) {
                                    app.set_result(ActionResult::Error(format!(
                                        "editor: {e}"
                                    )));
                                } else {
                                    app.set_result(ActionResult::Success(format!(
                                        "edited · {}",
                                        path.file_name()
                                            .and_then(|s| s.to_str())
                                            .unwrap_or("spec")
                                    )));
                                    // Image may have changed — reload so the
                                    // tri-state reflects it on the next frame.
                                    app.reload_specs();
                                }
                            }
                        } else if key.code == KeyCode::Char('u') {
                            // Upgrade: only meaningful when state == Updated.
                            if let Some(i) = app.specs_list_state.selected() {
                                if let Some(entry) = app.specs.get(i).cloned() {
                                    let state = app.spec_state(&entry);
                                    if state != SpecState::Updated {
                                        app.set_result(ActionResult::Error(
                                            "nothing to upgrade — spec is up to date".into(),
                                        ));
                                    } else {
                                        let name = entry.container_name.clone();
                                        let r = run_cli_subprocess(
                                            terminal,
                                            "nordkraft",
                                            &["upgrade", &name, "--yes"],
                                        );
                                        match r {
                                            Ok(_) => {
                                                app.set_result(ActionResult::Success(format!(
                                                    "upgraded · {}",
                                                    entry.alias.as_deref().unwrap_or(&name)
                                                )));
                                                // Force container re-poll so
                                                // the list reflects the new image.
                                                app.last_poll = Instant::now()
                                                    - Duration::from_secs(
                                                        POLL_INTERVAL_SECS + 1,
                                                    );
                                                app.reload_specs();
                                            }
                                            Err(e) => {
                                                app.set_result(ActionResult::Error(format!(
                                                    "upgrade: {e}"
                                                )));
                                            }
                                        }
                                    }
                                }
                            }
                        } else {
                            handle_specs_key(app, key.code);
                        }
                    }
                    View::ContainerList => {}
                    View::Logs => handle_logs_key(app, key.code),
                    View::Describe => handle_describe_key(app, key.code),
                    View::Help => {
                        app.view = View::ContainerList;
                    }
                }
            }
        }
    }
}

// ─── Key Handlers ────────────────────────────────────────────────────────────

fn handle_list_key(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Down | KeyCode::Char('j') => app.next(),
        KeyCode::Up | KeyCode::Char('k') => app.prev(),
        KeyCode::Char('l') | KeyCode::Enter => {
            if let Some(name) = app.selected_name() {
                app.set_result(ActionResult::Pending(format!("fetching logs · {name}")));
                app.spawn_logs(name);
            }
        }
        KeyCode::Char('i') | KeyCode::Char('v') => {
            if let Some(name) = app.selected_name() {
                app.view = View::Describe;
                app.spawn_describe_container(name);
            }
        }
        KeyCode::Char('s') => {
            if let Some(c) = app.selected_container() {
                let s = c.status.to_lowercase();
                if s == "running" || s == "up" {
                    app.confirm_action = Some(ConfirmAction::Stop(c.name.clone()));
                }
            }
        }
        KeyCode::Char('r') => {
            if let Some(name) = app.selected_name() {
                app.confirm_action = Some(ConfirmAction::Restart(name));
            }
        }
        KeyCode::Char('d') => {
            if let Some(name) = app.selected_name() {
                app.confirm_action = Some(ConfirmAction::Remove(name));
            }
        }
        KeyCode::Char('R') => {
            app.last_poll = Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
        }
        KeyCode::Char('?') => app.view = View::Help,
        _ => {}
    }
}

fn handle_ingress_key(app: &mut App, key: KeyCode) {
    let len = app.ingress_routes.len();
    if len == 0 {
        return;
    }
    match key {
        KeyCode::Down | KeyCode::Char('j') => {
            let i = app.ingress_list_state.selected().unwrap_or(0);
            app.ingress_list_state.select(Some((i + 1) % len));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.ingress_list_state.selected().unwrap_or(0);
            app.ingress_list_state
                .select(Some(if i == 0 { len - 1 } else { i - 1 }));
        }
        KeyCode::Char('v') => {
            if app.ingress_list_state.selected().is_some() {
                app.view = View::Describe;
            }
        }
        KeyCode::Char('D') => {
            if let Some(i) = app.ingress_list_state.selected() {
                if let Some(route) = app.ingress_routes.get(i) {
                    if route.container_id.is_empty() {
                        app.set_result(ActionResult::Error(
                            "missing container_id — cannot delete".to_string(),
                        ));
                    } else {
                        app.confirm_action = Some(ConfirmAction::DeleteIngress(
                            route.container_id.clone(),
                            route.subdomain.clone(),
                        ));
                    }
                }
            }
        }
        KeyCode::Char('R') => {
            app.ingress_last_poll = Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
        }
        KeyCode::Char('?') => app.view = View::Help,
        _ => {}
    }
}

fn handle_nodes_key(app: &mut App, key: KeyCode) {
    let len = app.nodes.len();
    if len == 0 {
        if key == KeyCode::Char('R') {
            app.nodes_last_poll = Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
        }
        return;
    }
    match key {
        KeyCode::Down | KeyCode::Char('j') => {
            let i = app.nodes_list_state.selected().unwrap_or(0);
            app.nodes_list_state.select(Some((i + 1) % len));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.nodes_list_state.selected().unwrap_or(0);
            app.nodes_list_state
                .select(Some(if i == 0 { len - 1 } else { i - 1 }));
        }
        KeyCode::Char('v') => {
            if app.nodes_list_state.selected().is_some() {
                app.view = View::Describe;
            }
        }
        KeyCode::Char('R') => {
            app.nodes_last_poll = Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
        }
        KeyCode::Char('?') => app.view = View::Help,
        _ => {}
    }
}

fn handle_specs_key(app: &mut App, key: KeyCode) {
    let len = app.specs.len();
    if len == 0 {
        if key == KeyCode::Char('R') {
            app.reload_specs();
        }
        return;
    }
    match key {
        KeyCode::Down | KeyCode::Char('j') => {
            let i = app.specs_list_state.selected().unwrap_or(0);
            app.specs_list_state.select(Some((i + 1) % len));
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let i = app.specs_list_state.selected().unwrap_or(0);
            app.specs_list_state
                .select(Some(if i == 0 { len - 1 } else { i - 1 }));
        }
        KeyCode::Char('v') => {
            if app.specs_list_state.selected().is_some() {
                app.view = View::Describe;
            }
        }
        KeyCode::Char('D') => {
            if let Some(i) = app.specs_list_state.selected() {
                if let Some(s) = app.specs.get(i) {
                    let label = s
                        .alias
                        .clone()
                        .unwrap_or_else(|| truncate(&s.container_name, 30));
                    app.confirm_action =
                        Some(ConfirmAction::DeleteSpec(s.container_name.clone(), label));
                }
            }
        }
        KeyCode::Char('R') => app.reload_specs(),
        KeyCode::Char('?') => app.view = View::Help,
        _ => {}
    }
}

fn selected_spec_path(app: &App) -> Option<std::path::PathBuf> {
    let i = app.specs_list_state.selected()?;
    let entry = app.specs.get(i)?;
    Some(super::nk_path(&entry.container_name))
}

/// Suspend the alternate-screen TUI, run `$EDITOR` on `path`, and resume.
/// Restores terminal state on error paths so the user never gets a broken shell.
fn edit_spec_in_editor(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    path: &std::path::Path,
) -> Result<(), String> {
    // Tear down alternate screen + raw mode so vim gets a real TTY.
    disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|e| e.to_string())?;

    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let run_result = std::process::Command::new(&editor).arg(path).status();

    // Always restore the TUI, even if the editor failed — otherwise the user
    // is stuck with half-broken terminal state.
    enable_raw_mode().map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), EnterAlternateScreen).map_err(|e| e.to_string())?;
    terminal.clear().map_err(|e| e.to_string())?;

    match run_result {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{editor} exited with status {s}")),
        Err(e) => Err(format!("{editor} failed: {e}")),
    }
}

/// Run a shell command (like `nordkraft upgrade`) with full TTY access.
/// Same suspend/resume dance as the editor — but for CLI subprocesses that
/// want to print to the screen and prompt the user.
fn run_cli_subprocess(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    program: &str,
    args: &[&str],
) -> Result<(), String> {
    disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|e| e.to_string())?;

    println!("\n$ {} {}\n", program, args.join(" "));
    let run_result = std::process::Command::new(program).args(args).status();

    // Pause briefly so the user can read the output before we re-enter.
    println!("\n[press Enter to return to TUI]");
    let _ = std::io::stdin().read_line(&mut String::new());

    enable_raw_mode().map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), EnterAlternateScreen).map_err(|e| e.to_string())?;
    terminal.clear().map_err(|e| e.to_string())?;

    match run_result {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("{program} exited with status {s}")),
        Err(e) => Err(format!("{program} failed: {e}")),
    }
}

fn handle_logs_key(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc | KeyCode::Char('b') => app.view = View::ContainerList,
        KeyCode::Down | KeyCode::Char('j') => app.log_scroll = app.log_scroll.saturating_add(1),
        KeyCode::Up | KeyCode::Char('k') => app.log_scroll = app.log_scroll.saturating_sub(1),
        KeyCode::PageDown => app.log_scroll = app.log_scroll.saturating_add(20),
        KeyCode::PageUp => app.log_scroll = app.log_scroll.saturating_sub(20),
        KeyCode::Char('G') => app.log_scroll = app.log_lines.len().saturating_sub(1) as u16,
        KeyCode::Char('g') => app.log_scroll = 0,
        _ => {}
    }
}

fn handle_describe_key(app: &mut App, key: KeyCode) {
    if matches!(key, KeyCode::Esc | KeyCode::Char('b')) {
        app.view = View::ContainerList;
        app.describe_data = None;
        app.describe_loading = false;
    }
}

fn handle_confirm_key(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('y') | KeyCode::Enter => {
            if let Some(action) = app.confirm_action.take() {
                match action {
                    ConfirmAction::Stop(n) => {
                        app.set_result(ActionResult::Pending(format!("stop · {n}…")));
                        app.spawn_action(n, "stop");
                    }
                    ConfirmAction::Remove(n) => {
                        app.set_result(ActionResult::Pending(format!("remove · {n}…")));
                        app.spawn_action(n, "remove");
                    }
                    ConfirmAction::Restart(n) => {
                        app.set_result(ActionResult::Pending(format!("restart · {n}…")));
                        app.spawn_action(n, "restart");
                    }
                    ConfirmAction::DeleteIngress(container_id, subdomain) => {
                        app.set_result(ActionResult::Pending(format!(
                            "disable ingress · {subdomain}…"
                        )));
                        app.spawn_ingress_delete(container_id);
                    }
                    ConfirmAction::DeleteSpec(container_name, label) => {
                        let path = super::nk_path(&container_name);
                        match std::fs::remove_file(&path) {
                            Ok(_) => {
                                app.set_result(ActionResult::Success(format!(
                                    "deleted spec · {label}"
                                )));
                                app.reload_specs();
                            }
                            Err(e) => {
                                app.set_result(ActionResult::Error(format!(
                                    "delete spec: {e}"
                                )));
                            }
                        }
                    }
                }
            }
        }
        _ => {
            app.confirm_action = None;
        }
    }
}

// ─── UI Root ─────────────────────────────────────────────────────────────────

fn ui(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.render_widget(Block::default().style(Style::default().bg(PANEL_BG)), area);

    match app.view {
        View::ContainerList => render_main(f, app, area),
        View::Logs => render_logs(f, app, area),
        View::Describe => render_describe(f, app, area),
        View::Help => render_help(f, area),
    }

    if let Some(ref action) = app.confirm_action.clone() {
        render_confirm(f, action, area);
    }

    render_status_bar(f, app, area);
}

// ─── Main View ───────────────────────────────────────────────────────────────

fn render_main(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // header / cluster bar
            Constraint::Length(3), // gauge row
            Constraint::Length(2), // tab bar
            Constraint::Min(0),    // tab content
            Constraint::Length(1), // keybind strip
        ])
        .split(area);

    render_header(f, app, chunks[0]);
    render_gauges(f, app, chunks[1]);
    render_tab_bar(f, app, chunks[2]);

    match app.tab {
        Tab::Containers => render_container_list(f, app, chunks[3]),
        Tab::Ingress => render_ingress_list(f, app, chunks[3]),
        Tab::Nodes => render_nodes_list(f, app, chunks[3]),
        Tab::Specs => render_specs_list(f, app, chunks[3]),
    }

    render_keybinds(f, app, chunks[4]);
}

// ── Header ────────────────────────────────────────────────────────────────────

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(36)])
        .split(area);

    let spin = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner = if app.is_loading {
        spin[(app.tick as usize / 2) % spin.len()]
    } else {
        "◆"
    };

    let running = app.running_count();
    let total = app.containers.len();

    let left_lines = vec![
        Line::from(vec![
            Span::styled(
                format!(" {} ", spinner),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "NordKraft.io",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ·  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("Cloud Garage: {}", app.cluster.garage),
                Style::default().fg(INDIGO).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if app.cluster.server_endpoint.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", app.cluster.server_endpoint)
                },
                Style::default().fg(MUTED),
            ),
        ]),
        Line::from(vec![Span::raw("   "), status_pill(running, total)]),
        Line::from(vec![
            Span::raw("   "),
            connection_badge(app.cluster.connected),
            Span::raw("   "),
            Span::styled(
                format!("{:.1} GB traffic this month", app.cluster.traffic_gb),
                Style::default().fg(MUTED),
            ),
            Span::styled(
                "  [TODO: /api/usage]",
                Style::default().fg(Color::Rgb(45, 50, 65)),
            ),
        ]),
        Line::from(vec![
            Span::raw("   "),
            co2_badge(app.cluster.co2_g_kwh),
            Span::styled(
                "  [TODO: Energinet API]",
                Style::default().fg(Color::Rgb(45, 50, 65)),
            ),
        ]),
    ];

    let left = Paragraph::new(left_lines).block(
        Block::default()
            .borders(Borders::BOTTOM | Borders::RIGHT)
            .border_style(Style::default().fg(Color::Rgb(30, 40, 60)))
            .style(Style::default().bg(HEADER_BG)),
    );
    f.render_widget(left, cols[0]);

    let mut node_lines = vec![Line::from(Span::styled(
        " NODES",
        Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
    ))];

    if app.nodes.is_empty() {
        node_lines.push(Line::from(Span::styled(
            if app.nodes_poll_in_flight {
                " ⟳ fetching…"
            } else {
                " (none registered)"
            },
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
    } else {
        let online = app.nodes_online();
        let total = app.nodes.len();
        let color = if online == total { EMERALD } else { AMBER };
        node_lines.push(Line::from(vec![
            Span::styled(" ● ", Style::default().fg(color)),
            Span::styled(
                format!("{online}/{total} online"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]));
        node_lines.push(Line::from(Span::styled(
            " see Nodes tab for detail",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )));
    }

    let right = Paragraph::new(node_lines).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::Rgb(30, 40, 60)))
            .style(Style::default().bg(HEADER_BG)),
    );
    f.render_widget(right, cols[1]);
}

fn status_pill(running: usize, total: usize) -> Span<'static> {
    let stopped = total - running;
    let color = if stopped > 0 { AMBER } else { EMERALD };
    Span::styled(
        format!("{running} running  {stopped} stopped  {total} total"),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

fn co2_badge(g: f64) -> Span<'static> {
    let (label, color) = if g < 100.0 {
        (format!("⚡ {g:.0} gCO₂/kWh  green"), EMERALD)
    } else if g < 200.0 {
        (format!("⚡ {g:.0} gCO₂/kWh  mixed"), AMBER)
    } else {
        (format!("⚡ {g:.0} gCO₂/kWh  grid"), ROSE)
    };
    Span::styled(label, Style::default().fg(color))
}

fn connection_badge(connected: bool) -> Span<'static> {
    if connected {
        Span::styled(
            "● connected",
            Style::default().fg(EMERALD).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(
            "○ disconnected",
            Style::default().fg(ROSE).add_modifier(Modifier::BOLD),
        )
    }
}

// ── Gauge row ─────────────────────────────────────────────────────────────────

fn render_gauges(f: &mut Frame, app: &App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(33),
            Constraint::Percentage(34),
        ])
        .split(area);

    render_gauge(
        f,
        cols[0],
        "CPU allocated",
        &app.cluster.cpu_label,
        app.cluster.cpu_used,
    );
    render_gauge(
        f,
        cols[1],
        "RAM allocated",
        &app.cluster.ram_label,
        app.cluster.ram_used,
    );
    render_gauge(
        f,
        cols[2],
        "DISK allocated",
        &app.cluster.disk_label,
        app.cluster.disk_used,
    );
}

fn render_gauge(f: &mut Frame, area: Rect, label: &str, detail: &str, ratio: f64) {
    let ratio = ratio.clamp(0.0, 1.0);
    let pct = (ratio * 100.0) as u16;
    let color = if ratio < 0.85 {
        EMERALD
    } else if ratio < 0.95 {
        AMBER
    } else {
        ROSE
    };
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(30, 40, 60)))
                .title(Span::styled(
                    format!(" {label} · {detail} "),
                    Style::default().fg(MUTED),
                ))
                .style(Style::default().bg(PANEL_BG)),
        )
        .gauge_style(Style::default().fg(color).bg(Color::Rgb(20, 20, 30)))
        .ratio(ratio)
        .label(Span::styled(
            format!("{pct}%"),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(gauge, area);
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

fn render_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let tabs = [Tab::Containers, Tab::Ingress, Tab::Nodes, Tab::Specs];
    let mut spans = vec![Span::raw(" ")];
    for tab in &tabs {
        let active = &app.tab == tab;
        let count_badge = match tab {
            Tab::Containers => format!(" {} ", app.containers.len()),
            Tab::Ingress => format!(" {} ", app.ingress_routes.len()),
            Tab::Nodes => format!(" {} ", app.nodes.len()),
            Tab::Specs => format!(" {} ", app.specs.len()),
        };
        if active {
            spans.push(Span::styled(
                format!("  {}{}  ", tab.title(), count_badge),
                Style::default()
                    .fg(PANEL_BG)
                    .bg(CYAN)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!("  {}{}  ", tab.title(), count_badge),
                Style::default().fg(MUTED).bg(Color::Rgb(18, 25, 40)),
            ));
        }
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        "   Tab/Shift+Tab  ←/→",
        Style::default().fg(Color::Rgb(55, 65, 85)),
    ));

    let tab_line = Paragraph::new(Line::from(spans)).block(
        Block::default()
            .borders(Borders::BOTTOM)
            .border_style(Style::default().fg(Color::Rgb(25, 35, 55)))
            .style(Style::default().bg(HEADER_BG)),
    );
    f.render_widget(tab_line, area);
}

// ── Container list ────────────────────────────────────────────────────────────

fn render_container_list(f: &mut Frame, app: &mut App, area: Rect) {
    let header_area = Rect { height: 1, ..area };
    let list_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    // Build reverse alias map once per render (cheap: usually <20 entries).
    // Maps container_name → alias so we can show "nginx" instead of the
    // raw app-UUID as the primary identifier.
    let aliases = super::load_aliases();
    let reverse_aliases: std::collections::HashMap<&String, &String> =
        aliases.iter().map(|(alias, name)| (name, alias)).collect();

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   ST  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:<22}", "NAME"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<40}", "ID"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<24}", "IMAGE"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<11}", "STATUS"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<16}", "IP"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<17}", "CREATED"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "V6",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ])),
        header_area,
    );

    let items: Vec<ListItem> = app
        .containers
        .iter()
        .map(|c| {
            let (icon, status_color) = status_icon_color(&c.status);
            // Primary display: alias if present, otherwise fall back to the
            // container name. The raw ID is always shown next to it in a
            // dimmed color so the full handle is still visible for copying.
            let alias_display = match reverse_aliases.get(&c.name) {
                Some(alias) => alias.as_str().to_string(),
                None => "—".to_string(),
            };
            let image = truncate(&c.image, 23);
            let ip = c.container_ip.as_deref().unwrap_or("—");
            let created = fmt_ts(&c.created_at);
            let status_text = truncate(&c.status, 10);
            let v6 = if c.ipv6_enabled {
                Span::styled(
                    "v6",
                    Style::default().fg(INDIGO).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled("  ", Style::default())
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {icon}  "), Style::default().fg(status_color)),
                Span::styled(
                    format!("{:<22}", truncate(&alias_display, 21)),
                    Style::default()
                        .fg(CYAN)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<40}", truncate(&c.name, 39)),
                    // Dimmed — still visible (for copying into CLI commands)
                    // but visually recedes into the background so the alias
                    // reads as the primary identifier.
                    Style::default().fg(Color::Rgb(70, 80, 100)),
                ),
                Span::styled(
                    format!("{:<24}", image),
                    Style::default().fg(Color::Rgb(150, 160, 180)),
                ),
                Span::styled(
                    format!("{:<11}", status_text),
                    Style::default().fg(status_color),
                ),
                Span::styled(format!("{:<16}", ip), Style::default().fg(MUTED)),
                Span::styled(format!("{:<17}", created), Style::default().fg(MUTED)),
                v6,
            ]))
        })
        .collect();

    let list =
        List::new(items).highlight_style(Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, list_area, &mut app.list_state);
}

fn status_icon_color(status: &str) -> (&'static str, Color) {
    let s = status.to_lowercase();
    if s == "running" || s == "up" {
        ("●", EMERALD)
    } else if s == "stopped" || s == "exited" || s.starts_with("exited") {
        ("○", ROSE)
    } else if s == "starting" || s == "deploying" {
        ("◎", AMBER)
    } else if s.starts_with("failed") {
        ("✖", ROSE)
    } else if s == "paused" {
        ("⏸", INDIGO)
    } else {
        ("?", MUTED)
    }
}

// ── Keybind strip ─────────────────────────────────────────────────────────────

fn render_keybinds(f: &mut Frame, app: &App, area: Rect) {
    let line = match app.tab {
        Tab::Containers => Line::from(vec![
            kb("↑↓", "nav"),
            kb("l/↵", "logs"),
            kb("v", "describe"),
            kb("e", "edit spec"),
            kb("s", "stop"),
            kb("r", "restart"),
            kb("d", "delete"),
            kb("R", "refresh"),
            kb("?", "help"),
            kb("q", "quit"),
        ]),
        Tab::Ingress => Line::from(vec![
            kb("↑↓", "nav"),
            kb("v", "describe"),
            kb("⇧D", "disable"),
            kb("R", "refresh"),
            kb("?", "help"),
            kb("q", "quit"),
        ]),
        Tab::Nodes => Line::from(vec![
            kb("↑↓", "nav"),
            kb("v", "describe"),
            kb("R", "refresh"),
            kb("?", "help"),
            kb("q", "quit"),
        ]),
        Tab::Specs => Line::from(vec![
            kb("↑↓", "nav"),
            kb("v", "describe"),
            kb("e", "edit"),
            kb("u", "upgrade"),
            kb("⇧D", "delete"),
            kb("R", "reload"),
            kb("?", "help"),
            kb("q", "quit"),
        ]),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn kb(key: &str, label: &str) -> Span<'static> {
    Span::styled(format!("  [{key}]{label}"), Style::default().fg(MUTED))
}

// ─── Nodes List ──────────────────────────────────────────────────────────────

fn render_nodes_list(f: &mut Frame, app: &mut App, area: Rect) {
    if app.nodes.is_empty() {
        let msg = if app.nodes_poll_in_flight {
            " ⟳ fetching nodes…"
        } else {
            " No nodes registered.  Agents register via POST /api/nodes/register"
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(MUTED)))
                .block(Block::default().style(Style::default().bg(PANEL_BG))),
            area,
        );
        return;
    }

    let header_area = Rect { height: 1, ..area };
    let list_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   ST  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:<24}", "ID"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<14}", "STATUS"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "LAST HEARTBEAT",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ])),
        header_area,
    );

    let items: Vec<ListItem> = app
        .nodes
        .iter()
        .map(|n| {
            let online = n.status.to_lowercase() == "online";
            let (icon, color) = if online { ("●", EMERALD) } else { ("○", ROSE) };
            let hb = n
                .last_heartbeat
                .as_deref()
                .map(fmt_ts)
                .unwrap_or_else(|| "—".to_string());
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {icon}  "), Style::default().fg(color)),
                Span::styled(
                    format!("{:<24}", truncate(&n.id, 23)),
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<14}", truncate(&n.status, 13)),
                    Style::default().fg(color),
                ),
                Span::styled(hb, Style::default().fg(MUTED)),
            ]))
        })
        .collect();

    let list =
        List::new(items).highlight_style(Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, list_area, &mut app.nodes_list_state);
}

// ─── Specs List ──────────────────────────────────────────────────────────────

fn render_specs_list(f: &mut Frame, app: &mut App, area: Rect) {
    if app.specs.is_empty() {
        let msg = " No deployment specs found.  Run 'nordkraft init <container>' to generate one.";
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(MUTED)))
                .block(Block::default().style(Style::default().bg(PANEL_BG))),
            area,
        );
        return;
    }

    let header_area = Rect { height: 1, ..area };
    let list_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   ST  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:<22}", "ALIAS"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<44}", "IMAGE"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "STATE",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ])),
        header_area,
    );

    let items: Vec<ListItem> = app
        .specs
        .iter()
        .map(|s| {
            let alias_label = s.alias.as_deref().unwrap_or("—");
            let image = s.image.as_deref().unwrap_or("—");
            let state = app.spec_state(s);
            let (icon, icon_color, state_text, state_color) = match state {
                SpecState::Deployed => ("●", EMERALD, "deployed", EMERALD),
                SpecState::Updated => ("◐", AMBER, "updated · press u", AMBER),
                SpecState::NotDeployed => ("◌", ROSE, "not deployed", ROSE),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {icon}  "), Style::default().fg(icon_color)),
                Span::styled(
                    format!("{:<22}", truncate(alias_label, 21)),
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<44}", truncate(image, 43)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    state_text.to_string(),
                    Style::default()
                        .fg(state_color)
                        .add_modifier(Modifier::BOLD),
                ),
            ]))
        })
        .collect();

    let list =
        List::new(items).highlight_style(Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, list_area, &mut app.specs_list_state);
}

// ─── Ingress List ────────────────────────────────────────────────────────────

fn render_ingress_list(f: &mut Frame, app: &mut App, area: Rect) {
    if app.ingress_routes.is_empty() {
        let msg = if app.ingress_poll_in_flight {
            " ⟳ fetching ingress routes…"
        } else {
            " No ingress routes configured.  nordkraft ingress enable <container> --subdomain <name>"
        };
        f.render_widget(
            Paragraph::new(Span::styled(msg, Style::default().fg(MUTED)))
                .block(Block::default().style(Style::default().bg(PANEL_BG))),
            area,
        );
        return;
    }

    let header_area = Rect { height: 1, ..area };
    let list_area = Rect {
        y: area.y + 1,
        height: area.height.saturating_sub(1),
        ..area
    };

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   ST  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:<18}", "SUBDOMAIN"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<42}", "URL"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<12}", "TARGET PORT"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<8}", "MODE"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "TARGET IP",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
        ])),
        header_area,
    );

    let items: Vec<ListItem> = app
        .ingress_routes
        .iter()
        .map(|r| {
            let (icon, color) = if r.is_active {
                ("●", EMERALD)
            } else {
                ("○", ROSE)
            };
            let mode = r.mode.as_deref().unwrap_or("https");
            let target = r.target_ip.as_deref().unwrap_or("—");
            let url = truncate(&r.url, 40);
            ListItem::new(Line::from(vec![
                Span::styled(format!("  {icon}  "), Style::default().fg(color)),
                Span::styled(
                    format!("{:<18}", truncate(&r.subdomain, 17)),
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{:<42}", url),
                    Style::default().fg(Color::Rgb(140, 160, 190)),
                ),
                Span::styled(format!("{:<12}", r.target_port), Style::default().fg(AMBER)),
                Span::styled(format!("{:<8}", mode), Style::default().fg(INDIGO)),
                Span::styled(target.to_owned(), Style::default().fg(MUTED)),
            ]))
        })
        .collect();

    let list =
        List::new(items).highlight_style(Style::default().bg(SEL_BG).add_modifier(Modifier::BOLD));
    f.render_stateful_widget(list, list_area, &mut app.ingress_list_state);
}

// ─── Log View ────────────────────────────────────────────────────────────────

fn render_logs(f: &mut Frame, app: &App, area: Rect) {
    let name = app
        .selected_container()
        .map(|c| c.name.as_str())
        .unwrap_or("?");
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let text: Vec<Line> = app
        .log_lines
        .iter()
        .skip(app.log_scroll as usize)
        .map(|l| {
            let color = if l.contains("ERROR") || l.contains("error") {
                ROSE
            } else if l.contains("WARN") || l.contains("warn") {
                AMBER
            } else if l.contains("INFO") || l.contains("info") {
                Color::Rgb(100, 180, 200)
            } else {
                Color::Rgb(130, 140, 160)
            };
            Line::from(Span::styled(l.clone(), Style::default().fg(color)))
        })
        .collect();

    let total = app.log_lines.len();
    let pos = app.log_scroll;

    let logs = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(30, 50, 80)))
                .title(Line::from(vec![
                    Span::styled(" Logs · ", Style::default().fg(MUTED)),
                    Span::styled(
                        name.to_owned(),
                        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {pos}/{total} "), Style::default().fg(MUTED)),
                ]))
                .style(Style::default().bg(PANEL_BG)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(logs, chunks[0]);

    let keys = Paragraph::new(Line::from(vec![
        kb("↑↓/jk", "scroll"),
        kb("PgUp/PgDn", "page"),
        kb("g", "top"),
        kb("G", "bottom"),
        kb("b/Esc", "back"),
    ]));
    f.render_widget(keys, chunks[1]);
}

// ─── Inspect View ────────────────────────────────────────────────────────────

// ─── Describe View ───────────────────────────────────────────────────────────
// Unified detail pane. Dispatches by the active tab so `v` works consistently
// on Containers / Ingress / Nodes / Specs.

fn render_describe(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let (title, lines) = match app.tab {
        Tab::Containers => describe_container_lines(app),
        Tab::Ingress => describe_ingress_lines(app),
        Tab::Nodes => describe_node_lines(app),
        Tab::Specs => describe_spec_lines(app),
    };

    let detail = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Rgb(30, 50, 80)))
                .title(Line::from(vec![
                    Span::styled(" Describe · ", Style::default().fg(MUTED)),
                    Span::styled(
                        title,
                        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" "),
                ]))
                .style(Style::default().bg(PANEL_BG)),
        );
    f.render_widget(detail, chunks[0]);

    let keys = Paragraph::new(Line::from(vec![kb("b/Esc", "back")]));
    f.render_widget(keys, chunks[1]);
}

// ── Containers ───────────────────────────────────────────────────────────────

fn describe_container_lines(app: &App) -> (String, Vec<Line<'static>>) {
    let slim = match app.selected_container() {
        Some(c) => c,
        None => {
            return (
                "—".into(),
                vec![Line::from(Span::styled(
                    "  No container selected.",
                    Style::default().fg(MUTED),
                ))],
            )
        }
    };
    let title = truncate(&slim.name, 40);

    // Prefer rich data from `/containers/{name}` when it's loaded. Fall back
    // to the slim list data instantly so the view is never blank.
    if let Some(d) = &app.describe_data {
        let (_, status_color) = status_icon_color(&d.status);
        let mut lines = vec![
            Line::from(""),
            row("Name", &d.name),
            row("Image", &d.image),
            row_col("Status", &d.status, status_color),
        ];
        if let Some(up) = compute_uptime(d) {
            lines.push(row("Uptime", &up));
        }
        if !d.node_id.is_empty() {
            lines.push(row("Node", &d.node_id));
        }
        if !d.runtime.is_empty() {
            lines.push(row("Runtime", &d.runtime));
        }
        if let Some(rc) = d.restart_count {
            lines.push(row("Restarts", &rc.to_string()));
        }
        lines.push(Line::from(""));
        lines.push(section("  NETWORK"));
        lines.push(row("IPv4", d.container_ip.as_deref().unwrap_or("—")));
        lines.push(row("IPv6", d.ipv6_address.as_deref().unwrap_or("—")));
        if let Some(h) = &d.hostname {
            lines.push(row("Hostname", h));
        }
        if !d.ports.is_empty() {
            for p in &d.ports {
                let txt = p
                    .as_object()
                    .map(|o| {
                        let cp = o.get("container_port").and_then(|v| v.as_i64()).unwrap_or(0);
                        let proto = o
                            .get("protocol")
                            .and_then(|v| v.as_str())
                            .unwrap_or("tcp");
                        let hp = o.get("host_port").and_then(|v| v.as_i64()).unwrap_or(cp);
                        let host_ip = o
                            .get("host_ip")
                            .and_then(|v| v.as_str())
                            .unwrap_or(d.container_ip.as_deref().unwrap_or("—"));
                        format!("{cp}/{proto} → {host_ip}:{hp}")
                    })
                    .unwrap_or_else(|| p.to_string());
                lines.push(row("Port", &txt));
            }
        }

        lines.push(Line::from(""));
        lines.push(section("  TIMING"));
        lines.push(row("Created", &fmt_ts(&d.created_at)));
        if let Some(s) = &d.started_at {
            lines.push(row("Started", &fmt_ts(s)));
        }
        if let Some(f) = &d.finished_at {
            lines.push(row("Finished", &fmt_ts(f)));
        }
        if let Some(ec) = d.exit_code {
            lines.push(row("Exit code", &ec.to_string()));
        }

        if !d.volume_mounts.is_empty() {
            lines.push(Line::from(""));
            lines.push(section("  VOLUMES"));
            for v in &d.volume_mounts {
                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(v.clone(), Style::default().fg(Color::White)),
                ]));
            }
        }

        if d.cpu_limit.is_some() || d.memory_limit.is_some() {
            lines.push(Line::from(""));
            lines.push(section("  LIMITS"));
            if let Some(c) = d.cpu_limit {
                lines.push(row("CPU", &format!("{c:.1} vCPU")));
            }
            if let Some(m) = d.memory_limit {
                lines.push(row("Memory", &format!("{m} MB")));
            }
        }

        if !d.env_vars.is_empty() {
            lines.push(Line::from(""));
            lines.push(section(&format!("  ENV ({})", d.env_vars.len())));
            // Limit to 10 to keep the view scannable — full set is in the CLI
            for e in d.env_vars.iter().take(10) {
                // Hide values that look like secrets (key=value where key
                // contains TOKEN/SECRET/PASSWORD/KEY).
                let redacted = redact_env(e);
                lines.push(Line::from(vec![
                    Span::styled("   ", Style::default()),
                    Span::styled(redacted, Style::default().fg(MUTED)),
                ]));
            }
            if d.env_vars.len() > 10 {
                lines.push(Line::from(Span::styled(
                    format!("   …+{} more", d.env_vars.len() - 10),
                    Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
                )));
            }
        }

        if let Some(id) = &d.container_id {
            lines.push(Line::from(""));
            lines.push(row("ID", id));
        }
        return (title, lines);
    }

    // Fallback: slim list data + loading hint
    let (_, status_color) = status_icon_color(&slim.status);
    let mut lines = vec![
        Line::from(""),
        row("Name", &slim.name),
        row("Image", &slim.image),
        row_col("Status", &slim.status, status_color),
        row("IPv4", slim.container_ip.as_deref().unwrap_or("—")),
        row("IPv6", slim.ipv6_address.as_deref().unwrap_or("—")),
        row("Created", &fmt_ts(&slim.created_at)),
        row("ID", &slim.container_id),
        Line::from(""),
    ];
    if app.describe_loading {
        lines.push(Line::from(Span::styled(
            "  ⟳ fetching details…",
            Style::default().fg(AMBER).add_modifier(Modifier::ITALIC),
        )));
    }
    (title, lines)
}

/// Compute uptime from `started_at` (ISO 8601) → "Nd Nh".
fn compute_uptime(d: &ContainerDescribe) -> Option<String> {
    let s = d.started_at.as_deref()?;
    if d.status.to_lowercase() != "running" {
        return None;
    }
    // Parse seconds-since-epoch from ISO 8601 without pulling chrono in here;
    // crude but good enough for a display field.
    let secs = parse_iso8601_to_epoch(s)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let delta = (now - secs).max(0);
    let days = delta / 86_400;
    let hours = (delta % 86_400) / 3600;
    let mins = (delta % 3600) / 60;
    Some(if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins}m")
    } else {
        format!("{mins}m")
    })
}

/// Minimal ISO 8601 → epoch parser. Accepts `2026-03-17T22:17:37.605Z` style.
/// Returns None on parse failure — caller silently drops the uptime row.
fn parse_iso8601_to_epoch(s: &str) -> Option<i64> {
    let s = s.trim_end_matches('Z');
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let mo: i64 = date_parts.next()?.parse().ok()?;
    let d: i64 = date_parts.next()?.parse().ok()?;
    let time = time.split('.').next().unwrap_or(time);
    let mut time_parts = time.split(':');
    let h: i64 = time_parts.next()?.parse().ok()?;
    let mi: i64 = time_parts.next()?.parse().ok()?;
    let se: i64 = time_parts.next().unwrap_or("0").parse().ok()?;
    // Days from civil (Howard Hinnant's algorithm, returns days since 1970-01-01)
    let y = if mo <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = (y - era * 400) as i64;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + h * 3600 + mi * 60 + se)
}

fn redact_env(e: &str) -> String {
    let (k, v) = match e.split_once('=') {
        Some(p) => p,
        None => return e.to_string(),
    };
    let lk = k.to_uppercase();
    let sensitive = lk.contains("TOKEN")
        || lk.contains("SECRET")
        || lk.contains("PASSWORD")
        || lk.contains("KEY")
        || lk.contains("API");
    if sensitive && !v.is_empty() {
        format!("{k}=••••••")
    } else {
        e.to_string()
    }
}

// ── Ingress ──────────────────────────────────────────────────────────────────

fn describe_ingress_lines(app: &App) -> (String, Vec<Line<'static>>) {
    let route = match app
        .ingress_list_state
        .selected()
        .and_then(|i| app.ingress_routes.get(i))
    {
        Some(r) => r,
        None => {
            return (
                "—".into(),
                vec![Line::from(Span::styled(
                    "  No route selected.",
                    Style::default().fg(MUTED),
                ))],
            )
        }
    };
    let title = truncate(&route.subdomain, 40);
    let status_text = if route.is_active { "active" } else { "inactive" };
    let status_color = if route.is_active { EMERALD } else { ROSE };

    // Cross-reference the container_id against the live container list so
    // the user can see the alias/image for the backing container.
    let backing = app
        .containers
        .iter()
        .find(|c| c.name == route.container_id);

    let mut lines = vec![
        Line::from(""),
        row("Subdomain", &route.subdomain),
        row("URL", &route.url),
        row_col("State", status_text, status_color),
        row("External port", "443 (https)"),
        row("Target port", &route.target_port.to_string()),
        row("Mode", route.mode.as_deref().unwrap_or("https")),
        row("Target IP", route.target_ip.as_deref().unwrap_or("—")),
        Line::from(""),
        section("  BACKING CONTAINER"),
        row("ID", &route.container_id),
    ];
    if let Some(c) = backing {
        lines.push(row("Image", &c.image));
        lines.push(row("Status", &c.status));
    } else {
        lines.push(Line::from(Span::styled(
            "   (container not found in live list)",
            Style::default().fg(ROSE).add_modifier(Modifier::ITALIC),
        )));
    }
    (title, lines)
}

// ── Nodes ────────────────────────────────────────────────────────────────────

fn describe_node_lines(app: &App) -> (String, Vec<Line<'static>>) {
    let node = match app
        .nodes_list_state
        .selected()
        .and_then(|i| app.nodes.get(i))
    {
        Some(n) => n,
        None => {
            return (
                "—".into(),
                vec![Line::from(Span::styled(
                    "  No node selected.",
                    Style::default().fg(MUTED),
                ))],
            )
        }
    };
    let title = truncate(&node.id, 40);
    let online = node.status.to_lowercase() == "online";
    let (_, color) = if online { ("●", EMERALD) } else { ("○", ROSE) };

    let address = if node.port > 0 {
        format!("{}:{}", node.address, node.port)
    } else {
        node.address.clone()
    };

    let lines = vec![
        Line::from(""),
        row("ID", &node.id),
        row_col("Status", &node.status, color),
        row("Address", &address),
        row(
            "Last heartbeat",
            &node
                .last_heartbeat
                .as_deref()
                .map(fmt_ts)
                .unwrap_or_else(|| "—".to_string()),
        ),
    ];
    (title, lines)
}

// ── Specs ────────────────────────────────────────────────────────────────────

fn describe_spec_lines(app: &App) -> (String, Vec<Line<'static>>) {
    let entry = match app
        .specs_list_state
        .selected()
        .and_then(|i| app.specs.get(i))
    {
        Some(s) => s,
        None => {
            return (
                "—".into(),
                vec![Line::from(Span::styled(
                    "  No spec selected.",
                    Style::default().fg(MUTED),
                ))],
            )
        }
    };
    let title = entry
        .alias
        .clone()
        .unwrap_or_else(|| truncate(&entry.container_name, 40));

    let path = super::nk_path(&entry.container_name);
    let state = app.spec_state(entry);
    let (state_text, state_color) = match state {
        SpecState::Deployed => ("deployed", EMERALD),
        SpecState::Updated => ("updated · press u to upgrade", AMBER),
        SpecState::NotDeployed => ("not deployed", ROSE),
    };

    let mut lines = vec![
        Line::from(""),
        row("Alias", entry.alias.as_deref().unwrap_or("—")),
        row("Container", &entry.container_name),
        row("Image", entry.image.as_deref().unwrap_or("—")),
        row_col("State", state_text, state_color),
        row("Path", path.to_str().unwrap_or("?")),
    ];

    // If the spec is in the updated state, surface the live image so the
    // user can see exactly what's about to change.
    if state == SpecState::Updated {
        if let Some(live) = app
            .containers
            .iter()
            .find(|c| c.name == entry.container_name)
        {
            lines.push(row("Live image", &live.image));
        }
    }

    // Best-effort TOML preview — first ~20 lines, so the user gets a quick
    // sense of what's in the spec without leaving the TUI.
    if let Ok(content) = std::fs::read_to_string(&path) {
        lines.push(Line::from(""));
        lines.push(section("  .nk SPEC"));
        for raw in content.lines().take(24) {
            lines.push(Line::from(Span::styled(
                format!("   {raw}"),
                Style::default().fg(Color::Rgb(150, 160, 180)),
            )));
        }
        if content.lines().count() > 24 {
            lines.push(Line::from(Span::styled(
                format!("   …+{} more lines", content.lines().count() - 24),
                Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
            )));
        }
    }
    (title, lines)
}

// ─── Help Overlay ────────────────────────────────────────────────────────────

fn render_help(f: &mut Frame, area: Rect) {
    let popup = centered_rect(52, 78, area);
    f.render_widget(Clear, popup);

    let text = vec![
        Line::from(""),
        section("  Navigation"),
        Line::from(""),
        hint("  Tab / Shift+Tab", "Switch tabs"),
        hint("  ← / →", "Switch tabs"),
        hint("  v", "Describe selected row"),
        Line::from(""),
        section("  Containers Tab"),
        Line::from(""),
        hint("  ↑↓ / j k", "Navigate"),
        hint("  l / Enter", "View logs"),
        hint("  v", "Describe (rich inspect)"),
        hint("  e", "Edit .nk spec in $EDITOR"),
        hint("  s", "Stop container"),
        hint("  r", "Restart container"),
        hint("  d", "Delete container"),
        hint("  R", "Force refresh"),
        Line::from(""),
        section("  Ingress Tab"),
        Line::from(""),
        hint("  ↑↓ / j k", "Navigate"),
        hint("  v", "Describe route"),
        hint("  Shift+D", "Disable ingress (confirm)"),
        hint("  R", "Force refresh"),
        Line::from(""),
        section("  Deployment Specs Tab"),
        Line::from(""),
        hint("  ↑↓ / j k", "Navigate"),
        hint("  v", "Describe spec (.nk preview)"),
        hint("  e", "Edit .nk spec in $EDITOR"),
        hint("  u", "Upgrade (when state = updated)"),
        hint("  Shift+D", "Delete .nk file (confirm)"),
        hint("  R", "Reload specs from disk"),
        Line::from(""),
        section("  Nodes Tab"),
        Line::from(""),
        hint("  ↑↓ / j k", "Navigate"),
        hint("  v", "Describe node"),
        hint("  R", "Force refresh"),
        Line::from(""),
        section("  Log View"),
        Line::from(""),
        hint("  ↑↓ / j k", "Scroll lines"),
        hint("  PgUp / PgDn", "Scroll page"),
        hint("  g / G", "Top / Bottom"),
        hint("  b / Esc", "Back"),
        Line::from(""),
        Line::from(Span::styled(
            "  any key to close",
            Style::default().fg(MUTED).add_modifier(Modifier::ITALIC),
        )),
    ];

    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(INDIGO))
                .title(Span::styled(
                    " Help ",
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(HEADER_BG)),
        ),
        popup,
    );
}

fn section(s: &str) -> Line<'static> {
    Line::from(Span::styled(
        s.to_owned(),
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
    ))
}
fn hint(key: &str, label: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{key:<22}"), Style::default().fg(AMBER)),
        Span::styled(label.to_owned(), Style::default().fg(Color::White)),
    ])
}

// ─── Confirm Dialog ──────────────────────────────────────────────────────────

fn render_confirm(f: &mut Frame, action: &ConfirmAction, area: Rect) {
    let popup = centered_rect(46, 22, area);
    f.render_widget(Clear, popup);

    let (verb, name) = match action {
        ConfirmAction::Stop(n) => ("Stop", n.as_str()),
        ConfirmAction::Remove(n) => ("Delete", n.as_str()),
        ConfirmAction::Restart(n) => ("Restart", n.as_str()),
        ConfirmAction::DeleteIngress(_id, subdomain) => ("Delete ingress", subdomain.as_str()),
        ConfirmAction::DeleteSpec(_name, label) => ("Delete spec", label.as_str()),
    };
    let color = if verb.starts_with("Delete") {
        ROSE
    } else {
        AMBER
    };

    let text = vec![
        Line::from(""),
        Line::from(Span::styled(
            format!("  {} {}", verb, truncate(name, 30)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  [y / Enter]  confirm",
            Style::default().fg(EMERALD),
        )),
        Line::from(Span::styled(
            "  [any other]  cancel",
            Style::default().fg(MUTED),
        )),
    ];

    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(color))
                .title(Span::styled(
                    format!(" {verb} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ))
                .style(Style::default().bg(HEADER_BG)),
        ),
        popup,
    );
}

// ─── Status Bar ──────────────────────────────────────────────────────────────

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let bar = Rect {
        x: 0,
        y: area.height.saturating_sub(1),
        width: area.width,
        height: 1,
    };
    let (msg, color) = match &app.action_result {
        ActionResult::None => return,
        ActionResult::Success(s) => (format!(" ✓ {s}"), EMERALD),
        ActionResult::Error(e) => (format!(" ✗ {e}"), ROSE),
        ActionResult::Pending(p) => (format!(" ⟳ {p}"), AMBER),
    };
    f.render_widget(Clear, bar);
    f.render_widget(
        Paragraph::new(Span::styled(
            msg,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
        bar,
    );
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn row(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<10} "), Style::default().fg(MUTED)),
        Span::styled(value.to_owned(), Style::default().fg(Color::White)),
    ])
}

fn row_col(label: &str, value: &str, color: Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {label:<10} "), Style::default().fg(MUTED)),
        Span::styled(
            value.to_owned(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}

/// Read the `.nk` file for `container_name` and extract `deployment.image`.
/// Returns None if the file doesn't exist, can't be parsed, or is missing
/// the field. Non-fatal — the Specs tab still renders with an empty image.
fn parse_spec_image(container_name: &str) -> Option<String> {
    let path = super::nk_path(container_name);
    let content = std::fs::read_to_string(&path).ok()?;
    let value: toml::Value = toml::from_str(&content).ok()?;
    value
        .get("deployment")?
        .get("image")?
        .as_str()
        .map(|s| s.to_string())
}

fn fmt_ts(ts: &str) -> String {
    ts.get(..16).unwrap_or(ts).replace('T', " ").to_string()
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(v[1])[1]
}
