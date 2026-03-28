// guards.rs - Security-focused guards with raw socket IP
//
// Authentication flow (production):
//   1. Extract raw TCP socket IP (unforgeable via WireGuard /32 enforcement)
//   2. Look up IP in embedded PeerCache (wg show → public key)
//   3. Look up public key in PostgreSQL → User
//
// No separate auth-api service needed.

use rocket::http::Status;
use rocket::request::{self, FromRequest, Outcome, Request};
use sqlx::PgPool;
use tracing::{error, info};

use crate::models::User;
use crate::services::peer_resolver;
use crate::AppState;

// ============= CLIENT IP GUARD =============
// CRITICAL: Uses raw TCP socket address for security
// This IP is unforgeable - it comes from the actual TCP connection
// WireGuard's kernel-level /32 enforcement means only the legitimate
// peer can send packets from this IP

pub struct ClientIP(pub String);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ClientIP {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        // CRITICAL: Use the actual TCP socket address, not headers
        // Headers like X-Real-IP and X-Forwarded-For can be spoofed
        // The socket address cannot be forged when using WireGuard
        if let Some(addr) = request.remote() {
            return Outcome::Success(ClientIP(addr.ip().to_string()));
        }

        Outcome::Error((Status::BadRequest, ()))
    }
}

// ============= AUTHENTICATED USER GUARD =============

pub struct AuthenticatedUser(pub User);

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AuthenticatedUser {
    type Error = String;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        // Get client IP from raw socket (unforgeable)
        let client_ip = match ClientIP::from_request(request).await {
            Outcome::Success(ip) => ip.0,
            _ => {
                return Outcome::Error((
                    Status::BadRequest,
                    "Could not determine client IP".to_string(),
                ))
            }
        };

        // Get app state
        let app_state = match request.guard::<&rocket::State<AppState>>().await {
            Outcome::Success(state) => state,
            _ => {
                return Outcome::Error((
                    Status::InternalServerError,
                    "App state not available".to_string(),
                ))
            }
        };

        // Get database pool from managed state
        let pool = match request.guard::<&rocket::State<PgPool>>().await {
            Outcome::Success(p) => p,
            _ => {
                return Outcome::Error((
                    Status::InternalServerError,
                    "Database not available".to_string(),
                ))
            }
        };

        // Dev mode shortcut
        if app_state.config.dev_mode {
            let dev_user = User {
                id: "dev-user-id".to_string(),
                email: "dev@example.com".to_string(),
                full_name: "Development User".to_string(),
                wireguard_public_key: app_state.config.dev_user_public_key.clone(),
                wireguard_ip: client_ip,
                plan_id: "dev-plan".to_string(),
                account_status: "active".to_string(),
                allowed_actions: vec![
                    "deploy".to_string(),
                    "list".to_string(),
                    "delete".to_string(),
                    "stop".to_string(),
                ],
                primary_garage_id: "ry".to_string(),
                user_slot: Some(99),
            };
            return Outcome::Success(AuthenticatedUser(dev_user));
        }

        // Production: resolve via embedded peer cache (no external service)
        match resolve_user_from_ip(&client_ip, app_state, pool).await {
            Some(user) => {
                info!("✅ Socket IP {} → User {}", client_ip, user.email);
                Outcome::Success(AuthenticatedUser(user))
            }
            None => {
                error!("❌ Socket IP {} → No user found", client_ip);
                Outcome::Error((
                    Status::Unauthorized,
                    "User not found or inactive".to_string(),
                ))
            }
        }
    }
}

async fn resolve_user_from_ip(
    client_ip: &str,
    app_state: &AppState,
    pool: &PgPool,
) -> Option<User> {
    info!(
        "🔍 Resolving socket IP {} via embedded peer cache",
        client_ip
    );

    // Use embedded peer cache to map IP → WireGuard public key
    // No HTTP call to a separate service - it's all in-process now
    let peer_info = match peer_resolver::resolve_peer(
        client_ip,
        &app_state.peer_cache,
        app_state.config.dev_mode,
        &app_state.config.dev_user_public_key,
    )
    .await
    {
        Ok(info) => {
            info!(
                "✅ Peer cache: {} → public key {}...",
                client_ip,
                &info.public_key[..20.min(info.public_key.len())]
            );
            info
        }
        Err(status) => {
            error!(
                "❌ Peer resolution failed for {} with status: {:?}",
                client_ip, status
            );
            return None;
        }
    };

    // Look up user in database using the resolved public key
    app_state
        .get_user_by_public_key(&peer_info.public_key, pool)
        .await
}
