//! 邮件：SMTP 配置、两套模板、验证码、注册邮箱验证与找回密码。
//!
//! 配置和模板都存数据库，每次发信重新读取，所以后台改完立刻生效。口令只
//! 进不出；验证码只存摘要。发送记录只留时间、邮箱、用途和结果。

use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::HeaderMap;
use lettre::message::Mailbox;
use lettre::message::header::ContentType;
use lettre::transport::smtp::authentication::Credentials;
use lettre::{Message, SmtpTransport, Transport as _};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api::{self, AppState};
use crate::auth;
use crate::config::Config;
use crate::db::{Db, EmailCodeCheck, EmailSettings, EmailTemplate};
use crate::error::{ApiError, ApiResult};
use crate::util;

pub const PURPOSE_REGISTER: &str = "register_verification";
pub const PURPOSE_RESET: &str = "password_reset";
pub const PURPOSE_TEST: &str = "test";

/// 验证码有效期、长度与输错上限。
const CODE_MINUTES: i64 = 10;
const CODE_MAX_ATTEMPTS: i64 = 5;
const CODE_LENGTH: usize = 6;
/// 同一邮箱两次发送之间的最短间隔。
const RESEND_SECONDS: i64 = 60;
/// 同一邮箱每小时最多发送的验证码封数。
const HOURLY_LIMIT: i64 = 5;

const PLAIN_WARNING: &str = "明文连接会让口令与邮件内容在网络中以明文传输，仅限本机或可信内网使用";

/// 环境变量只在库为空时播种一次，之后以后台保存的值为准。
pub fn seed_from_config(config: &Config, db: &Db) -> anyhow::Result<()> {
    let Some(host) = config.smtp_host.as_deref().filter(|host| !host.is_empty()) else {
        return Ok(());
    };
    let mut settings = EmailSettings::defaults();
    settings.host = host.to_string();
    settings.port = config.smtp_port.unwrap_or(587);
    settings.encryption = match config.smtp_encryption.as_deref() {
        Some("tls") => "tls".into(),
        Some("plain") => "plain".into(),
        Some("starttls") | None => "starttls".into(),
        Some(other) => {
            tracing::warn!(
                encryption = other,
                "SHELLAS_SMTP_ENCRYPTION 不认识，改用 starttls"
            );
            "starttls".into()
        }
    };
    settings.username = config.smtp_username.clone().unwrap_or_default();
    settings.password = config.smtp_password.clone().unwrap_or_default();
    settings.from_address = config.smtp_from.clone().unwrap_or_default();
    settings.from_name = config
        .smtp_from_name
        .clone()
        .unwrap_or_else(|| "Agree ShellAs".into());
    settings.enabled = !settings.from_address.is_empty();
    settings.updated_at = util::now();
    if db.seed_email_settings(&settings)? {
        tracing::info!("已用环境变量播种 SMTP 配置");
    }
    Ok(())
}

pub fn require_verification(db: &Db) -> ApiResult<bool> {
    Ok(db
        .email_settings()?
        .is_some_and(|settings| settings.require_verification))
}

// ---- 模板 ----

/// 校验模板里的占位符。出错时返回给人看的说明。
fn placeholders(subject: &str, body: &str) -> Result<(), String> {
    for text in [subject, body] {
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '{' => {
                    let mut name = String::new();
                    let mut closed = false;
                    for ch in chars.by_ref() {
                        if ch == '}' {
                            closed = true;
                            break;
                        }
                        if ch == '{' {
                            return Err("占位符里不能再出现 {".into());
                        }
                        name.push(ch);
                    }
                    if !closed {
                        return Err("占位符 { 没有闭合".into());
                    }
                    if !matches!(name.as_str(), "username" | "code" | "minutes") {
                        return Err(format!(
                            "不支持的占位符：{{{name}}}；只允许 {{username}}、{{code}}、{{minutes}}"
                        ));
                    }
                }
                '}' => return Err("多了一个 }".into()),
                _ => {}
            }
        }
    }
    Ok(())
}

