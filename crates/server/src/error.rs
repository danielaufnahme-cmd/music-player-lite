use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// Every handler error becomes `{"error": "..."}` with a matching status code.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Unavailable(String),
    Internal(String),
    /// Any other status (401, 403, 409, 429 ...) with a message.
    Status(StatusCode, String),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    pub fn status(status: StatusCode, msg: impl Into<String>) -> Self {
        Self::Status(status, msg.into())
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(m) | Self::NotFound(m) | Self::Unavailable(m) | Self::Internal(m) | Self::Status(_, m) => f.write_str(m),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Status(s, _) => *s,
        };
        if status.is_server_error() {
            tracing::error!("{status}: {self}");
        }
        (status, Json(json!({ "error": self.to_string() }))).into_response()
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::QueryReturnedNoRows => Self::NotFound("not found".into()),
            other => Self::Internal(format!("database error: {other}")),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        Self::Internal(format!("io error: {e}"))
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        Self::Unavailable(format!("network error: {e}"))
    }
}
