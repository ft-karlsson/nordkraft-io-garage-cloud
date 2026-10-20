// routes/stream.rs — exec and shell over WebSocket, kubectl style.
//
//   GET  /api/containers/<id>/stream?tty=1&cmd=sh[&cmd=-l][&cwd=][&env=K=V][&user=][&cols=&rows=]
//        user-facing, auth = WireGuard source IP (the existing guard).
//        Local container (hybrid/dev) → session runs in-process.
//        Remote container → the controller opens the agent's internal
//        stream and copies frames both ways without reading them.
//
//   GET  /api/internal/containers/<id>/stream?...&owner=<pubkey>
//        agent-side, reachable on the WireGuard interface only; accepted from
//        the controller's address alone. The owner label is enforced here
//        too, so a confused controller still can't cross tenants.
//
//   POST /api/containers/<id>/exec   {cmd, cwd?, env?, user?, stdin?, timeout_s?}
//        buffered, non-interactive — the Mastra/Daytona `exec()` shape.
//        Same stream underneath; this just collects it.
//
// Frames follow stream::protocol (channel byte + payload).

use crate::guards::{AuthenticatedUser, ClientIP};
use crate::models::NodeInfo;
use crate::storage::get_node_network_info;
use crate::stream::containerd_exec::ExecSpec;
use crate::stream::protocol::Resize;
use crate::stream::{relay, session};
use crate::AppState;
use rocket::http::uri::Origin;
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket_ws as ws;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

const EXEC_DEFAULT_TIMEOUT: u64 = 120;
const EXEC_MAX_TIMEOUT: u64 = 3600;
const EXEC_OUTPUT_CAP: usize = 1024 * 1024;

type Refused = (Status, String);

/// Query parameters shared by both stream routes.
#[derive(Debug, Clone, rocket::FromForm)]
#[allow(dead_code)] // `tty` is read through is_tty()
pub struct StreamQuery {
    /// "1"/"true"/"yes"/"on" → TTY. (Rocket's bool only takes true/false.)
    pub tty: Option<String>,
    pub cmd: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<String>,
    pub user: Option<String>,
    pub cols: Option<u16>,
    pub rows: Option<u16>,
    /// Attach stdin (default true, as kubectl's `-i`). `stdin=false` gives the
    /// process EOF from /dev/null and never closes stdin through containerd —
    /// use it for commands that will not read input. See ExecSpec::stdin.
    pub stdin: Option<String>,
    /// Internal route only: owner pubkey the controller already verified.
    pub owner: Option<String>,
}

fn truthy(v: Option<&str>) -> Option<bool> {
    match v.map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("1") | Some("true") | Some("yes") | Some("on") => Some(true),
        Some("0") | Some("false") | Some("no") | Some("off") => Some(false),
        _ => None,
    }
}

impl StreamQuery {
    fn is_tty(&self) -> bool {
        truthy(self.tty.as_deref()).unwrap_or(false)
    }

    fn wants_stdin(&self) -> bool {
        truthy(self.stdin.as_deref()).unwrap_or(true)
    }

    fn into_spec(self, container: String, owner_pubkey: Option<String>) -> ExecSpec {
        let tty = self.is_tty();
        let stdin = self.wants_stdin();
        let cmd = if self.cmd.is_empty() {
            vec!["/bin/sh".to_string()]
        } else {
            self.cmd
        };
        ExecSpec {
            container,
            owner_pubkey,
            cmd,
            env: self.env,
            cwd: self.cwd,
            user: self.user,
            tty,
            initial_size: match (self.cols, self.rows) {
                (Some(width), Some(height)) => Some(Resize { width, height }),
                _ => None,
            },
            stdin,
        }
    }
}

// ---------------------------------------------------------------------------
// Where does this container live?
// ---------------------------------------------------------------------------

enum Placement {
    Local,
    Remote {
        agent_ip: String,
        agent_port: u16,
    },
    /// A sandbox: talk to nordkraft-sandboxd on the container's own address.
    /// Nothing here goes through the runtime's exec path.
    Daemon {
        ip: String,
        port: u16,
        token: String,
    },
}

