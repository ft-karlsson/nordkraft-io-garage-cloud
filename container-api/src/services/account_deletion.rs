// services/account_deletion.rs — a customer leaving, from "I want out" to
// "your data is gone".
//
// Flow (see setup/ACCOUNT_LIFECYCLE.md):
//   1. `nordkraft account delete` → POST /api/account/deletion/request:
//      a six-digit code goes to the account's address (email_verifications,
//      purpose 'delete'). Nothing else changes.
//   2. The code is typed back → POST /api/account/deletion/confirm:
//      a row in account_deletions with scheduled_for = 03:00 Copenhagen time,
//      grace-period days ahead (so the date in the mail is exact and the purge
//      runs at night), or right now with `immediate` (`--now`). The account
//      stays active so the customer can fetch what they want to keep, and
//      `nordkraft account restore` (…/cancel) undoes it.
//   3. When scheduled_for passes, the runner below purges the tenant with the
//      same code path as the admin purge (services/tenant_purge.rs), marks the
//      deletion complete, sends the last mail and forgets the address.
//
// The founder hears about every step (schedule, cancel, purge, failure) via
// MailClient::notify, which the backend turns into a push and a mail.
//
// ACCOUNT_DELETION_AUTO_PURGE=false turns the runner into a logger; the admin
// purge endpoint then finishes due deletions by hand, completing the record
// and sending the last mail the same way.

use crate::services::container_manager::ContainerManager;
use crate::services::haproxy_client::HAProxyClientTrait;
use crate::services::mail_client::{Mail, MailClient};
use crate::services::nats_service::NatsService;
use crate::services::peer_reconciler;
use crate::services::pfsense_client::PfSenseClientTrait;
use crate::services::route_manager::StaticRouteManager;
use crate::services::tenant_purge::{self, PurgeInventory, PurgeReport};
use chrono::{DateTime, Duration, TimeZone, Utc};
use chrono_tz::Europe::Copenhagen;
use serde::Serialize;
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tracing::{error, info, warn};
use uuid::Uuid;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Everything a purge needs, owned, so it can run from a request handler or a
/// background task alike.
pub struct PurgeDeps {
    pub pool: PgPool,
    pub haproxy: Arc<dyn HAProxyClientTrait>,
    pub pfsense: Arc<dyn PfSenseClientTrait>,
    pub nats: Option<Arc<NatsService>>,
    pub container_manager: Arc<ContainerManager>,
    pub route_manager: Arc<StaticRouteManager>,
    /// Set on a hybrid node: this process also runs containers.
    pub local_node: Option<String>,
    pub mail: MailClient,
}

/// An open or finished deletion request as the API reports it.
#[derive(Debug, Clone, Serialize)]
pub struct DeletionRecord {
    pub id: String,
    pub requested_at: DateTime<Utc>,
    pub scheduled_for: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
}

/// The customer's open (not cancelled, not completed) request, if any.
pub async fn open_request(pool: &PgPool, user_id: &str) -> Result<Option<DeletionRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id, requested_at, scheduled_for, started_at FROM account_deletions
         WHERE user_id = $1 AND cancelled_at IS NULL AND completed_at IS NULL",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| DeletionRecord {
        id: r.get("id"),
        requested_at: r.get("requested_at"),
        scheduled_for: r.get("scheduled_for"),
        started_at: r.get("started_at"),
    }))
}

/// Handle to the runner's dependencies for request handlers: `None` on a
/// plain agent (no runner), so an immediate deletion simply waits for the
/// controller's next tick.
pub struct Runner(pub Option<Arc<PurgeDeps>>);

/// When a confirmed deletion runs: 03:00 in Copenhagen, `grace_days` calendar
/// days after `now` (local date). That is also the day the customer reads in
/// the mail; the UTC day is the same because 03:00 local is 01:00 or 02:00 UTC.
pub fn purge_moment(now: DateTime<Utc>, grace_days: i64) -> DateTime<Utc> {
    let local_day = now.with_timezone(&Copenhagen).date_naive() + Duration::days(grace_days.max(0));
    let at = |h| {
        local_day
            .and_hms_opt(h, 0, 0)
            .and_then(|t| Copenhagen.from_local_datetime(&t).earliest())
    };
    at(3).or_else(|| at(4)).map(|t| t.with_timezone(&Utc)).unwrap_or(now)
}

