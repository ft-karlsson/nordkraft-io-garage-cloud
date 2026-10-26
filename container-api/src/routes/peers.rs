// src/routes/peers.rs
//
// Child WireGuard peers: extra devices or people an owner lets into their own
// container range, and nothing else. See setup/CHILD_PEERS.md.
//
// The endpoints only change rows in tenant_peers / tenant_peer_targets and
// then run one PeerReconciler pass, which turns the rows into `wg set` and
// the generated nftables ruleset. A child peer never becomes a user: it has
// no API identity (guards.rs refuses its address range outright).
//
// Endpoints (owner-authenticated over WireGuard):
//   POST   /api/peers                    create {name, public_key, scope?, targets?}
//   GET    /api/peers                    list, with live handshake data
//   GET    /api/peers/<name>             one peer
//   PATCH  /api/peers/<name>             {scope?, targets? | add_targets? / remove_targets?}
//   POST   /api/peers/<name>/enable      put the peer back
//   POST   /api/peers/<name>/disable     keep the row, remove peer + rules
//   POST   /api/peers/<name>/rotate      {public_key}: new key, same address
//   DELETE /api/peers/<name>
//
// Private keys never reach this API: the CLI generates them on the owner's
// machine, or the sub-user brings their own public key.

use crate::guards::AuthenticatedUser;
use crate::services::peer_reconciler;
use crate::services::tenant_purge::is_wireguard_key;
use crate::storage::get_user_garage_subnet;
use crate::AppState;

use rocket::serde::json::Json;
use rocket::serde::Deserialize;
use rocket::State;
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use tracing::{info, warn};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

const MIN_INDEX: i16 = 2;
const MAX_INDEX: i16 = 254;

// ============= REQUESTS =============

#[derive(Debug, Clone, Deserialize)]
pub struct TargetRequest {
    /// Container name or container id; stored by name so a redeploy keeps the grant.
    pub container: String,
    /// Accepted TCP ports; omitted or empty = every port.
    #[serde(default)]
    pub ports: Option<Vec<u16>>,
}

#[derive(Debug, Deserialize)]
pub struct CreatePeerRequest {
    pub name: String,
    pub public_key: String,
    /// "all" or "containers". Defaults to "containers" when targets are given, else "all".
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub targets: Vec<TargetRequest>,
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchPeerRequest {
    #[serde(default)]
    pub scope: Option<String>,
    /// Replace the whole target list (implies scope "containers").
    #[serde(default)]
    pub targets: Option<Vec<TargetRequest>>,
    /// Add to the list (implies scope "containers").
    #[serde(default)]
    pub add_targets: Option<Vec<TargetRequest>>,
    /// Remove by container name or id.
    #[serde(default)]
    pub remove_targets: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct RotateRequest {
    pub public_key: String,
}

// ============= VALIDATION =============

/// [a-z0-9-]{1,63}, no leading/trailing dash: it ends up in an nft comment and a file name.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err("name must be 1-63 characters of a-z, 0-9 and '-', e.g. anna-laptop".into())
    }
}

fn validate_ports(ports: &Option<Vec<u16>>) -> Result<Option<Vec<u16>>, String> {
    match ports {
        None => Ok(None),
        Some(p) if p.is_empty() => Ok(None),
        Some(p) => {
            if p.contains(&0) {
                return Err("ports must be 1-65535".into());
            }
            let mut v = p.clone();
            v.sort_unstable();
            v.dedup();
            Ok(Some(v))
        }
    }
}

fn parse_scope(s: &str) -> Result<&'static str, String> {
    match s {
        "all" => Ok("all"),
        "containers" | "container" | "only" => Ok("containers"),
        _ => Err("scope must be \"all\" or \"containers\"".into()),
    }
}

// ============= DB HELPERS =============

struct PeerRow {
    id: String,
    name: String,
    peer_index: i16,
    public_key: String,
    scope: String,
    status: String,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
}

