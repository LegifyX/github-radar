use axum::{
    extract::FromRequestParts,
    http::{header, request::Parts, StatusCode},
};
use chrono::{Duration, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{error::AppError, state::AppState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub id: i64,
    pub username: String,
    pub is_admin: bool,
    pub session_expires_at: String,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cookie_header = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let mut session_token = None;
        for part in cookie_header.split(';') {
            let trimmed = part.trim();
            if let Some(val) = trimmed.strip_prefix("radar_session=") {
                session_token = Some(val.to_string());
                break;
            }
        }

        let token = session_token
            .ok_or_else(|| AppError(StatusCode::UNAUTHORIZED, "Authentication required".into()))?;

        let conn = state.db()?;
        let now_str = Utc::now().to_rfc3339();

        let user = conn
            .query_row(
                "SELECT u.id, u.username, u.is_admin, s.expires_at
                 FROM sessions s
                 JOIN users u ON u.id = s.user_id
                 WHERE s.token = ?1 AND s.expires_at > ?2 AND u.is_active = 1",
                params![token, now_str],
                |row| {
                    Ok(AuthUser {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        is_admin: row.get::<_, i64>(2)? != 0,
                        session_expires_at: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| {
                AppError(
                    StatusCode::UNAUTHORIZED,
                    "Session expired or invalid".into(),
                )
            })?;

        // Sliding 30-day session extension
        let new_exp = (Utc::now() + Duration::days(30)).to_rfc3339();
        let _ = conn.execute(
            "UPDATE sessions SET expires_at = ?1 WHERE token = ?2",
            params![new_exp, token],
        );

        Ok(user)
    }
}

/// bcrypt is deliberately slow (~250 ms at the default cost), so it runs on the
/// blocking pool instead of stalling a Tokio worker.
pub async fn hash_password(password: String) -> Result<String, AppError> {
    tokio::task::spawn_blocking(move || bcrypt::hash(&password, bcrypt::DEFAULT_COST))
        .await
        .map_err(|err| {
            AppError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Password hashing failed: {err}"),
            )
        })?
        .map_err(AppError::from)
}

pub async fn verify_password(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || bcrypt::verify(&password, &hash).unwrap_or(false))
        .await
        .unwrap_or(false)
}
