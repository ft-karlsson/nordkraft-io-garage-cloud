//! Postgres side of the energy tables (setup/migration_energy.sql).

use super::NodePowerSample;
use chrono::{DateTime, Duration, Utc};
use sqlx::{PgPool, Row};
use std::collections::HashMap;

pub async fn insert_node_sample(pool: &PgPool, s: &NodePowerSample) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO node_power_samples (node_id, ts, watts, energy_wh, source)
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(&s.node_id)
    .bind(s.ts)
    .bind(s.watts)
    .bind(s.energy_wh)
    .bind(&s.source)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct NodeMeta {
    pub idle_watts: Option<f64>,
    pub price_area: String,
}

pub async fn node_meta(pool: &PgPool, node_id: &str) -> sqlx::Result<Option<NodeMeta>> {
    let row = sqlx::query("SELECT power_idle_watts, price_area FROM nodes WHERE node_id = $1")
        .bind(node_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| NodeMeta {
        idle_watts: r.get("power_idle_watts"),
        price_area: r.get("price_area"),
    }))
}

/// Every price area a node sits in, for the Energinet poller.
pub async fn price_areas(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    let rows = sqlx::query("SELECT DISTINCT price_area FROM nodes")
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| r.get("price_area")).collect())
}

#[derive(Debug, Clone)]
pub struct TenantContainer {
    pub container_id: String,
    pub container_name: String,
    pub user_id: String,
    pub cpu_limit: f64,
}

/// Containers the controller knows on a node, keyed by name. Anything the
/// agent reports that is not here (a registry, a leftover) is platform
/// overhead and is attributed to nobody.
pub async fn tenant_containers_on_node(
    pool: &PgPool,
    node_id: &str,
) -> sqlx::Result<HashMap<String, TenantContainer>> {
    let rows = sqlx::query(
        "SELECT container_id, container_name, user_id, cpu_limit
         FROM containers WHERE node_id = $1 AND status <> 'deleted'",
    )
    .bind(node_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let c = TenantContainer {
                container_id: r.get("container_id"),
                container_name: r.get("container_name"),
                user_id: r.get("user_id"),
                cpu_limit: r.get::<Option<f64>, _>("cpu_limit").unwrap_or(0.5),
            };
            (c.container_name.clone(), c)
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct GridCo2 {
    pub ts: DateTime<Utc>,
    pub g_per_kwh: f64,
    pub forecast: bool,
}

/// The grid intensity to price `at` with: the latest measured value within
/// the last 45 minutes, otherwise the nearest forecast. None if Energinet
/// has not been reachable for a while; CO₂ is then left blank, not guessed.
pub async fn grid_co2_at(
    pool: &PgPool,
    price_area: &str,
    at: DateTime<Utc>,
) -> sqlx::Result<Option<GridCo2>> {
    let row = sqlx::query(
        "SELECT ts, g_per_kwh, forecast FROM grid_co2
         WHERE price_area = $1 AND ts <= $2 + interval '5 minutes'
           AND ts >= $2 - interval '45 minutes'
         ORDER BY forecast ASC, ts DESC LIMIT 1",
    )
    .bind(price_area)
    .bind(at)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| GridCo2 {
        ts: r.get("ts"),
        g_per_kwh: r.get("g_per_kwh"),
        forecast: r.get("forecast"),
    }))
}

pub async fn upsert_grid_co2(
    pool: &PgPool,
    price_area: &str,
    forecast: bool,
    rows: &[(DateTime<Utc>, f64)],
) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let mut n = 0;
    for (ts, g) in rows {
        n += sqlx::query(
            "INSERT INTO grid_co2 (price_area, ts, forecast, g_per_kwh, fetched_at)
             VALUES ($1, $2, $3, $4, now())
             ON CONFLICT (price_area, ts, forecast)
             DO UPDATE SET g_per_kwh = EXCLUDED.g_per_kwh, fetched_at = now()",
        )
        .bind(price_area)
        .bind(ts)
        .bind(forecast)
        .bind(g)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    }
    tx.commit().await?;
    Ok(n)
}

#[derive(Debug, Clone)]
pub struct MinuteRow {
    pub container_id: String,
    pub ts: DateTime<Utc>,
    pub user_id: String,
    pub node_id: String,
    pub container_name: String,
    pub interval_s: f64,
    pub watts: f64,
    pub wh: f64,
    pub co2_g: Option<f64>,
    pub g_per_kwh: Option<f64>,
    pub method: &'static str,
}