/// Ownership + placement in one step. Sandboxes (stored config carries a
/// daemon token) are reached directly; other local containers are checked
/// against the runtime's owner label (no DB involved, so dev mode works);
/// remote ones against the containers table, then the owning node's WG address.
async fn place(
    container: &str,
    user: &AuthenticatedUser,
    app_state: &AppState,
    orchestrator: &crate::controller::OrchestratorService,
    pool: &sqlx::PgPool,
) -> Result<Placement, Refused> {
    if let Some(p) = place_daemon(container, user, pool).await? {
        return Ok(p);
    }
    place_runtime(container, user, app_state, orchestrator, pool).await
}

/// The daemon placement alone, when the container is a sandbox.
async fn place_daemon(
    container: &str,
    user: &AuthenticatedUser,
    pool: &sqlx::PgPool,
) -> Result<Option<Placement>, Refused> {
    let cfg = match crate::storage::get_container_config(pool, container, &user.0.id).await {
        Ok(Some(cfg)) => cfg,
        _ => return Ok(None),
    };
    let Some(daemon) = cfg.sandboxd else {
        return Ok(None);
    };
    let ip = crate::storage::get_container_ipv4(pool, &cfg.container_name, &user.0.id)
        .await
        .map_err(|e| (Status::InternalServerError, e.to_string()))?
        .filter(|ip| !ip.is_empty() && ip != "pending")
        .ok_or((Status::BadGateway, "sandbox has no address yet".to_string()))?;
    Ok(Some(Placement::Daemon {
        ip,
        port: daemon.port,
        token: daemon.token,
    }))
}

