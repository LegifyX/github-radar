use axum::{extract::State, Json};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::{
    auth::AuthUser, db::ensure_user_settings, error::AppError, models::SearchQuery, state::AppState,
};

pub async fn api_export_backup(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    let settings = ensure_user_settings(&conn, user.id)?;

    let mut q_stmt = conn.prepare(
        "SELECT id, query_text, min_stars, initial_lookback_days, notify_webhook, is_enabled, last_run_at
         FROM search_queries WHERE user_id = ?1",
    )?;
    let queries: Vec<SearchQuery> = q_stmt
        .query_map(params![user.id], |r| {
            Ok(SearchQuery {
                id: r.get(0)?,
                query_text: r.get(1)?,
                min_stars: r.get(2)?,
                initial_lookback_days: r.get(3)?,
                notify_webhook: r.get::<_, i64>(4)? != 0,
                is_enabled: r.get::<_, i64>(5)? != 0,
                last_run_at: r.get(6)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();

    let mut r_stmt = conn.prepare(
        "SELECT github_repo_id, full_name, html_url, description, language, stars,
                topics_json, pushed_at, default_branch, status, ignore_type,
                is_pinned, sort_order, notify_webhook
         FROM repos WHERE user_id = ?1 AND (status = 'tracked' OR ignore_type = 'manual')",
    )?;
    let repos: Vec<Value> = r_stmt
        .query_map(params![user.id], |r| {
            Ok(json!({
                "github_repo_id": r.get::<_, i64>(0)?,
                "full_name": r.get::<_, String>(1)?,
                "html_url": r.get::<_, String>(2)?,
                "description": r.get::<_, String>(3)?,
                "language": r.get::<_, String>(4)?,
                "stars": r.get::<_, i64>(5)?,
                "topics_json": r.get::<_, String>(6)?,
                "pushed_at": r.get::<_, String>(7)?,
                "default_branch": r.get::<_, String>(8)?,
                "status": r.get::<_, String>(9)?,
                "ignore_type": r.get::<_, String>(10)?,
                "is_pinned": r.get::<_, i64>(11)?,
                "sort_order": r.get::<_, i64>(12)?,
                "notify_webhook": r.get::<_, i64>(13)?
            }))
        })?
        .filter_map(Result::ok)
        .collect();

    Ok(Json(json!({
        "version": "1.0",
        "exported_at": Utc::now().to_rfc3339(),
        "settings": {
            "refresh_frequency_minutes": settings.refresh_frequency_minutes,
            "auto_ignore_days": settings.auto_ignore_days,
            "max_results_per_query": settings.max_results_per_query,
            "discovery_webhook_url": settings.discovery_webhook_url,
            "discovery_webhook_template": settings.discovery_webhook_template,
            "tracked_webhook_url": settings.tracked_webhook_url,
            "tracked_webhook_template": settings.tracked_webhook_template,
            "ai_enabled": settings.ai_enabled,
            "ai_base_url": settings.ai_base_url,
            "ai_model": settings.ai_model,
            "ai_summarize_readme": settings.ai_summarize_readme,
            "ai_summarize_commits": settings.ai_summarize_commits
        },
        "queries": queries,
        "repos": repos
    })))
}

pub async fn api_import_backup(
    State(state): State<AppState>,
    user: AuthUser,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let mut conn = state.db()?;
    ensure_user_settings(&conn, user.id)?;
    let now = Utc::now().to_rfc3339();
    let tx = conn.transaction()?;

    if let Some(queries) = payload["queries"].as_array() {
        for q in queries {
            if let Some(qt) = q["query_text"].as_str() {
                let exists: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM search_queries WHERE user_id = ?1 AND query_text = ?2",
                        params![user.id, qt],
                        |r| r.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    tx.execute(
                        "INSERT INTO search_queries (user_id, query_text, min_stars, initial_lookback_days, notify_webhook, is_enabled, created_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                        params![
                            user.id,
                            qt,
                            q["min_stars"].as_i64().unwrap_or(0),
                            q["initial_lookback_days"].as_i64().unwrap_or(7),
                            if q["notify_webhook"].as_bool().unwrap_or(true) { 1 } else { 0 },
                            if q["is_enabled"].as_bool().unwrap_or(true) { 1 } else { 0 },
                            now
                        ],
                    )?;
                }
            }
        }
    }

    if let Some(repos) = payload["repos"].as_array() {
        for r in repos {
            let gh_id = r["github_repo_id"].as_i64().unwrap_or(0);
            let full_name = r["full_name"].as_str().unwrap_or("");
            if gh_id > 0 && !full_name.is_empty() {
                tx.execute(
                    "INSERT INTO repos (
                        user_id, github_repo_id, full_name, html_url, description, language, stars,
                        topics_json, pushed_at, default_branch, status, ignore_type, matched_query,
                        discovered_at, is_pinned, sort_order, notify_webhook
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 'Imported', ?13, ?14, ?15, ?16)
                     ON CONFLICT(user_id, github_repo_id) DO UPDATE SET
                        status = excluded.status,
                        ignore_type = excluded.ignore_type,
                        is_pinned = excluded.is_pinned,
                        sort_order = excluded.sort_order,
                        notify_webhook = excluded.notify_webhook",
                    params![
                        user.id,
                        gh_id,
                        full_name,
                        r["html_url"].as_str().unwrap_or(""),
                        r["description"].as_str().unwrap_or(""),
                        r["language"].as_str().unwrap_or(""),
                        r["stars"].as_i64().unwrap_or(0),
                        r["topics_json"].as_str().unwrap_or("[]"),
                        r["pushed_at"].as_str().unwrap_or(&now),
                        r["default_branch"].as_str().unwrap_or("main"),
                        r["status"].as_str().unwrap_or("tracked"),
                        r["ignore_type"].as_str().unwrap_or("none"),
                        now,
                        r["is_pinned"].as_i64().unwrap_or(0),
                        r["sort_order"].as_i64().unwrap_or(0),
                        r["notify_webhook"].as_i64().unwrap_or(1)
                    ],
                )?;
            }
        }
    }

    tx.commit()?;
    Ok(Json(json!({ "ok": true })))
}
