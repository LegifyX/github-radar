use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::AuthUser,
    db::ensure_user_settings,
    error::AppError,
    integrations::ai::generate_ai_for_repo,
    integrations::github::{
        fetch_and_sanitize_readme, parse_github_slug, update_rate_limit_from_headers,
    },
    models::CommitRecord,
    scanner::tracked::sync_single_tracked_repo,
    state::AppState,
};

#[derive(Deserialize)]
pub struct QuickTrackInput {
    repo_input: String,
}

pub async fn api_quick_track(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<QuickTrackInput>,
) -> Result<Json<Value>, AppError> {
    let slug = parse_github_slug(&input.repo_input).ok_or_else(|| {
        AppError(
            StatusCode::BAD_REQUEST,
            "Invalid GitHub repository URL or owner/repo format".into(),
        )
    })?;

    let settings = {
        let conn = state.db()?;
        ensure_user_settings(&conn, user.id)?
    };

    let api_url = format!("https://api.github.com/repos/{}", slug);
    let mut req = state
        .http_github
        .get(&api_url)
        .header("Accept", "application/vnd.github+json");
    if !settings.github_pat.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", settings.github_pat));
    }

    let resp = req.send().await?;
    update_rate_limit_from_headers(&state, user.id, resp.headers());

    if !resp.status().is_success() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("GitHub API returned {} for {}", resp.status(), slug),
        ));
    }

    let gh_repo: Value = resp.json().await?;
    let gh_id = gh_repo["id"].as_i64().unwrap_or(0);
    let full_name = gh_repo["full_name"].as_str().unwrap_or(&slug).to_string();
    let html_url = gh_repo["html_url"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("https://github.com/{}", full_name));
    let description = gh_repo["description"].as_str().unwrap_or("").to_string();
    let language = gh_repo["language"].as_str().unwrap_or("").to_string();
    let stars = gh_repo["stargazers_count"].as_i64().unwrap_or(0);
    let pushed_at = gh_repo["pushed_at"]
        .as_str()
        .unwrap_or(&Utc::now().to_rfc3339())
        .to_string();
    let default_branch = gh_repo["default_branch"]
        .as_str()
        .unwrap_or("main")
        .to_string();
    let topics: Vec<String> = gh_repo["topics"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let topics_json = serde_json::to_string(&topics).unwrap_or_else(|_| "[]".into());
    let now = Utc::now().to_rfc3339();

    let repo_db_id = {
        let mut conn = state.db()?;
        let tx = conn.transaction()?;

        tx.execute(
            "DELETE FROM deleted_tombstones WHERE user_id = ?1 AND github_repo_id = ?2",
            params![user.id, gh_id],
        )?;

        let max_order: i64 = tx
            .query_row(
                "SELECT COALESCE(MAX(sort_order), 0) FROM repos WHERE user_id = ?1 AND status = 'tracked'",
                params![user.id],
                |r| r.get(0),
            )
            .unwrap_or(0);

        tx.execute(
            "INSERT INTO repos (
                user_id, github_repo_id, full_name, html_url, description, language, stars,
                topics_json, pushed_at, default_branch, status, ignore_type, matched_query,
                discovered_at, sort_order, notify_webhook, is_missing
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'tracked', 'none', 'Quick-Track', ?11, ?12, 1, 0)
             ON CONFLICT(user_id, github_repo_id) DO UPDATE SET
                full_name = excluded.full_name,
                html_url = excluded.html_url,
                description = excluded.description,
                language = excluded.language,
                stars = excluded.stars,
                topics_json = excluded.topics_json,
                pushed_at = excluded.pushed_at,
                default_branch = excluded.default_branch,
                status = 'tracked',
                ignore_type = 'none',
                is_missing = 0",
            params![
                user.id,
                gh_id,
                full_name,
                html_url,
                description,
                language,
                stars,
                topics_json,
                pushed_at,
                default_branch,
                now,
                max_order + 1
            ],
        )?;

        let id: i64 = tx.query_row(
            "SELECT id FROM repos WHERE user_id = ?1 AND github_repo_id = ?2",
            params![user.id, gh_id],
            |r| r.get::<_, i64>(0),
        )?;
        tx.commit()?;
        id
    };

    // Spawn immediate baseline commit + README fetch
    let bg_state = state.clone();
    tokio::spawn(async move {
        if let Err(err) = sync_single_tracked_repo(&bg_state, user.id, repo_db_id, true).await {
            tracing::warn!("Baseline sync failed for repo {}: {}", repo_db_id, err);
        }
    });

    Ok(Json(json!({ "ok": true, "repo_id": repo_db_id })))
}

