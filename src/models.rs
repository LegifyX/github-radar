use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct UserSettings {
    /// Server-side only. Never serialized back to the client.
    #[serde(skip_serializing)]
    pub github_pat: String,
    pub refresh_frequency_minutes: i64,
    pub auto_ignore_days: i64,
    pub max_results_per_query: i64,
    pub discovery_webhook_url: String,
    pub discovery_webhook_template: String,
    pub tracked_webhook_url: String,
    pub tracked_webhook_template: String,
    pub ai_enabled: bool,
    pub ai_base_url: String,
    /// Server-side only. Never serialized back to the client.
    #[serde(skip_serializing)]
    pub ai_api_key: String,
    pub ai_model: String,
    pub ai_summarize_readme: bool,
    pub ai_summarize_commits: bool,
    pub last_scan_at: Option<String>,
    pub next_scan_at: Option<String>,
    pub is_scanning: bool,
    pub last_error: Option<String>,
    pub rate_limit_remaining: Option<i64>,
    pub rate_limit_limit: Option<i64>,
    pub rate_limit_reset: Option<String>,
    /// Derived: whether a PAT / AI key is stored, plus a masked hint for the UI.
    pub github_pat_set: bool,
    pub github_pat_hint: String,
    pub ai_api_key_set: bool,
    pub ai_api_key_hint: String,
}

impl Default for UserSettings {
    fn default() -> Self {
        Self {
            github_pat: String::new(),
            refresh_frequency_minutes: 30,
            auto_ignore_days: 7,
            max_results_per_query: 15,
            discovery_webhook_url: String::new(),
            discovery_webhook_template: "🔍 **New Discovery** via `{{query}}`\n**[{{repo}}]({{url}})** (⭐ {{stars}})\n{{description}}".into(),
            tracked_webhook_url: String::new(),
            tracked_webhook_template: "🚀 **Tracked Repo Update:** **[{{repo}}]({{url}})** (`{{branch}}`)\n**Author:** {{author}} ({{date}})\n**Commits:**\n{{commits}}\n\n**AI Summary:** {{ai_summary}}".into(),
            ai_enabled: false,
            ai_base_url: "http://localhost:11434/v1".into(),
            ai_api_key: String::new(),
            ai_model: "gemma2:2b".into(),
            ai_summarize_readme: true,
            ai_summarize_commits: true,
            last_scan_at: None,
            next_scan_at: None,
            is_scanning: false,
            last_error: None,
            rate_limit_remaining: None,
            rate_limit_limit: None,
            rate_limit_reset: None,
            github_pat_set: false,
            github_pat_hint: String::new(),
            ai_api_key_set: false,
            ai_api_key_hint: String::new(),
        }
    }
}

/// `"…a1b2"` for a stored secret, never the whole value.
pub fn secret_hint(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    match chars.len() {
        0 => String::new(),
        n if n <= 8 => "set".to_string(),
        n => format!("…{}", chars[n - 4..].iter().collect::<String>()),
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SearchQuery {
    pub id: i64,
    pub query_text: String,
    pub min_stars: i64,
    pub initial_lookback_days: i64,
    pub notify_webhook: bool,
    pub is_enabled: bool,
    pub last_run_at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct RepoRecord {
    pub id: i64,
    pub github_repo_id: i64,
    pub full_name: String,
    pub html_url: String,
    pub description: String,
    pub language: String,
    pub stars: i64,
    pub topics: Vec<String>,
    pub pushed_at: String,
    pub default_branch: String,
    pub status: String,
    pub ignore_type: String,
    pub matched_query: String,
    pub discovered_at: String,
    pub is_pinned: bool,
    pub sort_order: i64,
    pub notify_webhook: bool,
    pub unread_count: i64,
    pub was_renamed: bool,
    pub previous_name: Option<String>,
    pub is_missing: bool,
    pub readme_summary: Option<String>,
    pub diff_summary: Option<String>,
    pub ai_status: String,
    pub readme_html: Option<String>,
    pub readme_updated_at: Option<String>,
    pub is_auto_expired: bool,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct CommitRecord {
    pub id: i64,
    pub repo_id: i64,
    pub sha: String,
    pub branches: Vec<String>,
    pub author: String,
    pub message: String,
    pub committed_at: String,
    pub html_url: String,
    pub is_read: bool,
}
