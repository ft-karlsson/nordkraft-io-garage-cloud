// routes/account.rs — the customer's own account: status and leaving.
//
// All routes are WireGuard-authenticated like the rest of the API. Deletion
// is a two-step flow with an emailed code, so a stolen laptop with a live
// tunnel is not enough to delete an account; see services/account_deletion.rs.

use crate::guards::AuthenticatedUser;
use crate::services::account_deletion as deletion;
use crate::services::mail_client::{Mail, MailClient};
use crate::services::verification;
use crate::AppState;
use chrono::{DateTime, Utc};
use rocket::http::Status;
use rocket::serde::json::Json;
use rocket::State;
use serde::Deserialize;
use sqlx::{PgPool, Row};
use tracing::{error, info, warn};
use uuid::Uuid;

type Reply = (Status, Json<serde_json::Value>);

fn err(status: Status, msg: impl Into<String>) -> Reply {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}

fn db_err(e: sqlx::Error) -> Reply {
    error!("account route: database error: {}", e);
    err(Status::InternalServerError, "database error")
}

#[get("/account")]
pub async fn get_account(user: AuthenticatedUser, pool: &State<PgPool>) -> Reply {
    let u = &user.0;
    let open = match deletion::open_request(pool.inner(), &u.id).await {
        Ok(o) => o,
        Err(e) => return db_err(e),
    };
    (
        Status::Ok,
        Json(serde_json::json!({
            "id": u.id,
            "email": u.email,
            "full_name": u.full_name,
            "plan_id": u.plan_id,
            "account_status": u.account_status,
            "user_slot": u.user_slot,
            "deletion": open,
        })),
    )
}

/// Step one: email a code. Nothing about the account changes.
#[post("/account/deletion/request")]
pub async fn request_deletion(
    user: AuthenticatedUser,
    pool: &State<PgPool>,
    mail: &State<MailClient>,
) -> Reply {
    let u = &user.0;
    let pool = pool.inner();

    match deletion::open_request(pool, &u.id).await {
        Ok(Some(open)) => {
            return (
                Status::Conflict,
                Json(serde_json::json!({
                    "error": "deletion already requested",
                    "deletion": open,
                })),
            )
        }
        Ok(None) => {}
        Err(e) => return db_err(e),
    }

    // One code a minute: the mail takes a moment, and this is the only
    // unauthenticated-by-code step that costs an email.
    let recent: Option<DateTime<Utc>> = match sqlx::query_scalar(
        "SELECT MAX(created_at) FROM email_verifications
         WHERE user_id = $1 AND purpose = 'delete' AND verified_at IS NULL",
    )
    .bind(&u.id)
    .fetch_one(pool)
    .await
    {
        Ok(t) => t,
        Err(e) => return db_err(e),
    };
    if let Some(t) = recent {
        if (Utc::now() - t).num_seconds() < verification::RESEND_COOLDOWN_SECONDS {
            return err(
                Status::TooManyRequests,
                "a code was sent a moment ago; check your inbox (and spam) before asking again",
            );
        }
    }

    let id = Uuid::new_v4().to_string();
    let code = verification::generate_code();
    let now = Utc::now();
    let expires_at = verification::expiry_from(now);
    let mut tx = match pool.begin().await {
        Ok(t) => t,
        Err(e) => return db_err(e),
    };
    for q in [
        sqlx::query("DELETE FROM email_verifications WHERE user_id = $1 AND purpose = 'delete' AND verified_at IS NULL")
            .bind(&u.id),
        sqlx::query(
            "INSERT INTO email_verifications (id, user_id, purpose, code_hash, attempts, created_at, expires_at)
             VALUES ($1, $2, 'delete', $3, 0, $4, $5)",
        )
        .bind(&id)
        .bind(&u.id)
        .bind(verification::hash_code(&id, &code))
        .bind(now)
        .bind(expires_at),
    ] {
        if let Err(e) = q.execute(&mut *tx).await {
            return db_err(e);
        }
    }
    if let Err(e) = tx.commit().await {
        return db_err(e);
    }

    if let Err(e) = mail
        .send(&u.email, &u.full_name, Mail::DeletionCode { code: &code })
        .await
    {
        warn!("deletion request for {}: code mail not sent: {}", u.id, e);
        let _ = sqlx::query("DELETE FROM email_verifications WHERE id = $1")
            .bind(&id)
            .execute(pool)
            .await;
        return err(
            Status::BadGateway,
            "could not send the confirmation email right now; try again in a moment",
        );
    }
    info!("🧹 deletion requested by {}: code sent", u.id);
    (
        Status::Accepted,
        Json(serde_json::json!({
            "verification_id": id,
            "email": u.email,
            "expires_at": expires_at,
            "attempts": verification::MAX_ATTEMPTS,
        })),
    )
}

#[derive(Debug, Deserialize)]
pub struct ConfirmRequest {
    pub verification_id: String,
    pub code: String,
    /// `nordkraft account delete --now`: no grace period.
    #[serde(default)]
    pub immediate: bool,
}

