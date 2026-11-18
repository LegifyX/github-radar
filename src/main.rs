use axum::{
    extract::{FromRequestParts, Path, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    env, fs,
    path::Path as FsPath,
    sync::Arc,
    time::Duration as StdDuration,
};
use tokio::sync::{Mutex, Semaphore};
use tracing::{info, warn};
use uuid::Uuid;

static INDEX_HTML: &str = include_str!("../static/index.html");

const MANIFEST_JSON: &str = r##"{
  "name": "GitHub Radar",
  "short_name": "GH Radar",
  "description": "Self-hosted GitHub repository discovery and multi-branch commit tracker",
  "start_url": "/",
  "display": "standalone",
  "background_color": "#0d1117",
  "theme_color": "#161b22",
  "icons": [
    {
      "src": "/icon.svg",
      "sizes": "any",
      "type": "image/svg+xml",
      "purpose": "any maskable"
    }
  ]
}"##;

const SW_JS: &str = r#"
const CACHE_NAME = 'gh-radar-v1';
const STATIC_ASSETS = ['/manifest.json', '/icon.svg'];

self.addEventListener('install', (event) => {
  self.skipWaiting();
  event.waitUntil(
    caches.open(CACHE_NAME).then((cache) => cache.addAll(STATIC_ASSETS))
  );
});

self.addEventListener('activate', (event) => {
  event.waitUntil(
    caches.keys().then((keys) =>
      Promise.all(keys.filter((k) => k !== CACHE_NAME).map((k) => caches.delete(k)))
    ).then(() => self.clients.claim())
  );
});

self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  if (url.pathname.startsWith('/api/')) {
    return;
  }
  if (url.pathname === '/' || url.pathname === '/index.html') {
    event.respondWith(
      fetch(event.request).catch(() => caches.match('/'))
    );
    return;
  }
  event.respondWith(
    caches.match(event.request).then((cached) => cached || fetch(event.request))
  );
});
"#;

const ICON_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 128 128">
  <rect width="128" height="128" rx="26" fill="#0d1117"/>
  <circle cx="64" cy="64" r="46" fill="none" stroke="#238636" stroke-width="4" opacity="0.35"/>
  <circle cx="64" cy="64" r="30" fill="none" stroke="#238636" stroke-width="4" opacity="0.65"/>
  <circle cx="64" cy="64" r="14" fill="none" stroke="#58a6ff" stroke-width="4"/>
  <path d="M64 64 L100 34" stroke="#58a6ff" stroke-width="5" stroke-linecap="round"/>
  <circle cx="64" cy="64" r="6" fill="#58a6ff"/>
  <circle cx="88" cy="44" r="5" fill="#3fb950"/>
</svg>"##;

#[derive(Clone)]
struct AppState {
    db_path: String,
    http_github: reqwest::Client,
    http_ai: reqwest::Client,
    scanning_users: Arc<Mutex<HashSet<i64>>>,
    ai_semaphore: Arc<Semaphore>,
}

impl AppState {
    fn db(&self) -> Result<Connection, AppError> {
        let conn = Connection::open(&self.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(StdDuration::from_millis(5000))?;
        Ok(conn)
    }
}

#[derive(Debug)]
struct AppError(StatusCode, String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.1 }));
        (self.0, body).into_response()
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        AppError(StatusCode::BAD_GATEWAY, err.to_string())
    }
}

