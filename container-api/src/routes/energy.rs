// routes/energy.rs — watts and CO₂ per container
//
// GET /api/energy?since=24h                    account overview
// GET /api/containers/<name>/energy?since=24h  one container, with hourly series
//
// Numbers come from the controller's energy tables (services/energy). A
// container's watts are its attributed share of the node's measured draw;
// the response says which method produced them.

use crate::guards::AuthenticatedUser;
use crate::services::energy::attribution::{self, METHOD_BASELINE, METHOD_CPUTIME};
use crate::services::energy::store;
use chrono::{DateTime, Duration, Utc};
use rocket::serde::json::Json;

/// `24h`, `7d`, `30d`, `90m`, `2w`; default 24h, capped at 400 days.
pub fn parse_since(s: Option<&str>) -> Result<Duration, String> {
    let s = s.unwrap_or("24h").trim();
    let (num, unit) = s.split_at(s.trim_end_matches(|c: char| c.is_ascii_alphabetic()).len());
    let n: i64 = num
        .parse()
        .map_err(|_| format!("invalid window '{s}', use e.g. 24h, 7d, 30d"))?;
    if n <= 0 {
        return Err("window must be positive".into());
    }
    let d = match unit {
        "m" => Duration::minutes(n),
        "h" => Duration::hours(n),
        "d" => Duration::days(n),
        "w" => Duration::weeks(n),
        _ => return Err(format!("invalid window '{s}', use e.g. 24h, 7d, 30d")),
    };
    Ok(d.min(Duration::days(400)))
}

fn method_text(method: &str) -> &'static str {
    match method {
        m if m == METHOD_BASELINE => {
            "The node's watts are measured at the wall. Its idle draw is split between containers by reserved vCPU, the rest by CPU time used in each interval. CO₂ uses Energinet's 5-minute grid intensity for the node's price area."
        }
        m if m == METHOD_CPUTIME => {
            "The node's watts are measured at the wall and split between containers by CPU time used in each interval (no idle baseline is set for the node). CO₂ uses Energinet's 5-minute grid intensity for the node's price area."
        }
        _ => "Attributed share of the node's measured watts.",
    }
}

/// The method the user's rows were produced with; before any row exists, the
/// one the next rows will use.
async fn method_json(
    pool: &sqlx::PgPool,
    user_id: &str,
    since: DateTime<Utc>,
) -> serde_json::Value {
    let used = match store::method_for_user(pool, user_id, since).await {
        Ok(Some(m)) => m,
        _ => {
            let idle = store::any_idle_baseline(pool).await.unwrap_or(false);
            attribution::method(if idle { Some(0.0) } else { None }).to_string()
        }
    };
    serde_json::json!({ "id": used, "text": method_text(&used) })
}

async fn grid_now(pool: &sqlx::PgPool, area: &str) -> serde_json::Value {
    match store::grid_co2_at(pool, area, Utc::now()).await {
        Ok(Some(g)) => serde_json::json!({
            "price_area": area,
            "g_per_kwh": g.g_per_kwh,
            "ts": g.ts,
            "forecast": g.forecast,
        }),
        _ => serde_json::json!({ "price_area": area, "g_per_kwh": null }),
    }
}

fn summarize(
    containers: &[store::ContainerTotals],
    since: DateTime<Utc>,
) -> (serde_json::Value, serde_json::Value, serde_json::Value) {
    let wh: f64 = containers.iter().map(|c| c.wh).sum();
    let co2: Option<f64> = containers
        .iter()
        .filter_map(|c| c.co2_g)
        .fold(None, |acc, g| Some(acc.unwrap_or(0.0) + g));
    let unpriced: i64 = containers.iter().map(|c| c.unpriced_samples).sum();
    let watts_now: Option<f64> = containers
        .iter()
        .filter_map(|c| c.watts_now)
        .fold(None, |acc, w| Some(acc.unwrap_or(0.0) + w));
    let as_of = containers.iter().filter_map(|c| c.watts_as_of).max();
    (
        serde_json::json!({ "since": since, "until": Utc::now() }),
        serde_json::json!({ "watts": watts_now, "as_of": as_of }),
        serde_json::json!({ "wh": wh, "co2_g": co2, "unpriced_samples": unpriced }),
    )
}