fn email_template_error(message: String) -> ApiError {
    ApiError::EmailTemplateInvalid(message)
}

fn rendered_text(text: &str, username: &str, code: &str) -> String {
    text.replace("{username}", username)
        .replace("{code}", code)
        .replace("{minutes}", &CODE_MINUTES.to_string())
}

fn render(template: &EmailTemplate, username: &str, code: &str) -> ApiResult<(String, String)> {
    if template.subject.trim().is_empty() || template.body.trim().is_empty() {
        return Err(ApiError::EmailTemplateMissing);
    }
    placeholders(&template.subject, &template.body).map_err(email_template_error)?;
    Ok((
        rendered_text(&template.subject, username, code),
        rendered_text(&template.body, username, code),
    ))
}

fn display_name(email: &str, username: Option<&str>) -> String {
    username
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| email.split('@').next().unwrap_or("用户").to_string())
}

// ---- 验证码 ----

fn new_code() -> String {
    format!("{:06}", rand::random::<u32>() % 1_000_000)
}

fn code_hash(purpose: &str, email: &str, code: &str) -> String {
    let material = format!("{purpose}\n{}\n{code}", email.trim().to_ascii_lowercase());
    util::sha256_hex(material.as_bytes())
}

fn valid_code_shape(code: &str) -> bool {
    code.len() == CODE_LENGTH && code.bytes().all(|byte| byte.is_ascii_digit())
}

fn check_code(state: &AppState, purpose: &str, email: &str, code: &str) -> ApiResult<()> {
    let code = code.trim();
    if !valid_code_shape(code) {
        return Err(ApiError::EmailCodeInvalid);
    }
    match state
        .db
        .check_email_code(purpose, email, &code_hash(purpose, email, code))?
    {
        EmailCodeCheck::Ok => Ok(()),
        EmailCodeCheck::Invalid => Err(ApiError::EmailCodeInvalid),
        EmailCodeCheck::Expired => Err(ApiError::EmailCodeExpired),
        EmailCodeCheck::Locked => Err(ApiError::EmailCodeLocked),
    }
}

pub fn verify_register_code(state: &AppState, email: &str, code: &str) -> ApiResult<()> {
    check_code(state, PURPOSE_REGISTER, email, code)
}

/// 签发并发送一封验证码。发送失败时把刚写入的码作废，用户重试即可。
async fn issue_code(state: &AppState, purpose: &str, email: &str, username: &str) -> ApiResult<()> {
    let settings = state
        .db
        .email_settings()?
        .unwrap_or_else(EmailSettings::defaults);
    if !configured(&settings) {
        return Err(ApiError::EmailNotConfigured);
    }
    let template = state
        .db
        .email_template(purpose)?
        .ok_or(ApiError::EmailTemplateMissing)?;
    if template.subject.trim().is_empty() || template.body.trim().is_empty() {
        return Err(ApiError::EmailTemplateMissing);
    }
    placeholders(&template.subject, &template.body).map_err(email_template_error)?;

    if let Some(last) = state.db.last_email_code_at(email)?
        && last > util::seconds_ago(RESEND_SECONDS)
    {
        return Err(ApiError::TooMany(
            "同一邮箱每分钟只能发送一封验证码，请稍后再试".into(),
        ));
    }
    if state
        .db
        .email_code_count_since(email, &util::seconds_ago(3600))?
        >= HOURLY_LIMIT
    {
        return Err(ApiError::TooMany(
            "同一邮箱每小时最多发送五封验证码，请稍后再试".into(),
        ));
    }

    let code = new_code();
    let (subject, body) = render(&template, username, &code)?;
    state.db.issue_email_code(
        purpose,
        email,
        &code_hash(purpose, email, &code),
        &util::later_minutes(CODE_MINUTES),
        CODE_MAX_ATTEMPTS,
    )?;
    match deliver(&settings, email, &subject, &body).await {
        Ok(()) => {
            state.db.insert_email_send(email, purpose, "ok", "")?;
            Ok(())
        }
        Err(failure) => {
            state.db.invalidate_email_codes(purpose, email)?;
            state
                .db
                .insert_email_send(email, purpose, failure.category(), failure.detail())?;
            Err(failure.into_api_error())
        }
    }
}

