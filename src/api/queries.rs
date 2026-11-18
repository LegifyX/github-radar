use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{auth::AuthUser, error::AppError, state::AppState};

// ============================================================================
// Search Queries CRUD
// ============================================================================

#[derive(Deserialize)]
pub struct SearchQueryInput {
    query_text: String,
    min_stars: i64,
    initial_lookback_days: i64,
    notify_webhook: bool,
    is_enabled: Option<bool>,
}

pub async fn api_create_query(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<SearchQueryInput>,
) -> Result<Json<Value>, AppError> {
    let q = input.query_text.trim().to_string();
    if q.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Query text cannot be empty".into(),
        ));
    }
    let conn = state.db()?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO search_queries (user_id, query_text, min_stars, initial_lookback_days, notify_webhook, is_enabled, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            user.id,
            q,
            input.min_stars.max(0),
            input.initial_lookback_days.clamp(1, 365),
            if input.notify_webhook { 1 } else { 0 },
            if input.is_enabled.unwrap_or(true) { 1 } else { 0 },
            now
        ],
    )?;
    Ok(Json(json!({ "ok": true, "id": conn.last_insert_rowid() })))
}

pub async fn api_update_query(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<SearchQueryInput>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    conn.execute(
        "UPDATE search_queries
         SET query_text = ?1, min_stars = ?2, initial_lookback_days = ?3, notify_webhook = ?4, is_enabled = ?5
         WHERE id = ?6 AND user_id = ?7",
        params![
            input.query_text.trim(),
            input.min_stars.max(0),
            input.initial_lookback_days.clamp(1, 365),
            if input.notify_webhook { 1 } else { 0 },
            if input.is_enabled.unwrap_or(true) { 1 } else { 0 },
            id,
            user.id
        ],
    )?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn api_delete_query(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    conn.execute(
        "DELETE FROM search_queries WHERE id = ?1 AND user_id = ?2",
        params![id, user.id],
    )?;
    Ok(Json(json!({ "ok": true })))
}
