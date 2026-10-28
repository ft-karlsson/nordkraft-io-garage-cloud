// services/mail_client.rs — customer mail, sent by the signup backend.
//
// The signup backend (cloud.nordkraft.io, same host as the controller in
// production) owns the SMTP sender and the Danish templates. This client asks
// it to send one of the deletion-flow mails over `POST /api/internal/mail`,
// authenticated with the shared admin key the backend already uses to call
// this service. The recipient travels in the body because, for the final
// mail, the users row is already gone.

use chrono::{DateTime, Utc};
use std::time::Duration;
use tracing::{info, warn};

#[derive(Clone)]
pub struct MailClient {
    base_url: String,
    admin_key: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Copy)]
pub enum Mail<'a> {
    DeletionCode { code: &'a str },
    DeletionScheduled { scheduled_for: DateTime<Utc> },
    DeletionCancelled,
    DeletionCompleted,
}

impl Mail<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Mail::DeletionCode { .. } => "deletion_code",
            Mail::DeletionScheduled { .. } => "deletion_scheduled",
            Mail::DeletionCancelled => "deletion_cancelled",
            Mail::DeletionCompleted => "deletion_completed",
        }
    }
}

impl MailClient {
    pub fn new(signup_api_url: &str, admin_key: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("reqwest client");
        Self {
            base_url: signup_api_url.trim_end_matches('/').to_string(),
            admin_key: admin_key.to_string(),
            http,
        }
    }

    /// Ask the backend to send one mail. The backend queues it and answers at
    /// once; an Err here means the backend was unreachable or refused, not
    /// that the relay failed later (that is in the backend's log).
    pub async fn send(&self, to_email: &str, full_name: &str, mail: Mail<'_>) -> Result<(), String> {
        let mut body = serde_json::json!({
            "kind": mail.kind(),
            "to_email": to_email,
            "full_name": full_name,
        });
        match mail {
            Mail::DeletionCode { code } => body["code"] = serde_json::json!(code),
            Mail::DeletionScheduled { scheduled_for } => {
                body["scheduled_for"] = serde_json::json!(scheduled_for)
            }
            _ => {}
        }
        let url = format!("{}/api/internal/mail", self.base_url);
        let resp = self
            .http
            .post(&url)
            .header("X-Admin-API-Key", &self.admin_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("signup backend unreachable at {}: {}", url, e))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            warn!("mail {} refused by backend: {} {}", mail.kind(), status, text);
            return Err(format!("signup backend returned {}", status));
        }
        info!("📧 mail {} queued at the signup backend", mail.kind());
        Ok(())
    }
}
