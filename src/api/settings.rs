use axum::{extract::State, http::StatusCode, Json};
use chrono::{Duration, Utc};
use rusqlite::params;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    auth::AuthUser, db::ensure_user_settings, error::AppError,
    integrations::ai::call_openai_compatible, integrations::github::update_rate_limit_from_headers,
    integrations::webhook::send_webhook_payload, state::AppState,
};

// ============================================================================
// Configuration, Testing & Backup Endpoints
// ============================================================================

#[derive(Deserialize)]
pub struct UpdateSettingsInput {
    /// Empty means "keep the stored PAT": the client never receives it.
    #[serde(default)]
    github_pat: String,
    #[serde(default)]
    clear_github_pat: bool,
    refresh_frequency_minutes: i64,
    auto_ignore_days: i64,
    max_results_per_query: i64,
    discovery_webhook_url: String,
    discovery_webhook_template: String,
    tracked_webhook_url: String,
    tracked_webhook_template: String,
    ai_enabled: bool,
    ai_base_url: String,
    /// Empty means "keep the stored key".
    #[serde(default)]
    ai_api_key: String,
    #[serde(default)]
    clear_ai_api_key: bool,
    ai_model: String,
    ai_summarize_readme: bool,
    ai_summarize_commits: bool,
}

pub async fn api_update_settings(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<UpdateSettingsInput>,
) -> Result<Json<Value>, AppError> {
    let freq = input.refresh_frequency_minutes.clamp(5, 1440);
    let next_scan = (Utc::now() + Duration::minutes(freq)).to_rfc3339();
    let conn = state.db()?;
    let current = ensure_user_settings(&conn, user.id)?;

    // An empty secret field means "leave it alone"; clearing requires the
    // explicit flag so a normal save can never wipe a configured token.
    let github_pat = match (input.clear_github_pat, input.github_pat.trim()) {
        (true, _) => String::new(),
        (false, "") => current.github_pat.clone(),
        (false, pat) => pat.to_string(),
    };
    let ai_api_key = match (input.clear_ai_api_key, input.ai_api_key.trim()) {
        (true, _) => String::new(),
        (false, "") => current.ai_api_key.clone(),
        (false, key) => key.to_string(),
    };

    conn.execute(
        "UPDATE user_settings SET
            github_pat = ?1,
            refresh_frequency_minutes = ?2,
            auto_ignore_days = ?3,
            max_results_per_query = ?4,
            discovery_webhook_url = ?5,
            discovery_webhook_template = ?6,
            tracked_webhook_url = ?7,
            tracked_webhook_template = ?8,
            ai_enabled = ?9,
            ai_base_url = ?10,
            ai_api_key = ?11,
            ai_model = ?12,
            ai_summarize_readme = ?13,
            ai_summarize_commits = ?14,
            next_scan_at = ?15
         WHERE user_id = ?16",
        params![
            github_pat,
            freq,
            input.auto_ignore_days.clamp(1, 365),
            input.max_results_per_query.clamp(1, 100),
            input.discovery_webhook_url.trim(),
            input.discovery_webhook_template,
            input.tracked_webhook_url.trim(),
            input.tracked_webhook_template,
            if input.ai_enabled { 1 } else { 0 },
            input.ai_base_url.trim().trim_end_matches('/'),
            ai_api_key,
            input.ai_model.trim(),
            if input.ai_summarize_readme { 1 } else { 0 },
            if input.ai_summarize_commits { 1 } else { 0 },
            next_scan,
            user.id
        ],
    )?;

    Ok(Json(json!({ "ok": true, "next_scan_at": next_scan })))
}

#[derive(Deserialize)]
pub struct TestGithubInput {
    github_pat: String,
}

pub async fn api_test_github(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<TestGithubInput>,
) -> Result<Json<Value>, AppError> {
    // A blank field means "test the stored token".
    let pat = if input.github_pat.trim().is_empty() {
        let conn = state.db()?;
        ensure_user_settings(&conn, user.id)?.github_pat
    } else {
        input.github_pat.trim().to_string()
    };

    let mut req = state
        .http_github
        .get("https://api.github.com/rate_limit")
        .header("Accept", "application/vnd.github+json");
    if !pat.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", pat));
    }

    let resp = req.send().await?;
    update_rate_limit_from_headers(&state, user.id, resp.headers());
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));

    if !status.is_success() {
        return Err(AppError::bad_request(format!(
            "GitHub API returned {}: {}",
            status,
            body["message"].as_str().unwrap_or("Invalid token")
        )));
    }

    let core = &body["resources"]["core"];
    let search = &body["resources"]["search"];
    Ok(Json(json!({
        "ok": true,
        "core_remaining": core["remaining"],
        "core_limit": core["limit"],
        "search_remaining": search["remaining"],
        "search_limit": search["limit"]
    })))
}

#[derive(Deserialize)]
pub struct TestWebhookInput {
    webhook_url: String,
    stream: String,
}

pub async fn api_test_webhook(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(input): Json<TestWebhookInput>,
) -> Result<Json<Value>, AppError> {
    let url = input.webhook_url.trim();
    if url.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Webhook URL is empty".into(),
        ));
    }
    let msg = format!(
        "✅ **GitHub Radar Test Notification** ({} stream) — Webhook is configured and working properly!",
        input.stream
    );
    send_webhook_payload(&state.http_github, url, &msg)
        .await
        .map_err(AppError::bad_gateway)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
pub struct TestAiInput {
    ai_base_url: String,
    ai_api_key: String,
    ai_model: String,
}

pub async fn api_test_ai(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<TestAiInput>,
) -> Result<Json<Value>, AppError> {
    // Blank key = use the stored one (the client never has it to resend).
    let api_key = if input.ai_api_key.trim().is_empty() {
        let conn = state.db()?;
        ensure_user_settings(&conn, user.id)?.ai_api_key
    } else {
        input.ai_api_key.trim().to_string()
    };

    let reply = call_openai_compatible(
        &state.http_ai,
        input.ai_base_url.trim(),
        &api_key,
        input.ai_model.trim(),
        "Reply with a single short sentence confirming you are online for GitHub Radar.",
    )
    .await
    .map_err(AppError::bad_gateway)?;

    Ok(Json(json!({ "ok": true, "reply": reply })))
}
