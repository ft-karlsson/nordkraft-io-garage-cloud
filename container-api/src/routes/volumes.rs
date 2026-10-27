// routes/volumes.rs — full volume backup and restore.
//
//   GET  /api/containers/<id>/volume
//        user-facing. Streams the container's volume as tar.zst. Local
//        container (hybrid/dev) → produced in-process; remote → relayed
//        byte for byte from the agent's internal route.
//
//   PUT  /api/containers/<id>/volume            body = tar.zst
//        user-facing. Replaces the volume with the archive. Header
//        X-Nordkraft-Sha256 (optional) is verified on the node.
//
//   GET  /api/internal/volumes/<id>/export?owner=<pubkey>
//   PUT  /api/internal/volumes/<id>/import?owner=<pubkey>&sha256=
//        agent-side, accepted from the controller's address alone.
//
// Response headers on a backup: X-Nordkraft-Manifest (the manifest JSON
// that is also inside the archive), Content-Type application/zstd.

use crate::guards::{AuthenticatedUser, ClientIP};
use crate::routes::stream::{place, urlencode, Placement};
use crate::services::volume_backup::{self, RestoreReport};
use crate::stream::relay::host_for_url;
use crate::AppState;
use futures::{StreamExt, TryStreamExt};
use rocket::data::{Data, ToByteUnit};
use rocket::http::{ContentType, Status};
use rocket::request::{FromRequest, Outcome, Request};
use rocket::response::{self, Responder, Response};
use rocket::serde::json::Json;
use std::pin::Pin;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio_util::io::{ReaderStream, StreamReader};
use tracing::{info, warn};

pub const MANIFEST_HEADER: &str = "X-Nordkraft-Manifest";
pub const SHA256_HEADER: &str = "X-Nordkraft-Sha256";

type Refused = (Status, Json<serde_json::Value>);

fn refuse(status: Status, msg: impl Into<String>) -> Refused {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

// ---------------------------------------------------------------------------
// Streaming download responder
// ---------------------------------------------------------------------------

pub struct VolumeDownload {
    manifest_json: String,
    body: Pin<Box<dyn AsyncRead + Send>>,
}

impl<'r> Responder<'r, 'static> for VolumeDownload {
    fn respond_to(self, _req: &'r Request<'_>) -> response::Result<'static> {
        Response::build()
            .header(ContentType::new("application", "zstd"))
            .raw_header(MANIFEST_HEADER, self.manifest_json)
            .streamed_body(self.body)
            .ok()
    }
}

/// Upload metadata the client sends as headers.
pub struct UploadHeaders {
    pub sha256: Option<String>,
    pub content_length: Option<u64>,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for UploadHeaders {
    type Error = ();
    async fn from_request(req: &'r Request<'_>) -> Outcome<Self, ()> {
        let sha256 = req
            .headers()
            .get_one(SHA256_HEADER)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let content_length = req
            .headers()
            .get_one("Content-Length")
            .and_then(|s| s.trim().parse().ok());
        Outcome::Success(UploadHeaders {
            sha256,
            content_length,
        })
    }
}

fn trusted(client: &ClientIP, app_state: &AppState) -> bool {
    let cfg = &app_state.config;
    client.0 == cfg.controller_internal_ip
        || client.0 == "127.0.0.1"
        || client.0 == "::1"
        || cfg.dev_mode
}

fn relay_client() -> Result<reqwest::Client, Refused> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| refuse(Status::InternalServerError, e.to_string()))
}