impl From<bcrypt::BcryptError> for AppError {
    fn from(err: bcrypt::BcryptError) -> Self {
        AppError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthUser {
    id: i64,
    username: String,
    is_admin: bool,
    session_expires_at: String,
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let cookie_header = parts
            .headers
            .get(header::COOKIE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let mut session_token = None;
        for part in cookie_header.split(';') {
            let trimmed = part.trim();
            if let Some(val) = trimmed.strip_prefix("radar_session=") {
                session_token = Some(val.to_string());
                break;
            }
        }

        let token = session_token.ok_or_else(|| {
            AppError(StatusCode::UNAUTHORIZED, "Authentication required".into())
        })?;

        let conn = state.db()?;
        let now_str = Utc::now().to_rfc3339();

        let user = conn
            .query_row(
                "SELECT u.id, u.username, u.is_admin, s.expires_at
                 FROM sessions s
                 JOIN users u ON u.id = s.user_id
                 WHERE s.token = ?1 AND s.expires_at > ?2 AND u.is_active = 1",
                params![token, now_str],
                |row| {
                    Ok(AuthUser {
                        id: row.get(0)?,
                        username: row.get(1)?,
                        is_admin: row.get::<_, i64>(2)? != 0,
                        session_expires_at: row.get(3)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| {
                AppError(
                    StatusCode::UNAUTHORIZED,
                    "Session expired or invalid".into(),
                )
            })?;

        // Sliding 30-day session extension
        let new_exp = (Utc::now() + Duration::days(30)).to_rfc3339();
        let _ = conn.execute(
            "UPDATE sessions SET expires_at = ?1 WHERE token = ?2",
            params![new_exp, token],
        );

        Ok(user)
    }
}

#[derive(Serialize, Deserialize, Clone)]
struct UserSettings {
    github_pat: String,
    refresh_frequency_minutes: i64,
    auto_ignore_days: i64,
    max_results_per_query: i64,
    discovery_webhook_url: String,
    discovery_webhook_template: String,
    tracked_webhook_url: String,
    tracked_webhook_template: String,
    ai_enabled: bool,
    ai_base_url: String,
    ai_api_key: String,
    ai_model: String,
    ai_summarize_readme: bool,
    ai_summarize_commits: bool,
    last_scan_at: Option<String>,
    next_scan_at: Option<String>,
    is_scanning: bool,
    last_error: Option<String>,
    rate_limit_remaining: Option<i64>,
    rate_limit_limit: Option<i64>,
    rate_limit_reset: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct SearchQuery {
    id: i64,
    query_text: String,
    min_stars: i64,
    initial_lookback_days: i64,
    notify_webhook: bool,
    is_enabled: bool,
    last_run_at: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct RepoRecord {
    id: i64,
    github_repo_id: i64,
    full_name: String,
    html_url: String,
    description: String,
    language: String,
    stars: i64,
    topics: Vec<String>,
    pushed_at: String,
    default_branch: String,
    status: String,
    ignore_type: String,
    matched_query: String,
    discovered_at: String,
    is_pinned: bool,
    sort_order: i64,
    notify_webhook: bool,
    unread_count: i64,
    was_renamed: bool,
    previous_name: Option<String>,
    is_missing: bool,
    readme_summary: Option<String>,
    diff_summary: Option<String>,
    ai_status: String,
    readme_html: Option<String>,
    readme_updated_at: Option<String>,
    is_auto_expired: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct CommitRecord {
    id: i64,
    repo_id: i64,
    sha: String,
    branches: Vec<String>,
    author: String,
    message: String,
    committed_at: String,
    html_url: String,
    is_read: bool,
}

fn init_db(conn: &Connection) -> Result<(), rusqlite::Error> {
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

fn ensure_user_settings(conn: &Connection, user_id: i64) -> Result<UserSettings, rusqlite::Error> {
    let next_scan = (Utc::now() + Duration::minutes(30)).to_rfc3339();
    conn.execute(
        "INSERT OR IGNORE INTO user_settings (user_id, next_scan_at) VALUES (?1, ?2)",
        params![user_id, next_scan],
    )?;

    conn.query_row(
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
            })
        },
    )
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "github_radar=info".into()),
        )
        .init();

    let db_path = env::var("DATABASE_PATH").unwrap_or_else(|_| "/data/radar.db".to_string());
    if let Some(parent) = FsPath::new(&db_path).parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).expect("Failed to create database directory");
        }
    }

    let http_github = reqwest::Client::builder()
        .user_agent("GitHub-Radar-SelfHosted/1.0")
        .connect_timeout(StdDuration::from_secs(15))
        .timeout(StdDuration::from_secs(30))
        .build()
        .expect("Failed to build GitHub HTTP client");

    let http_ai = reqwest::Client::builder()
        .user_agent("GitHub-Radar-AI/1.0")
        .connect_timeout(StdDuration::from_secs(10))
        .timeout(StdDuration::from_secs(90))
        .build()
        .expect("Failed to build AI HTTP client");

    let state = AppState {
        db_path: db_path.clone(),
        http_github,
        http_ai,
        scanning_users: Arc::new(Mutex::new(HashSet::new())),
        ai_semaphore: Arc::new(Semaphore::new(1)),
    };

    {
        let conn = state.db().expect("Failed to open SQLite database");
        init_db(&conn).expect("Failed to initialize database schema");
    }

    // Background scheduler loop (checks every 10 seconds)
    let scheduler_state = state.clone();
    tokio::spawn(async move {
        let mut tick_count: u64 = 0;
        loop {
            tokio::time::sleep(StdDuration::from_secs(10)).await;
            tick_count += 1;

            // Check users due for scan
            if let Ok(conn) = scheduler_state.db() {
                let now_str = Utc::now().to_rfc3339();
                let mut stmt = match conn.prepare(
                    "SELECT u.id FROM users u
                     JOIN user_settings s ON s.user_id = u.id
                     WHERE u.is_active = 1 AND s.is_scanning = 0
                       AND (s.next_scan_at IS NULL OR s.next_scan_at <= ?1)",
                ) {
                    Ok(st) => st,
                    Err(_) => continue,
                };

                let due_users: Vec<i64> = stmt
                    .query_map(params![now_str], |r| r.get(0))
                    .ok()
                    .map(|iter| iter.filter_map(Result::ok).collect())
                    .unwrap_or_default();

                for uid in due_users {
                    trigger_user_scan(scheduler_state.clone(), uid);
                }

                // Daily maintenance job (~every 8640 ticks of 10s = 24 hours)
                if tick_count % 8640 == 0 {
                    run_daily_maintenance(&conn);
                }
            }
        }
    });

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/manifest.json", get(serve_manifest))
        .route("/sw.js", get(serve_sw))
        .route("/icon.svg", get(serve_icon))
        .route("/api/health", get(api_health))
        // Auth routes
        .route("/api/auth/status", get(api_auth_status))
        .route("/api/auth/setup", post(api_auth_setup))
        .route("/api/auth/login", post(api_auth_login))
        .route("/api/auth/logout", post(api_auth_logout))
        // Core dashboard state & scanner
        .route("/api/state", get(api_get_state))
        .route("/api/scan/force", post(api_force_scan))
        // Search Queries CRUD
        .route("/api/queries", post(api_create_query))
        .route("/api/queries/:id", put(api_update_query).delete(api_delete_query))
        // Repos & Triage
        .route("/api/repos/quick-track", post(api_quick_track))
        .route("/api/repos/ignore-batch", post(api_ignore_batch))
        .route("/api/repos/reorder", post(api_reorder_repos))
        .route("/api/repos/:id/action", post(api_repo_action))
        .route("/api/repos/:id/details", get(api_repo_details))
        .route("/api/repos/:id/mark-read", post(api_repo_mark_read))
        .route("/api/repos/:id/refresh-readme", post(api_repo_refresh_readme))
        .route("/api/repos/:id/regenerate-ai", post(api_repo_regenerate_ai))
        .route("/api/repos/:id", delete(api_delete_repo))
        // Settings, Tests & Backup
        .route("/api/settings", put(api_update_settings))
        .route("/api/settings/test-github", post(api_test_github))
        .route("/api/settings/test-webhook", post(api_test_webhook))
        .route("/api/settings/test-ai", post(api_test_ai))
        .route("/api/backup/export", get(api_export_backup))
        .route("/api/backup/import", post(api_import_backup))
        // Admin endpoints
        .route("/api/admin/users", get(api_admin_list_users).post(api_admin_create_user))
        .route("/api/admin/users/:id", put(api_admin_update_user).delete(api_admin_delete_user))
        .with_state(state.clone());

    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    info!("GitHub Radar listening on http://{}", bind_addr);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .expect("Failed to bind address");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state))
        .await
        .expect("Server error");
}

