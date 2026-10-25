// bootstrap.rs — database schema + dev seed, applied in-process on startup.
//
// No SQL files to ship: setup/schema.sql and the migrations are embedded at
// compile time and applied when the `users` table is missing. The dev seed
// creates exactly the rows the guards' dev user expects (see guards.rs), so
// `container-api --dev` can deploy on a fresh database with zero manual steps.

use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::AppConfig;

const SCHEMA_SQL: &str = include_str!("../../setup/schema.sql");
const MIGRATION_CUSTOM_DOMAINS_SQL: &str = include_str!("../../setup/migration_custom_domains.sql");
const MIGRATION_CONTAINER_STATUS_SQL: &str =
    include_str!("../../setup/migration_container_status.sql");
const MIGRATION_CHILD_PEERS_SQL: &str = include_str!("../../setup/migration_child_peers.sql");

/// Identity of the dev user — must match the shortcut in guards.rs.
pub const DEV_USER_ID: &str = "dev-user-id";
pub const DEV_USER_SLOT: i32 = 99;
pub const DEV_GARAGE_ID: &str = "local-dev";
pub const DEV_PLAN_ID: &str = "dev-plan";

/// Strip pg_dump artifacts that only psql understands, and ownership /
/// grant statements that assume a specific role exists. The connecting
/// user owns the database, so it needs no grants.
fn clean_dump(sql: &str) -> String {
    sql.lines()
        .filter(|l| {
            let t = l.trim_start();
            !(t.starts_with("\\restrict")
                || t.starts_with("\\unrestrict")
                || t.contains(" OWNER TO ")
                || t.contains("set_config('search_path'")
                || t.starts_with("GRANT ")
                || t.starts_with("REVOKE ")
                || t.starts_with("ALTER DEFAULT PRIVILEGES"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn schema_present(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_schema = 'public' AND table_name = 'users')",
    )
    .fetch_one(pool)
    .await
}

/// Apply the embedded schema if this database has never been initialised.
/// Idempotent: a database that already has `users` is left untouched.
pub async fn ensure_schema(
    pool: &PgPool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    if schema_present(pool).await? {
        return Ok(false);
    }

    info!("🗄️ Empty database — applying embedded schema");
    sqlx::raw_sql(&clean_dump(SCHEMA_SQL)).execute(pool).await?;
    sqlx::raw_sql(&clean_dump(MIGRATION_CUSTOM_DOMAINS_SQL))
        .execute(pool)
        .await?;
    sqlx::raw_sql(MIGRATION_CONTAINER_STATUS_SQL)
        .execute(pool)
        .await?;
    info!("✅ Schema applied");
    Ok(true)
}

/// Idempotent migrations that an existing database may lack. Applied on every
/// start, so a new binary never runs ahead of its tables. Each statement is
/// IF NOT EXISTS; a database that already has them is left untouched.
pub async fn ensure_migrations(
    pool: &PgPool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    sqlx::raw_sql(MIGRATION_CHILD_PEERS_SQL)
        .execute(pool)
        .await?;
    Ok(())
}

/// Seed plan + garage + node + dev user + subnet allocation for `--dev`.
/// Every statement is ON CONFLICT DO NOTHING, so restarts are free.
pub async fn seed_dev(
    pool: &PgPool,
    config: &AppConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let arch = std::env::consts::ARCH;
    let dev_subnet = format!("172.21.{}.0/24", DEV_USER_SLOT);

    sqlx::query(
        "INSERT INTO plans (id, name, display_name, cpu, memory, storage, price, description, is_active)
         VALUES ($1, $1, 'Dev plan', 'unlimited', 'unlimited', 'unlimited', '0', 'Local development', true)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(DEV_PLAN_ID)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO garages (garage_id, name, location, country, vpn_endpoint, container_subnet_base, status)
         VALUES ($1, 'Local dev garage', 'this machine', 'DK', '127.0.0.1:51820', '172.21.1.0/24', 'active')
         ON CONFLICT (garage_id) DO NOTHING",
    )
    .bind(DEV_GARAGE_ID)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO nodes (node_id, name, location, ip_range, internal_ip, garage_id, hardware_type, architecture, status, network_interface)
         VALUES ($1, $1, 'localhost', $2, '127.0.0.1', $3, 'hybrid', $4, 'active', 'lo')
         ON CONFLICT (node_id) DO NOTHING",
    )
    .bind(&config.node_id)
    .bind(&dev_subnet)
    .bind(DEV_GARAGE_ID)
    .bind(arch)
    .execute(pool)
    .await?;

    sqlx::query(
        "INSERT INTO users (id, email, full_name, address, wireguard_public_key, wireguard_ip, client_ip,
                            node_id, plan_id, account_status, primary_garage_id, user_slot)
         VALUES ($1, 'dev@example.com', 'Development User', 'localhost', $2, '172.20.0.99', '127.0.0.1',
                 $3, $4, 'active', $5, $6)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(DEV_USER_ID)
    .bind(&config.dev_user_public_key)
    .bind(&config.node_id)
    .bind(DEV_PLAN_ID)
    .bind(DEV_GARAGE_ID)
    .bind(DEV_USER_SLOT)
    .execute(pool)
    .await?;

    // Allocation row — only if the (user, garage) pair is missing (no unique
    // constraint on the pair in the schema, so check first).
    let has_alloc: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM garage_container_allocations WHERE user_id = $1 AND garage_id = $2)",
    )
    .bind(DEV_USER_ID)
    .bind(DEV_GARAGE_ID)
    .fetch_one(pool)
    .await?;

    if !has_alloc {
        sqlx::query(
            "INSERT INTO garage_container_allocations (user_id, garage_id, user_slot, container_subnet, is_active)
             VALUES ($1, $2, $3, $4, true)",
        )
        .bind(DEV_USER_ID)
        .bind(DEV_GARAGE_ID)
        .bind(DEV_USER_SLOT)
        .bind(&dev_subnet)
        .execute(pool)
        .await?;
    }

    info!(
        "🌱 Dev seed ready: user={} slot={} garage={} node={}",
        DEV_USER_ID, DEV_USER_SLOT, DEV_GARAGE_ID, config.node_id
    );
    Ok(())
}