async fn local_export(
    app_state: &AppState,
    container: &str,
    owner: &str,
) -> Result<VolumeDownload, Refused> {
    let cm = &app_state.container_manager;
    let target = volume_backup::resolve_volume(cm, container, owner)
        .await
        .map_err(|e| refuse(Status::NotFound, e.to_string()))?;
    let export = volume_backup::export(&target, &app_state.config.node_id)
        .await
        .map_err(|e| refuse(Status::InternalServerError, e.to_string()))?;
    let manifest_json = serde_json::to_string(&export.manifest)
        .map_err(|e| refuse(Status::InternalServerError, e.to_string()))?;
    let stream = futures::stream::unfold(export.stream, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    Ok(VolumeDownload {
        manifest_json,
        body: Box::pin(StreamReader::new(stream)),
    })
}

async fn local_import<R: AsyncRead + Unpin>(
    app_state: &AppState,
    container: &str,
    owner: &str,
    body: R,
    headers: &UploadHeaders,
) -> Result<Json<RestoreReport>, Refused> {
    let cm = &app_state.container_manager;
    let target = volume_backup::resolve_volume(cm, container, owner)
        .await
        .map_err(|e| refuse(Status::NotFound, e.to_string()))?;
    info!(
        "📥 restore {} ({}, running={})",
        target.container_id, target.mount_path, target.running
    );
    volume_backup::import(
        cm,
        owner,
        &target,
        body,
        headers.sha256.as_deref(),
        headers.content_length,
    )
    .await
    .map(Json)
    .map_err(|e| refuse(Status::BadRequest, e.to_string()))
}

// ---------------------------------------------------------------------------
// User-facing routes
// ---------------------------------------------------------------------------

#[get("/containers/<container>/volume")]
pub async fn backup_volume(
    container: String,
    user: AuthenticatedUser,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<crate::controller::OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Result<VolumeDownload, Refused> {
    let placement = place(&container, &user, app_state, orchestrator, pool)
        .await
        .map_err(|(s, m)| refuse(s, m))?;
    let pubkey = user.0.wireguard_public_key.clone();
    match placement {
        Placement::Local => {
            info!("📦 backup {} local for {}", container, user.0.email);
            local_export(app_state, &container, &pubkey).await
        }
        Placement::Remote {
            agent_ip,
            agent_port,
        } => {
            let url = format!(
                "http://{}:{agent_port}/api/internal/volumes/{container}/export?owner={}",
                host_for_url(&agent_ip),
                urlencode(&pubkey)
            );
            info!(
                "📦 backup {} relayed from {}:{} for {}",
                container, agent_ip, agent_port, user.0.email
            );
            let resp = relay_client()?
                .get(&url)
                .send()
                .await
                .map_err(|e| refuse(Status::BadGateway, format!("agent: {e}")))?;
            if !resp.status().is_success() {
                let status = Status::new(resp.status().as_u16());
                let msg = agent_error_message(resp).await;
                return Err(refuse(status, msg));
            }
            let manifest_json = resp
                .headers()
                .get(MANIFEST_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("{}")
                .to_string();
            let stream = resp.bytes_stream().map_err(std::io::Error::other);
            Ok(VolumeDownload {
                manifest_json,
                body: Box::pin(StreamReader::new(stream)),
            })
        }
    }
}

#[put("/containers/<container>/volume", data = "<data>")]
#[allow(clippy::too_many_arguments)] // Rocket guards, one per concern
pub async fn restore_volume(
    container: String,
    user: AuthenticatedUser,
    headers: UploadHeaders,
    data: Data<'_>,
    app_state: &rocket::State<AppState>,
    orchestrator: &rocket::State<crate::controller::OrchestratorService>,
    pool: &rocket::State<sqlx::PgPool>,
) -> Result<Json<RestoreReport>, Refused> {
    let placement = place(&container, &user, app_state, orchestrator, pool)
        .await
        .map_err(|(s, m)| refuse(s, m))?;
    let pubkey = user.0.wireguard_public_key.clone();
    let limit = app_state.config.volume_restore_max_bytes.bytes();
    if let Some(len) = headers.content_length {
        if len > app_state.config.volume_restore_max_bytes {
            return Err(refuse(
                Status::PayloadTooLarge,
                format!(
                    "archive is {} bytes, the limit is {} bytes",
                    len, app_state.config.volume_restore_max_bytes
                ),
            ));
        }
    }
    match placement {
        Placement::Local => {
            info!("📥 restore {} local for {}", container, user.0.email);
            local_import(app_state, &container, &pubkey, data.open(limit), &headers).await
        }
        Placement::Remote {
            agent_ip,
            agent_port,
        } => {
            let mut url = format!(
                "http://{}:{agent_port}/api/internal/volumes/{container}/import?owner={}",
                host_for_url(&agent_ip),
                urlencode(&pubkey)
            );
            if let Some(sha) = &headers.sha256 {
                url.push_str(&format!("&sha256={}", urlencode(sha)));
            }
            info!(
                "📥 restore {} relayed to {}:{} for {}",
                container, agent_ip, agent_port, user.0.email
            );
            let mut req = relay_client()?.put(&url);
            if let Some(len) = headers.content_length {
                req = req.header("Content-Length", len.to_string());
            }
            // The request body must be 'static, but the upload stream is tied
            // to this request: pump it through a channel while the relayed
            // request runs on its own task.
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(8);
            let relayed = futures::stream::unfold(rx, |mut rx| async move {
                rx.recv().await.map(|item| (item, rx))
            });
            let sender = tokio::spawn(req.body(reqwest::Body::wrap_stream(relayed)).send());
            let mut upload = ReaderStream::new(data.open(limit));
            while let Some(chunk) = upload.next().await {
                if tx.send(chunk).await.is_err() {
                    break; // the agent closed the connection; its reply says why
                }
            }
            drop(tx);
            let resp = sender
                .await
                .map_err(|e| refuse(Status::InternalServerError, e.to_string()))?
                .map_err(|e| refuse(Status::BadGateway, format!("agent: {e}")))?;
            let status = Status::new(resp.status().as_u16());
            if !resp.status().is_success() {
                let msg = agent_error_message(resp).await;
                return Err(refuse(status, msg));
            }
            resp.json::<RestoreReport>()
                .await
                .map(Json)
                .map_err(|e| refuse(Status::BadGateway, format!("agent reply: {e}")))
        }
    }
}

async fn agent_error_message(resp: reqwest::Response) -> String {
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .unwrap_or(text)
}

// ---------------------------------------------------------------------------
// Agent-side internal routes
// ---------------------------------------------------------------------------

#[get("/internal/volumes/<container>/export?<owner>")]
pub async fn internal_export(
    container: String,
    owner: String,
    client: ClientIP,
    app_state: &rocket::State<AppState>,
) -> Result<VolumeDownload, Refused> {
    if !trusted(&client, app_state) {
        warn!("internal volume export refused from {}", client.0);
        return Err(refuse(Status::Forbidden, "internal endpoint"));
    }
    if owner.is_empty() {
        return Err(refuse(Status::BadRequest, "owner required"));
    }
    local_export(app_state, &container, &owner).await
}

#[put(
    "/internal/volumes/<container>/import?<owner>&<sha256>",
    data = "<data>"
)]
pub async fn internal_import(
    container: String,
    owner: String,
    sha256: Option<String>,
    headers: UploadHeaders,
    client: ClientIP,
    data: Data<'_>,
    app_state: &rocket::State<AppState>,
) -> Result<Json<RestoreReport>, Refused> {
    if !trusted(&client, app_state) {
        warn!("internal volume import refused from {}", client.0);
        return Err(refuse(Status::Forbidden, "internal endpoint"));
    }
    if owner.is_empty() {
        return Err(refuse(Status::BadRequest, "owner required"));
    }
    let headers = UploadHeaders {
        sha256: sha256.filter(|s| !s.is_empty()).or(headers.sha256),
        content_length: headers.content_length,
    };
    let limit = app_state.config.volume_restore_max_bytes.bytes();
    local_import(app_state, &container, &owner, data.open(limit), &headers).await
}