async fn shutdown_signal(state: AppState) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutting down gracefully, checkpointing SQLite WAL...");
    if let Ok(conn) = state.db() {
        let _ = conn.execute_batch("UPDATE user_settings SET is_scanning = 0; PRAGMA wal_checkpoint(TRUNCATE);");
    }
}

fn run_daily_maintenance(conn: &Connection) {
    info!("Running daily SQLite maintenance and retention pruning...");
    let now_str = Utc::now().to_rfc3339();
    let tombstone_cutoff = (Utc::now() - Duration::days(90)).to_rfc3339();
    let _ = conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", params![now_str]);
    let _ = conn.execute(
        "DELETE FROM deleted_tombstones WHERE deleted_at <= ?1",
        params![tombstone_cutoff],
    );

    // Prune commits beyond the most recent 100 per repo
    if let Ok(mut stmt) = conn.prepare("SELECT id FROM repos WHERE status = 'tracked'") {
        let repo_ids: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .ok()
            .map(|it| it.filter_map(Result::ok).collect())
            .unwrap_or_default();
        for rid in repo_ids {
            let _ = conn.execute(
                "DELETE FROM commits WHERE repo_id = ?1 AND id NOT IN (
                    SELECT id FROM commits WHERE repo_id = ?1 ORDER BY committed_at DESC LIMIT 100
                )",
                params![rid],
            );
        }
    }

    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;");
}

// ============================================================================
// Static & PWA Asset Handlers
// ============================================================================

async fn serve_index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

async fn serve_manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST_JSON,
    )
}

async fn serve_sw() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        SW_JS,
    )
}

async fn serve_icon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], ICON_SVG)
}

async fn api_health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

// ============================================================================
// Authentication & First-User Bootstrap
// ============================================================================

async fn api_auth_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    let user_count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;

    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let mut token_opt = None;
    for part in cookie_header.split(';') {
        if let Some(val) = part.trim().strip_prefix("radar_session=") {
            token_opt = Some(val.to_string());
            break;
        }
    }

    if let Some(token) = token_opt {
        let now_str = Utc::now().to_rfc3339();
        if let Some(u) = conn
            .query_row(
                "SELECT u.id, u.username, u.is_admin, s.expires_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token = ?1 AND s.expires_at > ?2 AND u.is_active = 1",
                params![token, now_str],
                |r| {
                    Ok(AuthUser {
                        id: r.get(0)?,
                        username: r.get(1)?,
                        is_admin: r.get::<_, i64>(2)? != 0,
                        session_expires_at: r.get(3)?,
                    })
                },
            )
            .optional()?
        {
            return Ok(Json(json!({
                "needs_setup": false,
                "authenticated": true,
                "user": u
            })));
        }
    }

    Ok(Json(json!({
        "needs_setup": user_count == 0,
        "authenticated": false
    })))
}

