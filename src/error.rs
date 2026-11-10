//! Central error type mapped to RFC7807-ish JSON error responses.

use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

/// Application error. Every variant maps to a stable machine-readable code.
#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(&'static str),
    Forbidden(&'static str),
    NotFound(&'static str),
    Conflict(String),
    PayloadTooLarge(&'static str),
    TooManyRequests {
        retry_after_secs: u64,
        message: &'static str,
    },
    UnsupportedMediaType(&'static str),
    Unavailable(String),
    Cancelled,
    Internal(String),
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn conflict(msg: impl Into<String>) -> Self {
        Self::Conflict(msg.into())
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::Unavailable(msg.into())
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "bad_request",
            Self::Unauthorized(_) => "unauthorized",
            Self::Forbidden(_) => "forbidden",
            Self::NotFound(_) => "not_found",
            Self::Conflict(_) => "conflict",
            Self::PayloadTooLarge(_) => "payload_too_large",
            Self::TooManyRequests { .. } => "too_many_requests",
            Self::UnsupportedMediaType(_) => "unsupported_media_type",
            Self::Unavailable(_) => "unavailable",
            Self::Cancelled => "cancelled",
            Self::Internal(_) => "internal_error",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::Forbidden(_) => StatusCode::FORBIDDEN,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::TooManyRequests { .. } => StatusCode::TOO_MANY_REQUESTS,
            Self::UnsupportedMediaType(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Cancelled => StatusCode::CONFLICT,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Public message. Internal errors are never leaked to clients.
    pub fn public_message(&self) -> String {
        match self {
            Self::BadRequest(m) | Self::Conflict(m) | Self::Unavailable(m) | Self::Internal(m) => {
                m.clone()
            }
            Self::Unauthorized(m)
            | Self::Forbidden(m)
            | Self::NotFound(m)
            | Self::PayloadTooLarge(m)
            | Self::UnsupportedMediaType(m) => (*m).to_string(),
            Self::Cancelled => "operation cancelled".to_string(),
            Self::TooManyRequests { message, .. } => (*message).to_string(),
        }
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code(), self.public_message())
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        }
        let mut body = Json(json!({
            "error": {
                "code": self.code(),
                "message": self.public_message(),
            }
        }))
        .into_response();
        *body.status_mut() = status;
        if let AppError::TooManyRequests {
            retry_after_secs, ..
        } = &self
        {
            body.headers_mut().insert(
                header::RETRY_AFTER,
                retry_after_secs
                    .to_string()
                    .parse()
                    .unwrap_or_else(|_| header::HeaderValue::from_static("60")),
            );
        }
        body.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        body
    }
}

impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Self::NotFound("not found"),
            sqlx::Error::Database(ref db) if db.is_unique_violation() => {
                Self::Conflict("resource already exists".into())
            }
            sqlx::Error::PoolTimedOut => Self::Unavailable("database is busy, try again".into()),
            other => {
                tracing::error!(error = %other, "database error");
                Self::Internal("database error".into())
            }
        }
    }
}

impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        use std::io::ErrorKind;
        match e.kind() {
            ErrorKind::NotFound => Self::NotFound("file not found"),
            ErrorKind::PermissionDenied => Self::Forbidden("permission denied"),
            ErrorKind::AlreadyExists => Self::Conflict("already exists".into()),
            ErrorKind::InvalidInput => Self::BadRequest("invalid input".into()),
            other => {
                tracing::error!(error = %e, kind = ?other, "io error");
                Self::Internal("filesystem error".into())
            }
        }
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(e: tokio::task::JoinError) -> Self {
        tracing::error!(error = %e, "task join error");
        Self::Internal("internal task error".into())
    }
}

pub type AppResult<T> = Result<T, AppError>;
