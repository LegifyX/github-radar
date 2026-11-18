use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Every handler returns `Result<_, AppError>`; this renders it as a
/// `{"error": "..."}` body with the matching status code.
#[derive(Debug)]
pub struct AppError(pub StatusCode, pub String);

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        AppError(StatusCode::BAD_REQUEST, msg.into())
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        AppError(StatusCode::UNAUTHORIZED, msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        AppError(StatusCode::FORBIDDEN, msg.into())
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        AppError(StatusCode::NOT_FOUND, msg.into())
    }

    pub fn bad_gateway(msg: impl Into<String>) -> Self {
        AppError(StatusCode::BAD_GATEWAY, msg.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = Json(json!({ "error": self.1 }));
        (self.0, body).into_response()
    }
}

impl From<rusqlite::Error> for AppError {
    /// Database failures are translated into client-meaningful status codes and
    /// the SQL detail is logged rather than sent to the browser.
    fn from(err: rusqlite::Error) -> Self {
        match err {
            rusqlite::Error::QueryReturnedNoRows => {
                AppError(StatusCode::NOT_FOUND, "Record not found".into())
            }
            rusqlite::Error::SqliteFailure(e, _)
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                AppError(StatusCode::CONFLICT, "That record already exists".into())
            }
            other => {
                tracing::error!(error = %other, "SQLite error");
                AppError(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Internal database error".into(),
                )
            }
        }
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        AppError(StatusCode::BAD_GATEWAY, err.to_string())
    }
}

impl From<bcrypt::BcryptError> for AppError {
    fn from(err: bcrypt::BcryptError) -> Self {
        tracing::error!(error = %err, "bcrypt error");
        AppError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Could not process password".into(),
        )
    }
}