#[derive(Deserialize)]
struct AuthCredentials {
    username: String,
    password: String,
}

fn create_session_cookie(conn: &Connection, user_id: i64) -> Result<HeaderMap, AppError> {
    let token = Uuid::new_v4().to_string();
    let expires_at = (Utc::now() + Duration::days(30)).to_rfc3339();
    conn.execute(
        "INSERT INTO sessions (token, user_id, expires_at) VALUES (?1, ?2, ?3)",
        params![token, user_id, expires_at],
    )?;

    let mut headers = HeaderMap::new();
    let cookie = format!(
        "radar_session={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000",
        token
    );
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());
    Ok(headers)
}

async fn api_auth_setup(
    State(state): State<AppState>,
    Json(req): Json<AuthCredentials>,
) -> Result<(HeaderMap, Json<Value>), AppError> {
    let username = req.username.trim().to_string();
    if username.is_empty() || req.password.len() < 6 {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Username required and password must be at least 6 characters".into(),
        ));
    }

    let conn = state.db()?;
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?;
    if count > 0 {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Initial admin setup already completed".into(),
        ));
    }

    let hash = bcrypt::hash(&req.password, bcrypt::DEFAULT_COST)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO users (username, password_hash, is_admin, is_active, created_at)
         VALUES (?1, ?2, 1, 1, ?3)",
        params![username, hash, now],
    )?;
    let user_id = conn.last_insert_rowid();
    ensure_user_settings(&conn, user_id)?;

    let headers = create_session_cookie(&conn, user_id)?;
    Ok((headers, Json(json!({ "ok": true }))))
}

async fn api_auth_login(
    State(state): State<AppState>,
    Json(req): Json<AuthCredentials>,
) -> Result<(HeaderMap, Json<Value>), AppError> {
    let conn = state.db()?;
    let row: Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT id, password_hash, is_active FROM users WHERE username = ?1",
            params![req.username.trim()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    let (user_id, hash, is_active) = row.ok_or_else(|| {
        AppError(StatusCode::UNAUTHORIZED, "Invalid username or password".into())
    })?;

    if is_active == 0 {
        return Err(AppError(
            StatusCode::FORBIDDEN,
            "Account has been disabled by an administrator".into(),
        ));
    }

    if !bcrypt::verify(&req.password, &hash).unwrap_or(false) {
        return Err(AppError(
            StatusCode::UNAUTHORIZED,
            "Invalid username or password".into(),
        ));
    }

    ensure_user_settings(&conn, user_id)?;
    let headers = create_session_cookie(&conn, user_id)?;
    Ok((headers, Json(json!({ "ok": true }))))
}

async fn api_auth_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<(HeaderMap, Json<Value>), AppError> {
    let cookie_header = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    for part in cookie_header.split(';') {
        if let Some(token) = part.trim().strip_prefix("radar_session=") {
            if let Ok(conn) = state.db() {
                let _ = conn.execute("DELETE FROM sessions WHERE token = ?1", params![token]);
            }
        }
    }

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        header::SET_COOKIE,
        "radar_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
            .parse()
            .unwrap(),
    );
    Ok((resp_headers, Json(json!({ "ok": true }))))
}

// ============================================================================
// Dashboard State & Retroactive SQL Windowing
// ============================================================================

