//! 接口错误：统一成 `{"error": "...", "message": "..."}`。

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    /// 手机号格式不对；单独一个错误码，客户端好定位到手机号输入框。
    InvalidPhone(String),
    Unauthorized,
    /// 账号或口令不对。与「请先登录」分开，便于后台登录页显示原因。
    Credentials(String),
    /// 登录了但角色不够。
    Forbidden(String),
    NotFound,
    Conflict(String),
    /// 手机号已被别的账号占用；与用户名、邮箱重复的 `conflict` 区分开。
    PhoneTaken,
    /// 开关打开时注册没填邮箱。
    EmailRequired,
    /// 开关打开时注册没填验证码。
    EmailVerificationRequired,
    /// 开关关闭时调用了获取注册验证码。
    EmailVerificationDisabled,
    EmailCodeInvalid,
    EmailCodeExpired,
    /// 连续输错达到上限，验证码已作废。
    EmailCodeLocked,
    /// 后台保存模板时占位符写错。
    TemplateInvalid(String),
    /// 发信前发现模板缺失或占位符写错。
    EmailTemplateInvalid(String),
    EmailTemplateMissing,
    EmailNotConfigured,
    EmailUnreachable(String),
    EmailAuthFailed(String),
    EmailRejected(String),
    Quota {
        used: i64,
        quota: i64,
    },
    TooLarge(String),
    /// 同一个地址短时间内来得太多，隔一会儿再来。
    TooMany(String),
    /// 失败次数太多，账号被临时锁定。
    Locked(String),
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
            Self::InvalidPhone(message) => {
                (StatusCode::BAD_REQUEST, "invalid_phone", message, None)
            }
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
            Self::PhoneTaken => (
                StatusCode::CONFLICT,
                "phone_taken",
                "该手机号已被注册".to_string(),
                None,
            ),
            Self::EmailRequired => (
                StatusCode::BAD_REQUEST,
                "email_required",
                "请填写邮箱".to_string(),
                None,
            ),
            Self::EmailVerificationRequired => (
                StatusCode::BAD_REQUEST,
                "email_verification_required",
                "请填写邮箱验证码".to_string(),
                None,
            ),
            Self::EmailVerificationDisabled => (
                StatusCode::BAD_REQUEST,
                "email_verification_disabled",
                "当前不需要邮箱验证".to_string(),
                None,
            ),
            Self::EmailCodeInvalid => (
                StatusCode::BAD_REQUEST,
                "email_code_invalid",
                "验证码不对".to_string(),
                None,
            ),
            Self::EmailCodeExpired => (
                StatusCode::BAD_REQUEST,
                "email_code_expired",
                "验证码已过期，请重新获取".to_string(),
                None,
            ),
            Self::EmailCodeLocked => (
                StatusCode::BAD_REQUEST,
                "email_code_locked",
                "验证码输错次数太多，已作废，请重新获取".to_string(),
                None,
            ),
            Self::TemplateInvalid(message) => {
                (StatusCode::BAD_REQUEST, "template_invalid", message, None)
            }
            Self::EmailTemplateInvalid(message) => (
                StatusCode::BAD_REQUEST,
                "email_template_invalid",
                message,
                None,
            ),
            Self::EmailTemplateMissing => (
                StatusCode::SERVICE_UNAVAILABLE,
                "email_template_missing",
                "邮件模板缺失，请联系管理员".to_string(),
                None,
            ),
            Self::EmailNotConfigured => (
                StatusCode::SERVICE_UNAVAILABLE,
                "email_not_configured",
                "邮件服务尚未配置，请联系管理员".to_string(),
                None,
            ),
            Self::EmailUnreachable(detail) => {
                (StatusCode::BAD_GATEWAY, "email_unreachable", detail, None)
            }
            Self::EmailAuthFailed(detail) => {
                (StatusCode::BAD_GATEWAY, "email_auth_failed", detail, None)
            }
            Self::EmailRejected(detail) => {
                (StatusCode::BAD_GATEWAY, "email_rejected", detail, None)
            }
            Self::Quota { used, quota } => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "quota_exceeded",
                format!("同步空间已满：已用 {used} / {quota} 字节"),
                Some((used, quota)),
            ),
            Self::TooLarge(message) => (StatusCode::PAYLOAD_TOO_LARGE, "too_large", message, None),
            Self::TooMany(message) => (StatusCode::TOO_MANY_REQUESTS, "too_many", message, None),
            Self::Locked(message) => (StatusCode::TOO_MANY_REQUESTS, "locked", message, None),
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
