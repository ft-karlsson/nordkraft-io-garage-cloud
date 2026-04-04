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
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
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
    io, time::{Duration, Instant}
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
    status: Option<String>,
    error: Option<String>,
}

// ─── Ingress Types ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
struct IngressRoute {
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

// ─── Background Task Messages ────────────────────────────────────────────────
// All network I/O runs in a spawned task and sends results here.
// The event loop never awaits a network call directly → always responsive.

enum BgMsg {
    Containers(Vec<ContainerInfo>),
    ContainersFailed(String),
    Ingress(Vec<IngressRoute>),
    IngressFailed(String),
    Logs {
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

#[derive(Debug, Clone, Default)]
struct ClusterSnapshot {
    garage: String,
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
    fn stub() -> Self {
        Self {
            garage: "Kolo DK1 · Skanderborg".into(),
            node_count: 2,
            nodes_online: 2,
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

// ─── App State ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tab {
    Containers,
    Ingress,
}

impl Tab {
    fn next(&self) -> Tab {
        match self {
            Tab::Containers => Tab::Ingress,
            Tab::Ingress => Tab::Containers,
        }
    }
    fn prev(&self) -> Tab {
        self.next()
    } // only 2 tabs, wraps both ways
    fn title(&self) -> &'static str {
        match self {
            Tab::Containers => "Containers",
            Tab::Ingress => "Ingress",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum View {
    ContainerList,
    Logs,
    Inspect,
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
            log_lines: Vec::new(),
            log_scroll: 0,
            log_lines_count: 200,
            last_poll: Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1),
            is_loading: false,
            poll_in_flight: false,
            action_result: ActionResult::None,
            action_result_at: None,
            confirm_action: None,
            cluster: ClusterSnapshot::stub(),
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
                    self.last_poll = Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut app = App::new();
    let result = run_loop(&mut terminal, &mut app).await;
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
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
                        continue;
                    }
                }

                // ── Per-view routing ──────────────────────────────────────
                match app.view {
                    View::ContainerList if app.tab == Tab::Containers => {
                        handle_list_key(app, key.code)
                    }
                    View::ContainerList if app.tab == Tab::Ingress => {
                        handle_ingress_key(app, key.code)
                    }
                    View::ContainerList => {}
                    View::Logs => handle_logs_key(app, key.code),
                    View::Inspect => handle_inspect_key(app, key.code),
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
        KeyCode::Char('i') => {
            if app.selected_container().is_some() {
                app.view = View::Inspect;
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
        KeyCode::Char('R') => {
            app.ingress_last_poll = Instant::now() - Duration::from_secs(POLL_INTERVAL_SECS + 1);
        }
        KeyCode::Char('?') => app.view = View::Help,
        _ => {}
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

fn handle_inspect_key(app: &mut App, key: KeyCode) {
    if matches!(key, KeyCode::Esc | KeyCode::Char('b')) {
        app.view = View::ContainerList;
    }
}

fn handle_confirm_key(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('y') | KeyCode::Enter => {
            if let Some(action) = app.confirm_action.take() {
                let (name, verb) = match action {
                    ConfirmAction::Stop(n) => (n, "stop"),
                    ConfirmAction::Remove(n) => (n, "remove"),
                    ConfirmAction::Restart(n) => (n, "restart"),
                };
                app.set_result(ActionResult::Pending(format!("{verb} · {name}…")));
                app.spawn_action(name, verb);
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
        View::Inspect => render_inspect(f, app, area),
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
            Span::styled(app.cluster.garage.clone(), Style::default().fg(INDIGO)),
        ]),
        Line::from(vec![Span::raw("   "), status_pill(running, total)]),
        Line::from(vec![
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
    // TODO: replace with real data from /api/nodes
    let stub_nodes = vec![
        ("dell-r640", "online", "172.20.0.1"),
        ("optiplex-01", "online", "172.20.0.2"),
    ];
    for (id, status, addr) in &stub_nodes {
        let (icon, color) = if *status == "online" {
            ("●", EMERALD)
        } else {
            ("○", ROSE)
        };
        node_lines.push(Line::from(vec![
            Span::styled(format!(" {icon} "), Style::default().fg(color)),
            Span::styled(format!("{id:<14}"), Style::default().fg(Color::White)),
            Span::styled(format!(" {addr}"), Style::default().fg(MUTED)),
        ]));
    }
    node_lines.push(Line::from(Span::styled(
        " [TODO: /api/nodes]",
        Style::default().fg(Color::Rgb(45, 50, 65)),
    )));

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
    let tabs = [Tab::Containers, Tab::Ingress];
    let mut spans = vec![Span::raw(" ")];
    for tab in &tabs {
        let active = &app.tab == tab;
        let count_badge = match tab {
            Tab::Containers => format!(" {} ", app.containers.len()),
            Tab::Ingress => format!(" {} ", app.ingress_routes.len()),
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

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("   ST  ", Style::default().fg(MUTED)),
            Span::styled(
                format!("{:<30}", "NAME"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<32}", "IMAGE"),
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<18}", "IP"),
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
            let image = truncate(&c.image, 31);
            let ip = c.container_ip.as_deref().unwrap_or("—");
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
                    format!("{:<30}", truncate(&c.name, 29)),
                    Style::default().fg(Color::White),
                ),
                Span::styled(
                    format!("{:<32}", image),
                    Style::default().fg(Color::Rgb(150, 160, 180)),
                ),
                Span::styled(format!("{:<18}", ip), Style::default().fg(MUTED)),
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
            kb("i", "inspect"),
            kb("s", "stop"),
            kb("r", "restart"),
            kb("d", "delete"),
            kb("R", "refresh"),
            kb("?", "help"),
            kb("q", "quit"),
        ]),
        Tab::Ingress => Line::from(vec![
            kb("↑↓", "nav"),
            kb("R", "refresh"),
            kb("?", "help"),
            kb("q", "quit"),
        ]),
    };
    f.render_widget(Paragraph::new(line), area);
}

fn kb(key: &str, label: &str) -> Span<'static> {
    Span::styled(format!("  [{key}]{label}"), Style::default().fg(MUTED))
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
                format!("{:<8}", "PORT"),
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
                Span::styled(format!("{:<8}", r.target_port), Style::default().fg(AMBER)),
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

fn render_inspect(f: &mut Frame, app: &App, area: Rect) {
    let c = match app.selected_container() {
        Some(c) => c,
        None => return,
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(30)])
        .split(chunks[0]);

    let (_, status_color) = status_icon_color(&c.status);

    let lines = vec![
        Line::from(""),
        row("Name", &c.name),
        row("Image", &c.image),
        row_col("Status", &c.status, status_color),
        row("IP", c.container_ip.as_deref().unwrap_or("—")),
        row("IPv6", c.ipv6_address.as_deref().unwrap_or("—")),
        row("Created", &fmt_ts(&c.created_at)),
        row("ID", &c.container_id),
        Line::from(""),
        Line::from(Span::styled(
            "  [⌨  shell  → MARK II]",
            Style::default()
                .fg(Color::Rgb(50, 60, 80))
                .add_modifier(Modifier::ITALIC),
        )),
    ];

    let detail = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(30, 50, 80)))
            .title(Line::from(vec![
                Span::styled(" Inspect · ", Style::default().fg(MUTED)),
                Span::styled(
                    truncate(&c.name, 40),
                    Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ]))
            .style(Style::default().bg(PANEL_BG)),
    );
    f.render_widget(detail, cols[0]);

    let right_lines = vec![
        Line::from(Span::styled(
            " RESOURCES",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        mini_bar("CPU ", 0.22), // TODO: per-container metrics MARK II
        Line::from(""),
        mini_bar("MEM ", 0.45), // TODO: per-container metrics MARK II
        Line::from(""),
        mini_bar("DISK", 0.08), // TODO: per-container metrics MARK II
        Line::from(""),
        Line::from(Span::styled(
            " [TODO: MARK II]",
            Style::default()
                .fg(Color::Rgb(50, 55, 70))
                .add_modifier(Modifier::ITALIC),
        )),
    ];
    let right = Paragraph::new(right_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(30, 50, 80)))
            .style(Style::default().bg(HEADER_BG)),
    );
    f.render_widget(right, cols[1]);

    let keys = Paragraph::new(Line::from(vec![kb("b/Esc", "back")]));
    f.render_widget(keys, chunks[1]);
}

fn mini_bar(label: &str, ratio: f64) -> Line<'static> {
    let filled = (ratio * 16.0) as usize;
    let empty = 16 - filled;
    let color = if ratio < 0.85 {
        EMERALD
    } else if ratio < 0.95 {
        AMBER
    } else {
        ROSE
    };
    let pct = (ratio * 100.0) as u16;
    Line::from(vec![
        Span::styled(format!(" {label} "), Style::default().fg(MUTED)),
        Span::styled("█".repeat(filled), Style::default().fg(color)),
        Span::styled(
            "░".repeat(empty),
            Style::default().fg(Color::Rgb(40, 45, 55)),
        ),
        Span::styled(
            format!(" {pct}%"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ])
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
        Line::from(""),
        section("  Container Tab"),
        Line::from(""),
        hint("  ↑↓ / j k", "Navigate"),
        hint("  l / Enter", "View logs"),
        hint("  i", "Inspect container"),
        hint("  s", "Stop container"),
        hint("  r", "Restart container"),
        hint("  d", "Delete container"),
        hint("  R", "Force refresh"),
        hint("  q", "Quit"),
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
        ConfirmAction::Stop(n) => ("Stop", n),
        ConfirmAction::Remove(n) => ("Delete", n),
        ConfirmAction::Restart(n) => ("Restart", n),
    };
    let color = if verb == "Delete" { ROSE } else { AMBER };

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