async fn api_get_state(
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

async fn api_force_scan(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    trigger_user_scan(state, user.id);
    Ok(Json(json!({ "ok": true, "message": "Scan triggered" })))
}

// ============================================================================
// Search Queries CRUD
// ============================================================================

#[derive(Deserialize)]
struct SearchQueryInput {
    query_text: String,
    min_stars: i64,
    initial_lookback_days: i64,
    notify_webhook: bool,
    is_enabled: Option<bool>,
}

async fn api_create_query(
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

async fn api_update_query(
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

async fn api_delete_query(
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

// ============================================================================
// Repository Actions, Quick-Track & Multi-Branch Details
// ============================================================================

fn parse_github_slug(raw: &str) -> Option<String> {
    let mut s = raw.trim().trim_end_matches('/').to_string();
    if let Some(idx) = s.find("github.com/") {
        s = s[idx + "github.com/".len()..].to_string();
    }
    let parts: Vec<&str> = s
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() >= 2 {
        let owner = parts[0];
        let repo = parts[1].trim_end_matches(".git");
        if !owner.is_empty() && !repo.is_empty() {
            return Some(format!("{}/{}", owner, repo));
        }
    }
    None
}

#[derive(Deserialize)]
struct QuickTrackInput {
    repo_input: String,
}

async fn api_quick_track(
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
        let conn = state.db()?;
        let _ = conn.execute(
            "DELETE FROM deleted_tombstones WHERE user_id = ?1 AND github_repo_id = ?2",
            params![user.id, gh_id],
        );

        let max_order: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(sort_order), 0) FROM repos WHERE user_id = ?1 AND status = 'tracked'",
                params![user.id],
                |r| r.get(0),
            )
            .unwrap_or(0);

        conn.execute(
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

        conn.query_row(
            "SELECT id FROM repos WHERE user_id = ?1 AND github_repo_id = ?2",
            params![user.id, gh_id],
            |r| r.get::<_, i64>(0),
        )?
    };

    // Spawn immediate baseline commit + README fetch
    let bg_state = state.clone();
    tokio::spawn(async move {
        let _ = sync_single_tracked_repo(&bg_state, user.id, repo_db_id, true).await;
    });

    Ok(Json(json!({ "ok": true, "repo_id": repo_db_id })))
}

#[derive(Deserialize)]
struct RepoActionInput {
    action: String, // "track", "ignore", "unignore", "toggle_pin", "toggle_webhook", "clear_rename"
}

async fn api_repo_action(
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
                "UPDATE repos SET status = 'tracked', ignore_type = 'none', sort_order = ?1 WHERE id = ?2 AND user_id = ?3",
                params![max_order + 1, id, user.id],
            )?;
            let bg_state = state.clone();
            tokio::spawn(async move {
                let _ = sync_single_tracked_repo(&bg_state, user.id, id, true).await;
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
        _ => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "Unknown action".into(),
            ))
        }
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct IgnoreBatchInput {
    repo_ids: Vec<i64>,
}

async fn api_ignore_batch(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<IgnoreBatchInput>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    for rid in input.repo_ids {
        conn.execute(
            "UPDATE repos SET ignore_type = 'manual' WHERE id = ?1 AND user_id = ?2",
            params![rid, user.id],
        )?;
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct ReorderInput {
    repo_id: i64,
    direction: String, // "up" | "down"
}

async fn api_reorder_repos(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<ReorderInput>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    let target_pin: i64 = conn.query_row(
        "SELECT is_pinned FROM repos WHERE id = ?1 AND user_id = ?2",
        params![input.repo_id, user.id],
        |r| r.get(0),
    )?;

    let mut stmt = conn.prepare(
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

    for (idx, rid) in ids.iter().enumerate() {
        conn.execute(
            "UPDATE repos SET sort_order = ?1 WHERE id = ?2 AND user_id = ?3",
            params![(idx + 1) as i64, rid, user.id],
        )?;
    }

    Ok(Json(json!({ "ok": true })))
}

async fn api_repo_details(
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

async fn api_repo_mark_read(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    conn.execute(
        "UPDATE commits SET is_read = 1 WHERE repo_id = ?1 AND user_id = ?2",
        params![id, user.id],
    )?;
    conn.execute(
        "UPDATE repos SET unread_count = 0 WHERE id = ?1 AND user_id = ?2",
        params![id, user.id],
    )?;
    Ok(Json(json!({ "ok": true })))
}

async fn api_repo_refresh_readme(
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
        return Ok(Json(json!({ "ok": true, "readme_html": html, "readme_updated_at": now })));
    }

    Err(AppError(
        StatusCode::BAD_GATEWAY,
        "Could not fetch README from GitHub".into(),
    ))
}

async fn api_repo_regenerate_ai(
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

async fn api_delete_repo(
    State(state): State<AppState>,
    user: AuthUser,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    if let Some(gh_id) = conn
        .query_row(
            "SELECT github_repo_id FROM repos WHERE id = ?1 AND user_id = ?2",
            params![id, user.id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR REPLACE INTO deleted_tombstones (user_id, github_repo_id, deleted_at)
             VALUES (?1, ?2, ?3)",
            params![user.id, gh_id, now],
        )?;
        conn.execute(
            "DELETE FROM repos WHERE id = ?1 AND user_id = ?2",
            params![id, user.id],
        )?;
    }
    Ok(Json(json!({ "ok": true })))
}

// ============================================================================
// Configuration, Testing & Backup Endpoints
// ============================================================================

#[derive(Deserialize)]
struct UpdateSettingsInput {
    github_pat: String,
    refresh_frequency_minutes: i64,
    auto_ignore_days: i64,
    max_results_per_query: i64,
    discovery_webhook_url: String,
    discovery_webhook_template: String,
    tracked_webhook_url: String,
    tracked_webhook_template: String,
    ai_enabled: bool,
    ai_base_url: String,
    ai_api_key: String,
    ai_model: String,
    ai_summarize_readme: bool,
    ai_summarize_commits: bool,
}

async fn api_update_settings(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<UpdateSettingsInput>,
) -> Result<Json<Value>, AppError> {
    let freq = input.refresh_frequency_minutes.clamp(5, 1440);
    let next_scan = (Utc::now() + Duration::minutes(freq)).to_rfc3339();
    let conn = state.db()?;
    ensure_user_settings(&conn, user.id)?;

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
            input.github_pat.trim(),
            freq,
            input.auto_ignore_days.clamp(1, 365),
            input.max_results_per_query.clamp(1, 100),
            input.discovery_webhook_url.trim(),
            input.discovery_webhook_template,
            input.tracked_webhook_url.trim(),
            input.tracked_webhook_template,
            if input.ai_enabled { 1 } else { 0 },
            input.ai_base_url.trim().trim_end_matches('/'),
            input.ai_api_key.trim(),
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
struct TestGithubInput {
    github_pat: String,
}

async fn api_test_github(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<TestGithubInput>,
) -> Result<Json<Value>, AppError> {
    let mut req = state
        .http_github
        .get("https://api.github.com/rate_limit")
        .header("Accept", "application/vnd.github+json");
    let pat = input.github_pat.trim();
    if !pat.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", pat));
    }

    let resp = req.send().await?;
    update_rate_limit_from_headers(&state, user.id, resp.headers());
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or_else(|_| json!({}));

    if !status.is_success() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!(
                "GitHub API returned {}: {}",
                status,
                body["message"].as_str().unwrap_or("Invalid token")
            ),
        ));
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
struct TestWebhookInput {
    webhook_url: String,
    stream: String,
}

async fn api_test_webhook(
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
        .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct TestAiInput {
    ai_base_url: String,
    ai_api_key: String,
    ai_model: String,
}

async fn api_test_ai(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(input): Json<TestAiInput>,
) -> Result<Json<Value>, AppError> {
    let reply = call_openai_compatible(
        &state.http_ai,
        input.ai_base_url.trim(),
        input.ai_api_key.trim(),
        input.ai_model.trim(),
        "Reply with a single short sentence confirming you are online for GitHub Radar.",
    )
    .await
    .map_err(|e| AppError(StatusCode::BAD_GATEWAY, e))?;

    Ok(Json(json!({ "ok": true, "reply": reply })))
}

async fn api_export_backup(
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

async fn api_import_backup(
    State(state): State<AppState>,
    user: AuthUser,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let conn = state.db()?;
    ensure_user_settings(&conn, user.id)?;
    let now = Utc::now().to_rfc3339();

    if let Some(queries) = payload["queries"].as_array() {
        for q in queries {
            if let Some(qt) = q["query_text"].as_str() {
                let exists: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM search_queries WHERE user_id = ?1 AND query_text = ?2",
                        params![user.id, qt],
                        |r| r.get(0),
                    )
                    .optional()?;
                if exists.is_none() {
                    conn.execute(
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
                conn.execute(
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

    Ok(Json(json!({ "ok": true })))
}

// ============================================================================
// Admin User Management Panel
// ============================================================================

async fn api_admin_list_users(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, AppError> {
    if !user.is_admin {
        return Err(AppError(StatusCode::FORBIDDEN, "Admin access required".into()));
    }
    let conn = state.db()?;
    let mut stmt = conn.prepare(
        "SELECT id, username, is_admin, is_active, created_at FROM users ORDER BY id ASC",
    )?;
    let users: Vec<Value> = stmt
        .query_map([], |r| {
            Ok(json!({
                "id": r.get::<_, i64>(0)?,
                "username": r.get::<_, String>(1)?,
                "is_admin": r.get::<_, i64>(2)? != 0,
                "is_active": r.get::<_, i64>(3)? != 0,
                "created_at": r.get::<_, String>(4)?
            }))
        })?
        .filter_map(Result::ok)
        .collect();
    Ok(Json(json!({ "users": users })))
}

async fn api_admin_create_user(
    State(state): State<AppState>,
    user: AuthUser,
    Json(req): Json<AuthCredentials>,
) -> Result<Json<Value>, AppError> {
    if !user.is_admin {
        return Err(AppError(StatusCode::FORBIDDEN, "Admin access required".into()));
    }
    let uname = req.username.trim();
    if uname.is_empty() || req.password.len() < 6 {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Username required and password must be >= 6 chars".into(),
        ));
    }
    let hash = bcrypt::hash(&req.password, bcrypt::DEFAULT_COST)?;
    let now = Utc::now().to_rfc3339();
    let conn = state.db()?;
    conn.execute(
        "INSERT INTO users (username, password_hash, is_admin, is_active, created_at)
         VALUES (?1, ?2, 0, 1, ?3)",
        params![uname, hash, now],
    )?;
    let new_id = conn.last_insert_rowid();
    ensure_user_settings(&conn, new_id)?;
    Ok(Json(json!({ "ok": true, "id": new_id })))
}

#[derive(Deserialize)]
struct AdminUpdateUserInput {
    password: Option<String>,
    is_active: Option<bool>,
}

async fn api_admin_update_user(
    State(state): State<AppState>,
    user: AuthUser,
    Path(target_id): Path<i64>,
    Json(input): Json<AdminUpdateUserInput>,
) -> Result<Json<Value>, AppError> {
    if !user.is_admin {
        return Err(AppError(StatusCode::FORBIDDEN, "Admin access required".into()));
    }
    let conn = state.db()?;
    if let Some(pw) = input.password {
        if pw.len() < 6 {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "Password must be >= 6 characters".into(),
            ));
        }
        let hash = bcrypt::hash(&pw, bcrypt::DEFAULT_COST)?;
        conn.execute(
            "UPDATE users SET password_hash = ?1 WHERE id = ?2",
            params![hash, target_id],
        )?;
    }
    if let Some(active) = input.is_active {
        if target_id == user.id && !active {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "Cannot deactivate your own admin account".into(),
            ));
        }
        conn.execute(
            "UPDATE users SET is_active = ?1 WHERE id = ?2",
            params![if active { 1 } else { 0 }, target_id],
        )?;
        if !active {
            conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![target_id])?;
        }
    }
    Ok(Json(json!({ "ok": true })))
}

async fn api_admin_delete_user(
    State(state): State<AppState>,
    user: AuthUser,
    Path(target_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    if !user.is_admin {
        return Err(AppError(StatusCode::FORBIDDEN, "Admin access required".into()));
    }
    if target_id == user.id {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Cannot delete your own admin account".into(),
        ));
    }
    let conn = state.db()?;
    let target_admin: i64 = conn
        .query_row(
            "SELECT is_admin FROM users WHERE id = ?1",
            params![target_id],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if target_admin != 0 {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "Cannot delete an admin account".into(),
        ));
    }
    conn.execute("DELETE FROM users WHERE id = ?1", params![target_id])?;
    Ok(Json(json!({ "ok": true })))
}

// ============================================================================
// Background Scanner Engine (Discoveries, Tracked Multi-Branch, AI & Webhooks)
// ============================================================================

fn trigger_user_scan(state: AppState, user_id: i64) {
    tokio::spawn(async move {
        {
            let mut active = state.scanning_users.lock().await;
            if active.contains(&user_id) {
                return;
            }
            active.insert(user_id);
        }

        if let Ok(conn) = state.db() {
            let _ = conn.execute(
                "UPDATE user_settings SET is_scanning = 1, last_error = NULL WHERE user_id = ?1",
                params![user_id],
            );
        }

        let scan_res = execute_user_scan(&state, user_id).await;

        if let Ok(conn) = state.db() {
            let settings = ensure_user_settings(&conn, user_id).unwrap_or(UserSettings {
                github_pat: String::new(),
                refresh_frequency_minutes: 30,
                auto_ignore_days: 7,
                max_results_per_query: 15,
                discovery_webhook_url: String::new(),
                discovery_webhook_template: String::new(),
                tracked_webhook_url: String::new(),
                tracked_webhook_template: String::new(),
                ai_enabled: false,
                ai_base_url: String::new(),
                ai_api_key: String::new(),
                ai_model: String::new(),
                ai_summarize_readme: true,
                ai_summarize_commits: true,
                last_scan_at: None,
                next_scan_at: None,
                is_scanning: false,
                last_error: None,
                rate_limit_remaining: None,
                rate_limit_limit: None,
                rate_limit_reset: None,
            });
            let now = Utc::now();
            let next = (now + Duration::minutes(settings.refresh_frequency_minutes.max(5))).to_rfc3339();
            let err_str = scan_res.err();
            let _ = conn.execute(
                "UPDATE user_settings SET is_scanning = 0, last_scan_at = ?1, next_scan_at = ?2, last_error = ?3 WHERE user_id = ?4",
                params![now.to_rfc3339(), next, err_str, user_id],
            );
        }

        let mut active = state.scanning_users.lock().await;
        active.remove(&user_id);
    });
}

fn update_rate_limit_from_headers(state: &AppState, user_id: i64, headers: &HeaderMap) {
    let rem = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let lim = headers
        .get("x-ratelimit-limit")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let reset = headers
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|ts| DateTime::from_timestamp(ts, 0).map(|dt| dt.to_rfc3339()));

    if rem.is_some() || lim.is_some() {
        if let Ok(conn) = state.db() {
            let _ = conn.execute(
                "UPDATE user_settings SET rate_limit_remaining = COALESCE(?1, rate_limit_remaining),
                 rate_limit_limit = COALESCE(?2, rate_limit_limit),
                 rate_limit_reset = COALESCE(?3, rate_limit_reset)
                 WHERE user_id = ?4",
                params![rem, lim, reset, user_id],
            );
        }
    }
}

async fn execute_user_scan(state: &AppState, user_id: i64) -> Result<(), String> {
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

        if resp.status() == StatusCode::FORBIDDEN || resp.status() == StatusCode::TOO_MANY_REQUESTS {
            return Err("GitHub API rate limit reached. Add a GitHub PAT or wait for reset.".into());
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
                let default_branch = item["default_branch"].as_str().unwrap_or("main").to_string();
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

                    // Check tombstone (if user hard-deleted this repo, only allow back if pushed_at > deleted_at)
                    let tombstone_dt: Option<String> = conn
                        .query_row(
                            "SELECT deleted_at FROM deleted_tombstones WHERE user_id = ?1 AND github_repo_id = ?2",
                            params![user_id, gh_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .unwrap_or(None);

                    if let Some(del_at) = tombstone_dt {
                        if pushed_at <= del_at {
                            continue;
                        } else {
                            let _ = conn.execute(
                                "DELETE FROM deleted_tombstones WHERE user_id = ?1 AND github_repo_id = ?2",
                                params![user_id, gh_id],
                            );
                        }
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

async fn sync_single_tracked_repo(
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
        return Ok(());
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
    let default_branch = gh_repo["default_branch"].as_str().unwrap_or("main").to_string();

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
    if let Ok(b_resp) = b_req.send().await {
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
            _ => continue,
        };
        update_rate_limit_from_headers(state, user_id, c_resp.headers());

        let c_list: Value = match c_resp.json().await {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(arr) = c_list.as_array() {
            let conn = state.db().map_err(|e| e.1)?;
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

                let existing_branches: Option<String> = conn
                    .query_row(
                        "SELECT branches_json FROM commits WHERE user_id = ?1 AND repo_id = ?2 AND sha = ?3",
                        params![user_id, repo_id, sha],
                        |r| r.get(0),
                    )
                    .optional()
                    .unwrap_or(None);

                if let Some(b_json) = existing_branches {
                    let mut b_vec: Vec<String> = serde_json::from_str(&b_json).unwrap_or_default();
                    if !b_vec.contains(branch) {
                        b_vec.push(branch.clone());
                        let updated_json =
                            serde_json::to_string(&b_vec).unwrap_or_else(|_| "[]".into());
                        let _ = conn.execute(
                            "UPDATE commits SET branches_json = ?1 WHERE user_id = ?2 AND repo_id = ?3 AND sha = ?4",
                            params![updated_json, user_id, repo_id, sha],
                        );
                    }
                } else {
                    let b_json =
                        serde_json::to_string(&vec![branch.clone()]).unwrap_or_else(|_| "[]".into());
                    let is_read_val = if is_initial_baseline { 1 } else { 0 };
                    let _ = conn.execute(
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
                    );

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
        }
    }

    // Update unread count on repo
    {
        let conn = state.db().map_err(|e| e.1)?;
        let unread: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM commits WHERE repo_id = ?1 AND user_id = ?2 AND is_read = 0",
                params![repo_id, user_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let _ = conn.execute(
            "UPDATE repos SET unread_count = ?1 WHERE id = ?2 AND user_id = ?3",
            params![unread, repo_id, user_id],
        );
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
                    format!("• `{}` (`{}`) {} — _{}_", &sha[..7.min(sha.len())], br, msg, auth)
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

// ============================================================================
// README Sanitizer & OpenAI-Compatible `/v1` AI Client
// ============================================================================

async fn fetch_and_sanitize_readme(
    state: &AppState,
    full_name: &str,
    default_branch: &str,
    pat: &str,
) -> Option<String> {
    let url = format!("https://api.github.com/repos/{}/readme", full_name);
    let mut req = state
        .http_github
        .get(&url)
        .header("Accept", "application/vnd.github.html+json");
    if !pat.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", pat));
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let raw_html = resp.text().await.ok()?;

    // Rewrite relative image src and link href attributes to raw.githubusercontent.com / github.com
    let raw_base = format!(
        "https://raw.githubusercontent.com/{}/{}/",
        full_name, default_branch
    );
    let blob_base = format!(
        "https://github.com/{}/blob/{}/",
        full_name, default_branch
    );

    let rewritten = raw_html
        .replace("src=\"./", &format!("src=\"{}", raw_base))
        .replace("src=\"/", &format!("src=\"{}", raw_base))
        .replace("href=\"./", &format!("href=\"{}", blob_base));

    Some(rewritten)
}

async fn generate_ai_for_repo(
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
            let commit_url = format!(
                "https://api.github.com/repos/{}/commits/{}",
                full_name, sha
            );
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

async fn call_openai_compatible(
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

async fn send_webhook_payload(
    client: &reqwest::Client,
    webhook_url: &str,
    message: &str,
) -> Result<(), String> {
    // Compatible with Discord ("content"), Slack ("text"), and generic webhooks simultaneously
    let payload = if webhook_url.contains("discord.com/api/webhooks") {
        json!({ "content": message })
    } else if webhook_url.contains("hooks.slack.com") {
        json!({ "text": message })
    } else {
        json!({ "content": message, "text": message })
    };

    let resp = client
        .post(webhook_url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("Webhook request error: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("Webhook returned HTTP {}", resp.status()));
    }
    Ok(())
}