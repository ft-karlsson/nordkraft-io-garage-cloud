// bootstrap.rs — database schema + dev seed, applied in-process on startup.
//
// No SQL files to ship: setup/schema.sql and the migrations are embedded at
// compile time and applied when the `users` table is missing. The dev seed
// creates exactly the rows the guards' dev user expects (see guards.rs), so
// `serve --dev` can deploy on a fresh database with zero manual steps.

use sqlx::PgPool;
use tracing::{info, warn};

use crate::config::AppConfig;

const SCHEMA_SQL: &str = include_str!("../../setup/schema.sql");
const MIGRATION_CUSTOM_DOMAINS_SQL: &str = include_str!("../../setup/migration_custom_domains.sql");

/// Identity of the dev user — must match the shortcut in guards.rs.
pub const DEV_USER_ID: &str = "dev-user-id";
pub const DEV_USER_SLOT: i32 = 99;
pub const DEV_GARAGE_ID: &str = "ry";
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
    info!("✅ Schema applied");
    Ok(true)
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
         VALUES ($1, 'Dev garage', 'localhost', 'DK', '127.0.0.1:51820', '172.21.1.0/24', 'active')
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

/// Minimal argument handling for the server binary.
///
///   nordkraft serve            production (all config from env)
///   nordkraft serve --dev      local dev preset (see config::apply_dev_preset)
///   nordkraft --version
///
/// `serve` is accepted and ignored so the systemd unit and the docs can
/// keep saying `nordkraft serve`.
pub fn parse_args() -> bool {
    let mut dev = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "serve" => {}
            "--dev" | "-d" => dev = true,
            "--version" | "-V" => {
                println!("nordkraft {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("Nordkraft container API\n");
                println!("Usage: nordkraft [serve] [--dev]\n");
                println!("  --dev        Local development: no WireGuard, no NATS, bridge");
                println!("               networking, auto-seeded database on 127.0.0.1:8001");
                println!("  --version    Print version");
                std::process::exit(0);
            }
            other => {
                warn!("Ignoring unknown argument: {}", other);
            }
        }
    }
    dev
}