/// Where the runtime runs the container: this node, or a remote agent.
async fn place_runtime(
    container: &str,
    user: &AuthenticatedUser,
    app_state: &AppState,
    orchestrator: &crate::controller::OrchestratorService,
    pool: &sqlx::PgPool,
) -> Result<Placement, Refused> {
    if let Ok(list) = app_state
        .container_manager
        .list_user_containers(&user.0.wireguard_public_key)
        .await
    {
        if list
            .iter()
            .any(|c| c.name == container || c.container_id == container)
        {
            return Ok(Placement::Local);
        }
    }

    let owned: Option<String> = sqlx::query_scalar(
        "SELECT node_id FROM containers
         WHERE (container_id = $1 OR container_name = $1) AND user_id = $2 AND status != 'deleted'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(container)
    .bind(&user.0.id)
    .fetch_optional(pool)
    .await
    .map_err(|e| (Status::InternalServerError, e.to_string()))?;

    let node_id = match owned {
        Some(n) => n,
        None => return Err((Status::NotFound, "container not found".into())),
    };
    if node_id == orchestrator.node_id {
        // Recorded as ours but not in the runtime listing: stopped/removed.
        return Err((
            Status::NotFound,
            "container not running on this node".into(),
        ));
    }

    let (agent_ip, _iface) = get_node_network_info(pool, &node_id)
        .await
        .map_err(|_| (Status::BadGateway, format!("node {node_id} has no address")))?;
    let agent_port = orchestrator
        .get_nodes()
        .await
        .into_iter()
        .find(|n: &NodeInfo| n.id == node_id)
        .map(|n| n.port)
        .unwrap_or(app_state.config.bind_port);
    Ok(Placement::Remote {
        agent_ip,
        agent_port,
    })
}

// ---------------------------------------------------------------------------
// GET /containers/<id>/stream  — user-facing
// ---------------------------------------------------------------------------

#[get("/containers/<container>/stream?<q..>")]
#[allow(clippy::too_many_arguments)] // Rocket guards, one per concern
pub async fn container_stream(
    container: String,
    q: StreamQuery,
    origin: &Origin<'_>,
    ws: ws::WebSocket,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<crate::controller::OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Result<ws::Channel<'static>, Refused> {
    let placement = place(&container, &user, app_state, orchestrator, pool).await?;
    let pubkey = user.0.wireguard_public_key.clone();
    let exec = app_state.exec.clone();
    let limits = session::Limits::default();

    match placement {
        Placement::Daemon { ip, port, token } => {
            let query = origin.query().map(|s| s.as_str()).unwrap_or("");
            let url = relay::daemon_stream_url(&ip, port, query);
            info!(
                "🖥️  stream {} relayed to sandboxd at {}:{} for {}",
                container, ip, port, user.0.email
            );
            let daemon_ws = relay::connect_daemon(&url, &token)
                .await
                .map_err(|e| (Status::BadGateway, format!("sandbox: {e}")))?;
            Ok(ws.channel(move |duplex| {
                Box::pin(async move {
                    let (user_to_daemon_tx, user_to_daemon_rx) = mpsc::channel::<Vec<u8>>(64);
                    let (daemon_to_user_tx, daemon_to_user_rx) = mpsc::channel::<Vec<u8>>(64);
                    let daemon_side = tokio::spawn(async move {
                        relay::pump_ws(daemon_ws, daemon_to_user_tx, user_to_daemon_rx).await
                    });
                    relay::pump_ws(duplex, user_to_daemon_tx, daemon_to_user_rx).await;
                    daemon_side.abort();
                    Ok(())
                })
            }))
        }
        Placement::Local => {
            info!(
                "🖥️  stream {} local tty={} for {}",
                container,
                q.is_tty(),
                user.0.email
            );
            let spec = q.into_spec(container, Some(pubkey));
            Ok(ws.channel(move |duplex| {
                Box::pin(async move {
                    let (to_session_tx, to_session_rx) = mpsc::channel::<Vec<u8>>(64);
                    let (from_session_tx, from_session_rx) = mpsc::channel::<Vec<u8>>(64);
                    let sess = tokio::spawn(async move {
                        session::run(&exec, spec, to_session_rx, from_session_tx, limits).await
                    });
                    relay::pump_ws(duplex, to_session_tx, from_session_rx).await;
                    let _ = sess.await;
                    Ok(())
                })
            }))
        }
        Placement::Remote {
            agent_ip,
            agent_port,
        } => {
            // Forward the query verbatim plus the verified owner.
            let query = origin.query().map(|s| s.as_str()).unwrap_or("");
            let url = relay::agent_stream_url(
                &agent_ip,
                agent_port,
                &container,
                &format!("{query}&owner={}", urlencode(&pubkey)),
            );
            info!(
                "🖥️  stream {} relayed to {}:{} for {}",
                container, agent_ip, agent_port, user.0.email
            );
            let agent_ws = relay::connect_agent(&url)
                .await
                .map_err(|e| (Status::BadGateway, format!("agent: {e}")))?;
            Ok(ws.channel(move |duplex| {
                Box::pin(async move {
                    // Two pumps, channels crossed: the relay never parses a frame.
                    let (user_to_agent_tx, user_to_agent_rx) = mpsc::channel::<Vec<u8>>(64);
                    let (agent_to_user_tx, agent_to_user_rx) = mpsc::channel::<Vec<u8>>(64);
                    let agent_side = tokio::spawn(async move {
                        relay::pump_ws(agent_ws, agent_to_user_tx, user_to_agent_rx).await
                    });
                    relay::pump_ws(duplex, user_to_agent_tx, agent_to_user_rx).await;
                    agent_side.abort();
                    Ok(())
                })
            }))
        }
    }
}

// ---------------------------------------------------------------------------
// GET /internal/containers/<id>/stream  — agent side
// ---------------------------------------------------------------------------

#[get("/internal/containers/<container>/stream?<q..>")]
pub async fn internal_stream(
    container: String,
    q: StreamQuery,
    ws: ws::WebSocket,
    client: ClientIP,
    app_state: &rocket::State<AppState>,
) -> Result<ws::Channel<'static>, Refused> {
    let cfg = &app_state.config;
    let trusted = client.0 == cfg.controller_internal_ip
        || client.0 == "127.0.0.1"
        || client.0 == "::1"
        || cfg.dev_mode;
    if !trusted {
        warn!("internal stream refused from {}", client.0);
        return Err((Status::Forbidden, "internal endpoint".into()));
    }
    let owner = match q.owner.clone() {
        Some(o) if !o.is_empty() => o,
        _ => return Err((Status::BadRequest, "owner required".into())),
    };
    let exec = app_state.exec.clone();
    let spec = q.into_spec(container, Some(owner));
    let limits = session::Limits::default();
    Ok(ws.channel(move |duplex| {
        Box::pin(async move {
            let (to_session_tx, to_session_rx) = mpsc::channel::<Vec<u8>>(64);
            let (from_session_tx, from_session_rx) = mpsc::channel::<Vec<u8>>(64);
            let sess = tokio::spawn(async move {
                session::run(&exec, spec, to_session_rx, from_session_tx, limits).await
            });
            relay::pump_ws(duplex, to_session_tx, from_session_rx).await;
            let _ = sess.await;
            Ok(())
        })
    }))
}

