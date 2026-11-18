use axum::{extract::State, Json};
use chrono::{Duration, Utc};
use rusqlite::params;
use serde_json::{json, Value};

use crate::{
    auth::AuthUser,
    db::ensure_user_settings,
    error::AppError,
    models::{RepoRecord, SearchQuery},
    scanner::trigger_user_scan,
    state::AppState,
};

// ============================================================================
// Dashboard State & Retroactive SQL Windowing
// ============================================================================

pub async fn api_get_state(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    let settings = ensure_user_settings(&conn, user.id)?;

    // Fetch Search Queries
    let mut q_stmt = conn.prepare(
        "SELECT id, query_text, min_stars, initial_lookback_days, notify_webhook, is_enabled, last_run_at
         FROM search_queries WHERE user_id = ?1 ORDER BY id ASC",
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

    // Retroactive cutoff calculation
    let cutoff = (Utc::now() - Duration::days(settings.auto_ignore_days.max(1))).to_rfc3339();

    let mut r_stmt = conn.prepare(
        "SELECT id, github_repo_id, full_name, html_url, description, language, stars,
                topics_json, pushed_at, default_branch, status, ignore_type, matched_query,
                discovered_at, is_pinned, sort_order, notify_webhook, unread_count,
                was_renamed, previous_name, is_missing, readme_summary, diff_summary,
                ai_status, readme_html, readme_updated_at
         FROM repos WHERE user_id = ?1
         ORDER BY is_pinned DESC, sort_order ASC, discovered_at DESC",
    )?;

    let all_repos: Vec<RepoRecord> = r_stmt
        .query_map(params![user.id], |r| {
            let topics_str: String = r.get(7)?;
            let topics: Vec<String> = serde_json::from_str(&topics_str).unwrap_or_default();
            let status: String = r.get(10)?;
            let ignore_type: String = r.get(11)?;
            let discovered_at: String = r.get(13)?;
            let is_auto_expired =
                status == "discovery" && ignore_type == "none" && discovered_at <= cutoff;

            Ok(RepoRecord {
                id: r.get(0)?,
                github_repo_id: r.get(1)?,
                full_name: r.get(2)?,
                html_url: r.get(3)?,
                description: r.get(4)?,
                language: r.get(5)?,
                stars: r.get(6)?,
                topics,
                pushed_at: r.get(8)?,
                default_branch: r.get(9)?,
                status,
                ignore_type,
                matched_query: r.get(12)?,
                discovered_at,
                is_pinned: r.get::<_, i64>(14)? != 0,
                sort_order: r.get(15)?,
                notify_webhook: r.get::<_, i64>(16)? != 0,
                unread_count: r.get(17)?,
                was_renamed: r.get::<_, i64>(18)? != 0,
                previous_name: r.get(19)?,
                is_missing: r.get::<_, i64>(20)? != 0,
                readme_summary: r.get(21)?,
                diff_summary: r.get(22)?,
                ai_status: r.get(23)?,
                readme_html: None, // Loaded on-demand in Tab 2 detail endpoint to keep state payload fast
                readme_updated_at: r.get(25)?,
                is_auto_expired,
            })
        })?
        .filter_map(Result::ok)
        .collect();

    let mut discoveries = Vec::new();
    let mut tracked = Vec::new();
    let mut ignored = Vec::new();

    for repo in all_repos {
        if repo.status == "tracked" && repo.ignore_type == "none" {
            tracked.push(repo);
        } else if repo.ignore_type == "manual" || repo.is_auto_expired {
            ignored.push(repo);
        } else if repo.status == "discovery" && repo.ignore_type == "none" {
            discoveries.push(repo);
        }
    }

    // Sort discoveries newest first
    discoveries.sort_by(|a, b| b.pushed_at.cmp(&a.pushed_at));

    Ok(Json(json!({
        "user": user,
        "settings": settings,
        "queries": queries,
        "discoveries": discoveries,
        "tracked": tracked,
        "ignored": ignored
    })))
}

pub async fn api_force_scan(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    trigger_user_scan(state, user.id);
    Ok(Json(json!({ "ok": true, "message": "Scan triggered" })))
}
