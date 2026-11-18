use chrono::{Duration, Utc};
use rusqlite::{params, Connection};

use crate::models::{secret_hint, UserSettings};

pub fn init_db(conn: &Connection) -> Result<(), rusqlite::Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS users (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            username TEXT NOT NULL UNIQUE,
            password_hash TEXT NOT NULL,
            is_admin INTEGER NOT NULL DEFAULT 0,
            is_active INTEGER NOT NULL DEFAULT 1,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sessions (
            token TEXT PRIMARY KEY,
            user_id INTEGER NOT NULL,
            expires_at TEXT NOT NULL,
            FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS user_settings (
            user_id INTEGER PRIMARY KEY,
            github_pat TEXT NOT NULL DEFAULT '',
            refresh_frequency_minutes INTEGER NOT NULL DEFAULT 30,
            auto_ignore_days INTEGER NOT NULL DEFAULT 7,
            max_results_per_query INTEGER NOT NULL DEFAULT 15,
            discovery_webhook_url TEXT NOT NULL DEFAULT '',
            discovery_webhook_template TEXT NOT NULL DEFAULT '🔍 **New Discovery** via `{{query}}`\n**[{{repo}}]({{url}})** (⭐ {{stars}})\n{{description}}',
            tracked_webhook_url TEXT NOT NULL DEFAULT '',
            tracked_webhook_template TEXT NOT NULL DEFAULT '🚀 **Tracked Repo Update:** **[{{repo}}]({{url}})** (`{{branch}}`)\n**Author:** {{author}} ({{date}})\n**Commits:**\n{{commits}}\n\n**AI Summary:** {{ai_summary}}',
            ai_enabled INTEGER NOT NULL DEFAULT 0,
            ai_base_url TEXT NOT NULL DEFAULT 'http://localhost:11434/v1',
            ai_api_key TEXT NOT NULL DEFAULT '',
            ai_model TEXT NOT NULL DEFAULT 'gemma2:2b',
            ai_summarize_readme INTEGER NOT NULL DEFAULT 1,
            ai_summarize_commits INTEGER NOT NULL DEFAULT 1,
            last_scan_at TEXT,
            next_scan_at TEXT,
            is_scanning INTEGER NOT NULL DEFAULT 0,
            last_error TEXT,
            rate_limit_remaining INTEGER,
            rate_limit_limit INTEGER,
            rate_limit_reset TEXT,
            FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS search_queries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            query_text TEXT NOT NULL,
            min_stars INTEGER NOT NULL DEFAULT 0,
            initial_lookback_days INTEGER NOT NULL DEFAULT 7,
            notify_webhook INTEGER NOT NULL DEFAULT 1,
            is_enabled INTEGER NOT NULL DEFAULT 1,
            last_run_at TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS repos (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            github_repo_id INTEGER NOT NULL,
            full_name TEXT NOT NULL,
            html_url TEXT NOT NULL,
            description TEXT NOT NULL DEFAULT '',
            language TEXT NOT NULL DEFAULT '',
            stars INTEGER NOT NULL DEFAULT 0,
            topics_json TEXT NOT NULL DEFAULT '[]',
            pushed_at TEXT NOT NULL DEFAULT '',
            default_branch TEXT NOT NULL DEFAULT 'main',
            status TEXT NOT NULL DEFAULT 'discovery',
            ignore_type TEXT NOT NULL DEFAULT 'none',
            matched_query TEXT NOT NULL DEFAULT '',
            discovered_at TEXT NOT NULL,
            is_pinned INTEGER NOT NULL DEFAULT 0,
            sort_order INTEGER NOT NULL DEFAULT 0,
            notify_webhook INTEGER NOT NULL DEFAULT 1,
            unread_count INTEGER NOT NULL DEFAULT 0,
            was_renamed INTEGER NOT NULL DEFAULT 0,
            previous_name TEXT,
            is_missing INTEGER NOT NULL DEFAULT 0,
            readme_summary TEXT,
            diff_summary TEXT,
            ai_status TEXT NOT NULL DEFAULT 'none',
            readme_html TEXT,
            readme_updated_at TEXT,
            UNIQUE(user_id, github_repo_id),
            FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS commits (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            user_id INTEGER NOT NULL,
            repo_id INTEGER NOT NULL,
            sha TEXT NOT NULL,
            branches_json TEXT NOT NULL DEFAULT '[]',
            author TEXT NOT NULL DEFAULT '',
            message TEXT NOT NULL DEFAULT '',
            committed_at TEXT NOT NULL,
            html_url TEXT NOT NULL DEFAULT '',
            is_read INTEGER NOT NULL DEFAULT 0,
            UNIQUE(user_id, repo_id, sha),
            FOREIGN KEY(repo_id) REFERENCES repos(id) ON DELETE CASCADE
        );

        CREATE TABLE IF NOT EXISTS deleted_tombstones (
            user_id INTEGER NOT NULL,
            github_repo_id INTEGER NOT NULL,
            deleted_at TEXT NOT NULL,
            PRIMARY KEY(user_id, github_repo_id),
            FOREIGN KEY(user_id) REFERENCES users(id) ON DELETE CASCADE
        );

        -- Boot recovery sweep: clear any stale locks or pending AI statuses after container restart
        UPDATE user_settings SET is_scanning = 0;
        UPDATE repos SET ai_status = 'none' WHERE ai_status = 'pending';
        ",
    )?;
    Ok(())
}

pub fn ensure_user_settings(
    conn: &Connection,
    user_id: i64,
) -> Result<UserSettings, rusqlite::Error> {
    let next_scan = (Utc::now() + Duration::minutes(30)).to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO user_settings (user_id, next_scan_at) VALUES (?1, ?2)",
        params![user_id, next_scan],
    )?;

    let mut settings = conn.query_row(
        "SELECT github_pat, refresh_frequency_minutes, auto_ignore_days, max_results_per_query,
                discovery_webhook_url, discovery_webhook_template, tracked_webhook_url, tracked_webhook_template,
                ai_enabled, ai_base_url, ai_api_key, ai_model, ai_summarize_readme, ai_summarize_commits,
                last_scan_at, next_scan_at, is_scanning, last_error,
                rate_limit_remaining, rate_limit_limit, rate_limit_reset
         FROM user_settings WHERE user_id = ?1",
        params![user_id],
        |row| {
            Ok(UserSettings {
                github_pat: row.get(0)?,
                refresh_frequency_minutes: row.get(1)?,
                auto_ignore_days: row.get(2)?,
                max_results_per_query: row.get(3)?,
                discovery_webhook_url: row.get(4)?,
                discovery_webhook_template: row.get(5)?,
                tracked_webhook_url: row.get(6)?,
                tracked_webhook_template: row.get(7)?,
                ai_enabled: row.get::<_, i64>(8)? != 0,
                ai_base_url: row.get(9)?,
                ai_api_key: row.get(10)?,
                ai_model: row.get(11)?,
                ai_summarize_readme: row.get::<_, i64>(12)? != 0,
                ai_summarize_commits: row.get::<_, i64>(13)? != 0,
                last_scan_at: row.get(14)?,
                next_scan_at: row.get(15)?,
                is_scanning: row.get::<_, i64>(16)? != 0,
                last_error: row.get(17)?,
                rate_limit_remaining: row.get(18)?,
                rate_limit_limit: row.get(19)?,
                rate_limit_reset: row.get(20)?,
                ..Default::default()
            })
        },
    )?;

    // Derived, read-only hints. The secrets themselves are never serialized
    // to the client (see the `skip_serializing` attributes on UserSettings).
    settings.github_pat_set = !settings.github_pat.is_empty();
    settings.github_pat_hint = secret_hint(&settings.github_pat);
    settings.ai_api_key_set = !settings.ai_api_key.is_empty();
    settings.ai_api_key_hint = secret_hint(&settings.ai_api_key);
    Ok(settings)
}