// ---------------------------------------------------------------------------
// POST /containers/<id>/exec  — buffered
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ExecRequest {
    pub cmd: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
}

#[post("/containers/<container>/exec", data = "<req>")]
pub async fn container_exec(
    container: String,
    req: Json<ExecRequest>,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<crate::controller::OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Result<Json<session::ExecResult>, Refused> {
    let req = req.into_inner();
    if req.cmd.is_empty() {
        return Err((Status::BadRequest, "cmd required".into()));
    }
    let placement = place(&container, &user, app_state, orchestrator, pool).await?;
    let timeout = Duration::from_secs(
        req.timeout_s
            .unwrap_or(EXEC_DEFAULT_TIMEOUT)
            .clamp(1, EXEC_MAX_TIMEOUT),
    );
    if let Placement::Daemon { ip, port, token } = &placement {
        // Same request and reply shapes: the daemon is a drop-in for the
        // containerd path, so the JSON passes straight through.
        let url = format!("http://{}:{port}/v1/exec", relay::host_for_url(ip));
        let client = reqwest::Client::builder()
            .timeout(timeout + Duration::from_secs(15))
            .build()
            .map_err(|e| (Status::InternalServerError, e.to_string()))?;
        let resp = client
            .post(&url)
            .bearer_auth(token)
            .json(&req)
            .send()
            .await
            .map_err(|e| (Status::BadGateway, format!("sandbox: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| (Status::BadGateway, e.to_string()))?;
        if !status.is_success() {
            return Err((Status::BadGateway, format!("sandbox {status}: {body}")));
        }
        let result: session::ExecResult = serde_json::from_str(&body)
            .map_err(|e| (Status::BadGateway, format!("sandbox reply: {e}")))?;
        return Ok(Json(result));
    }
    let pubkey = user.0.wireguard_public_key.clone();
    let env: Vec<String> = req.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    // No `stdin` in the request means the process gets /dev/null; `"stdin": ""`
    // attaches an empty stream that is closed at once.
    let stdin = req.stdin.map(String::into_bytes);
    let with_stdin = stdin.is_some();

    let (to_session_tx, to_session_rx) = mpsc::channel::<Vec<u8>>(64);
    let (from_session_tx, from_session_rx) = mpsc::channel::<Vec<u8>>(64);

    match placement {
        Placement::Daemon { .. } => unreachable!("handled above"),
        Placement::Local => {
            let exec = app_state.exec.clone();
            let spec = ExecSpec {
                container,
                owner_pubkey: Some(pubkey),
                cmd: req.cmd,
                env,
                cwd: req.cwd,
                user: req.user,
                tty: false,
                initial_size: None,
                stdin: with_stdin,
            };
            let limits = session::Limits {
                idle: timeout,
                max: timeout,
            };
            tokio::spawn(async move {
                session::run(&exec, spec, to_session_rx, from_session_tx, limits).await
            });
        }
        Placement::Remote {
            agent_ip,
            agent_port,
        } => {
            let mut query = vec![
                ("tty", "false".to_string()),
                ("stdin", with_stdin.to_string()),
                ("owner", pubkey),
            ];
            for c in &req.cmd {
                query.push(("cmd", c.clone()));
            }
            for e in &env {
                query.push(("env", e.clone()));
            }
            if let Some(cwd) = &req.cwd {
                query.push(("cwd", cwd.clone()));
            }
            if let Some(u) = &req.user {
                query.push(("user", u.clone()));
            }
            let qs = query
                .iter()
                .map(|(k, v)| format!("{k}={}", urlencode(v)))
                .collect::<Vec<_>>()
                .join("&");
            let url = relay::agent_stream_url(&agent_ip, agent_port, &container, &qs);
            let agent_ws = relay::connect_agent(&url)
                .await
                .map_err(|e| (Status::BadGateway, format!("agent: {e}")))?;
            tokio::spawn(
                async move { relay::pump_ws(agent_ws, from_session_tx, to_session_rx).await },
            );
        }
    }

    Ok(Json(
        session::collect(
            to_session_tx,
            from_session_rx,
            stdin,
            timeout,
            EXEC_OUTPUT_CAP,
        )
        .await,
    ))
}

// ---------------------------------------------------------------------------
// POST /containers/<id>/wait  — block until the container's main process exits
// ---------------------------------------------------------------------------

const WAIT_DEFAULT_TIMEOUT: u64 = 300;
const WAIT_MAX_TIMEOUT: u64 = 3600;

#[derive(Debug, serde::Serialize)]
pub struct WaitResult {
    /// Exit code of the main process; null if the container has no task
    /// (never started, or already removed).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

/// Mastra/Daytona-style `wait`: returns when the container stops, or after
/// `timeout_s` (default 300, max 3600) with `timed_out: true`. Long-poll;
/// callers loop while timed_out.
#[post("/containers/<container>/wait?<timeout_s>")]
pub async fn container_wait(
    container: String,
    timeout_s: Option<u64>,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<crate::controller::OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Result<Json<WaitResult>, Refused> {
    let timeout = Duration::from_secs(
        timeout_s
            .unwrap_or(WAIT_DEFAULT_TIMEOUT)
            .clamp(1, WAIT_MAX_TIMEOUT),
    );
    let pubkey = user.0.wireguard_public_key.clone();
    // `wait` is about the container's main process, which the runtime owns
    // for sandboxes too — so it always takes the runtime placement.
    match place_runtime(&container, &user, app_state, orchestrator, pool).await? {
        Placement::Daemon { .. } => unreachable!("place_runtime never yields Daemon"),
        Placement::Local => wait_result(
            app_state
                .exec
                .wait_container(&container, Some(&pubkey), timeout)
                .await,
        ),
        Placement::Remote {
            agent_ip,
            agent_port,
        } => {
            let url = format!(
                "http://{}:{}/api/internal/containers/{}/wait?timeout_s={}&owner={}",
                relay::host_for_url(&agent_ip),
                agent_port,
                container,
                timeout.as_secs(),
                urlencode(&pubkey)
            );
            let client = reqwest::Client::builder()
                .timeout(timeout + Duration::from_secs(15))
                .build()
                .map_err(|e| (Status::InternalServerError, e.to_string()))?;
            let resp = client
                .post(&url)
                .send()
                .await
                .map_err(|e| (Status::BadGateway, format!("agent: {e}")))?;
            let status = resp.status();
            let body = resp
                .text()
                .await
                .map_err(|e| (Status::BadGateway, e.to_string()))?;
            if !status.is_success() {
                return Err((Status::BadGateway, format!("agent {status}: {body}")));
            }
            let r: serde_json::Value = serde_json::from_str(&body)
                .map_err(|e| (Status::BadGateway, format!("agent reply: {e}")))?;
            Ok(Json(WaitResult {
                exit_code: r["exit_code"].as_i64().map(|c| c as i32),
                timed_out: r["timed_out"].as_bool().unwrap_or(false),
            }))
        }
    }
}

#[post("/internal/containers/<container>/wait?<timeout_s>&<owner>")]
pub async fn internal_wait(
    container: String,
    timeout_s: Option<u64>,
    owner: Option<String>,
    client: ClientIP,
    app_state: &rocket::State<AppState>,
) -> Result<Json<WaitResult>, Refused> {
    let cfg = &app_state.config;
    let trusted = client.0 == cfg.controller_internal_ip
        || client.0 == "127.0.0.1"
        || client.0 == "::1"
        || cfg.dev_mode;
    if !trusted {
        return Err((Status::Forbidden, "internal endpoint".into()));
    }
    let owner = owner
        .filter(|o| !o.is_empty())
        .ok_or((Status::BadRequest, "owner required".to_string()))?;
    let timeout = Duration::from_secs(
        timeout_s
            .unwrap_or(WAIT_DEFAULT_TIMEOUT)
            .clamp(1, WAIT_MAX_TIMEOUT),
    );
    wait_result(
        app_state
            .exec
            .wait_container(&container, Some(&owner), timeout)
            .await,
    )
}

fn wait_result(
    r: Result<Option<i32>, crate::stream::containerd_exec::ExecError>,
) -> Result<Json<WaitResult>, Refused> {
    match r {
        Ok(code) => Ok(Json(WaitResult {
            exit_code: code,
            timed_out: false,
        })),
        Err(e) if e.to_string() == "timeout" => Ok(Json(WaitResult {
            exit_code: None,
            timed_out: true,
        })),
        Err(e) if e.to_string() == "container not found" => Err((Status::NotFound, e.to_string())),
        Err(e) => Err((Status::InternalServerError, e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Files in and out of a sandbox — proxied to nordkraft-sandboxd
// ---------------------------------------------------------------------------

/// Largest upload accepted in one request (buffered on the controller).
const FILE_UPLOAD_LIMIT_MIB: u64 = 256;

async fn daemon_placement_or_400(
    container: &str,
    user: &AuthenticatedUser,
    pool: &sqlx::PgPool,
) -> Result<(String, u16, String), Refused> {
    match place_daemon(container, user, pool).await? {
        Some(Placement::Daemon { ip, port, token }) => Ok((ip, port, token)),
        _ => Err((
            Status::BadRequest,
            "file transfer needs a sandbox (deploy with \"sandbox\": true)".into(),
        )),
    }
}

/// `PUT /containers/<id>/files?path=/abs/file[&mode=755]` with the file as the
/// body. Parent directories are created; the write is atomic on the far end.
#[put("/containers/<container>/files?<path>&<mode>", data = "<data>")]
pub async fn container_file_put(
    container: String,
    path: String,
    mode: Option<String>,
    data: rocket::Data<'_>,
    user: AuthenticatedUser,
    pool: &rocket::State<sqlx::PgPool>,
) -> Result<Json<serde_json::Value>, Refused> {
    use rocket::data::ToByteUnit;
    let (ip, port, token) = daemon_placement_or_400(&container, &user, pool).await?;
    let body = data
        .open(FILE_UPLOAD_LIMIT_MIB.mebibytes())
        .into_bytes()
        .await
        .map_err(|e| (Status::BadRequest, format!("reading upload: {e}")))?;
    if !body.is_complete() {
        return Err((
            Status::PayloadTooLarge,
            format!("upload exceeds {FILE_UPLOAD_LIMIT_MIB} MiB"),
        ));
    }
    let mut url = format!(
        "http://{}:{port}/v1/files?path={}",
        relay::host_for_url(&ip),
        urlencode(&path)
    );
    if let Some(m) = &mode {
        url.push_str(&format!("&mode={}", urlencode(m)));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| (Status::InternalServerError, e.to_string()))?;
    let resp = client
        .put(&url)
        .bearer_auth(&token)
        .body(body.into_inner())
        .send()
        .await
        .map_err(|e| (Status::BadGateway, format!("sandbox: {e}")))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| (Status::BadGateway, e.to_string()))?;
    if !status.is_success() {
        return Err((Status::BadGateway, format!("sandbox {status}: {text}")));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| (Status::BadGateway, format!("sandbox reply: {e}")))?;
    Ok(Json(v))
}

/// `GET /containers/<id>/files?path=/abs/file` streams the file back.
#[get("/containers/<container>/files?<path>")]
pub async fn container_file_get(
    container: String,
    path: String,
    user: AuthenticatedUser,
    pool: &rocket::State<sqlx::PgPool>,
) -> Result<rocket::response::stream::ByteStream![Vec<u8>], Refused> {
    use futures::StreamExt;
    let (ip, port, token) = daemon_placement_or_400(&container, &user, pool).await?;
    let url = format!(
        "http://{}:{port}/v1/files?path={}",
        relay::host_for_url(&ip),
        urlencode(&path)
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| (Status::InternalServerError, e.to_string()))?;
    let resp = client
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| (Status::BadGateway, format!("sandbox: {e}")))?;
    let status = resp.status();
    if status == reqwest::StatusCode::NOT_FOUND {
        return Err((Status::NotFound, "no such file".into()));
    }
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err((Status::BadGateway, format!("sandbox {status}: {text}")));
    }
    let mut chunks = resp.bytes_stream();
    Ok(rocket::response::stream::ByteStream! {
        while let Some(Ok(chunk)) = chunks.next().await {
            yield chunk.to_vec();
        }
    })
}

/// Minimal percent-encoding for query values (WireGuard keys contain '+' and '/').
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_wg_key() {
        assert_eq!(urlencode("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[test]
    fn stdin_defaults_to_attached() {
        assert_eq!(truthy(None), None);
        assert_eq!(truthy(Some("false")), Some(false));
        assert_eq!(truthy(Some("0")), Some(false));
        assert_eq!(truthy(Some("TRUE")), Some(true));
        assert_eq!(truthy(Some("maybe")), None);
    }
}
