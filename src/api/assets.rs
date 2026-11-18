use axum::{
    http::header,
    response::{Html, IntoResponse},
    Json,
};
use serde_json::json;

static INDEX_HTML: &str = include_str!("../../static/index.html");
static MANIFEST_JSON: &str = include_str!("../../static/manifest.json");
static SW_JS: &str = include_str!("../../static/sw.js");
static ICON_SVG: &str = include_str!("../../static/icon.svg");

pub async fn serve_index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

pub async fn serve_manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        MANIFEST_JSON,
    )
}

pub async fn serve_sw() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        SW_JS,
    )
}

pub async fn serve_icon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], ICON_SVG)
}

pub async fn api_health() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}
