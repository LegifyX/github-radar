pub mod admin;
pub mod assets;
pub mod auth;
pub mod backup;
pub mod queries;
pub mod repos;
pub mod settings;
pub mod state;

use axum::{
    extract::Request,
    http::{header, HeaderValue},
    middleware::Next,
    response::Response,
    routing::{delete, get, post, put},
    Router,
};

use crate::state::AppState;

/// Baseline browser hardening. The app is a single inline-script document, so
/// `'unsafe-inline'` is required for script/style, but everything else (remote
/// scripts, frames, plugin content) stays blocked.
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data: https:; media-src 'self' https:; style-src 'self' 'unsafe-inline'; script-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// The whole HTTP surface of the app: static PWA assets, auth, the dashboard
/// API, and the admin panel. Handlers live in the sibling modules.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(assets::serve_index))
        .route("/manifest.json", get(assets::serve_manifest))
        .route("/sw.js", get(assets::serve_sw))
        .route("/icon.svg", get(assets::serve_icon))
        .route("/api/health", get(assets::api_health))
        // Auth routes
        .route("/api/auth/status", get(auth::api_auth_status))
        .route("/api/auth/setup", post(auth::api_auth_setup))
        .route("/api/auth/login", post(auth::api_auth_login))
        .route("/api/auth/logout", post(auth::api_auth_logout))
        // Core dashboard state & scanner
        .route("/api/state", get(state::api_get_state))
        .route("/api/scan/force", post(state::api_force_scan))
        // Search Queries CRUD
        .route("/api/queries", post(queries::api_create_query))
        .route(
            "/api/queries/{id}",
            put(queries::api_update_query).delete(queries::api_delete_query),
        )
        // Repos & Triage
        .route("/api/repos/quick-track", post(repos::api_quick_track))
        .route("/api/repos/ignore-batch", post(repos::api_ignore_batch))
        .route("/api/repos/reorder", post(repos::api_reorder_repos))
        .route("/api/repos/{id}/action", post(repos::api_repo_action))
        .route("/api/repos/{id}/details", get(repos::api_repo_details))
        .route("/api/repos/{id}/mark-read", post(repos::api_repo_mark_read))
        .route(
            "/api/repos/{id}/refresh-readme",
            post(repos::api_repo_refresh_readme),
        )
        .route(
            "/api/repos/{id}/regenerate-ai",
            post(repos::api_repo_regenerate_ai),
        )
        .route("/api/repos/{id}", delete(repos::api_delete_repo))
        // Settings, Tests & Backup
        .route("/api/settings", put(settings::api_update_settings))
        .route("/api/settings/test-github", post(settings::api_test_github))
        .route(
            "/api/settings/test-webhook",
            post(settings::api_test_webhook),
        )
        .route("/api/settings/test-ai", post(settings::api_test_ai))
        .route("/api/backup/export", get(backup::api_export_backup))
        .route("/api/backup/import", post(backup::api_import_backup))
        // Admin endpoints
        .route(
            "/api/admin/users",
            get(admin::api_admin_list_users).post(admin::api_admin_create_user),
        )
        .route(
            "/api/admin/users/{id}",
            put(admin::api_admin_update_user).delete(admin::api_admin_delete_user),
        )
        .layer(axum::middleware::from_fn(security_headers))
        .with_state(state)
}
