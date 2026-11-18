use axum::http::StatusCode;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use tracing::warn;

use crate::{
    db::ensure_user_settings,
    integrations::ai::generate_ai_for_repo,
    integrations::github::{fetch_and_sanitize_readme, update_rate_limit_from_headers},
    integrations::webhook::send_webhook_payload,
    state::AppState,
};

pub async fn sync_single_tracked_repo(
    state: &AppState,
    user_id: i64,
    repo_id: i64,
    is_initial_baseline: bool,
) -> Result<(), String> {
    let (settings, stored_full_name, notify_webhook, has_readme) = {
        let conn = state.db().map_err(|e| e.1)?;
        let s = ensure_user_settings(&conn, user_id).map_err(|e| e.to_string())?;
        let row: (String, i64, Option<String>) = conn
            .query_row(
                "SELECT full_name, notify_webhook, readme_html FROM repos WHERE id = ?1 AND user_id = ?2",
                params![repo_id, user_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| e.to_string())?;
        (s, row.0, row.1 != 0, row.2.is_some())
    };

    let repo_url = format!("https://api.github.com/repos/{}", stored_full_name);
    let mut req = state
        .http_github
        .get(&repo_url)
        .header("Accept", "application/vnd.github+json");
    if !settings.github_pat.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", settings.github_pat));
    }

    let resp = req.send().await.map_err(|e| e.to_string())?;
    update_rate_limit_from_headers(state, user_id, resp.headers());

    if resp.status() == StatusCode::NOT_FOUND || resp.status() == StatusCode::GONE {
        if let Ok(conn) = state.db() {
            let _ = conn.execute(
                "UPDATE repos SET is_missing = 1 WHERE id = ?1 AND user_id = ?2",
                params![repo_id, user_id],
            );
        }
        return Ok(());
    }

    if !resp.status().is_success() {
        return Err(format!(
            "GitHub returned {} for {}",
            resp.status(),
            stored_full_name
        ));
    }

    let gh_repo: Value = resp.json().await.map_err(|e| e.to_string())?;
    let current_full_name = gh_repo["full_name"]
        .as_str()
        .unwrap_or(&stored_full_name)
        .to_string();
    let html_url = gh_repo["html_url"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("https://github.com/{}", current_full_name));
    let stars = gh_repo["stargazers_count"].as_i64().unwrap_or(0);
    let pushed_at = gh_repo["pushed_at"].as_str().unwrap_or("").to_string();
    let default_branch = gh_repo["default_branch"]
        .as_str()
        .unwrap_or("main")
        .to_string();

    // Detect Repository Rename (301 redirect followed by reqwest -> full_name changed)
    {
        let conn = state.db().map_err(|e| e.1)?;
        if current_full_name.to_lowercase() != stored_full_name.to_lowercase() {
            let _ = conn.execute(
                "UPDATE repos SET full_name = ?1, html_url = ?2, was_renamed = 1, previous_name = ?3,
                 stars = ?4, pushed_at = ?5, default_branch = ?6, is_missing = 0
                 WHERE id = ?7 AND user_id = ?8",
                params![
                    current_full_name,
                    html_url,
                    stored_full_name,
                    stars,
                    pushed_at,
                    default_branch,
                    repo_id,
                    user_id
                ],
            );
        } else {
            let _ = conn.execute(
                "UPDATE repos SET stars = ?1, pushed_at = ?2, default_branch = ?3, is_missing = 0
                 WHERE id = ?4 AND user_id = ?5",
                params![stars, pushed_at, default_branch, repo_id, user_id],
            );
        }
    }

    // Fetch active branches (capped to 5 branches to protect rate limit, always prioritizing default_branch)
    let branches_url = format!(
        "https://api.github.com/repos/{}/branches?per_page=15",
        current_full_name
    );
    let mut b_req = state
        .http_github
        .get(&branches_url)
        .header("Accept", "application/vnd.github+json");
    if !settings.github_pat.is_empty() {
        b_req = b_req.header("Authorization", format!("Bearer {}", settings.github_pat));
    }

    let mut branch_names = vec![default_branch.clone()];
    match b_req.send().await {
        Ok(b_resp) if b_resp.status().is_success() => {
            update_rate_limit_from_headers(state, user_id, b_resp.headers());
            if let Ok(b_arr) = b_resp.json::<Value>().await {
                if let Some(arr) = b_arr.as_array() {
                    for b in arr {
                        if let Some(name) = b["name"].as_str() {
                            if !branch_names.contains(&name.to_string()) && branch_names.len() < 5 {
                                branch_names.push(name.to_string());
                            }
                        }
                    }
                }
            }
        }
        Ok(b_resp) => warn!(
            "Branch list for {} returned {}; only the default branch will be scanned",
            current_full_name,
            b_resp.status()
        ),
        Err(err) => warn!("Branch list for {} failed: {}", current_full_name, err),
    }

    // Fetch recent commits across active branches & deduplicate by SHA
    let mut new_commits_for_webhook: Vec<(String, String, String, String, String)> = Vec::new();

    for branch in &branch_names {
        let commits_url = format!(
            "https://api.github.com/repos/{}/commits?sha={}&per_page=10",
            current_full_name,
            urlencoding::encode(branch)
        );
        let mut c_req = state
            .http_github
            .get(&commits_url)
            .header("Accept", "application/vnd.github+json");
        if !settings.github_pat.is_empty() {
            c_req = c_req.header("Authorization", format!("Bearer {}", settings.github_pat));
        }

        let c_resp = match c_req.send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                warn!(
                    "Commit fetch for {}@{} returned {}; skipping branch",
                    current_full_name,
                    branch,
                    r.status()
                );
                continue;
            }
            Err(err) => {
                warn!(
                    "Commit fetch for {}@{} failed: {}",
                    current_full_name, branch, err
                );
                continue;
            }
        };
        update_rate_limit_from_headers(state, user_id, c_resp.headers());

        let c_list: Value = match c_resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let Some(arr) = c_list.as_array() else {
            continue;
        };

        // One transaction per branch: the naive version needed a write
        // transaction per commit.
        let mut conn = state.db().map_err(|e| e.1)?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;

        for c in arr {
            let sha = c["sha"].as_str().unwrap_or("").to_string();
            if sha.is_empty() {
                continue;
            }
            let author = c["commit"]["author"]["name"]
                .as_str()
                .or_else(|| c["author"]["login"].as_str())
                .unwrap_or("unknown")
                .to_string();
            let message = c["commit"]["message"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .to_string();
            let committed_at = c["commit"]["author"]["date"]
                .as_str()
                .unwrap_or(&Utc::now().to_rfc3339())
                .to_string();
            let commit_url = c["html_url"].as_str().unwrap_or("").to_string();

            let existing_branches: Option<String> = tx
                .query_row(
                    "SELECT branches_json FROM commits WHERE user_id = ?1 AND repo_id = ?2 AND sha = ?3",
                    params![user_id, repo_id, sha],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;

            if let Some(b_json) = existing_branches {
                let mut b_vec: Vec<String> = serde_json::from_str(&b_json).unwrap_or_default();
                if !b_vec.contains(branch) {
                    b_vec.push(branch.clone());
                    let updated_json =
                        serde_json::to_string(&b_vec).unwrap_or_else(|_| "[]".into());
                    tx.execute(
                        "UPDATE commits SET branches_json = ?1 WHERE user_id = ?2 AND repo_id = ?3 AND sha = ?4",
                        params![updated_json, user_id, repo_id, sha],
                    )
                    .map_err(|e| e.to_string())?;
                }
            } else {
                let b_json =
                    serde_json::to_string(&vec![branch.clone()]).unwrap_or_else(|_| "[]".into());
                let is_read_val = if is_initial_baseline { 1 } else { 0 };
                tx.execute(
                    "INSERT INTO commits (user_id, repo_id, sha, branches_json, author, message, committed_at, html_url, is_read)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        user_id,
                        repo_id,
                        sha,
                        b_json,
                        author,
                        message,
                        committed_at,
                        commit_url,
                        is_read_val
                    ],
                )
                .map_err(|e| e.to_string())?;

                if !is_initial_baseline {
                    new_commits_for_webhook.push((
                        sha,
                        branch.clone(),
                        author,
                        message,
                        committed_at,
                    ));
                }
            }
        }

        tx.commit().map_err(|e| e.to_string())?;
    }

    // Update unread count on repo
    {
        let mut conn = state.db().map_err(|e| e.1)?;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let unread: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM commits WHERE repo_id = ?1 AND user_id = ?2 AND is_read = 0",
                params![repo_id, user_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        tx.execute(
            "UPDATE repos SET unread_count = ?1 WHERE id = ?2 AND user_id = ?3",
            params![unread, repo_id, user_id],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
    }

    // Fetch & cache README if missing or if new commits arrived
    if !has_readme || !new_commits_for_webhook.is_empty() || is_initial_baseline {
        if let Some(html) = fetch_and_sanitize_readme(
            state,
            &current_full_name,
            &default_branch,
            &settings.github_pat,
        )
        .await
        {
            let now = Utc::now().to_rfc3339();
            if let Ok(conn) = state.db() {
                let _ = conn.execute(
                    "UPDATE repos SET readme_html = ?1, readme_updated_at = ?2 WHERE id = ?3",
                    params![html, now, repo_id],
                );
            }
        }
    }

    // If new commits arrived (or initial baseline with AI enabled), generate AI diff summary and fire Tracked Webhook
    if !new_commits_for_webhook.is_empty() || (is_initial_baseline && settings.ai_enabled) {
        let mut diff_summary_text = String::from("AI summary disabled or unavailable.");
        if settings.ai_enabled && settings.ai_summarize_commits {
            if let Ok(conn) = state.db() {
                let _ = conn.execute(
                    "UPDATE repos SET ai_status = 'pending' WHERE id = ?1",
                    params![repo_id],
                );
            }
            generate_ai_for_repo(state, user_id, repo_id, true).await;
            if let Ok(conn) = state.db() {
                diff_summary_text = conn
                    .query_row(
                        "SELECT COALESCE(diff_summary, 'No summary available.') FROM repos WHERE id = ?1",
                        params![repo_id],
                        |r| r.get(0),
                    )
                    .unwrap_or_else(|_| "No summary available.".into());
            }
        }

        if !is_initial_baseline
            && !new_commits_for_webhook.is_empty()
            && notify_webhook
            && !settings.tracked_webhook_url.is_empty()
        {
            let first = &new_commits_for_webhook[0];
            let commits_bullet_list: String = new_commits_for_webhook
                .iter()
                .take(8)
                .map(|(sha, br, auth, msg, _)| {
                    format!(
                        "• `{}` (`{}`) {} — _{}_",
                        &sha[..7.min(sha.len())],
                        br,
                        msg,
                        auth
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");

            let rendered = settings
                .tracked_webhook_template
                .replace("{{repo}}", &current_full_name)
                .replace("{{branch}}", &first.1)
                .replace("{{author}}", &first.2)
                .replace("{{date}}", &first.4)
                .replace("{{url}}", &html_url)
                .replace("{{commits}}", &commits_bullet_list)
                .replace("{{ai_summary}}", &diff_summary_text);

            let _ =
                send_webhook_payload(&state.http_github, &settings.tracked_webhook_url, &rendered)
                    .await;
        }
    }

    Ok(())
}
