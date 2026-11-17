//! 接口错误：统一成 `{"error": "...", "message": "..."}`。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized,
    /// 账号或口令不对。与「请先登录」分开，便于后台登录页显示原因。
    Credentials(String),
    /// 登录了但角色不够。
    Forbidden(String),
    NotFound,
    Conflict(String),
    Quota {
        used: i64,
        quota: i64,
    },
    TooLarge(String),
    /// 同一个地址短时间内来得太多，隔一会儿再来。
    TooMany(String),
    /// 失败次数太多，账号被临时锁定。
    Locked(String),
    /// 接口位置留好了，功能还没开放。
    NotImplemented(String),
    Internal(String),
}

impl ApiError {
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(error)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message, quota) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, "bad_request", message, None),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "请先登录".to_string(),
                None,
            ),
            Self::Credentials(message) => (StatusCode::UNAUTHORIZED, "credentials", message, None),
            Self::Forbidden(message) => (StatusCode::FORBIDDEN, "forbidden", message, None),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "没有找到".to_string(),
                None,
            ),
            Self::Conflict(message) => (StatusCode::CONFLICT, "conflict", message, None),
            Self::Quota { used, quota } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "quota_exceeded",
                format!("同步空间已满：已用 {used} / {quota} 字节"),
                Some((used, quota)),
            ),
            Self::TooLarge(message) => (StatusCode::PAYLOAD_TOO_LARGE, "too_large", message, None),
            Self::TooMany(message) => (StatusCode::TOO_MANY_REQUESTS, "too_many", message, None),
            Self::Locked(message) => (StatusCode::TOO_MANY_REQUESTS, "locked", message, None),
            Self::NotImplemented(message) => (
                StatusCode::NOT_IMPLEMENTED,
                "not_implemented",
                message,
                None,
            ),
            Self::Internal(message) => {
                // 内部细节只进日志，不回给客户端。
                tracing::error!(error = %message, "接口出错");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal",
                    "服务器出错了，请稍后再试".to_string(),
                    None,
                )
            }
        };
        let mut body = json!({ "error": code, "message": message });
        if let Some((used, quota)) = quota {
            body["used_bytes"] = json!(used);
            body["quota_bytes"] = json!(quota);
        }
        (status, Json(body)).into_response()
    }
}