/// Confirmed: record the request. The user row is untouched. `immediate`
/// (`nordkraft account delete --now`) schedules it for right now; the runner
/// picks it up on its next tick, or at once via [`kick`].
pub async fn schedule(
    pool: &PgPool,
    user_id: &str,
    user_slot: Option<i32>,
    email: &str,
    full_name: &str,
    grace_days: i64,
    immediate: bool,
) -> Result<DeletionRecord, sqlx::Error> {
    let now = Utc::now();
    let rec = DeletionRecord {
        id: Uuid::new_v4().to_string(),
        requested_at: now,
        scheduled_for: if immediate { now } else { purge_moment(now, grace_days) },
        started_at: None,
    };
    sqlx::query(
        "INSERT INTO account_deletions (id, user_id, user_slot, email, full_name, requested_at, scheduled_for)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&rec.id)
    .bind(user_id)
    .bind(user_slot)
    .bind(email)
    .bind(full_name)
    .bind(rec.requested_at)
    .bind(rec.scheduled_for)
    .execute(pool)
    .await?;
    Ok(rec)
}

/// `nordkraft account restore`. Returns false when there was nothing open or
/// the purge has already started (too late: the account is suspended by then
/// and the guard would not have let the call in).
pub async fn cancel(pool: &PgPool, user_id: &str) -> Result<bool, sqlx::Error> {
    let n = sqlx::query(
        "UPDATE account_deletions SET cancelled_at = NOW()
         WHERE user_id = $1 AND cancelled_at IS NULL AND completed_at IS NULL AND started_at IS NULL",
    )
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n == 1)
}

/// Everything the admin purge route does after its dry-run and confirm-email
/// checks, so the runner and the route cannot drift apart. Ok when the rows
/// are gone; Err carries what to tell the caller and the partial report.
pub async fn execute_purge(deps: &PurgeDeps, inv: &PurgeInventory) -> Result<PurgeReport, (String, PurgeReport)> {
    let mut report = PurgeReport::default();
    tenant_purge::suspend(&deps.pool, &inv.user, &mut report).await;
    if report.errors.is_empty() {
        let ctx = tenant_purge::PurgeContext {
            pool: &deps.pool,
            haproxy: &deps.haproxy,
            pfsense: &deps.pfsense,
            nats: deps.nats.as_deref(),
            local: deps
                .local_node
                .as_deref()
                .map(|n| (deps.container_manager.as_ref(), n)),
        };
        tenant_purge::purge_external(&ctx, inv, &mut report).await;

        // The account is suspended, so the reconciler no longer wants its
        // peer, its children or any of their rules: one pass removes them.
        match peer_reconciler::global() {
            Some(rec) => {
                let r = rec.reconcile_now().await;
                if r.errors.is_empty() {
                    report.done.push(format!(
                        "WireGuard peers and nft rules converged ({} peer(s) removed)",
                        r.wg_removed.len()
                    ));
                } else {
                    report
                        .errors
                        .push(format!("WireGuard/nft: {}", r.errors.join("; ")));
                }
            }
            None => report
                .errors
                .push("no peer reconciler in this mode".to_string()),
        }
    }

    if !report.errors.is_empty() {
        return Err((
            "The account is suspended and its DB rows are kept. Fix the errors and run the purge again.".to_string(),
            report,
        ));
    }

    if let Err(e) = tenant_purge::delete_rows(&deps.pool, &inv.user).await {
        report.errors.push(format!("DB delete: {}", e));
        return Err((
            "Everything outside the DB is gone, but the rows could not be deleted. Run the purge again.".to_string(),
            report,
        ));
    }
    report.done.push("DB rows deleted".to_string());
    peer_reconciler::kick();

    tenant_purge::remove_controller_routes(&deps.route_manager, inv).await;
    Ok(report)
}

/// After a successful purge: if the customer had asked for it, close the
/// record, send the last mail and forget the address. An admin purge of an
/// account nobody asked to delete (a test account) sends nothing.
pub async fn finish_requested(deps: &PurgeDeps, user_id: &str) {
    let row = match sqlx::query(
        "UPDATE account_deletions SET completed_at = NOW(), last_error = NULL
         WHERE user_id = $1 AND cancelled_at IS NULL AND completed_at IS NULL
         RETURNING id, email, full_name",
    )
    .bind(user_id)
    .fetch_optional(&deps.pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            error!("account deletion: could not mark {} complete: {}", user_id, e);
            return;
        }
    };
    let Some(row) = row else { return };
    let id: String = row.get("id");
    let email: Option<String> = row.get("email");
    let full_name: Option<String> = row.get("full_name");
    info!("🧹 account deletion {} complete for user {}", id, user_id);

    if let Some(email) = email.as_deref() {
        let name = full_name.as_deref().unwrap_or("");
        match deps.mail.send(email, name, Mail::DeletionCompleted).await {
            Ok(()) => {}
            Err(e) => warn!("account deletion {}: final mail not sent: {}", id, e),
        }
    }
    // The record stays (when, which slot); the person does not.
    if let Err(e) = sqlx::query("UPDATE account_deletions SET email = NULL, full_name = NULL WHERE id = $1")
        .bind(&id)
        .execute(&deps.pool)
        .await
    {
        error!("account deletion {}: could not clear the address: {}", id, e);
    }
}

/// Background task: purge due deletions (controller/hybrid only).
pub fn spawn_runner(deps: Arc<PurgeDeps>, auto_purge: bool, interval: std::time::Duration) {
    tokio::spawn(async move {
        // First look a minute after boot, so a restart during a purge does not
        // start the next attempt before the reconcilers have settled.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            tick(&deps, auto_purge).await;
            tokio::time::sleep(interval).await;
        }
    });
}