/// `--dev --reset`: wipe the dev database and every Nordkraft-managed
/// container on this node, then exit. Refuses outside dev mode.
pub async fn reset_dev(
    pool: &PgPool,
    runtime: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("🧹 Dev reset: removing all Nordkraft-managed containers");
    let ps = tokio::process::Command::new(runtime)
        .args(["ps", "-aq", "--filter", "label=managed_by=nordkraft"])
        .output()
        .await?;
    let ids: Vec<String> = String::from_utf8_lossy(&ps.stdout)
        .lines()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if !ids.is_empty() {
        let mut args = vec!["rm".to_string(), "-f".to_string()];
        args.extend(ids.iter().cloned());
        let out = tokio::process::Command::new(runtime)
            .args(&args)
            .output()
            .await?;
        if !out.status.success() {
            warn!(
                "container cleanup: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    info!("🧹 Dev reset: {} container(s) removed", ids.len());

    info!("🧹 Dev reset: dropping database schema");
    sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public;")
        .execute(pool)
        .await?;
    info!("✅ Dev reset complete — next start re-applies schema and seed");
    Ok(())
}

/// Command-line flags for the server binary.
#[derive(Debug, Default)]
pub struct Args {
    pub dev: bool,
    pub reset: bool,
}

/// Minimal argument handling for the server binary.
///
///   nordkraft-node             production (all config from env)
///   nordkraft-node --dev       local dev preset (see config::apply_dev_preset)
///   nordkraft-node --dev --reset  wipe dev DB + containers, then exit
///   nordkraft-node --version
///
/// `serve` is accepted and ignored for compatibility with older unit files
/// and docs that said `nordkraft serve`. This binary is the server; the
/// CLI is the separate `nordkraft` binary from client-cli.
pub fn parse_args() -> Args {
    let mut a = Args::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "serve" => {}
            "--dev" | "-d" => a.dev = true,
            "--reset" => a.reset = true,
            "--version" | "-V" => {
                println!("nordkraft-node {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("Nordkraft node — controller / agent / hybrid, set by NORDKRAFT_MODE\n");
                println!("Usage: nordkraft-node [--dev]\n");
                println!("  --dev        Local development: no WireGuard, no NATS, bridge");
                println!("               networking, auto-seeded database on 127.0.0.1:8001");
                println!("  --reset      With --dev: wipe the dev database and all managed");
                println!("               containers, then exit");
                println!("  --version    Print version");
                std::process::exit(0);
            }
            other => {
                warn!("Ignoring unknown argument: {}", other);
            }
        }
    }
    a
}
