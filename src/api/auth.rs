use std::time::{Duration as StdDuration, Instant};

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    Json,
};
use chrono::{Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    auth::{hash_password, verify_password, AuthUser},
    db::ensure_user_settings,
    error::AppError,
    state::AppState,
};

pub async fn api_auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    let user_count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;

    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut token_opt = None;
    for part in cookie_header.split(';') {
        if let Some(val) = part.trim().strip_prefix("radar_session=") {
            token_opt = Some(val.to_string());
            break;
        }
    }

    if let Some(token) = token_opt {
        let now_str = Utc::now().to_rfc3339();
        if let Some(u) = conn
            .query_row(
                "SELECT u.id, u.username, u.is_admin, s.expires_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token = ?1 AND s.expires_at > ?2 AND u.is_active = 1",
                params![token, now_str],
                |r| {
                    Ok(AuthUser {
                        id: r.get(0)?,
                        username: r.get(1)?,
                        is_admin: r.get::<_, i64>(2)? != 0,
                        session_expires_at: r.get(3)?,
                    })
                },
            )
            .optional()?
        {
            return Ok(Json(json!({
                "needs_setup": false,
                "authenticated": true,
                "user": u
            })));
        }
    }

    Ok(Json(json!({
        "needs_setup": user_count == 0,
        "authenticated": false
    })))
}

#[derive(Deserialize)]
pub struct AuthCredentials {
    pub username: String,
    pub password: String,
}

const LOGIN_MAX_FAILURES: u32 = 5;
const LOGIN_WINDOW: StdDuration = StdDuration::from_secs(15 * 60);

/// Case-insensitive key so `Admin` and `admin` share one failure budget.
fn login_key(username: &str) -> String {
    username.trim().to_lowercase()
}

fn login_blocked(state: &AppState, key: &str) -> bool {
    let failures = state
        .login_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    failures
        .get(key)
        .is_some_and(|(count, last)| *count >= LOGIN_MAX_FAILURES && last.elapsed() < LOGIN_WINDOW)
}

fn record_login_failure(state: &AppState, key: &str) {
    let mut failures = state
        .login_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    failures.retain(|_, (_, last)| last.elapsed() < LOGIN_WINDOW);
    let entry = failures
        .entry(key.to_owned())
        .or_insert((0, Instant::now()));
    entry.0 += 1;
    entry.1 = Instant::now();
}

fn clear_login_failures(state: &AppState, key: &str) {
    state
        .login_failures
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(key);
}

/// `Secure` is only sent when the deployment is actually served over HTTPS
/// (`COOKIE_SECURE=1`), so plain-HTTP LAN installs keep working.
fn secure_flag(secure: bool) -> &'static str {
    if secure {
        "; Secure"
    } else {
        ""
    }
}

fn create_session_cookie(
    conn: &Connection,
    user_id: i64,
    secure: bool,
) -> Result<HeaderMap, AppError> {
    let token = Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + Duration::days(30)).to_rfc3339();
    conn.execute(
        "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, ?2, ?3)",
        params![token, user_id, expires_at],
    )?;

    let mut headers = HeaderMap::new();
    let cookie = format!(
        "radar_session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000{}",
        token,
        secure_flag(secure)
    );
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());
    Ok(headers)
}

pub async fn api_auth_setup(
    State(state): State<AppState>,
    Json(req): Json<AuthCredentials>,
) -> Result<(HeaderMap, Json<Value>), AppError> {
    let username = req.username.trim().to_string();
    if username.is_empty() || req.password.len() < 6 {
        return Err(AppError::bad_request(
            "Username required and password must be at least 6 characters",
        ));
    }

    let conn = state.db()?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if count > 0 {
        return Err(AppError::forbidden("Initial admin setup already completed"));
    }

    let hash = hash_password(req.password).await?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO users (username, password_hash, is_admin, is_active, created_at)
         VALUES (?1, ?2, 1, 1, ?3)",
        params![username, hash, now],
    )?;
    let user_id = conn.last_insert_rowid();
    ensure_user_settings(&conn, user_id)?;

    let headers = create_session_cookie(&conn, user_id, state.cookie_secure)?;
    Ok((headers, Json(json!({ "ok": true }))))
}

pub async fn api_auth_login(
    State(state): State<AppState>,
    Json(req): Json<AuthCredentials>,
) -> Result<(HeaderMap, Json<Value>), AppError> {
    let key = login_key(&req.username);
    if login_blocked(&state, &key) {
        return Err(AppError(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed sign-in attempts. Try again in 15 minutes.".into(),
        ));
    }

    let conn = state.db()?;
    let row: Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT id, password_hash, is_active FROM users WHERE username = ?1",
            params![req.username.trim()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let Some((user_id, hash, is_active)) = row else {
        record_login_failure(&state, &key);
        return Err(AppError::unauthorized("Invalid username or password"));
    };

    if is_active == 0 {
        record_login_failure(&state, &key);
        return Err(AppError::forbidden(
            "Account has been disabled by an administrator",
        ));
    }

    if !verify_password(req.password, hash).await {
        record_login_failure(&state, &key);
        return Err(AppError::unauthorized("Invalid username or password"));
    }

    clear_login_failures(&state, &key);
    ensure_user_settings(&conn, user_id)?;
    let headers = create_session_cookie(&conn, user_id, state.cookie_secure)?;
    Ok((headers, Json(json!({ "ok": true }))))
}

pub async fn api_auth_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(HeaderMap, Json<Value>), AppError> {
    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for part in cookie_header.split(';') {
        if let Some(token) = part.trim().strip_prefix("radar_session=") {
            if let Ok(conn) = state.db() {
                let _ = conn.execute("DELETE FROM sessions WHERE token = ?1", params![token]);
            }
        }
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        header::SET_COOKIE,
        format!(
            "radar_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{}",
            secure_flag(state.cookie_secure)
        )
        .parse()
        .unwrap(),
    );
    Ok((resp_headers, Json(json!({ "ok": true }))))
}