/// Run a tick soon, outside the schedule: after an immediate deletion is
/// confirmed. The delay lets the confirming request finish and the CLI print
/// its goodbye before the tunnel goes away.
pub fn kick(runner: &Runner, auto_purge: bool) {
    let Some(deps) = runner.0.clone() else { return };
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        tick(&deps, auto_purge).await;
    });
}

async fn due(pool: &PgPool) -> Result<Vec<(String, String, i32)>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, user_id, attempts FROM account_deletions
         WHERE cancelled_at IS NULL AND completed_at IS NULL AND scheduled_for <= NOW()
         ORDER BY scheduled_for",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get("id"), r.get("user_id"), r.get("attempts")))
        .collect())
}

async fn tick(deps: &PurgeDeps, auto_purge: bool) {
    let due = match due(&deps.pool).await {
        Ok(d) => d,
        Err(e) => {
            warn!("account deletion runner: query failed: {}", e);
            return;
        }
    };
    if due.is_empty() {
        return;
    }
    if !auto_purge {
        warn!(
            "🧹 {} account deletion(s) due; ACCOUNT_DELETION_AUTO_PURGE is off, finish them with POST /api/admin/tenant/purge: {}",
            due.len(),
            due.iter().map(|(_, u, _)| u.as_str()).collect::<Vec<_>>().join(", ")
        );
        return;
    }
    for (id, user_id, attempts) in due {
        if attempts >= 3 {
            // Said once, when the third attempt failed; not every tick.
            continue;
        }
        if let Err(e) = purge_one(deps, &id, &user_id).await {
            error!("account deletion {}: {}", id, e);
            let _ = sqlx::query("UPDATE account_deletions SET last_error = $2 WHERE id = $1")
                .bind(&id)
                .bind(e.to_string())
                .execute(&deps.pool)
                .await;
            let giving_up = attempts + 1 >= 3;
            deps.mail
                .notify(
                    if giving_up { "Sletning fejlede, giver op" } else { "Sletning fejlede" },
                    &format!(
                        "Deletion {id} (forsøg {}): {e}{}",
                        attempts + 1,
                        if giving_up {
                            "\nKontoen er suspenderet; afslut med POST /api/admin/tenant/purge."
                        } else {
                            "\nPrøver igen ved næste tick."
                        }
                    ),
                )
                .await;
        }
    }
}

async fn purge_one(deps: &PurgeDeps, id: &str, user_id: &str) -> Result<(), BoxError> {
    let Some(_lock) = tenant_purge::try_lock() else {
        return Err("another purge is running".into());
    };
    sqlx::query(
        "UPDATE account_deletions SET started_at = COALESCE(started_at, NOW()), attempts = attempts + 1 WHERE id = $1",
    )
    .bind(id)
    .execute(&deps.pool)
    .await?;

    let target = tenant_purge::load_target(&deps.pool, user_id).await?;
    let inv = tenant_purge::inventory(&deps.pool, target, deps.local_node.as_deref()).await?;
    warn!(
        "🧨 Scheduled purge started: user {} (slot {}), deletion {}",
        inv.user.user_id, inv.user.user_slot, id
    );
    match execute_purge(deps, &inv).await {
        Ok(report) => {
            info!("🧨 Scheduled purge complete: {:?}", report.done);
            finish_requested(deps, user_id).await;
            deps.mail
                .notify(
                    "Konto slettet",
                    &format!(
                        "Slot {} er fjernet som ønsket af kunden ({} trin).",
                        inv.user.user_slot,
                        report.done.len()
                    ),
                )
                .await;
            Ok(())
        }
        Err((msg, report)) => Err(format!("{} ({})", msg, report.errors.join("; ")).into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    #[test]
    fn purge_moment_is_three_in_copenhagen_on_the_right_day() {
        // 23:30 Danish time on 28 Oct (CET, UTC+1) is 22:30 UTC the same day;
        // 14 days on is 11 Nov, 03:00 CET = 02:00 UTC.
        let t = purge_moment(utc("2026-10-28T22:30:00Z"), 14);
        assert_eq!(t, utc("2026-11-11T02:00:00Z"));
        // Summer: 03:00 CEST = 01:00 UTC.
        let t = purge_moment(utc("2026-06-01T10:00:00Z"), 14);
        assert_eq!(t, utc("2026-06-15T01:00:00Z"));
        // The UTC calendar day matches the Danish one, so a UTC date formatter
        // prints the day the customer was told.
        assert_eq!(t.date_naive().to_string(), "2026-06-15");
    }

    #[test]
    fn purge_moment_zero_days_is_tonight_or_already_past() {
        // Grace 0 at 10:00 Danish time gives 03:00 the same day, i.e. due at once.
        let now = utc("2026-06-01T08:00:00Z");
        assert!(purge_moment(now, 0) <= now);
    }
}