#[derive(Deserialize)]
pub struct EmailCodeRequest {
    pub email: String,
    #[serde(default)]
    pub username: Option<String>,
}

/// `POST /api/v1/auth/email/code`：注册前获取邮箱验证码。
pub async fn send_email_code(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<EmailCodeRequest>,
) -> ApiResult<Json<Value>> {
    let ip = api::client_ip(&headers, addr);
    if !state
        .user_limiter
        .allow(format!("email_code:{ip}"), 10, Duration::from_secs(3600))
    {
        return Err(ApiError::TooMany("请求太频繁，请稍后再试".into()));
    }
    if !require_verification(&state.db)? {
        return Err(ApiError::EmailVerificationDisabled);
    }
    let email = request.email.trim();
    if !auth::valid_email(email) {
        return Err(ApiError::BadRequest("邮箱格式不对".into()));
    }
    if state.db.user_by_email(email)?.is_some() {
        return Err(ApiError::Conflict("该邮箱已被注册".into()));
    }
    let username = display_name(email, request.username.as_deref());
    issue_code(&state, PURPOSE_REGISTER, email, &username).await?;
    Ok(Json(json!({
        "success": true,
        "expires_in": CODE_MINUTES * 60,
        "resend_after": RESEND_SECONDS,
        "message": "验证码已发送"
    })))
}

// ---- 找回密码 ----

#[derive(Deserialize)]
pub struct ForgotRequest {
    pub email: String,
}