fn row_to_peer(r: sqlx::postgres::PgRow) -> PeerRow {
    PeerRow {
        id: r.get("id"),
        name: r.get("name"),
        peer_index: r.get("peer_index"),
        public_key: r.get("public_key"),
        scope: r.get("scope"),
        status: r.get("status"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

async fn load_peer(pool: &PgPool, user_id: &str, name: &str) -> Result<Option<PeerRow>, BoxError> {
    Ok(sqlx::query(
        "SELECT id, name, peer_index, public_key, scope, status, created_at, updated_at
         FROM tenant_peers WHERE user_id = $1 AND name = $2",
    )
    .bind(user_id)
    .bind(name)
    .fetch_optional(pool)
    .await?
    .map(row_to_peer))
}

async fn load_peers(pool: &PgPool, user_id: &str) -> Result<Vec<PeerRow>, BoxError> {
    Ok(sqlx::query(
        "SELECT id, name, peer_index, public_key, scope, status, created_at, updated_at
         FROM tenant_peers WHERE user_id = $1 ORDER BY peer_index",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?
    .into_iter()
    .map(row_to_peer)
    .collect())
}

/// (container_name, ports, current ip) per peer id. `ip` is None when no
/// non-deleted container of that name has an address: the target is dangling.
async fn load_targets(
    pool: &PgPool,
    user_id: &str,
) -> Result<BTreeMap<String, Vec<serde_json::Value>>, BoxError> {
    let rows = sqlx::query(
        "SELECT t.peer_id, t.container_name, t.ports,
                (SELECT c.internal_ip FROM containers c
                  WHERE c.user_id = p.user_id AND c.container_name = t.container_name
                    AND c.status != 'deleted' AND c.internal_ip IS NOT NULL
                  ORDER BY c.created_at DESC LIMIT 1) AS ip
         FROM tenant_peer_targets t
         JOIN tenant_peers p ON p.id = t.peer_id
         WHERE p.user_id = $1
         ORDER BY t.container_name",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut m: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    for r in rows {
        let peer_id: String = r.get("peer_id");
        let ports: Option<Vec<i32>> = r.get("ports");
        let ip: Option<String> = r.get("ip");
        m.entry(peer_id).or_default().push(serde_json::json!({
            "container": r.get::<String, _>("container_name"),
            "ports": ports,
            "ip": ip,
            "dangling": ip.is_none(),
        }));
    }
    Ok(m)
}

/// The container's canonical name, if it is the caller's and not deleted.
/// Accepts the name or the container id.
async fn resolve_container_name(
    pool: &PgPool,
    user_id: &str,
    container: &str,
) -> Result<Option<String>, BoxError> {
    Ok(sqlx::query_scalar::<_, String>(
        "SELECT container_name FROM containers
         WHERE user_id = $1 AND (container_name = $2 OR container_id = $2) AND status != 'deleted'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(container)
    .fetch_optional(pool)
    .await?)
}

/// Validate and resolve a target list: every container must be the caller's.
async fn resolve_targets(
    pool: &PgPool,
    user_id: &str,
    reqs: &[TargetRequest],
) -> Result<Vec<(String, Option<Vec<u16>>)>, String> {
    let mut out: Vec<(String, Option<Vec<u16>>)> = Vec::new();
    for t in reqs {
        let ports = validate_ports(&t.ports)?;
        let name = resolve_container_name(pool, user_id, t.container.trim())
            .await
            .map_err(|e| format!("Database error: {}", e))?
            .ok_or_else(|| format!("container {} not found (or not yours)", t.container))?;
        if let Some(existing) = out.iter_mut().find(|(n, _)| *n == name) {
            existing.1 = ports;
        } else {
            out.push((name, ports));
        }
    }
    Ok(out)
}

async fn replace_targets(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    peer_id: &str,
    targets: &[(String, Option<Vec<u16>>)],
) -> Result<(), BoxError> {
    sqlx::query("DELETE FROM tenant_peer_targets WHERE peer_id = $1")
        .bind(peer_id)
        .execute(&mut **tx)
        .await?;
    for (name, ports) in targets {
        let ports_i32: Option<Vec<i32>> = ports
            .as_ref()
            .map(|v| v.iter().map(|p| *p as i32).collect());
        sqlx::query(
            "INSERT INTO tenant_peer_targets (peer_id, container_name, ports) VALUES ($1, $2, $3)",
        )
        .bind(peer_id)
        .bind(name)
        .bind(ports_i32)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn next_free_index(pool: &PgPool, user_id: &str) -> Result<Option<i16>, BoxError> {
    let used: Vec<i16> =
        sqlx::query_scalar("SELECT peer_index FROM tenant_peers WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok((MIN_INDEX..=MAX_INDEX).find(|i| !used.contains(i)))
}

/// Is this key already an owner or a child anywhere? Keys are identities.
async fn key_in_use(pool: &PgPool, key: &str) -> Result<bool, BoxError> {
    let n: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM users WHERE wireguard_public_key = $1)
              + (SELECT count(*) FROM tenant_peers WHERE public_key = $1)",
    )
    .bind(key)
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

struct Garage {
    server_public_key: Option<String>,
    server_endpoint: String,
}

async fn load_garage(pool: &PgPool, garage_id: &str) -> Result<Garage, BoxError> {
    let row =
        sqlx::query("SELECT wireguard_public_key, vpn_endpoint FROM garages WHERE garage_id = $1")
            .bind(garage_id)
            .fetch_one(pool)
            .await?;
    Ok(Garage {
        server_public_key: row.get("wireguard_public_key"),
        server_endpoint: row.get("vpn_endpoint"),
    })
}

async fn max_peers(pool: &PgPool, user_id: &str) -> Result<i64, BoxError> {
    let n: Option<i32> = sqlx::query_scalar(
        "SELECT p.max_peers FROM users u JOIN plans p ON p.id = u.plan_id WHERE u.id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(n.unwrap_or(5) as i64)
}

/// Run a reconcile pass now so the answer reflects reality; None in dev/agent mode.
async fn converge() -> Option<peer_reconciler::ReconcileReport> {
    match peer_reconciler::global() {
        Some(rec) => Some(rec.reconcile_now().await),
        None => None,
    }
}

// ============= JSON =============

struct Context {
    slot: u8,
    container_subnet: String,
    garage: Garage,
}

async fn context(pool: &PgPool, user: &crate::models::User) -> Result<Context, String> {
    let slot = user
        .user_slot
        .and_then(|s| u8::try_from(s).ok())
        .filter(|s| *s > 0)
        .ok_or_else(|| "your account has no tenant slot yet".to_string())?;
    let container_subnet = get_user_garage_subnet(pool, &user.id, &user.primary_garage_id)
        .await
        .unwrap_or_else(|_| format!("172.21.{}.0/24", slot));
    let garage = load_garage(pool, &user.primary_garage_id)
        .await
        .map_err(|e| format!("garage {} not found: {}", user.primary_garage_id, e))?;
    Ok(Context {
        slot,
        container_subnet,
        garage,
    })
}

async fn peer_json(
    app_state: &AppState,
    ctx: &Context,
    p: &PeerRow,
    targets: &[serde_json::Value],
    reconcile: Option<&peer_reconciler::ReconcileReport>,
) -> serde_json::Value {
    let vpn_ip = format!("172.20.{}.{}", ctx.slot, p.peer_index);
    let live = app_state.peer_cache.resolve(&vpn_ip).await;
    let mut v = serde_json::json!({
        "name": p.name,
        "vpn_ip": vpn_ip,
        "public_key": p.public_key,
        "scope": p.scope,
        "status": p.status,
        "targets": if p.scope == "containers" { targets.to_vec() } else { Vec::new() },
        "dangling": targets.iter().filter(|t| t["dangling"] == true).count(),
        "created_at": p.created_at.to_rfc3339(),
        "updated_at": p.updated_at.to_rfc3339(),
        // What the device's [Peer] section needs. AllowedIPs is always the
        // owner's whole container range: scope changes never need a new config.
        "client": {
            "address": format!("{}/32", vpn_ip),
            "allowed_ips": [ctx.container_subnet],
            "server_public_key": ctx.garage.server_public_key,
            "server_endpoint": ctx.garage.server_endpoint,
            "persistent_keepalive": 25,
        },
        "live": live.as_ref().map(|l| serde_json::json!({
            "endpoint": l.endpoint,
            "latest_handshake": l.latest_handshake,
            "transfer_rx": l.transfer_rx,
            "transfer_tx": l.transfer_tx,
        })),
    });
    if let Some(r) = reconcile {
        if !r.errors.is_empty() {
            v["reconcile_errors"] = serde_json::json!(r.errors);
        }
    }
    v
}

fn err(msg: impl Into<String>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "error": msg.into() }))
}

// ============= ROUTES =============

#[post("/peers", data = "<req>")]
pub async fn create_peer(
    req: Json<CreatePeerRequest>,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    let pool = pool.inner();
    let user = &user.0;
    let name = req.name.trim().to_ascii_lowercase();
    if let Err(e) = validate_name(&name) {
        return err(e);
    }
    let key = req.public_key.trim().to_string();
    if !is_wireguard_key(&key) {
        return err("public_key is not a WireGuard public key (44 base64 characters)");
    }
    let scope = match req.scope.as_deref() {
        Some(s) => match parse_scope(s) {
            Ok(s) => s,
            Err(e) => return err(e),
        },
        None if req.targets.is_empty() => "all",
        None => "containers",
    };
    if scope == "containers" && req.targets.is_empty() {
        return err(
            "scope \"containers\" needs at least one target; use scope \"all\" for every container",
        );
    }
    let ctx = match context(pool, user).await {
        Ok(c) => c,
        Err(e) => return err(e),
    };

    match load_peer(pool, &user.id, &name).await {
        Ok(Some(_)) => return err(format!("you already have a peer named {}", name)),
        Ok(None) => {}
        Err(e) => return err(format!("Database error: {}", e)),
    }
    match key_in_use(pool, &key).await {
        Ok(true) => return err("that public key is already in use"),
        Ok(false) => {}
        Err(e) => return err(format!("Database error: {}", e)),
    }
    if app_state.config.quota_enforced {
        let have = match load_peers(pool, &user.id).await {
            Ok(v) => v.len() as i64,
            Err(e) => return err(format!("Database error: {}", e)),
        };
        let limit = max_peers(pool, &user.id).await.unwrap_or(5);
        if have >= limit {
            return err(format!(
                "your plan allows {} peer(s); remove one first",
                limit
            ));
        }
    }
    let targets = if scope == "containers" {
        match resolve_targets(pool, &user.id, &req.targets).await {
            Ok(t) => t,
            Err(e) => return err(e),
        }
    } else {
        Vec::new()
    };
    let index = match next_free_index(pool, &user.id).await {
        Ok(Some(i)) => i,
        Ok(None) => return err("no free peer address left in your range (253 peers)"),
        Err(e) => return err(format!("Database error: {}", e)),
    };

    let id = uuid::Uuid::new_v4().to_string();
    let result: Result<(), BoxError> = async {
        let mut tx = pool.begin().await?;
        sqlx::query(
            "INSERT INTO tenant_peers (id, user_id, name, peer_index, public_key, scope)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(&user.id)
        .bind(&name)
        .bind(index)
        .bind(&key)
        .bind(scope)
        .execute(&mut *tx)
        .await?;
        replace_targets(&mut tx, &id, &targets).await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        return err(format!("could not create peer: {}", e));
    }
    info!(
        "🔗 Child peer {} created for user {} → 172.20.{}.{} ({})",
        name, user.id, ctx.slot, index, scope
    );

    let report = converge().await;
    let (Ok(Some(p)), Ok(t)) = (
        load_peer(pool, &user.id, &name).await,
        load_targets(pool, &user.id).await,
    ) else {
        return err("peer created but could not be read back");
    };
    let mut v = peer_json(
        app_state,
        &ctx,
        &p,
        t.get(&p.id).map(Vec::as_slice).unwrap_or(&[]),
        report.as_ref(),
    )
    .await;
    v["status_message"] = serde_json::json!("created");
    Json(v)
}

#[get("/peers")]
pub async fn list_peers(
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    let pool = pool.inner();
    let user = &user.0;
    let ctx = match context(pool, user).await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let (peers, targets) = match (
        load_peers(pool, &user.id).await,
        load_targets(pool, &user.id).await,
    ) {
        (Ok(p), Ok(t)) => (p, t),
        (Err(e), _) | (_, Err(e)) => return err(format!("Database error: {}", e)),
    };
    let mut out = Vec::with_capacity(peers.len());
    for p in &peers {
        out.push(
            peer_json(
                app_state,
                &ctx,
                p,
                targets.get(&p.id).map(Vec::as_slice).unwrap_or(&[]),
                None,
            )
            .await,
        );
    }
    Json(serde_json::json!({
        "peers": out,
        "container_subnet": ctx.container_subnet,
        "peer_range": format!("172.20.{}.2-172.20.{}.254", ctx.slot, ctx.slot),
    }))
}

#[get("/peers/<name>")]
pub async fn get_peer(
    name: String,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    let pool = pool.inner();
    let user = &user.0;
    let ctx = match context(pool, user).await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let p = match load_peer(pool, &user.id, &name).await {
        Ok(Some(p)) => p,
        Ok(None) => return err(format!("no peer named {}", name)),
        Err(e) => return err(format!("Database error: {}", e)),
    };
    let targets = match load_targets(pool, &user.id).await {
        Ok(t) => t,
        Err(e) => return err(format!("Database error: {}", e)),
    };
    Json(
        peer_json(
            app_state,
            &ctx,
            &p,
            targets.get(&p.id).map(Vec::as_slice).unwrap_or(&[]),
            None,
        )
        .await,
    )
}

#[patch("/peers/<name>", data = "<req>")]
pub async fn patch_peer(
    name: String,
    req: Json<PatchPeerRequest>,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    let pool = pool.inner();
    let user = &user.0;
    let ctx = match context(pool, user).await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    let p = match load_peer(pool, &user.id, &name).await {
        Ok(Some(p)) => p,
        Ok(None) => return err(format!("no peer named {}", name)),
        Err(e) => return err(format!("Database error: {}", e)),
    };

    // Start from what is stored, apply the request, validate the result.
    let current: Vec<(String, Option<Vec<u16>>)> = match sqlx::query(
        "SELECT container_name, ports FROM tenant_peer_targets WHERE peer_id = $1 ORDER BY container_name",
    )
    .bind(&p.id)
    .fetch_all(pool)
    .await
    {
        Ok(rows) => rows
            .into_iter()
            .map(|r| {
                let ports: Option<Vec<i32>> = r.get("ports");
                (
                    r.get("container_name"),
                    ports.map(|v| v.into_iter().filter_map(|x| u16::try_from(x).ok()).collect()),
                )
            })
            .collect(),
        Err(e) => return err(format!("Database error: {}", e)),
    };

    let mut scope = p.scope.clone();
    let mut targets = current;
    if let Some(s) = req.scope.as_deref() {
        match parse_scope(s) {
            Ok(s) => scope = s.to_string(),
            Err(e) => return err(e),
        }
    }
    if let Some(list) = &req.targets {
        targets = match resolve_targets(pool, &user.id, list).await {
            Ok(t) => t,
            Err(e) => return err(e),
        };
        scope = "containers".into();
    }
    if let Some(add) = &req.add_targets {
        let add = match resolve_targets(pool, &user.id, add).await {
            Ok(t) => t,
            Err(e) => return err(e),
        };
        for (n, ports) in add {
            if let Some(existing) = targets.iter_mut().find(|(x, _)| *x == n) {
                existing.1 = ports;
            } else {
                targets.push((n, ports));
            }
        }
        scope = "containers".into();
    }
    if let Some(remove) = &req.remove_targets {
        for r in remove {
            let r = r.trim();
            // Accept the stored name directly (a deleted container can still be revoked)
            // or anything that resolves to it.
            let resolved = resolve_container_name(pool, &user.id, r)
                .await
                .ok()
                .flatten();
            let before = targets.len();
            targets.retain(|(n, _)| n != r && Some(n) != resolved.as_ref());
            if targets.len() == before {
                return err(format!("{} is not a target of {}", r, name));
            }
        }
    }
    if scope == "containers" && targets.is_empty() {
        return err("a peer with scope \"containers\" needs at least one target; use scope \"all\" or remove the peer");
    }
    if scope == "all" {
        targets.clear();
    }

    let result: Result<(), BoxError> = async {
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE tenant_peers SET scope = $2, updated_at = now() WHERE id = $1")
            .bind(&p.id)
            .bind(&scope)
            .execute(&mut *tx)
            .await?;
        replace_targets(&mut tx, &p.id, &targets).await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    if let Err(e) = result {
        return err(format!("could not update peer: {}", e));
    }
    info!(
        "🔗 Child peer {} of user {} updated: scope {} ({} target(s))",
        name,
        user.id,
        scope,
        targets.len()
    );
    let report = converge().await;
    respond_with_peer(app_state, pool, &ctx, &user.id, &name, report.as_ref()).await
}

async fn set_status(
    name: String,
    status: &str,
    user: &crate::models::User,
    app_state: &AppState,
    pool: &PgPool,
) -> Json<serde_json::Value> {
    let ctx = match context(pool, user).await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    match sqlx::query(
        "UPDATE tenant_peers SET status = $3, updated_at = now() WHERE user_id = $1 AND name = $2",
    )
    .bind(&user.id)
    .bind(&name)
    .bind(status)
    .execute(pool)
    .await
    {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return err(format!("no peer named {}", name)),
        Err(e) => return err(format!("Database error: {}", e)),
    }
    info!("🔗 Child peer {} of user {} → {}", name, user.id, status);
    let report = converge().await;
    respond_with_peer(app_state, pool, &ctx, &user.id, &name, report.as_ref()).await
}

#[post("/peers/<name>/enable")]
pub async fn enable_peer(
    name: String,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    set_status(name, "active", &user.0, app_state, pool.inner()).await
}

#[post("/peers/<name>/disable")]
pub async fn disable_peer(
    name: String,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    set_status(name, "disabled", &user.0, app_state, pool.inner()).await
}

#[post("/peers/<name>/rotate", data = "<req>")]
pub async fn rotate_peer(
    name: String,
    req: Json<RotateRequest>,
    user: AuthenticatedUser,
    app_state: &State<AppState>,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    let pool = pool.inner();
    let user = &user.0;
    let key = req.public_key.trim().to_string();
    if !is_wireguard_key(&key) {
        return err("public_key is not a WireGuard public key (44 base64 characters)");
    }
    let ctx = match context(pool, user).await {
        Ok(c) => c,
        Err(e) => return err(e),
    };
    match key_in_use(pool, &key).await {
        Ok(true) => return err("that public key is already in use"),
        Ok(false) => {}
        Err(e) => return err(format!("Database error: {}", e)),
    }
    match sqlx::query(
        "UPDATE tenant_peers SET public_key = $3, updated_at = now() WHERE user_id = $1 AND name = $2",
    )
    .bind(&user.id)
    .bind(&name)
    .bind(&key)
    .execute(pool)
    .await
    {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return err(format!("no peer named {}", name)),
        Err(e) => return err(format!("Database error: {}", e)),
    }
    info!("🔗 Child peer {} of user {} rotated its key", name, user.id);
    // The old key leaves wg on this pass; the new one arrives with the same address.
    let report = converge().await;
    respond_with_peer(app_state, pool, &ctx, &user.id, &name, report.as_ref()).await
}

#[delete("/peers/<name>")]
pub async fn delete_peer(
    name: String,
    user: AuthenticatedUser,
    pool: &State<PgPool>,
) -> Json<serde_json::Value> {
    let pool = pool.inner();
    let user = &user.0;
    match sqlx::query("DELETE FROM tenant_peers WHERE user_id = $1 AND name = $2")
        .bind(&user.id)
        .bind(&name)
        .execute(pool)
        .await
    {
        Ok(r) if r.rows_affected() == 1 => {}
        Ok(_) => return err(format!("no peer named {}", name)),
        Err(e) => return err(format!("Database error: {}", e)),
    }
    info!("🗑️ Child peer {} of user {} removed", name, user.id);
    let report = converge().await;
    let mut v = serde_json::json!({ "status": "removed", "name": name });
    if let Some(r) = report {
        if !r.errors.is_empty() {
            warn!(
                "peer {} removed but reconcile reported: {:?}",
                name, r.errors
            );
            v["reconcile_errors"] = serde_json::json!(r.errors);
        }
    }
    Json(v)
}

async fn respond_with_peer(
    app_state: &AppState,
    pool: &PgPool,
    ctx: &Context,
    user_id: &str,
    name: &str,
    report: Option<&peer_reconciler::ReconcileReport>,
) -> Json<serde_json::Value> {
    let (Ok(Some(p)), Ok(t)) = (
        load_peer(pool, user_id, name).await,
        load_targets(pool, user_id).await,
    ) else {
        return err("peer updated but could not be read back");
    };
    Json(
        peer_json(
            app_state,
            ctx,
            &p,
            t.get(&p.id).map(Vec::as_slice).unwrap_or(&[]),
            report,
        )
        .await,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("anna-laptop").is_ok());
        assert!(validate_name("ci2").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("Anna").is_err());
        assert!(validate_name("-x").is_err());
        assert!(validate_name("x-").is_err());
        assert!(validate_name("a b").is_err());
        assert!(validate_name(&"a".repeat(64)).is_err());
    }

    #[test]
    fn ports() {
        assert_eq!(validate_ports(&None).unwrap(), None);
        assert_eq!(validate_ports(&Some(vec![])).unwrap(), None);
        assert_eq!(
            validate_ports(&Some(vec![443, 80, 80])).unwrap(),
            Some(vec![80, 443])
        );
        assert!(validate_ports(&Some(vec![0])).is_err());
    }

    #[test]
    fn scopes() {
        assert_eq!(parse_scope("all").unwrap(), "all");
        assert_eq!(parse_scope("only").unwrap(), "containers");
        assert!(parse_scope("some").is_err());
    }
}
