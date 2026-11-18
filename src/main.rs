mod api;
mod auth;
mod db;
mod error;
mod integrations;
mod models;
mod scanner;
mod state;

use std::{
    collections::HashSet, env, fs, path::Path as FsPath, sync::Arc, time::Duration as StdDuration,
};

use chrono::Utc;
use rusqlite::params;
use tokio::sync::Semaphore;
use tracing::info;

use crate::{
    db::init_db,
    scanner::{run_daily_maintenance, trigger_user_scan},
    state::AppState,
};

/// Most first-run failures on a NAS are "the data directory isn't writable by
/// the container user", so say that instead of a bare OS error.
fn fail_data_dir(path: &FsPath, err: impl std::fmt::Display) -> ! {
    panic!(
        concat!(
            "Cannot use {}: {}.
",
            "In Docker the process runs as UID 10001, so the data directory must be writable ",
            "by that UID (chown -R 10001:10001 <dir>, or chmod 777 <dir>)."
        ),
        path.display(),
        err
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
            if let Err(err) = fs::create_dir_all(parent) {
                fail_data_dir(parent, err);
            }
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

    // Opt-in: only mark the session cookie Secure when TLS terminates in front
    // of us, otherwise plain-HTTP LAN installs could never log in.
    let cookie_secure = env::var("COOKIE_SECURE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let state = AppState {
        db_path: db_path.clone(),
        http_github,
        http_ai,
        scanning_users: Arc::new(std::sync::Mutex::new(HashSet::new())),
        login_failures: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        ai_semaphore: Arc::new(Semaphore::new(1)),
        cookie_secure,
    };

    {
        let conn = match state.db() {
            Ok(conn) => conn,
            Err(err) => fail_data_dir(FsPath::new(&db_path), err.1),
        };
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
                if tick_count.is_multiple_of(8640) {
                    run_daily_maintenance(&conn);
                }
            }
        }
    });

    let app = api::router(state.clone());

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
        let _ = conn.execute_batch(
            "UPDATE user_settings SET is_scanning = 0; PRAGMA wal_checkpoint(TRUNCATE);",
        );
    }
}