/// `POST /api/v1/auth/password/forgot`：不论邮箱是否存在，成功响应都一样。
pub async fn forgot_password(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<ForgotRequest>,
) -> ApiResult<Json<Value>> {
    let ip = api::client_ip(&headers, addr);
    if !state
        .user_limiter
        .allow(format!("forgot:{ip}"), 10, Duration::from_secs(3600))
    {
        return Err(ApiError::TooMany("请求太频繁，请稍后再试".into()));
    }
    let email = request.email.trim();
    if !auth::valid_email(email) {
        return Err(ApiError::BadRequest("邮箱格式不对".into()));
    }
    // 全局邮件能力不可用时，所有请求统一报同样的错，不泄露账号是否存在。
    let settings = state
        .db
        .email_settings()?
        .unwrap_or_else(EmailSettings::defaults);
    if !configured(&settings) {
        return Err(ApiError::EmailNotConfigured);
    }
    let template = state
        .db
        .email_template(PURPOSE_RESET)?
        .ok_or(ApiError::EmailTemplateMissing)?;
    if template.subject.trim().is_empty() || template.body.trim().is_empty() {
        return Err(ApiError::EmailTemplateMissing);
    }
    placeholders(&template.subject, &template.body).map_err(email_template_error)?;

    if let Some(user) = state.db.user_by_email(email)? {
        match issue_code(&state, PURPOSE_RESET, email, &user.username).await {
            Ok(())
            | Err(ApiError::TooMany(_))
            | Err(ApiError::EmailUnreachable(_))
            | Err(ApiError::EmailAuthFailed(_))
            | Err(ApiError::EmailRejected(_)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Json(json!({
        "success": true,
        "expires_in": CODE_MINUTES * 60,
        "resend_after": RESEND_SECONDS,
        "message": "如果该邮箱已注册，验证码已发送"
    })))
}

#[derive(Deserialize)]
pub struct ResetRequest {
    pub email: String,
    pub code: String,
    pub password: String,
}

/// `POST /api/v1/auth/password/reset`：验证码 + 新口令，成功后所有会话失效。
pub async fn reset_password(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<ResetRequest>,
) -> ApiResult<Json<Value>> {
    let ip = api::client_ip(&headers, addr);
    if !state
        .user_limiter
        .allow(format!("reset:{ip}"), 20, Duration::from_secs(3600))
    {
        return Err(ApiError::TooMany("尝试太频繁，请稍后再试".into()));
    }
    let email = request.email.trim();
    if !auth::valid_email(email) {
        return Err(ApiError::BadRequest("邮箱格式不对".into()));
    }
    if !auth::valid_password(&request.password) {
        return Err(ApiError::BadRequest(auth::PASSWORD_RULE.into()));
    }
    let Some(user) = state.db.user_by_email(email)? else {
        return Err(ApiError::EmailCodeInvalid);
    };
    check_code(&state, PURPOSE_RESET, email, &request.code)?;
    let password = request.password;
    let password_hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    state.db.set_user_password(user.id, &password_hash)?;
    state.db.set_email_verified(user.id, true)?;
    state.db.invalidate_email_codes(PURPOSE_RESET, email)?;
    Ok(Json(json!({
        "success": true,
        "message": "口令已重置，所有已登录会话已失效，请重新登录"
    })))
}

// ---- SMTP ----

#[derive(Debug)]
enum MailFailure {
    Unreachable(String),
    AuthFailed(String),
    Rejected(String),
}

impl MailFailure {
    fn category(&self) -> &'static str {
        match self {
            Self::Unreachable(_) => "unreachable",
            Self::AuthFailed(_) => "auth_failed",
            Self::Rejected(_) => "rejected",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Unreachable(detail) | Self::AuthFailed(detail) | Self::Rejected(detail) => detail,
        }
    }

    fn into_api_error(self) -> ApiError {
        match self {
            Self::Unreachable(detail) => ApiError::EmailUnreachable(detail),
            Self::AuthFailed(detail) => ApiError::EmailAuthFailed(detail),
            Self::Rejected(detail) => ApiError::EmailRejected(detail),
        }
    }
}

fn configured(settings: &EmailSettings) -> bool {
    settings.enabled
        && !settings.host.trim().is_empty()
        && !settings.from_address.trim().is_empty()
        && matches!(settings.encryption.as_str(), "starttls" | "tls" | "plain")
        && (1..=65535).contains(&settings.port)
        && (settings.username.trim().is_empty() || !settings.password.is_empty())
}

async fn deliver(
    settings: &EmailSettings,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), MailFailure> {
    let settings = settings.clone();
    let to = to.to_string();
    let subject = subject.to_string();
    let body = body.to_string();
    tokio::task::spawn_blocking(move || deliver_blocking(&settings, &to, &subject, &body))
        .await
        .map_err(|error| MailFailure::Unreachable(format!("邮件发送线程异常退出：{error}")))?
}

fn deliver_blocking(
    settings: &EmailSettings,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), MailFailure> {
    let from_address = settings
        .from_address
        .parse()
        .map_err(|error| MailFailure::Rejected(format!("发件人地址不可用：{error}")))?;
    let to_address = to
        .parse()
        .map_err(|error| MailFailure::Rejected(format!("收件人地址不可用：{error}")))?;
    let message = Message::builder()
        .from(Mailbox::new(
            Some(settings.from_name.trim().to_string()),
            from_address,
        ))
        .to(Mailbox::new(None, to_address))
        .subject(subject)
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_string())
        .map_err(|error| MailFailure::Rejected(format!("邮件内容构造失败：{error}")))?;

    let builder = match settings.encryption.as_str() {
        "tls" => SmtpTransport::relay(&settings.host),
        "starttls" => SmtpTransport::starttls_relay(&settings.host),
        _ => Ok(SmtpTransport::builder_dangerous(&settings.host)),
    }
    .map_err(|error| MailFailure::Unreachable(format!("TLS 初始化失败：{error}")))?;
    let mut builder = builder
        .port(settings.port as u16)
        .timeout(Some(Duration::from_secs(20)));
    if !settings.username.trim().is_empty() {
        builder = builder.credentials(Credentials::new(
            settings.username.clone(),
            settings.password.clone(),
        ));
    }
    builder.build().send(&message).map(|_| ()).map_err(classify)
}

