pub mod discoveries;
pub mod tracked;

use chrono::{Duration, Utc};
use rusqlite::{params, Connection};
use tracing::info;

use crate::{db::ensure_user_settings, scanner::discoveries::execute_user_scan, state::AppState};

pub fn run_daily_maintenance(conn: &Connection) {
    info!("Running daily SQLite maintenance and retention pruning...");
    let now_str = Utc::now().to_rfc3339();
    let tombstone_cutoff = (Utc::now() - Duration::days(90)).to_rfc3339();
    let _ = conn.execute(
        "DELETE FROM sessions WHERE expires_at <= ?1",
        params![now_str],
    );
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

pub fn trigger_user_scan(state: AppState, user_id: i64) {
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
            let settings = ensure_user_settings(&conn, user_id).unwrap_or_default();
            let now = Utc::now();
            let next =
                (now + Duration::minutes(settings.refresh_frequency_minutes.max(5))).to_rfc3339();
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