/// Step two: the code is right, the clock starts.
#[post("/account/deletion/confirm", data = "<req>")]
pub async fn confirm_deletion(
    user: AuthenticatedUser,
    req: Json<ConfirmRequest>,
    pool: &State<PgPool>,
    mail: &State<MailClient>,
    app_state: &State<AppState>,
    runner: &State<deletion::Runner>,
) -> Reply {
    let u = &user.0;
    let pool = pool.inner();

    let row = match sqlx::query(
        "SELECT id, code_hash, attempts, expires_at, verified_at FROM email_verifications
         WHERE id = $1 AND user_id = $2 AND purpose = 'delete'",
    )
    .bind(&req.verification_id)
    .bind(&u.id)
    .fetch_optional(pool)
    .await
    {
        Ok(Some(r)) => r,
        Ok(None) => return err(Status::NotFound, "unknown verification; run `nordkraft account delete` again"),
        Err(e) => return db_err(e),
    };
    let stored = verification::Stored {
        id: row.get("id"),
        code_hash: row.get("code_hash"),
        attempts: row.get("attempts"),
        expires_at: row.get("expires_at"),
        verified_at: row.get("verified_at"),
    };
    if stored.verified_at.is_some() {
        return err(Status::Conflict, "this code was already used");
    }
    match verification::check(&stored, &req.code, Utc::now()) {
        verification::Check::Dead => {
            return err(Status::Gone, "code expired; run `nordkraft account delete` again for a new one")
        }
        verification::Check::Wrong { attempts_left } => {
            let _ = sqlx::query("UPDATE email_verifications SET attempts = attempts + 1 WHERE id = $1")
                .bind(&stored.id)
                .execute(pool)
                .await;
            return (
                Status::BadRequest,
                Json(serde_json::json!({
                    "error": format!("wrong code ({} attempts left)", attempts_left),
                    "attempts_left": attempts_left,
                })),
            );
        }
        verification::Check::Ok => {}
    }

    if let Err(e) = sqlx::query("UPDATE email_verifications SET verified_at = NOW() WHERE id = $1")
        .bind(&stored.id)
        .execute(pool)
        .await
    {
        return db_err(e);
    }
    let grace_days = if req.immediate { 0 } else { app_state.config.account_deletion_grace_days };
    let rec = match deletion::schedule(
        pool, &u.id, u.user_slot, &u.email, &u.full_name, grace_days, req.immediate,
    )
    .await
    {
        Ok(r) => r,
        Err(e) if e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false) => {
            return err(Status::Conflict, "deletion already requested")
        }
        Err(e) => return db_err(e),
    };
    warn!(
        "🧹 deletion confirmed by {} (slot {:?}): scheduled for {}{}",
        u.id, u.user_slot, rec.scheduled_for, if req.immediate { " (immediate)" } else { "" }
    );
    let slot = u.user_slot.map(|s| s.to_string()).unwrap_or_else(|| "?".into());
    if req.immediate {
        // The "slettes den …" mail would arrive minutes before the final one;
        // the customer gets the goodbye mail when the purge is done.
        mail.notify(
            "Konto slettes nu",
            &format!("Slot {slot} har bekræftet sletning uden frist. Purge starter om lidt."),
        )
        .await;
        deletion::kick(runner, app_state.config.account_deletion_auto_purge);
    } else {
        if let Err(e) = mail
            .send(
                &u.email,
                &u.full_name,
                Mail::DeletionScheduled { scheduled_for: rec.scheduled_for },
            )
            .await
        {
            warn!("deletion {}: confirmation mail not sent: {}", rec.id, e);
        }
        mail.notify(
            "Sletning planlagt",
            &format!(
                "Slot {slot} har bekræftet sletning af sin konto. Slettes {} ({} dages frist). Kunden kan fortryde med nordkraft account restore.",
                rec.scheduled_for.format("%d-%m-%Y %H:%M UTC"),
                grace_days
            ),
        )
        .await;
    }
    (
        Status::Ok,
        Json(serde_json::json!({
            "status": "scheduled",
            "deletion": rec,
            "grace_days": grace_days,
            "immediate": req.immediate,
        })),
    )
}

/// `nordkraft account restore`: undo an open request before it runs.
#[post("/account/deletion/cancel")]
pub async fn cancel_deletion(
    user: AuthenticatedUser,
    pool: &State<PgPool>,
    mail: &State<MailClient>,
) -> Reply {
    let u = &user.0;
    match deletion::cancel(pool.inner(), &u.id).await {
        Ok(true) => {}
        Ok(false) => return err(Status::NotFound, "no pending deletion to cancel"),
        Err(e) => return db_err(e),
    }
    info!("🧹 deletion cancelled by {}", u.id);
    if let Err(e) = mail.send(&u.email, &u.full_name, Mail::DeletionCancelled).await {
        warn!("deletion cancel for {}: mail not sent: {}", u.id, e);
    }
    mail.notify(
        "Sletning annulleret",
        &format!(
            "Slot {} har fortrudt og beholder sin konto.",
            u.user_slot.map(|s| s.to_string()).unwrap_or_else(|| "?".into())
        ),
    )
    .await;
    (Status::Ok, Json(serde_json::json!({ "status": "cancelled" })))
}
