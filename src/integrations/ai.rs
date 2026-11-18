use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use tracing::warn;

use crate::{db::ensure_user_settings, state::AppState};

pub async fn generate_ai_for_repo(
    state: &AppState,
    user_id: i64,
    repo_id: i64,
    include_diff: bool,
) {
    let _permit = match state.ai_semaphore.acquire().await {
        Ok(p) => p,
        Err(_) => return,
    };

    let (settings, full_name, status, description) = {
        let conn = match state.db() {
            Ok(c) => c,
            Err(_) => return,
        };
        let s = match ensure_user_settings(&conn, user_id) {
            Ok(st) => st,
            Err(_) => return,
        };
        let row: Option<(String, String, String)> = conn
            .query_row(
                "SELECT full_name, status, description FROM repos WHERE id = ?1 AND user_id = ?2",
                params![repo_id, user_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .unwrap_or(None);
        match row {
            Some((f, st, d)) => (s, f, st, d),
            None => return,
        }
    };

    if !settings.ai_enabled {
        if let Ok(conn) = state.db() {
            let _ = conn.execute(
                "UPDATE repos SET ai_status = 'none' WHERE id = ?1",
                params![repo_id],
            );
        }
        return;
    }

    let mut any_failed = false;

    // 1. README Summary
    if settings.ai_summarize_readme && (status == "discovery" || include_diff) {
        let readme_url = format!("https://api.github.com/repos/{}/readme", full_name);
        let mut req = state
            .http_github
            .get(&readme_url)
            .header("Accept", "application/vnd.github.raw+json");
        if !settings.github_pat.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", settings.github_pat));
        }

        let readme_text = match req.send().await {
            Ok(r) if r.status().is_success() => r.text().await.unwrap_or_default(),
            _ => description.clone(),
        };

        let truncated: String = readme_text.chars().take(8000).collect();
        if !truncated.trim().is_empty() {
            let prompt = format!(
                "Summarize this GitHub repository ({}) in 2-3 concise sentences focusing on what problem it solves, key features, and tech stack:\n\n{}",
                full_name, truncated
            );
            match call_openai_compatible(
                &state.http_ai,
                &settings.ai_base_url,
                &settings.ai_api_key,
                &settings.ai_model,
                &prompt,
            )
            .await
            {
                Ok(summary) => {
                    if let Ok(conn) = state.db() {
                        let _ = conn.execute(
                            "UPDATE repos SET readme_summary = ?1 WHERE id = ?2",
                            params![summary, repo_id],
                        );
                    }
                }
                Err(e) => {
                    warn!("AI README summary failed for {}: {}", full_name, e);
                    any_failed = true;
                }
            }
        }
    }

    // 2. Commit Diff Summary (for Tracked repositories, stripping noisy lockfiles)
    if settings.ai_summarize_commits && (status == "tracked" || include_diff) {
        let latest_sha: Option<String> = state.db().ok().and_then(|conn| {
            conn.query_row(
                "SELECT sha FROM commits WHERE repo_id = ?1 ORDER BY committed_at DESC LIMIT 1",
                params![repo_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap_or(None)
        });

        if let Some(sha) = latest_sha {
            let commit_url = format!("https://api.github.com/repos/{}/commits/{}", full_name, sha);
            let mut req = state
                .http_github
                .get(&commit_url)
                .header("Accept", "application/vnd.github+json");
            if !settings.github_pat.is_empty() {
                req = req.header("Authorization", format!("Bearer {}", settings.github_pat));
            }

            if let Ok(resp) = req.send().await {
                if let Ok(c_json) = resp.json::<Value>().await {
                    let mut diff_context = String::new();
                    let msg = c_json["commit"]["message"].as_str().unwrap_or("");
                    diff_context.push_str(&format!("Commit message: {}\n\n", msg));

                    let ignored_files = [
                        "Cargo.lock",
                        "package-lock.json",
                        "yarn.lock",
                        "pnpm-lock.yaml",
                        "poetry.lock",
                        "go.sum",
                        "composer.lock",
                    ];

                    if let Some(files) = c_json["files"].as_array() {
                        for f in files {
                            let filename = f["filename"].as_str().unwrap_or("");
                            if ignored_files.iter().any(|ig| filename.ends_with(ig))
                                || filename.ends_with(".min.js")
                                || filename.ends_with(".svg")
                            {
                                continue;
                            }
                            let patch: String = f["patch"]
                                .as_str()
                                .unwrap_or("")
                                .chars()
                                .take(1500)
                                .collect();
                            diff_context.push_str(&format!("File: {}\n{}\n\n", filename, patch));
                            if diff_context.len() > 10000 {
                                break;
                            }
                        }
                    }

                    let prompt = format!(
                        "Summarize the technical changes in this recent commit to {} in 2-3 clear bullet points:\n\n{}",
                        full_name, diff_context
                    );

                    match call_openai_compatible(
                        &state.http_ai,
                        &settings.ai_base_url,
                        &settings.ai_api_key,
                        &settings.ai_model,
                        &prompt,
                    )
                    .await
                    {
                        Ok(summary) => {
                            if let Ok(conn) = state.db() {
                                let _ = conn.execute(
                                    "UPDATE repos SET diff_summary = ?1 WHERE id = ?2",
                                    params![summary, repo_id],
                                );
                            }
                        }
                        Err(e) => {
                            warn!("AI Diff summary failed for {}: {}", full_name, e);
                            any_failed = true;
                        }
                    }
                }
            }
        }
    }

    if let Ok(conn) = state.db() {
        let final_status = if any_failed { "failed" } else { "completed" };
        let _ = conn.execute(
            "UPDATE repos SET ai_status = ?1 WHERE id = ?2",
            params![final_status, repo_id],
        );
    }
}

pub async fn call_openai_compatible(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    user_prompt: &str,
) -> Result<String, String> {
    let endpoint = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let mut req = client.post(&endpoint).json(&json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "You are a concise, technical developer assistant built into GitHub Radar. Provide direct, high-signal summaries without filler."
            },
            {
                "role": "user",
                "content": user_prompt
            }
        ],
        "temperature": 0.2,
        "max_tokens": 250
    }));

    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", api_key));
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("AI connection failed: {}", e))?;
    let status = resp.status();
    let body: Value = resp
        .json()
        .await
        .map_err(|e| format!("Invalid JSON from AI provider: {}", e))?;

    if !status.is_success() {
        return Err(format!("AI returned HTTP {}: {}", status, body));
    }

    body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "Empty completion response from AI".into())
}
