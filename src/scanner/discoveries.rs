use std::time::Duration as StdDuration;

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, OptionalExtension};
use serde_json::Value;
use tracing::warn;

use crate::{
    db::ensure_user_settings, integrations::ai::generate_ai_for_repo,
    integrations::github::update_rate_limit_from_headers,
    integrations::webhook::send_webhook_payload, models::SearchQuery,
    scanner::tracked::sync_single_tracked_repo, state::AppState,
};

pub async fn execute_user_scan(state: &AppState, user_id: i64) -> Result<(), String> {
    let (settings, queries, tracked_ids) = {
        let conn = state.db().map_err(|e| e.1)?;
        let s = ensure_user_settings(&conn, user_id).map_err(|e| e.to_string())?;
        let mut q_stmt = conn
            .prepare(
                "SELECT id, query_text, min_stars, initial_lookback_days, notify_webhook, is_enabled, last_run_at
                 FROM search_queries WHERE user_id = ?1 AND is_enabled = 1",
            )
            .map_err(|e| e.to_string())?;
        let qs: Vec<SearchQuery> = q_stmt
            .query_map(params![user_id], |r| {
                Ok(SearchQuery {
                    id: r.get(0)?,
                    query_text: r.get(1)?,
                    min_stars: r.get(2)?,
                    initial_lookback_days: r.get(3)?,
                    notify_webhook: r.get::<_, i64>(4)? != 0,
                    is_enabled: r.get::<_, i64>(5)? != 0,
                    last_run_at: r.get(6)?,
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();

        let mut t_stmt = conn
            .prepare("SELECT id FROM repos WHERE user_id = ?1 AND status = 'tracked' AND ignore_type = 'none'")
            .map_err(|e| e.to_string())?;
        let tids: Vec<i64> = t_stmt
            .query_map(params![user_id], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();

        (s, qs, tids)
    };

    let mut newly_discovered_repo_ids = Vec::new();

    // 1. Process Search Queries (Discoveries)
    for query in queries {
        let mut full_q = query.query_text.trim().to_string();
        if query.min_stars > 0 && !full_q.contains("stars:") {
            full_q.push_str(&format!(" stars:>={}", query.min_stars));
        }

        if !full_q.contains("pushed:") {
            if let Some(ref last_run) = query.last_run_at {
                if let Ok(dt) = DateTime::parse_from_rfc3339(last_run) {
                    let overlap = (dt.with_timezone(&Utc) - Duration::minutes(10))
                        .format("%Y-%m-%dT%H:%M:%SZ");
                    full_q.push_str(&format!(" pushed:>{}", overlap));
                }
            } else {
                let lookback = (Utc::now() - Duration::days(query.initial_lookback_days.max(1)))
                    .format("%Y-%m-%d");
                full_q.push_str(&format!(" pushed:>{}", lookback));
            }
        }

        let per_page = settings.max_results_per_query.clamp(1, 100);
        let url = format!(
            "https://api.github.com/search/repositories?q={}&sort=updated&order=desc&per_page={}",
            urlencoding::encode(&full_q),
            per_page
        );

        let mut req = state
            .http_github
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if !settings.github_pat.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", settings.github_pat));
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                warn!("Search request error for query {}: {}", query.id, e);
                continue;
            }
        };

        update_rate_limit_from_headers(state, user_id, resp.headers());

        if resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::TOO_MANY_REQUESTS
        {
            return Err(
                "GitHub API rate limit reached. Add a GitHub PAT or wait for reset.".into(),
            );
        }
        if !resp.status().is_success() {
            continue;
        }

        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        let scan_timestamp = Utc::now().to_rfc3339();
        if let Some(items) = body["items"].as_array() {
            for item in items {
                let gh_id = item["id"].as_i64().unwrap_or(0);
                if gh_id == 0 {
                    continue;
                }
                let full_name = item["full_name"].as_str().unwrap_or("").to_string();
                let html_url = item["html_url"].as_str().unwrap_or("").to_string();
                let description = item["description"].as_str().unwrap_or("").to_string();
                let language = item["language"].as_str().unwrap_or("").to_string();
                let stars = item["stargazers_count"].as_i64().unwrap_or(0);
                let pushed_at = item["pushed_at"]
                    .as_str()
                    .unwrap_or(&scan_timestamp)
                    .to_string();
                let default_branch = item["default_branch"]
                    .as_str()
                    .unwrap_or("main")
                    .to_string();
                let topics: Vec<String> = item["topics"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let topics_json = serde_json::to_string(&topics).unwrap_or_else(|_| "[]".into());

                let inserted_id: Option<i64> = {
                    let conn = state.db().map_err(|e| e.1)?;

                    // A hard-deleted repo only comes back if it was pushed after the
                    // deletion. Compare parsed instants: GitHub returns `...Z` while we
                    // store `...+00:00`, and those two forms do not sort together as
                    // plain strings.
                    let tombstone_dt: Option<String> = conn
                        .query_row(
                            "SELECT deleted_at FROM deleted_tombstones WHERE user_id = ?1 AND github_repo_id = ?2",
                            params![user_id, gh_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .unwrap_or(None);

                    if let Some(del_at) = tombstone_dt {
                        if !pushed_after(&pushed_at, &del_at) {
                            continue;
                        }
                        let _ = conn.execute(
                            "DELETE FROM deleted_tombstones WHERE user_id = ?1 AND github_repo_id = ?2",
                            params![user_id, gh_id],
                        );
                    }

                    let existing: Option<i64> = conn
                        .query_row(
                            "SELECT id FROM repos WHERE user_id = ?1 AND github_repo_id = ?2",
                            params![user_id, gh_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .unwrap_or(None);

                    if let Some(ex_id) = existing {
                        let _ = conn.execute(
                            "UPDATE repos SET stars = ?1, pushed_at = ?2, topics_json = ?3, description = ?4 WHERE id = ?5",
                            params![stars, pushed_at, topics_json, description, ex_id],
                        );
                        None
                    } else {
                        let ai_initial = if settings.ai_enabled && settings.ai_summarize_readme {
                            "pending"
                        } else {
                            "none"
                        };
                        conn.execute(
                            "INSERT INTO repos (
                                user_id, github_repo_id, full_name, html_url, description, language, stars,
                                topics_json, pushed_at, default_branch, status, ignore_type, matched_query,
                                discovered_at, ai_status
                             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'discovery', 'none', ?11, ?12, ?13)",
                            params![
                                user_id,
                                gh_id,
                                full_name,
                                html_url,
                                description,
                                language,
                                stars,
                                topics_json,
                                pushed_at,
                                default_branch,
                                query.query_text,
                                scan_timestamp,
                                ai_initial
                            ],
                        )
                        .ok()
                        .map(|_| conn.last_insert_rowid())
                    }
                };

                if let Some(new_repo_id) = inserted_id {
                    newly_discovered_repo_ids.push(new_repo_id);

                    // Fire Discovery Webhook if enabled on this query
                    if query.notify_webhook && !settings.discovery_webhook_url.is_empty() {
                        let msg = settings
                            .discovery_webhook_template
                            .replace("{{query}}", &query.query_text)
                            .replace("{{repo}}", &full_name)
                            .replace("{{url}}", &html_url)
                            .replace("{{stars}}", &stars.to_string())
                            .replace(
                                "{{description}}",
                                if description.is_empty() {
                                    "No description provided."
                                } else {
                                    &description
                                },
                            );
                        let _ = send_webhook_payload(
                            &state.http_github,
                            &settings.discovery_webhook_url,
                            &msg,
                        )
                        .await;
                        tokio::time::sleep(StdDuration::from_millis(500)).await;
                    }
                }
            }
        }

        // Advance last_run_at cursor ONLY after successful HTTP 200 processing
        if let Ok(conn) = state.db() {
            let _ = conn.execute(
                "UPDATE search_queries SET last_run_at = ?1 WHERE id = ?2",
                params![scan_timestamp, query.id],
            );
        }

        tokio::time::sleep(StdDuration::from_millis(1200)).await;
    }

    // 2. Process Tracked Repositories (Multi-branch commits, Rename/404 detection, Webhooks)
    for repo_id in tracked_ids {
        let _ = sync_single_tracked_repo(state, user_id, repo_id, false).await;
        tokio::time::sleep(StdDuration::from_millis(800)).await;
    }

    // 3. Queue decoupled AI README summaries for new discoveries
    if settings.ai_enabled && settings.ai_summarize_readme {
        let ai_state = state.clone();
        tokio::spawn(async move {
            for rid in newly_discovered_repo_ids {
                generate_ai_for_repo(&ai_state, user_id, rid, false).await;
            }
        });
    }

    Ok(())
}

/// True when `pushed_at` is strictly newer than `deleted_at`.
///
/// Falls back to a string comparison only when one side is not RFC3339, so a
/// malformed GitHub timestamp can never silently un-delete a repo.
fn pushed_after(pushed_at: &str, deleted_at: &str) -> bool {
    match (
        DateTime::parse_from_rfc3339(pushed_at),
        DateTime::parse_from_rfc3339(deleted_at),
    ) {
        (Ok(pushed), Ok(deleted)) => pushed > deleted,
        _ => pushed_at > deleted_at,
    }
}

#[cfg(test)]
mod tests {
    use super::pushed_after;

    #[test]
    fn same_instant_is_not_newer() {
        assert!(!pushed_after(
            "2026-11-18T10:00:00Z",
            "2026-11-18T10:00:00+00:00"
        ));
    }

    #[test]
    fn sub_second_before_deletion_is_not_newer() {
        // The bug this guards: "…00Z" > "…00.123+00:00" as strings.
        assert!(!pushed_after(
            "2026-11-18T10:00:00Z",
            "2026-11-18T10:00:00.123+00:00"
        ));
    }

    #[test]
    fn push_after_deletion_resurrects() {
        assert!(pushed_after(
            "2026-11-18T10:00:01Z",
            "2026-11-18T10:00:00+00:00"
        ));
    }
}
