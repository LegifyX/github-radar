use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    api::auth::AuthCredentials, auth::AuthUser, db::ensure_user_settings, error::AppError,
    state::AppState,
};

// ============================================================================
// Admin User Management Panel
// ============================================================================

pub async fn api_admin_list_users(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    if !user.is_admin {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Admin access required".into(),
        ));
    }
    let conn = state.db()?;
    let mut stmt = conn.prepare(
        "SELECT id, username, is_admin, is_active, created_at FROM users ORDER BY id ASC",
    )?;
    let users: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "username": r.get::<_, String>(1)?,
                "is_admin": r.get::<_, i64>(2)? != 0,
                "is_active": r.get::<_, i64>(3)? != 0,
                "created_at": r.get::<_, String>(4)?
            }))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "users": users })))
}

/// Every admin endpoint starts with this.
fn require_admin(user: &AuthUser) -> Result<(), AppError> {
    if user.is_admin {
        Ok(())
    } else {
        Err(AppError::forbidden("Admin access required"))
    }
}

pub async fn api_admin_create_user(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<AuthCredentials>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;

    let uname = req.username.trim();
    if uname.is_empty() || req.password.len() < 6 {
        return Err(AppError::bad_request(
            "Username required and password must be >= 6 chars",
        ));
    }

    let conn = state.db()?;
    let taken: i64 = conn.query_row(
        "SELECT COUNT(*) FROM users WHERE username = ?1",
        params![uname],
        |r| r.get(0),
    )?;
    if taken > 0 {
        return Err(AppError(
            StatusCode::CONFLICT,
            "That username is already taken".into(),
        ));
    }

    let hash = bcrypt::hash(&req.password, bcrypt::DEFAULT_COST)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO users (username, password_hash, is_admin, is_active, created_at)
         VALUES (?1, ?2, 0, 1, ?3)",
        params![uname, hash, now],
    )?;
    let new_id = conn.last_insert_rowid();
    ensure_user_settings(&conn, new_id)?;
    Ok(Json(json!({ "ok": true, "id": new_id })))
}

#[derive(Deserialize)]
pub struct AdminUpdateUserInput {
    password: Option<String>,
    is_active: Option<bool>,
}

pub async fn api_admin_update_user(
    State(state): State<AppState>,
    user: AuthUser,
    Path(target_id): Path<i64>,
    Json(input): Json<AdminUpdateUserInput>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    let conn = state.db()?;

    if let Some(pw) = input.password {
        if pw.len() < 6 {
            return Err(AppError::bad_request("Password must be >= 6 characters"));
        }
        let hash = bcrypt::hash(&pw, bcrypt::DEFAULT_COST)?;
        let changed = conn.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            params![hash, target_id],
        )?;
        if changed == 0 {
            return Err(AppError::not_found("User not found"));
        }
        // A reset must invalidate existing sessions, otherwise a compromised
        // cookie keeps working after the password has been changed.
        conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1",
            params![target_id],
        )?;
    }

    if let Some(active) = input.is_active {
        if target_id == user.id && !active {
            return Err(AppError::bad_request(
                "Cannot deactivate your own admin account",
            ));
        }
        let changed = conn.execute(
            "UPDATE users SET is_active = ?1 WHERE id = ?2",
            params![if active { 1 } else { 0 }, target_id],
        )?;
        if changed == 0 {
            return Err(AppError::not_found("User not found"));
        }
        if !active {
            conn.execute(
                "DELETE FROM sessions WHERE user_id = ?1",
                params![target_id],
            )?;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

pub async fn api_admin_delete_user(
    State(state): State<AppState>,
    user: AuthUser,
    Path(target_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    require_admin(&user)?;
    if target_id == user.id {
        return Err(AppError::bad_request(
            "Cannot delete your own admin account",
        ));
    }
    let conn = state.db()?;
    let target_admin: i64 = conn
        .query_row(
            "SELECT is_admin FROM users WHERE id = ?1",
            params![target_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if target_admin != 0 {
        return Err(AppError::bad_request("Cannot delete an admin account"));
    }
    let changed = conn.execute("DELETE FROM users WHERE id = ?1", params![target_id])?;
    if changed == 0 {
        return Err(AppError::not_found("User not found"));
    }
    Ok(Json(json!({ "ok": true })))
}