#[derive(Deserialize)]
pub struct RepoActionInput {
    action: String, // "track", "ignore", "unignore", "toggle_pin", "toggle_webhook", "clear_rename"
}

pub async fn api_repo_action(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
    Json(input): Json<RepoActionInput>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    match input.action.as_str() {
        "track" => {
            let max_order: i64 = conn
                .query_row(
                    "SELECT COALESCE(MAX(sort_order), 0) FROM repos WHERE user_id = ?1 AND status = 'tracked'",
                    params![user.id],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            conn.execute(
                "UPDATE repos SET status = 'tracked', ignore_type = 'none', is_missing = 0, sort_order = ?1 WHERE id = ?2 AND user_id = ?3",
                params![max_order + 1, id, user.id],
            )?;
            let bg_state = state.clone();
            tokio::spawn(async move {
                if let Err(err) = sync_single_tracked_repo(&bg_state, user.id, id, true).await {
                    tracing::warn!("Baseline sync failed for repo {}: {}", id, err);
                }
            });
        }
        "ignore" => {
            conn.execute(
                "UPDATE repos SET ignore_type = 'manual', status = 'discovery' WHERE id = ?1 AND user_id = ?2",
                params![id, user.id],
            )?;
        }
        "unignore" => {
            let now = Utc::now().to_rfc3339();
            conn.execute(
                "UPDATE repos SET ignore_type = 'none', status = 'discovery', discovered_at = ?1 WHERE id = ?2 AND user_id = ?3",
                params![now, id, user.id],
            )?;
        }
        "toggle_pin" => {
            conn.execute(
                "UPDATE repos SET is_pinned = CASE WHEN is_pinned = 1 THEN 0 ELSE 1 END WHERE id = ?1 AND user_id = ?2",
                params![id, user.id],
            )?;
        }
        "toggle_webhook" => {
            conn.execute(
                "UPDATE repos SET notify_webhook = CASE WHEN notify_webhook = 1 THEN 0 ELSE 1 END WHERE id = ?1 AND user_id = ?2",
                params![id, user.id],
            )?;
        }
        "clear_rename" => {
            conn.execute(
                "UPDATE repos SET was_renamed = 0, previous_name = NULL WHERE id = ?1 AND user_id = ?2",
                params![id, user.id],
            )?;
        }
        _ => return Err(AppError(StatusCode::BAD_REQUEST, "Unknown action".into())),
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct IgnoreBatchInput {
    repo_ids: Vec<i64>,
}

pub async fn api_ignore_batch(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<IgnoreBatchInput>,
) -> Result<Json<Value>, AppError> {
    let mut conn = state.db()?;
    let tx = conn.transaction()?;
    for rid in input.repo_ids {
        tx.execute(
            "UPDATE repos SET ignore_type = 'manual' WHERE id = ?1 AND user_id = ?2",
            params![rid, user.id],
        )?;
    }
    tx.commit()?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct ReorderInput {
    repo_id: i64,
    direction: String, // "up" | "down"
}

pub async fn api_reorder_repos(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<ReorderInput>,
) -> Result<Json<Value>, AppError> {
    let mut conn = state.db()?;
    let tx = conn.transaction()?;
    let target_pin: i64 = tx.query_row(
        "SELECT is_pinned FROM repos WHERE id = ?1 AND user_id = ?2",
        params![input.repo_id, user.id],
        |r| r.get(0),
    )?;

    let mut stmt = tx.prepare(
        "SELECT id FROM repos
         WHERE user_id = ?1 AND status = 'tracked' AND ignore_type = 'none' AND is_pinned = ?2
         ORDER BY sort_order ASC, discovered_at DESC",
    )?;
    let mut ids: Vec<i64> = stmt
        .query_map(params![user.id, target_pin], |r| r.get(0))?
        .filter_map(Result::ok)
        .collect();

    if let Some(pos) = ids.iter().position(|&x| x == input.repo_id) {
        if input.direction == "up" && pos > 0 {
            ids.swap(pos, pos - 1);
        } else if input.direction == "down" && pos + 1 < ids.len() {
            ids.swap(pos, pos + 1);
        }
    }

    drop(stmt);
    for (idx, rid) in ids.iter().enumerate() {
        tx.execute(
            "UPDATE repos SET sort_order = ?1 WHERE id = ?2 AND user_id = ?3",
            params![(idx + 1) as i64, rid, user.id],
        )?;
    }
    tx.commit()?;

    Ok(Json(json!({ "ok": true })))
}

pub async fn api_repo_details(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    let (readme_html, readme_updated_at, diff_summary, ai_status): (
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = conn.query_row(
        "SELECT readme_html, readme_updated_at, diff_summary, ai_status
         FROM repos WHERE id = ?1 AND user_id = ?2",
        params![id, user.id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;

    let mut c_stmt = conn.prepare(
        "SELECT id, repo_id, sha, branches_json, author, message, committed_at, html_url, is_read
         FROM commits WHERE repo_id = ?1 AND user_id = ?2
         ORDER BY committed_at DESC LIMIT 100",
    )?;
    let commits: Vec<CommitRecord> = c_stmt
        .query_map(params![id, user.id], |r| {
            let b_json: String = r.get(3)?;
            let branches: Vec<String> = serde_json::from_str(&b_json).unwrap_or_default();
            Ok(CommitRecord {
                id: r.get(0)?,
                repo_id: r.get(1)?,
                sha: r.get(2)?,
                branches,
                author: r.get(4)?,
                message: r.get(5)?,
                committed_at: r.get(6)?,
                html_url: r.get(7)?,
                is_read: r.get::<_, i64>(8)? != 0,
            })
        })?
        .filter_map(Result::ok)
        .collect();

    Ok(Json(json!({
        "readme_html": readme_html,
        "readme_updated_at": readme_updated_at,
        "diff_summary": diff_summary,
        "ai_status": ai_status,
        "commits": commits
    })))
}

pub async fn api_repo_mark_read(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let mut conn = state.db()?;
    let tx = conn.transaction()?;
    tx.execute(
        "UPDATE commits SET is_read = 1 WHERE repo_id = ?1 AND user_id = ?2",
        params![id, user.id],
    )?;
    tx.execute(
        "UPDATE repos SET unread_count = 0 WHERE id = ?1 AND user_id = ?2",
        params![id, user.id],
    )?;
    tx.commit()?;
    Ok(Json(json!({ "ok": true })))
}

pub async fn api_repo_refresh_readme(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let (full_name, default_branch, pat) = {
        let conn = state.db()?;
        let settings = ensure_user_settings(&conn, user.id)?;
        let (fnm, dbr): (String, String) = conn.query_row(
            "SELECT full_name, default_branch FROM repos WHERE id = ?1 AND user_id = ?2",
            params![id, user.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        (fnm, dbr, settings.github_pat)
    };

    if let Some(html) = fetch_and_sanitize_readme(&state, &full_name, &default_branch, &pat).await {
        let now = Utc::now().to_rfc3339();
        let conn = state.db()?;
        conn.execute(
            "UPDATE repos SET readme_html = ?1, readme_updated_at = ?2 WHERE id = ?3 AND user_id = ?4",
            params![html, now, id, user.id],
        )?;
        return Ok(Json(
            json!({ "ok": true, "readme_html": html, "readme_updated_at": now }),
        ));
    }

    Err(AppError::bad_gateway("Could not fetch README from GitHub"))
}

pub async fn api_repo_regenerate_ai(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    {
        let conn = state.db()?;
        conn.execute(
            "UPDATE repos SET ai_status = 'pending' WHERE id = ?1 AND user_id = ?2",
            params![id, user.id],
        )?;
    }

    let bg_state = state.clone();
    tokio::spawn(async move {
        generate_ai_for_repo(&bg_state, user.id, id, true).await;
    });

    Ok(Json(json!({ "ok": true })))
}

pub async fn api_delete_repo(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let mut conn = state.db()?;
    let tx = conn.transaction()?;
    if let Some(gh_id) = tx
        .query_row(
            "SELECT github_repo_id FROM repos WHERE id = ?1 AND user_id = ?2",
            params![id, user.id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR REPLACE INTO deleted_tombstones (user_id, github_repo_id, deleted_at)
             VALUES (?1, ?2, ?3)",
            params![user.id, gh_id, now],
        )?;
        tx.execute(
            "DELETE FROM repos WHERE id = ?1 AND user_id = ?2",
            params![id, user.id],
        )?;
    }
    tx.commit()?;
    Ok(Json(json!({ "ok": true })))
}