fn classify(error: lettre::transport::smtp::Error) -> MailFailure {
    let detail = error.to_string();
    if let Some(code) = error.status() {
        let number = u16::from(code);
        if (530..=539).contains(&number) {
            return MailFailure::AuthFailed(format!(
                "服务器拒绝了认证（{number}），请检查用户名和口令"
            ));
        }
        return MailFailure::Rejected(format!("服务器拒收了这封邮件（{number}）：{detail}"));
    }
    if error.is_timeout() || error.is_tls() || error.is_transport_shutdown() {
        return MailFailure::Unreachable(format!("连不上 SMTP 服务器：{detail}"));
    }
    MailFailure::Unreachable(format!("发送失败：{detail}"))
}

// ---- 后台接口 ----

fn settings_json(settings: Option<&EmailSettings>) -> Value {
    let settings = settings.cloned().unwrap_or_else(EmailSettings::defaults);
    json!({
        "enabled": settings.enabled,
        "host": settings.host,
        "port": settings.port,
        "encryption": settings.encryption,
        "username": settings.username,
        "hasPassword": !settings.password.is_empty(),
        "fromAddress": settings.from_address,
        "fromName": settings.from_name,
        "requireVerification": settings.require_verification,
        "configured": configured(&settings),
        "warning": (settings.encryption == "plain").then_some(PLAIN_WARNING),
        "updatedAt": settings.updated_at,
    })
}

pub async fn admin_settings_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    api::require_admin(&state, &headers, "email:read")?;
    Ok(Json(json!({
        "success": true,
        "data": settings_json(state.db.email_settings()?.as_ref()),
    })))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SettingsForm {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<i64>,
    #[serde(default)]
    encryption: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    clear_password: Option<bool>,
    #[serde(default)]
    from_address: Option<String>,
    #[serde(default)]
    from_name: Option<String>,
    #[serde(default)]
    require_verification: Option<bool>,
}

pub async fn admin_settings_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<SettingsForm>,
) -> ApiResult<Json<Value>> {
    api::require_admin(&state, &headers, "email:write")?;
    let mut settings = state
        .db
        .email_settings()?
        .unwrap_or_else(EmailSettings::defaults);
    if let Some(enabled) = form.enabled {
        settings.enabled = enabled;
    }
    if let Some(host) = form.host {
        settings.host = host.trim().to_string();
    }
    if let Some(port) = form.port {
        if !(1..=65535).contains(&port) {
            return Err(ApiError::BadRequest("端口要在 1–65535 之间".into()));
        }
        settings.port = port;
    }
    if let Some(encryption) = form.encryption {
        if !matches!(encryption.as_str(), "starttls" | "tls" | "plain") {
            return Err(ApiError::BadRequest(
                "加密方式只能是 starttls、tls 或 plain".into(),
            ));
        }
        settings.encryption = encryption;
    }
    if let Some(username) = form.username {
        settings.username = username.trim().to_string();
    }
    if let Some(password) = form.password
        && !password.is_empty()
    {
        settings.password = password;
    }
    if form.clear_password.unwrap_or(false) {
        settings.password.clear();
    }
    if let Some(from_address) = form.from_address {
        settings.from_address = from_address.trim().to_string();
    }
    if let Some(from_name) = form.from_name {
        settings.from_name = from_name.trim().to_string();
    }
    if let Some(require_verification) = form.require_verification {
        settings.require_verification = require_verification;
    }

    if !settings.from_address.is_empty() && !auth::valid_email(&settings.from_address) {
        return Err(ApiError::BadRequest("发件人地址不是合法邮箱".into()));
    }
    if !settings.username.is_empty() && settings.password.is_empty() {
        return Err(ApiError::BadRequest(
            "填了用户名就要设置口令；不需要认证时请清空用户名".into(),
        ));
    }
    if settings.enabled && (settings.host.is_empty() || settings.from_address.is_empty()) {
        return Err(ApiError::BadRequest(
            "启用 SMTP 前要填写主机和发件人地址".into(),
        ));
    }
    if settings.require_verification && !configured(&settings) {
        return Err(ApiError::BadRequest(
            "开启「注册需验证邮箱」前，请先把 SMTP 配置完整并启用".into(),
        ));
    }
    settings.updated_at = util::now();
    state.db.save_email_settings(&settings)?;
    Ok(Json(json!({
        "success": true,
        "data": settings_json(Some(&settings)),
    })))
}