pub async fn insert_minutes(pool: &PgPool, rows: &[MinuteRow]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    for r in rows {
        sqlx::query(
            "INSERT INTO container_energy_minutes
             (container_id, ts, user_id, node_id, container_name, interval_s, watts, wh, co2_g, g_per_kwh, method)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT DO NOTHING",
        )
        .bind(&r.container_id)
        .bind(r.ts)
        .bind(&r.user_id)
        .bind(&r.node_id)
        .bind(&r.container_name)
        .bind(r.interval_s)
        .bind(r.watts)
        .bind(r.wh)
        .bind(r.co2_g)
        .bind(r.g_per_kwh)
        .bind(r.method)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Rebuild the hourly rows for the last 26 hours from the minute rows (the
/// current hour included, so it is simply overwritten as it fills), then
/// prune: minute rows after 7 days, node readings after 30, forecasts after
/// a day. Returns (hour rows written, minute rows pruned).
/// Node readings → `node_energy_hourly`, the platform's own long-term record.
/// Each reading covers the time since the previous one, capped at 150 s
/// (a longer gap is a missing minute, not a longer minute); the first
/// reading in the window counts as 60 s. CO₂ uses the grid value at the
/// reading's time: measured within 45 min preferred, else the forecast.
/// Re-run every tick over the last 26 hours, so late grid data or late
/// readings correct the hour they belong to.
pub const NODE_HOURLY_ROLLUP_SQL: &str = "
    WITH s AS (
        SELECT node_id, ts, watts,
               LEAST(EXTRACT(EPOCH FROM (ts - lag(ts) OVER (PARTITION BY node_id ORDER BY ts))), 150) AS dt
        FROM node_power_samples
        WHERE ts >= now() - interval '27 hours'
    ), p AS (
        SELECT s.node_id, s.ts, s.watts, COALESCE(s.dt, 60) AS dt,
               (SELECT g.g_per_kwh FROM grid_co2 g
                 JOIN nodes n ON n.node_id = s.node_id AND g.price_area = n.price_area
                WHERE g.ts <= s.ts AND g.ts >= s.ts - interval '45 minutes'
                ORDER BY g.forecast, g.ts DESC LIMIT 1) AS g_per_kwh
        FROM s
        WHERE s.ts >= now() - interval '26 hours'
    )
    INSERT INTO node_energy_hourly (node_id, hour, samples, unpriced, watts_avg, watts_max, wh, co2_g)
    SELECT node_id, date_trunc('hour', ts), count(*),
           count(*) FILTER (WHERE g_per_kwh IS NULL),
           avg(watts), max(watts),
           sum(watts * dt / 3600.0),
           coalesce(sum(watts * dt / 3600.0 * g_per_kwh / 1000.0), 0)
    FROM p
    GROUP BY node_id, date_trunc('hour', ts)
    ON CONFLICT (node_id, hour) DO UPDATE SET
        samples = EXCLUDED.samples, unpriced = EXCLUDED.unpriced,
        watts_avg = EXCLUDED.watts_avg, watts_max = EXCLUDED.watts_max,
        wh = EXCLUDED.wh, co2_g = EXCLUDED.co2_g";

pub async fn rollup_and_prune(pool: &PgPool) -> sqlx::Result<(u64, u64)> {
    let hours = sqlx::query(
        "INSERT INTO container_energy_hourly
            (container_id, hour, user_id, node_id, container_name, samples, watts_avg, wh, co2_g)
         SELECT container_id, date_trunc('hour', ts), user_id, node_id,
                max(container_name), count(*), avg(watts), sum(wh), sum(co2_g)
         FROM container_energy_minutes
         WHERE ts >= now() - interval '26 hours'
         GROUP BY container_id, date_trunc('hour', ts), user_id, node_id
         ON CONFLICT (container_id, hour) DO UPDATE SET
            samples = EXCLUDED.samples, watts_avg = EXCLUDED.watts_avg,
            wh = EXCLUDED.wh, co2_g = EXCLUDED.co2_g,
            container_name = EXCLUDED.container_name",
    )
    .execute(pool)
    .await?
    .rows_affected();
    sqlx::query(NODE_HOURLY_ROLLUP_SQL).execute(pool).await?;
    let pruned =
        sqlx::query("DELETE FROM container_energy_minutes WHERE ts < now() - interval '7 days'")
            .execute(pool)
            .await?
            .rows_affected();
    sqlx::query("DELETE FROM node_power_samples WHERE ts < now() - interval '30 days'")
        .execute(pool)
        .await?;
    sqlx::query("DELETE FROM grid_co2 WHERE forecast AND ts < now() - interval '1 day'")
        .execute(pool)
        .await?;
    Ok((hours, pruned))
}

// ============= Queries for the API =============

#[derive(Debug, Clone, serde::Serialize)]
pub struct ContainerTotals {
    pub container_id: String,
    pub name: String,
    pub node_id: String,
    /// Watt-hours over the window.
    pub wh: f64,
    /// Gram CO₂ over the window, from the samples that had a grid value.
    pub co2_g: Option<f64>,
    /// Samples in the window with no grid value (Energinet unreachable).
    pub unpriced_samples: i64,
    pub watts_now: Option<f64>,
    pub watts_as_of: Option<DateTime<Utc>>,
}

/// Totals per container for one customer since `since`. Windows up to
/// seven days read the minute rows, longer ones the hourly rollup.
pub async fn totals_for_user(
    pool: &PgPool,
    user_id: &str,
    since: DateTime<Utc>,
    name: Option<&str>,
) -> sqlx::Result<Vec<ContainerTotals>> {
    let use_minutes = Utc::now() - since <= Duration::days(7);
    let sql = if use_minutes {
        "SELECT container_id, max(container_name) AS name, max(node_id) AS node_id,
                coalesce(sum(wh), 0) AS wh, sum(co2_g) AS co2_g,
                count(*) FILTER (WHERE co2_g IS NULL) AS unpriced
         FROM container_energy_minutes
         WHERE user_id = $1 AND ts >= $2 AND ($3::text IS NULL OR container_name = $3)
         GROUP BY container_id ORDER BY name"
    } else {
        "SELECT container_id, max(container_name) AS name, max(node_id) AS node_id,
                coalesce(sum(wh), 0) AS wh, sum(co2_g) AS co2_g,
                coalesce(sum(samples) FILTER (WHERE co2_g IS NULL), 0)::bigint AS unpriced
         FROM container_energy_hourly
         WHERE user_id = $1 AND hour >= $2 AND ($3::text IS NULL OR container_name = $3)
         GROUP BY container_id ORDER BY name"
    };
    let rows = sqlx::query(sql)
        .bind(user_id)
        .bind(since)
        .bind(name)
        .fetch_all(pool)
        .await?;
    let now = watts_now_for_user(pool, user_id).await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let container_id: String = r.get("container_id");
            let (watts_now, watts_as_of) = now
                .get(&container_id)
                .map(|(w, t)| (Some(*w), Some(*t)))
                .unwrap_or((None, None));
            ContainerTotals {
                container_id,
                name: r.get("name"),
                node_id: r.get("node_id"),
                wh: r.get("wh"),
                co2_g: r.get("co2_g"),
                unpriced_samples: r.get("unpriced"),
                watts_now,
                watts_as_of,
            }
        })
        .collect())
}