#[get("/energy?<since>")]
pub async fn account_energy(
    since: Option<String>,
    user: AuthenticatedUser,
    pool: &rocket::State<sqlx::PgPool>,
    price_area: &rocket::State<EnergyPriceArea>,
) -> Json<serde_json::Value> {
    let window = match parse_since(since.as_deref()) {
        Ok(d) => d,
        Err(e) => return Json(serde_json::json!({ "error": e })),
    };
    let since_ts = Utc::now() - window;
    let containers = match store::totals_for_user(pool.inner(), &user.0.id, since_ts, None).await {
        Ok(c) => c,
        Err(e) => return Json(serde_json::json!({ "error": format!("energy query failed: {e}") })),
    };
    let (window_json, now, total) = summarize(&containers, since_ts);
    Json(serde_json::json!({
        "window": window_json,
        "grid": grid_now(pool.inner(), &price_area.0).await,
        "now": now,
        "total": total,
        "containers": containers,
        "method": method_json(pool.inner(), &user.0.id, since_ts).await,
    }))
}

#[get("/containers/<name>/energy?<since>")]
pub async fn container_energy(
    name: String,
    since: Option<String>,
    user: AuthenticatedUser,
    pool: &rocket::State<sqlx::PgPool>,
    price_area: &rocket::State<EnergyPriceArea>,
) -> Json<serde_json::Value> {
    let window = match parse_since(since.as_deref()) {
        Ok(d) => d,
        Err(e) => return Json(serde_json::json!({ "error": e })),
    };
    let since_ts = Utc::now() - window;
    let containers = match store::totals_for_user(pool.inner(), &user.0.id, since_ts, Some(&name))
        .await
    {
        Ok(c) => c,
        Err(e) => return Json(serde_json::json!({ "error": format!("energy query failed: {e}") })),
    };
    if containers.is_empty() {
        return Json(serde_json::json!({
            "error": format!("no energy data for '{name}' in the last {}", since.unwrap_or_else(|| "24h".into())),
            "hint": "numbers appear a minute or two after the node's meter and agent both report",
        }));
    }
    let hourly = store::hourly_for_container(pool.inner(), &user.0.id, &name, since_ts)
        .await
        .unwrap_or_default();
    let (window_json, now, total) = summarize(&containers, since_ts);
    Json(serde_json::json!({
        "container": containers[0],
        "window": window_json,
        "grid": grid_now(pool.inner(), &price_area.0).await,
        "now": now,
        "total": total,
        "hourly": hourly,
        "method": method_json(pool.inner(), &user.0.id, since_ts).await,
    }))
}

/// Managed state: the default price area for the "grid now" line.
pub struct EnergyPriceArea(pub String);

/// Best-effort block for `/usage` (and the TUI): live watts, 24 h totals,
/// grid intensity now. Never fails the usage call.
pub async fn usage_block(pool: &sqlx::PgPool, user_id: &str, area: &str) -> serde_json::Value {
    let since = Utc::now() - Duration::hours(24);
    let containers = store::totals_for_user(pool, user_id, since, None)
        .await
        .unwrap_or_default();
    let (_, now, total) = summarize(&containers, since);
    serde_json::json!({
        "now": now,
        "last_24h": total,
        "grid": grid_now(pool, area).await,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows() {
        assert_eq!(parse_since(None).unwrap(), Duration::hours(24));
        assert_eq!(parse_since(Some("7d")).unwrap(), Duration::days(7));
        assert_eq!(parse_since(Some("90m")).unwrap(), Duration::minutes(90));
        assert_eq!(parse_since(Some("2w")).unwrap(), Duration::weeks(2));
        assert_eq!(parse_since(Some("9999d")).unwrap(), Duration::days(400));
        assert!(parse_since(Some("x")).is_err());
        assert!(parse_since(Some("0h")).is_err());
        assert!(parse_since(Some("5y")).is_err());
    }
}