#[derive(Deserialize)]
pub struct TestForm {
    pub to: String,
}

pub async fn admin_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<TestForm>,
) -> ApiResult<Json<Value>> {
    api::require_admin(&state, &headers, "email:write")?;
    let to = form.to.trim();
    if !auth::valid_email(to) {
        return Err(ApiError::BadRequest("收件邮箱格式不对".into()));
    }
    let settings = state
        .db
        .email_settings()?
        .unwrap_or_else(EmailSettings::defaults);
    if !configured(&settings) {
        state
            .db
            .insert_email_send(to, PURPOSE_TEST, "not_configured", "")?;
        return Ok(Json(json!({
            "success": true,
            "data": {
                "ok": false,
                "category": "not_configured",
                "detail": "SMTP 配置不完整或未启用",
                "to": to,
                "latencyMs": 0,
                "sentAt": util::now(),
            }
        })));
    }
    let started = Instant::now();
    let result = deliver(
        &settings,
        to,
        "【Agree ShellAs】SMTP 测试",
        "这是一封 SMTP 配置测试邮件，收到即说明发信正常。",
    )
    .await;
    let latency = started.elapsed().as_millis() as i64;
    let (ok, category, detail) = match result {
        Ok(()) => {
            state.db.insert_email_send(to, PURPOSE_TEST, "ok", "")?;
            (true, "ok", String::new())
        }
        Err(failure) => {
            state
                .db
                .insert_email_send(to, PURPOSE_TEST, failure.category(), failure.detail())?;
            (false, failure.category(), failure.detail().to_string())
        }
    };
    Ok(Json(json!({
        "success": true,
        "data": {
            "ok": ok,
            "category": category,
            "detail": detail,
            "to": to,
            "latencyMs": latency,
            "sentAt": util::now(),
        }
    })))
}

fn template_json(purpose: &str, template: Option<EmailTemplate>) -> Value {
    let configured = template.as_ref().is_some_and(|template| {
        !template.subject.trim().is_empty() && !template.body.trim().is_empty()
    });
    let template = template.unwrap_or(EmailTemplate {
        purpose: purpose.to_string(),
        subject: String::new(),
        body: String::new(),
        updated_at: String::new(),
    });
    json!({
        "purpose": template.purpose,
        "subject": template.subject,
        "body": template.body,
        "placeholders": ["username", "code", "minutes"],
        "configured": configured,
        "updatedAt": template.updated_at,
    })
}

pub async fn admin_templates_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    api::require_admin(&state, &headers, "email:read")?;
    let mut data = Vec::new();
    for purpose in [PURPOSE_REGISTER, PURPOSE_RESET] {
        data.push(template_json(purpose, state.db.email_template(purpose)?));
    }
    Ok(Json(json!({ "success": true, "data": data })))
}

#[derive(Deserialize)]
pub struct TemplateForm {
    pub subject: String,
    pub body: String,
}

fn valid_purpose(purpose: &str) -> bool {
    matches!(purpose, PURPOSE_REGISTER | PURPOSE_RESET)
}