/// Latest attributed watts per container id, when read in the last three
/// minutes. Older than that the meter or the agent is silent, and the CLI
/// shows nothing rather than a stale number.
/// The attribution method behind this user's most recent minute row in the
/// window, so the API describes what was done, not what is configured. None
/// when there are no rows.
pub async fn method_for_user(
    pool: &PgPool,
    user_id: &str,
    since: DateTime<Utc>,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "SELECT method FROM container_energy_minutes
         WHERE user_id = $1 AND ts >= $2
         ORDER BY ts DESC LIMIT 1",
    )
    .bind(user_id)
    .bind(since)
    .fetch_optional(pool)
    .await
}

/// Whether any node has an idle baseline set (what the next rows will use).
pub async fn any_idle_baseline(pool: &PgPool) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM nodes WHERE power_idle_watts IS NOT NULL)")
        .fetch_one(pool)
        .await
}

pub async fn watts_now_for_user(
    pool: &PgPool,
    user_id: &str,
) -> sqlx::Result<HashMap<String, (f64, DateTime<Utc>)>> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (container_id) container_id, watts, ts
         FROM container_energy_minutes
         WHERE user_id = $1 AND ts > now() - interval '3 minutes'
         ORDER BY container_id, ts DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("container_id"), (r.get("watts"), r.get("ts"))))
        .collect())
}

/// Same as [`watts_now_for_user`], keyed by container name (for `ps`).
pub async fn watts_now_by_name(pool: &PgPool, user_id: &str) -> sqlx::Result<HashMap<String, f64>> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (container_name) container_name, watts
         FROM container_energy_minutes
         WHERE user_id = $1 AND ts > now() - interval '3 minutes'
         ORDER BY container_name, ts DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("container_name"), r.get("watts")))
        .collect())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct HourRow {
    pub hour: DateTime<Utc>,
    pub watts_avg: f64,
    pub wh: f64,
    pub co2_g: Option<f64>,
}

pub async fn hourly_for_container(
    pool: &PgPool,
    user_id: &str,
    name: &str,
    since: DateTime<Utc>,
) -> sqlx::Result<Vec<HourRow>> {
    let rows = sqlx::query(
        "SELECT hour, sum(wh) AS wh, sum(co2_g) AS co2_g,
                sum(watts_avg * samples) / nullif(sum(samples), 0) AS watts_avg
         FROM container_energy_hourly
         WHERE user_id = $1 AND container_name = $2 AND hour >= $3
         GROUP BY hour ORDER BY hour",
    )
    .bind(user_id)
    .bind(name)
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| HourRow {
            hour: r.get("hour"),
            watts_avg: r.get::<Option<f64>, _>("watts_avg").unwrap_or(0.0),
            wh: r.get::<Option<f64>, _>("wh").unwrap_or(0.0),
            co2_g: r.get("co2_g"),
        })
        .collect())
}

/// The latest reading per node, for operators (admin tooling to come).
#[allow(dead_code)]
pub async fn latest_node_samples(pool: &PgPool) -> sqlx::Result<Vec<NodePowerSample>> {
    let rows = sqlx::query(
        "SELECT DISTINCT ON (node_id) node_id, ts, watts, energy_wh, source
         FROM node_power_samples ORDER BY node_id, ts DESC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| NodePowerSample {
            node_id: r.get("node_id"),
            ts: r.get("ts"),
            watts: r.get("watts"),
            energy_wh: r.get("energy_wh"),
            source: r.get("source"),
        })
        .collect())
}