pub async fn admin_template_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(purpose): Path<String>,
    Json(form): Json<TemplateForm>,
) -> ApiResult<Json<Value>> {
    api::require_admin(&state, &headers, "email:write")?;
    if !valid_purpose(&purpose) {
        return Err(ApiError::NotFound);
    }
    let subject = form.subject.trim().to_string();
    let body = form.body;
    if subject.is_empty() || body.trim().is_empty() {
        return Err(ApiError::BadRequest("主题和正文都不能为空".into()));
    }
    placeholders(&subject, &body).map_err(ApiError::TemplateInvalid)?;
    state.db.save_email_template(&purpose, &subject, &body)?;
    Ok(Json(json!({
        "success": true,
        "data": template_json(&purpose, state.db.email_template(&purpose)?),
    })))
}

#[derive(Deserialize, Default)]
pub struct PreviewForm {
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
}

pub async fn admin_template_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(purpose): Path<String>,
    Json(form): Json<PreviewForm>,
) -> ApiResult<Json<Value>> {
    api::require_admin(&state, &headers, "email:read")?;
    if !valid_purpose(&purpose) {
        return Err(ApiError::NotFound);
    }
    let template = match (form.subject, form.body) {
        (Some(subject), Some(body)) => EmailTemplate {
            purpose: purpose.clone(),
            subject,
            body,
            updated_at: String::new(),
        },
        _ => state
            .db
            .email_template(&purpose)?
            .ok_or(ApiError::EmailTemplateMissing)?,
    };
    if template.subject.trim().is_empty() || template.body.trim().is_empty() {
        return Err(ApiError::EmailTemplateMissing);
    }
    placeholders(&template.subject, &template.body).map_err(ApiError::TemplateInvalid)?;
    let subject = rendered_text(&template.subject, "alice", "123456");
    let body = rendered_text(&template.body, "alice", "123456");
    Ok(Json(json!({
        "success": true,
        "data": {
            "subject": subject,
            "body": body,
            "variables": {
                "username": "alice",
                "code": "123456",
                "minutes": CODE_MINUTES.to_string(),
            }
        }
    })))
}

#[derive(Deserialize, Default)]
pub struct SendsQuery {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    offset: Option<i64>,
}

pub async fn admin_sends(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SendsQuery>,
) -> ApiResult<Json<Value>> {
    api::require_admin(&state, &headers, "email:read")?;
    let limit = query.limit.unwrap_or(50).clamp(1, 200);
    let offset = query.offset.unwrap_or(0).max(0);
    let (total, sends) = state.db.email_sends(limit, offset)?;
    let items: Vec<Value> = sends
        .iter()
        .map(|send| {
            json!({
                "id": send.id,
                "email": send.email,
                "purpose": send.purpose,
                "result": send.result,
                "detail": send.detail,
                "createdAt": send.created_at,
            })
        })
        .collect();
    Ok(Json(json!({
        "success": true,
        "data": { "total": total, "items": items },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_only_allow_the_three_names() {
        assert!(placeholders("你好 {username}", "{code} {minutes}").is_ok());
        assert!(placeholders("{user}", "x").is_err());
        assert!(placeholders("x", "{code").is_err());
        assert!(placeholders("x", "code}").is_err());
        assert!(placeholders("x", "{{code}}").is_err());
    }

    #[test]
    fn rendering_replaces_all_variables() {
        let template = EmailTemplate {
            purpose: PURPOSE_REGISTER.into(),
            subject: "给 {username}".into(),
            body: "{code} / {minutes}".into(),
            updated_at: String::new(),
        };
        let (subject, body) = render(&template, "alice", "012345").unwrap();
        assert_eq!(subject, "给 alice");
        assert_eq!(body, "012345 / 10");
    }

    #[test]
    fn codes_keep_leading_zeros() {
        for _ in 0..50 {
            let code = new_code();
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        }
    }

    #[test]
    fn hashes_differ_by_purpose_and_email() {
        assert_ne!(
            code_hash(PURPOSE_REGISTER, "a@b.com", "123456"),
            code_hash(PURPOSE_RESET, "a@b.com", "123456")
        );
        assert_eq!(
            code_hash(PURPOSE_REGISTER, "A@B.com", "123456"),
            code_hash(PURPOSE_REGISTER, "a@b.com", "123456")
        );
    }
}
