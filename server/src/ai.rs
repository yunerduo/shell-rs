//! 内置 AI · 试用：一套 OpenAI 兼容上游的服务端代理与粗略计量。
//!
//! 客户端只拿自己的登录态调这里的接口，永远接触不到上游地址与 Key。
//! 上游配置存数据库，改完下一次请求立即生效；数据库里只有计数与时间，
//! 不存任何对话正文。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{FixedOffset, TimeZone as _, Utc};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::{self, AppState};
use crate::db::{AiQuotaError, AiSettings, AiUsageRow, User};
use crate::error::ApiError;
use crate::util;

/// 上游地址不可达与上游返回错误必须分开：前者是网络问题，后者才是
/// 上游/密钥的问题，后台自检和用户错误码都按这个分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpstreamCategory {
    Ok,
    Unreachable,
    Timeout,
    AuthFailed,
    Rejected,
    Error,
}

impl UpstreamCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Unreachable => "unreachable",
            Self::Timeout => "timeout",
            Self::AuthFailed => "auth_failed",
            Self::Rejected => "rejected",
            Self::Error => "upstream_error",
        }
    }
}

/// 按用户的在途请求计数与共用的 HTTP 客户端。计数在单实例内存里，
/// 够用来挡住单个用户把并发打满。连接超时可在后台调整，所以按秒数
/// 缓存几个客户端，避免每次请求都重建连接池。
pub struct AiRuntime {
    clients: Mutex<HashMap<u64, reqwest::Client>>,
    in_flight: Mutex<HashMap<i64, usize>>,
}

impl Default for AiRuntime {
    fn default() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashMap::new()),
        }
    }
}

impl AiRuntime {
    fn client(&self, connect_timeout_seconds: i64) -> reqwest::Client {
        let seconds = connect_timeout_seconds.clamp(1, 60) as u64;
        let mut clients = self
            .clients
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        clients
            .entry(seconds)
            .or_insert_with(|| {
                reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(seconds))
                    .build()
                    .expect("构建内置 AI 的 HTTP 客户端失败")
            })
            .clone()
    }

    fn acquire(self: &Arc<Self>, user_id: i64, limit: i64) -> Option<InFlight> {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let count = in_flight.entry(user_id).or_insert(0);
        if limit > 0 && *count >= limit as usize {
            return None;
        }
        *count += 1;
        Some(InFlight {
            runtime: Arc::clone(self),
            user_id,
        })
    }
}

/// 请求结束（含流式读完后）自动归还并发名额。
struct InFlight {
    runtime: Arc<AiRuntime>,
    user_id: i64,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let mut in_flight = self
            .runtime
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = in_flight.get_mut(&self.user_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                in_flight.remove(&self.user_id);
            }
        }
    }
}

// ---- 错误 ----

/// 用户侧错误统一成 OpenAI 形状：`{"error":{"message","type","code"}}`。
#[derive(Debug)]
pub struct AiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    retry_after: Option<u64>,
    used: Option<i64>,
    limit: Option<i64>,
}

impl AiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after: None,
            used: None,
            limit: None,
        }
    }

    fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(error = %error, "内置 AI 接口出错");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "服务器出错了，请稍后再试",
        )
    }

    fn from_api(error: ApiError) -> Self {
        match error {
            ApiError::Unauthorized => {
                Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "请先登录")
            }
            ApiError::Credentials(message) => {
                Self::new(StatusCode::UNAUTHORIZED, "credentials", message)
            }
            ApiError::Forbidden(message) => {
                Self::new(StatusCode::FORBIDDEN, "account_disabled", message)
            }
            other => Self::internal(format!("{other:?}")),
        }
    }

    fn disabled() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "ai_disabled",
            "内置 AI 试用当前未开放",
        )
    }

    fn trial_disabled() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "ai_trial_disabled",
            "该账号的内置 AI 试用已关闭，请联系管理员",
        )
    }

    fn not_configured() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "not_configured",
            "内置 AI 还没有配置好",
        )
    }

    fn too_concurrent() -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_concurrent",
            "请求太密集，请稍后再试",
        );
        error.retry_after = Some(1);
        error
    }

    fn daily_limit(used: i64, limit: i64) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "daily_limit_reached",
            "今日内置 AI 试用次数已用完，明天再来",
        );
        error.retry_after = Some(seconds_until_tomorrow());
        error.used = Some(used);
        error.limit = Some(limit);
        error
    }

    fn tokens_exhausted(used: i64, limit: i64) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "tokens_exhausted",
            "今日内置 AI 试用额度已用完，明天再来",
        );
        error.retry_after = Some(seconds_until_tomorrow());
        error.used = Some(used);
        error.limit = Some(limit);
        error
    }

    fn global_tokens_exhausted(used: i64, limit: i64) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "global_tokens_exhausted",
            "内置 AI 试用今天的总用量已到上限，明天再来",
        );
        error.retry_after = Some(seconds_until_tomorrow());
        error.used = Some(used);
        error.limit = Some(limit);
        error
    }

    fn too_frequent(retry_after: i64) -> Self {
        let mut error = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "too_frequent",
            "请求太频繁，请稍后再试",
        );
        error.retry_after = Some(retry_after.max(1) as u64);
        error
    }

    fn input_too_long(tokens: i64, limit: i64) -> Self {
        let mut error = Self::new(
            StatusCode::BAD_REQUEST,
            "input_too_long",
            "输入内容超过了单次请求的 token 上限",
        );
        error.used = Some(tokens);
        error.limit = Some(limit);
        error
    }
}

impl From<rusqlite::Error> for AiError {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(error)
    }
}

impl IntoResponse for AiError {
    fn into_response(self) -> Response {
        let mut body = json!({
            "error": {
                "message": self.message,
                "type": self.code,
                "code": self.code,
            }
        });
        if let Some(seconds) = self.retry_after {
            body["error"]["retryAfterSeconds"] = json!(seconds);
        }
        if let Some(used) = self.used {
            body["used"] = json!(used);
        }
        if let Some(limit) = self.limit {
            body["limit"] = json!(limit);
        }
        let mut response = (self.status, Json(body)).into_response();
        if let Some(seconds) = self.retry_after {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        response
    }
}

fn map_transport(error: &reqwest::Error) -> AiError {
    // 连接阶段的失败（DNS、拒绝连接、TLS、连接超时）都算「不可达」；
    // 只有已经连上之后的整体超时才叫「上游超时」。
    if error.is_connect() || !error.is_timeout() {
        AiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_unreachable",
            "连接上游失败，请稍后再试",
        )
    } else {
        AiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream_timeout",
            "上游响应超时，请稍后再试",
        )
    }
}

/// 从上游错误体里取一句话原因，脱敏并截断。只有后台允许「可读提示」时
/// 才返回；绝不含 Key、地址或整段响应体。
fn upstream_hint(body: &[u8], api_key: &str, readable: bool) -> Option<String> {
    if !readable {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let message = value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))?
        .trim();
    if message.is_empty() {
        return None;
    }
    let mut message = message.to_string();
    if !api_key.is_empty() {
        message = message.replace(api_key, "***");
    }
    let message: String = message.chars().take(200).collect();
    Some(message.replace(['\n', '\r'], " "))
}

fn map_status(status: StatusCode, retry_after: Option<u64>, hint: Option<&str>) -> AiError {
    match status.as_u16() {
        401 | 403 => AiError::new(
            StatusCode::BAD_GATEWAY,
            "upstream_auth_failed",
            "服务端上游密钥无效，请联系管理员",
        ),
        _ => {
            let suffix = hint
                .map(|hint| format!("（上游：{hint}）"))
                .unwrap_or_default();
            let mut error = AiError::new(
                StatusCode::BAD_GATEWAY,
                if status.as_u16() == 429 {
                    "upstream_rate_limited"
                } else {
                    "upstream_rejected"
                },
                format!("上游拒绝了这次请求，请稍后再试{suffix}"),
            );
            error.retry_after = retry_after;
            error
        }
    }
}

// ---- 时间与估算 ----

/// 日界按东八区算；库里的时间戳仍是 UTC。
fn shanghai_day() -> String {
    Utc::now()
        .with_timezone(&FixedOffset::east_opt(8 * 3600).expect("东八区"))
        .format("%Y-%m-%d")
        .to_string()
}

fn seconds_until_tomorrow() -> u64 {
    let offset = FixedOffset::east_opt(8 * 3600).expect("东八区");
    let now = Utc::now().with_timezone(&offset);
    let tomorrow = (now.date_naive() + chrono::Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .expect("午夜总是有效");
    let tomorrow = offset
        .from_local_datetime(&tomorrow)
        .single()
        .expect("东八区没有夏令时");
    (tomorrow - now).num_seconds().max(1) as u64
}

/// 按字符估算：`ceil(字符数 / 4)`；一张图片按 1000 token 记。
fn estimate_chars(chars: i64) -> i64 {
    if chars <= 0 { 0 } else { (chars + 3) / 4 }
}

fn estimate_messages_tokens(messages: &[Value]) -> i64 {
    let mut chars = 0i64;
    let mut images = 0i64;
    for message in messages {
        match message.get("content") {
            Some(Value::String(text)) => chars += text.chars().count() as i64,
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                chars += text.chars().count() as i64;
                            }
                        }
                        Some("image_url") => images += 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                if let Some(arguments) = call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                {
                    chars += arguments.chars().count() as i64;
                }
            }
        }
    }
    estimate_chars(chars) + images * 1000
}

/// 从上游响应里取真实 usage；没有就按输出文本估算。
fn usage_from_response(body: &[u8], input_estimate: i64) -> (i64, i64) {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return (input_estimate, 0);
    };
    let prompt = value
        .get("usage")
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(Value::as_i64);
    let completion = value
        .get("usage")
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(Value::as_i64);
    let mut chars = 0i64;
    if let Some(choices) = value.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(text) = choice
                .get("message")
                .and_then(|message| message.get("content"))
                .and_then(Value::as_str)
            {
                chars += text.chars().count() as i64;
            }
            if let Some(calls) = choice
                .get("message")
                .and_then(|message| message.get("tool_calls"))
                .and_then(Value::as_array)
            {
                for call in calls {
                    if let Some(arguments) = call
                        .get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        chars += arguments.chars().count() as i64;
                    }
                }
            }
        }
    }
    (
        prompt.unwrap_or(input_estimate),
        completion.unwrap_or_else(|| estimate_chars(chars)),
    )
}

#[derive(Default)]
struct StreamUsage {
    buffer: Vec<u8>,
    chars: i64,
    prompt: Option<i64>,
    completion: Option<i64>,
}

impl StreamUsage {
    fn feed(&mut self, chunk: &[u8]) {
        self.buffer.extend_from_slice(chunk);
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                self.absorb(&value);
            }
        }
    }

    fn absorb(&mut self, value: &Value) {
        if let Some(usage) = value.get("usage") {
            if let Some(tokens) = usage.get("prompt_tokens").and_then(Value::as_i64) {
                self.prompt = Some(tokens);
            }
            if let Some(tokens) = usage.get("completion_tokens").and_then(Value::as_i64) {
                self.completion = Some(tokens);
            }
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let Some(delta) = choice.get("delta") else {
                    continue;
                };
                if let Some(text) = delta.get("content").and_then(Value::as_str) {
                    self.chars += text.chars().count() as i64;
                }
                if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        if let Some(arguments) = call
                            .get("function")
                            .and_then(|function| function.get("arguments"))
                            .and_then(Value::as_str)
                        {
                            self.chars += arguments.chars().count() as i64;
                        }
                    }
                }
            }
        }
    }

    fn settle(&self, input_estimate: i64) -> (i64, i64) {
        (
            self.prompt.unwrap_or(input_estimate),
            self.completion
                .unwrap_or_else(|| estimate_chars(self.chars)),
        )
    }
}

// ---- 上游调用 ----

fn upstream_url(settings: &AiSettings) -> String {
    format!(
        "{}/chat/completions",
        settings.base_url.trim().trim_end_matches('/')
    )
}

/// 自定义请求头只允许非敏感头：认证、Cookie、内容协商和逐跳头一律
/// 拒绝，尤其是 `Authorization` 不能被覆盖。
fn is_sensitive_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    const DENY: &[&str] = &[
        "authorization",
        "proxy-authorization",
        "cookie",
        "set-cookie",
        "host",
        "content-length",
        "content-type",
        "connection",
        "transfer-encoding",
        "upgrade",
        "te",
        "trailer",
        "expect",
        "accept-encoding",
    ];
    DENY.contains(&name.as_str())
        || ["token", "key", "secret", "password", "session", "auth"]
            .iter()
            .any(|part| name.contains(part))
}

/// 解析并校验自定义请求头。保存接口用它给出明确错误，发送前再用一次
/// （库里的历史值也可能是手改的）。
fn parse_custom_headers(raw: &str) -> Result<Vec<(String, String)>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value =
        serde_json::from_str(raw).map_err(|_| "自定义请求头必须是 JSON 对象".to_string())?;
    let Value::Object(map) = value else {
        return Err("自定义请求头必须是 JSON 对象".into());
    };
    let mut headers = Vec::with_capacity(map.len());
    for (name, value) in map {
        let name = name.trim().to_string();
        if HeaderName::from_bytes(name.as_bytes()).is_err() {
            return Err(format!("请求头名称不合法：{name}"));
        }
        if is_sensitive_header(&name) {
            return Err(format!("请求头 {name} 属于敏感头，不允许自定义"));
        }
        let Some(value) = value.as_str() else {
            return Err(format!("请求头 {name} 的值必须是字符串"));
        };
        if value.contains(['\r', '\n']) {
            return Err(format!("请求头 {name} 的值不能换行"));
        }
        headers.push((name, value.to_string()));
    }
    Ok(headers)
}

/// 指数退避：基础毫秒数 × 2^(n-1)，最多 10 秒。
fn retry_backoff(settings: &AiSettings, attempt: u32) -> Duration {
    let base = settings.retry_backoff_ms.max(0) as u64;
    let shift = attempt.saturating_sub(1).min(5);
    Duration::from_millis(base.saturating_mul(1u64 << shift).min(10_000))
}

async fn send_upstream(
    runtime: &AiRuntime,
    settings: &AiSettings,
    payload: &Value,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = upstream_url(settings);
    let client = runtime.client(settings.connect_timeout_seconds);
    let timeout = Duration::from_secs(settings.timeout_seconds.max(1) as u64);
    let headers = parse_custom_headers(&settings.custom_headers).unwrap_or_default();
    let mut attempt = 0u32;
    loop {
        let mut request = client.post(&url).timeout(timeout).json(payload);
        if !settings.api_key.is_empty() {
            request = request.bearer_auth(&settings.api_key);
        }
        for (name, value) in &headers {
            // 校验阶段已经拦过敏感头；这里再挡一次，防止旧数据或手改库。
            if is_sensitive_header(name) {
                continue;
            }
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(value),
            ) {
                request = request.header(name, value);
            }
        }
        match request.send().await {
            Ok(response) => {
                if response.status().is_server_error() && attempt < settings.retry_count as u32 {
                    attempt += 1;
                    tokio::time::sleep(retry_backoff(settings, attempt)).await;
                    continue;
                }
                return Ok(response);
            }
            Err(error) => {
                // 只重试连接阶段的失败；整体超时不再重试。
                if error.is_connect() && attempt < settings.retry_count as u32 {
                    attempt += 1;
                    tokio::time::sleep(retry_backoff(settings, attempt)).await;
                    continue;
                }
                return Err(error);
            }
        }
    }
}

fn parse_retry_after(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn classify_status(status: StatusCode) -> UpstreamCategory {
    match status.as_u16() {
        401 | 403 => UpstreamCategory::AuthFailed,
        429 => UpstreamCategory::Rejected,
        400..=499 => UpstreamCategory::Rejected,
        _ => UpstreamCategory::Error,
    }
}

// ---- 用户接口 ----

pub async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AiError> {
    let user = api::require_user(&state, &headers).map_err(AiError::from_api)?;
    let day = shanghai_day();
    let usage = state.db.ai_usage(user.id, &day)?;
    let global_tokens = state.db.ai_global_tokens(&day)?;
    let settings = state.db.ai_settings()?;
    let user_enabled = state.db.ai_user_enabled(user.id)?;

    let mut reason: Option<&str> = None;
    let available = match settings.as_ref() {
        None => {
            reason = Some("not_configured");
            false
        }
        Some(settings) if !settings.enabled => {
            reason = Some("disabled");
            false
        }
        Some(settings) if settings.base_url.is_empty() || settings.model.is_empty() => {
            reason = Some("not_configured");
            false
        }
        Some(_) if !user_enabled => {
            reason = Some("user_disabled");
            false
        }
        Some(settings)
            if settings.daily_request_limit > 0
                && usage.requests_today >= settings.daily_request_limit =>
        {
            reason = Some("daily_limit_reached");
            false
        }
        Some(settings)
            if settings.daily_token_limit > 0
                && usage.tokens_today >= settings.daily_token_limit =>
        {
            reason = Some("tokens_exhausted");
            false
        }
        Some(settings)
            if settings.global_daily_token_limit > 0
                && global_tokens >= settings.global_daily_token_limit =>
        {
            reason = Some("global_limit_reached");
            false
        }
        Some(_) => true,
    };
    let settings = settings.unwrap_or_else(AiSettings::defaults);
    let remaining = if settings.daily_request_limit > 0 {
        Some((settings.daily_request_limit - usage.requests_today).max(0))
    } else {
        None
    };
    let tokens_remaining = if settings.daily_token_limit > 0 {
        Some((settings.daily_token_limit - usage.tokens_today).max(0))
    } else {
        None
    };
    Ok(Json(json!({
        "available": available,
        "reason": reason,
        "model": settings.model,
        "maxContextTokens": settings.max_context_tokens,
        "maxOutputTokens": settings.max_output_tokens,
        "maxInputTokens": settings.max_input_tokens,
        "dailyRequestLimit": settings.daily_request_limit,
        "dailyTokenLimit": settings.daily_token_limit,
        "requestsUsedToday": usage.requests_today,
        "requestsRemainingToday": remaining,
        "tokensUsedToday": usage.tokens_today,
        "tokensRemainingToday": tokens_remaining,
    })))
}

pub async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AiError> {
    let user = api::require_user(&state, &headers).map_err(AiError::from_api)?;
    let Some(settings) = state.db.ai_settings()? else {
        return Err(AiError::not_configured());
    };
    if !settings.enabled {
        return Err(AiError::disabled());
    }
    if settings.base_url.is_empty() || settings.model.is_empty() {
        return Err(AiError::not_configured());
    }
    if !state.db.ai_user_enabled(user.id)? {
        return Err(AiError::trial_disabled());
    }
    if body.len() as i64 > settings.max_request_bytes {
        return Err(AiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "请求体太大了",
        ));
    }
    let Value::Object(mut payload) = serde_json::from_slice::<Value>(&body).map_err(|_| {
        AiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "请求体不是合法的 JSON 对象",
        )
    })?
    else {
        return Err(AiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "请求体必须是 JSON 对象",
        ));
    };

    validate_model(&payload, &settings)?;
    let messages = payload
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            AiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "messages 必须是非空数组",
            )
        })?;
    if messages.is_empty() {
        return Err(AiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "messages 必须是非空数组",
        ));
    }
    if let Some(n) = payload.get("n")
        && n.as_i64() != Some(1)
    {
        return Err(AiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_parameter",
            "内置 AI 只支持 n = 1",
        ));
    }
    if let Some(model) = payload.get("model")
        && !model.is_string()
        && !model.is_null()
    {
        return Err(AiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "model 必须是字符串",
        ));
    }

    let stream = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let input_tokens = estimate_messages_tokens(messages);
    if settings.max_input_tokens > 0 && input_tokens > settings.max_input_tokens {
        return Err(AiError::input_too_long(
            input_tokens,
            settings.max_input_tokens,
        ));
    }
    let max_output = clamp_output(&mut payload, &settings);
    if input_tokens + max_output > settings.max_context_tokens {
        return Err(AiError::new(
            StatusCode::BAD_REQUEST,
            "context_too_long",
            "对话超过了内置 AI 的上下文上限",
        ));
    }

    payload.insert("model".into(), json!(settings.model.clone()));
    payload.insert("max_tokens".into(), json!(max_output));
    if stream && settings.stream_include_usage {
        let options = payload.entry("stream_options").or_insert_with(|| json!({}));
        if let Some(options) = options.as_object_mut() {
            options.insert("include_usage".into(), json!(true));
        }
    }
    apply_sampling(&mut payload, &settings)?;

    let day = shanghai_day();
    let throttle_key = throttle_key(&user, &settings.min_interval_scope);
    let usage = state
        .db
        .ai_claim_request(
            user.id,
            &day,
            settings.daily_request_limit,
            settings.daily_token_limit,
            settings.global_daily_token_limit,
            &throttle_key,
            settings.min_interval_seconds,
        )
        .map_err(|error| match error {
            AiQuotaError::Requests { used, limit } => AiError::daily_limit(used, limit),
            AiQuotaError::Tokens { used, limit } => AiError::tokens_exhausted(used, limit),
            AiQuotaError::GlobalTokens { used, limit } => {
                AiError::global_tokens_exhausted(used, limit)
            }
            AiQuotaError::Interval { retry_after } => AiError::too_frequent(retry_after),
            AiQuotaError::Sql(error) => AiError::internal(error),
        })?;
    tracing::info!(
        user = %user.username,
        requests_today = usage.requests_today,
        stream,
        "内置 AI 请求已受理"
    );

    let guard = state
        .ai
        .acquire(user.id, settings.max_concurrent_per_user)
        .ok_or_else(AiError::too_concurrent)?;
    let payload = Value::Object(payload);

    let response = match send_upstream(&state.ai, &settings, &payload).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(user = %user.username, error = %error, "内置 AI 上游连接失败");
            return Err(map_transport(&error));
        }
    };
    let upstream_status = response.status();
    if !upstream_status.is_success() {
        let retry_after = parse_retry_after(response.headers());
        // 只有后台允许可读提示时才读错误体；读不完也无所谓。
        let hint = response.bytes().await.ok().and_then(|body| {
            upstream_hint(&body, &settings.api_key, settings.readable_upstream_hint)
        });
        tracing::warn!(
            user = %user.username,
            status = upstream_status.as_u16(),
            "内置 AI 上游返回错误"
        );
        return Err(map_status(upstream_status, retry_after, hint.as_deref()));
    }

    if stream {
        return Ok(stream_response(
            state.db.clone(),
            user.id,
            day,
            input_tokens,
            guard,
            response,
        ));
    }

    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(user = %user.username, error = %error, "内置 AI 读取上游响应失败");
            return Err(AiError::new(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "上游响应读取失败，请稍后再试",
            ));
        }
    };
    let (input_tokens, output_tokens) = usage_from_response(&bytes, input_tokens);
    if let Err(error) = state
        .db
        .ai_record_tokens(user.id, &day, input_tokens, output_tokens)
    {
        tracing::warn!(error = %error, "内置 AI 用量记录失败");
    }
    drop(guard);

    let mut response = Response::new(Body::from(bytes));
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    Ok(response)
}

fn validate_model(payload: &Map<String, Value>, settings: &AiSettings) -> Result<(), AiError> {
    match payload.get("model") {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(model)) if model.is_empty() || model == &settings.model => Ok(()),
        Some(_) => Err(AiError::new(
            StatusCode::BAD_REQUEST,
            "model_not_allowed",
            "只能使用后台配置的模型",
        )),
    }
}

/// 输出上限由后台配置夹紧，客户端传更大的值也不会生效。
fn clamp_output(payload: &mut Map<String, Value>, settings: &AiSettings) -> i64 {
    let configured = settings.max_output_tokens.max(1);
    for key in ["max_tokens", "max_completion_tokens"] {
        let requested = payload.get(key).and_then(Value::as_i64);
        if let Some(requested) = requested {
            let clamped = requested.clamp(1, configured);
            payload.insert(key.into(), json!(clamped));
        }
    }
    payload
        .get("max_tokens")
        .and_then(Value::as_i64)
        .unwrap_or(configured)
}

/// 采样参数：先补后台默认值，再校验范围；默认不同时传 temperature 与
/// top_p（有些模型会直接拒绝这种组合），只保留 temperature。
fn apply_sampling(payload: &mut Map<String, Value>, settings: &AiSettings) -> Result<(), AiError> {
    for key in ["temperature", "top_p"] {
        if payload.get(key).is_some_and(Value::is_null) {
            payload.remove(key);
        }
    }
    if let Some(value) = payload.get("temperature") {
        let Some(value) = value.as_f64() else {
            return Err(AiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_sampling",
                "temperature 必须是 0 到 2 之间的数字",
            ));
        };
        if !(0.0..=2.0).contains(&value) {
            return Err(AiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_sampling",
                "temperature 要在 0 到 2 之间",
            ));
        }
    }
    if let Some(value) = payload.get("top_p") {
        let Some(value) = value.as_f64() else {
            return Err(AiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_sampling",
                "top_p 必须是 0 到 1 之间的数字",
            ));
        };
        if !(0.0..=1.0).contains(&value) {
            return Err(AiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_sampling",
                "top_p 要在 0 到 1 之间",
            ));
        }
    }
    if !payload.contains_key("temperature")
        && let Some(temperature) = settings.temperature
    {
        payload.insert("temperature".into(), json!(temperature));
    }
    if !payload.contains_key("top_p")
        && let Some(top_p) = settings.top_p
    {
        payload.insert("top_p".into(), json!(top_p));
    }
    if !settings.sampling_both
        && payload.contains_key("temperature")
        && payload.contains_key("top_p")
    {
        payload.remove("top_p");
    }
    Ok(())
}

/// 最小间隔按用户还是按邮箱计；按邮箱时同一邮箱的多个账号共用节流。
fn throttle_key(user: &User, scope: &str) -> String {
    if scope == "email"
        && let Some(email) = user.email.as_deref()
    {
        let email = email.trim();
        if !email.is_empty() {
            return format!("email:{}", email.to_ascii_lowercase());
        }
    }
    format!("user:{}", user.id)
}

fn stream_response(
    db: Arc<crate::db::Db>,
    user_id: i64,
    day: String,
    input_tokens: i64,
    guard: InFlight,
    upstream: reqwest::Response,
) -> Response {
    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(16);
    tokio::spawn(async move {
        let _guard = guard;
        let mut usage = StreamUsage::default();
        let mut stream = upstream.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => {
                    usage.feed(&bytes);
                    if sender.send(Ok(bytes)).await.is_err() {
                        // 客户端断开了，不再往上读，但已读到的量照样记。
                        break;
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "内置 AI 流式读取中断");
                    let frame = format!(
                        "data: {}\n\n",
                        json!({
                            "error": {
                                "message": "上游连接中断，请重试",
                                "type": "upstream_error",
                                "code": "upstream_error",
                            }
                        })
                    );
                    let _ = sender.send(Ok(Bytes::from(frame))).await;
                    break;
                }
            }
        }
        let (input, output) = usage.settle(input_tokens);
        if let Err(error) = db.ai_record_tokens(user_id, &day, input, output) {
            tracing::warn!(error = %error, "内置 AI 流式用量记录失败");
        }
    });

    let body = Body::from_stream(futures_util::stream::unfold(
        receiver,
        |mut receiver| async move { receiver.recv().await.map(|item| (item, receiver)) },
    ));
    let mut response = Response::new(body);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response.headers_mut().insert(
        header::HeaderName::from_static("x-accel-buffering"),
        HeaderValue::from_static("no"),
    );
    response
}

// ---- 后台接口 ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsDto {
    enabled: bool,
    base_url: String,
    model: String,
    has_api_key: bool,
    max_context_tokens: i64,
    max_output_tokens: i64,
    max_input_tokens: i64,
    temperature: Option<f64>,
    top_p: Option<f64>,
    sampling_both: bool,
    stream_include_usage: bool,
    timeout_seconds: i64,
    connect_timeout_seconds: i64,
    retry_count: i64,
    retry_backoff_ms: i64,
    custom_headers: Value,
    daily_request_limit: i64,
    daily_token_limit: i64,
    global_daily_token_limit: i64,
    max_concurrent_per_user: i64,
    max_request_bytes: i64,
    min_interval_seconds: i64,
    min_interval_scope: String,
    readable_upstream_hint: bool,
    updated_at: String,
}

fn settings_dto(settings: &AiSettings) -> SettingsDto {
    SettingsDto {
        enabled: settings.enabled,
        base_url: settings.base_url.clone(),
        model: settings.model.clone(),
        has_api_key: !settings.api_key.is_empty(),
        max_context_tokens: settings.max_context_tokens,
        max_output_tokens: settings.max_output_tokens,
        max_input_tokens: settings.max_input_tokens,
        temperature: settings.temperature,
        top_p: settings.top_p,
        sampling_both: settings.sampling_both,
        stream_include_usage: settings.stream_include_usage,
        timeout_seconds: settings.timeout_seconds,
        connect_timeout_seconds: settings.connect_timeout_seconds,
        retry_count: settings.retry_count,
        retry_backoff_ms: settings.retry_backoff_ms,
        custom_headers: serde_json::from_str(&settings.custom_headers)
            .unwrap_or_else(|_| json!({})),
        daily_request_limit: settings.daily_request_limit,
        daily_token_limit: settings.daily_token_limit,
        global_daily_token_limit: settings.global_daily_token_limit,
        max_concurrent_per_user: settings.max_concurrent_per_user,
        max_request_bytes: settings.max_request_bytes,
        min_interval_seconds: settings.min_interval_seconds,
        min_interval_scope: settings.min_interval_scope.clone(),
        readable_upstream_hint: settings.readable_upstream_hint,
        updated_at: settings.updated_at.clone(),
    }
}

pub async fn admin_settings_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    api::require_admin(&state, &headers, "ai:read")?;
    let settings = state.db.ai_settings()?.unwrap_or_else(AiSettings::defaults);
    Ok(Json(json!({
        "success": true,
        "data": settings_dto(&settings),
    })))
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct SettingsForm {
    enabled: Option<bool>,
    base_url: Option<String>,
    api_key: Option<String>,
    clear_api_key: Option<bool>,
    model: Option<String>,
    max_context_tokens: Option<i64>,
    max_output_tokens: Option<i64>,
    max_input_tokens: Option<i64>,
    temperature: Option<f64>,
    clear_temperature: Option<bool>,
    top_p: Option<f64>,
    clear_top_p: Option<bool>,
    sampling_both: Option<bool>,
    stream_include_usage: Option<bool>,
    timeout_seconds: Option<i64>,
    connect_timeout_seconds: Option<i64>,
    retry_count: Option<i64>,
    retry_backoff_ms: Option<i64>,
    custom_headers: Option<Value>,
    daily_request_limit: Option<i64>,
    daily_token_limit: Option<i64>,
    global_daily_token_limit: Option<i64>,
    max_concurrent_per_user: Option<i64>,
    max_request_bytes: Option<i64>,
    min_interval_seconds: Option<i64>,
    min_interval_scope: Option<String>,
    readable_upstream_hint: Option<bool>,
}

fn merge_settings(form: SettingsForm, current: AiSettings) -> Result<AiSettings, String> {
    let mut settings = current;
    if let Some(enabled) = form.enabled {
        settings.enabled = enabled;
    }
    if let Some(base_url) = form.base_url {
        settings.base_url = base_url.trim().trim_end_matches('/').to_string();
    }
    if form.clear_api_key == Some(true) {
        settings.api_key.clear();
    } else if let Some(api_key) = form.api_key {
        let api_key = api_key.trim();
        if !api_key.is_empty() {
            settings.api_key = api_key.to_string();
        }
    }
    if let Some(model) = form.model {
        settings.model = model.trim().to_string();
    }
    if let Some(value) = form.max_context_tokens {
        settings.max_context_tokens = value;
    }
    if let Some(value) = form.max_output_tokens {
        settings.max_output_tokens = value;
    }
    if let Some(value) = form.max_input_tokens {
        settings.max_input_tokens = value;
    }
    if form.clear_temperature == Some(true) {
        settings.temperature = None;
    } else if let Some(temperature) = form.temperature {
        settings.temperature = Some(temperature);
    }
    if form.clear_top_p == Some(true) {
        settings.top_p = None;
    } else if let Some(top_p) = form.top_p {
        settings.top_p = Some(top_p);
    }
    if let Some(value) = form.sampling_both {
        settings.sampling_both = value;
    }
    if let Some(value) = form.stream_include_usage {
        settings.stream_include_usage = value;
    }
    if let Some(value) = form.timeout_seconds {
        settings.timeout_seconds = value;
    }
    if let Some(value) = form.connect_timeout_seconds {
        settings.connect_timeout_seconds = value;
    }
    if let Some(value) = form.retry_count {
        settings.retry_count = value;
    }
    if let Some(value) = form.retry_backoff_ms {
        settings.retry_backoff_ms = value;
    }
    if let Some(value) = form.custom_headers {
        settings.custom_headers = value.to_string();
    }
    if let Some(value) = form.daily_request_limit {
        settings.daily_request_limit = value;
    }
    if let Some(value) = form.daily_token_limit {
        settings.daily_token_limit = value;
    }
    if let Some(value) = form.global_daily_token_limit {
        settings.global_daily_token_limit = value;
    }
    if let Some(value) = form.max_concurrent_per_user {
        settings.max_concurrent_per_user = value;
    }
    if let Some(value) = form.max_request_bytes {
        settings.max_request_bytes = value;
    }
    if let Some(value) = form.min_interval_seconds {
        settings.min_interval_seconds = value;
    }
    if let Some(value) = form.min_interval_scope {
        settings.min_interval_scope = value.trim().to_ascii_lowercase();
    }
    if let Some(value) = form.readable_upstream_hint {
        settings.readable_upstream_hint = value;
    }
    validate_settings(&settings)?;
    settings.updated_at = util::now();
    Ok(settings)
}

fn validate_settings(settings: &AiSettings) -> Result<(), String> {
    if !settings.base_url.is_empty() {
        if !(settings.base_url.starts_with("http://") || settings.base_url.starts_with("https://"))
        {
            return Err("上游地址要以 http:// 或 https:// 开头".into());
        }
        if settings.base_url.contains('?') || settings.base_url.contains('#') {
            return Err("上游地址里不要带查询串或锚点".into());
        }
    }
    if settings.enabled && (settings.base_url.is_empty() || settings.model.is_empty()) {
        return Err("启用前要填好上游地址和模型 id".into());
    }
    if settings.max_context_tokens < 1024 {
        return Err("最大上下文不能小于 1024".into());
    }
    if settings.max_output_tokens < 1 || settings.max_output_tokens > settings.max_context_tokens {
        return Err("输出上限要在 1 到最大上下文之间".into());
    }
    if settings.max_input_tokens < 1 || settings.max_input_tokens > settings.max_context_tokens {
        return Err("输入上限要在 1 到最大上下文之间".into());
    }
    if let Some(temperature) = settings.temperature
        && !(0.0..=2.0).contains(&temperature)
    {
        return Err("温度要在 0 到 2 之间".into());
    }
    if let Some(top_p) = settings.top_p
        && !(0.0..=1.0).contains(&top_p)
    {
        return Err("top_p 要在 0 到 1 之间".into());
    }
    if !(5..=600).contains(&settings.timeout_seconds) {
        return Err("超时要在 5 到 600 秒之间".into());
    }
    if !(1..=60).contains(&settings.connect_timeout_seconds)
        || settings.connect_timeout_seconds > settings.timeout_seconds
    {
        return Err("连接超时要在 1 到 60 秒之间，且不能超过整体超时".into());
    }
    if !(0..=3).contains(&settings.retry_count) {
        return Err("重试次数要在 0 到 3 之间".into());
    }
    if !(0..=5000).contains(&settings.retry_backoff_ms) {
        return Err("重试退避要在 0 到 5000 毫秒之间".into());
    }
    parse_custom_headers(&settings.custom_headers)?;
    if settings.daily_request_limit < 0
        || settings.daily_token_limit < 0
        || settings.global_daily_token_limit < 0
    {
        return Err("每日限额不能是负数".into());
    }
    if !(1..=100).contains(&settings.max_concurrent_per_user) {
        return Err("单用户并发要在 1 到 100 之间".into());
    }
    if !(4096..=1024 * 1024).contains(&settings.max_request_bytes) {
        return Err("请求体上限要在 4 KiB 到 1 MiB 之间".into());
    }
    if !(0..=3600).contains(&settings.min_interval_seconds) {
        return Err("最小请求间隔要在 0 到 3600 秒之间".into());
    }
    if settings.min_interval_scope != "user" && settings.min_interval_scope != "email" {
        return Err("最小请求间隔只能按 user 或 email 计".into());
    }
    Ok(())
}

pub async fn admin_settings_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<SettingsForm>,
) -> Result<Json<Value>, ApiError> {
    api::require_admin(&state, &headers, "ai:write")?;
    let current = state.db.ai_settings()?.unwrap_or_else(AiSettings::defaults);
    let settings = merge_settings(form, current).map_err(ApiError::BadRequest)?;
    state.db.save_ai_settings(&settings)?;
    tracing::info!(
        enabled = settings.enabled,
        model = %settings.model,
        "后台保存了内置 AI 配置"
    );
    Ok(Json(json!({
        "success": true,
        "data": settings_dto(&settings),
    })))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TestResult {
    ok: bool,
    category: &'static str,
    detail: String,
    latency_ms: u64,
    checked_at: String,
}

/// 后台自检：始终 200 返回诊断结果，不把上游响应体或 Key 带出来。
pub async fn admin_test(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    api::require_admin(&state, &headers, "ai:write")?;
    let started = std::time::Instant::now();
    let Some(settings) = state.db.ai_settings()? else {
        return Ok(Json(json!({
            "success": true,
            "data": TestResult {
                ok: false,
                category: "not_configured",
                detail: "还没有保存上游配置".into(),
                latency_ms: 0,
                checked_at: util::now(),
            },
        })));
    };
    if settings.base_url.is_empty() || settings.model.is_empty() {
        return Ok(Json(json!({
            "success": true,
            "data": TestResult {
                ok: false,
                category: "not_configured",
                detail: "上游地址或模型 id 还没填".into(),
                latency_ms: 0,
                checked_at: util::now(),
            },
        })));
    }

    let payload = json!({
        "model": settings.model.clone(),
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": false,
    });
    let (ok, category, detail) = match send_upstream(&state.ai, &settings, &payload).await {
        Ok(response) => {
            let status = response.status();
            if status.is_success() {
                (
                    true,
                    UpstreamCategory::Ok,
                    "上游接受了配置与密钥".to_string(),
                )
            } else {
                let category = classify_status(status);
                let hint = response.bytes().await.ok().and_then(|body| {
                    upstream_hint(&body, &settings.api_key, settings.readable_upstream_hint)
                });
                let mut detail = match category {
                    UpstreamCategory::AuthFailed => {
                        format!("上游返回 {}，密钥无效或无权限", status.as_u16())
                    }
                    UpstreamCategory::Rejected => {
                        format!("上游拒绝了请求（HTTP {}），请检查模型 id", status.as_u16())
                    }
                    _ => format!("上游返回错误（HTTP {}）", status.as_u16()),
                };
                if let Some(hint) = hint {
                    detail.push_str(&format!("：{hint}"));
                }
                (false, category, detail)
            }
        }
        Err(error) => {
            // 这里只说网络层的原因，不回显带地址的完整错误。
            if error.is_timeout() && !error.is_connect() {
                (
                    false,
                    UpstreamCategory::Timeout,
                    "上游请求超时，请检查地址与网络".to_string(),
                )
            } else {
                (
                    false,
                    UpstreamCategory::Unreachable,
                    "无法连接到上游（网络不可达、拒绝连接或 TLS 失败）".to_string(),
                )
            }
        }
    };
    Ok(Json(json!({
        "success": true,
        "data": TestResult {
            ok,
            category: category.as_str(),
            detail,
            latency_ms: started.elapsed().as_millis() as u64,
            checked_at: util::now(),
        },
    })))
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct UsageQuery {
    page: Option<i64>,
    page_size: Option<i64>,
    search: Option<String>,
}

fn usage_row_json(row: &AiUsageRow) -> Value {
    json!({
        "userId": row.id,
        "username": row.username,
        "accountDisabled": row.account_disabled,
        "aiEnabled": row.ai_enabled,
        "requestsToday": row.requests_today,
        "tokensToday": row.tokens_today,
        "totalRequests": row.requests_total,
        "totalInputTokens": row.input_tokens_total,
        "totalOutputTokens": row.output_tokens_total,
        "lastUsedAt": row.last_used_at,
    })
}

pub async fn admin_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UsageQuery>,
) -> Result<Json<Value>, ApiError> {
    api::require_admin(&state, &headers, "ai:read")?;
    let page = query.page.unwrap_or(1).max(1);
    let page_size = query.page_size.unwrap_or(20).clamp(1, 200);
    let offset = (page - 1) * page_size;
    let (total, rows) = state.db.ai_usage_rows(
        query.search.as_deref().unwrap_or_default(),
        page_size,
        offset,
        &shanghai_day(),
    )?;
    let items: Vec<Value> = rows.iter().map(usage_row_json).collect();
    Ok(Json(json!({
        "success": true,
        "data": { "total": total, "items": items },
    })))
}

#[derive(Deserialize)]
pub struct UserSwitchForm {
    enabled: bool,
}

pub async fn admin_user_switch(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(form): Json<UserSwitchForm>,
) -> Result<Json<Value>, ApiError> {
    api::require_admin(&state, &headers, "ai:write")?;
    let Some(user) = state.db.user_by_id(id)? else {
        return Err(ApiError::NotFound);
    };
    state.db.set_ai_user_enabled(id, form.enabled)?;
    tracing::info!(user = %user.username, enabled = form.enabled, "后台调整了用户的内置 AI 开关");
    Ok(Json(json!({
        "success": true,
        "data": {
            "userId": id,
            "username": user.username,
            "aiEnabled": form.enabled,
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_become_rough_tokens() {
        assert_eq!(estimate_chars(0), 0);
        assert_eq!(estimate_chars(1), 1);
        assert_eq!(estimate_chars(4), 1);
        assert_eq!(estimate_chars(5), 2);
    }

    #[test]
    fn images_cost_a_thousand_tokens() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "text", "text": "abcd"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]
        })];
        assert_eq!(estimate_messages_tokens(&messages), 1001);
    }

    #[test]
    fn usage_prefers_upstream_numbers() {
        let body = br#"{"choices":[{"message":{"content":"hello"}}],"usage":{"prompt_tokens":12,"completion_tokens":7}}"#;
        assert_eq!(usage_from_response(body, 99), (12, 7));
    }

    #[test]
    fn usage_falls_back_to_estimation() {
        let body = br#"{"choices":[{"message":{"content":"abcdefgh"}}]}"#;
        assert_eq!(usage_from_response(body, 33), (33, 2));
    }

    #[test]
    fn stream_usage_reads_the_final_chunk() {
        let mut usage = StreamUsage::default();
        usage.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n");
        usage.feed(
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":9}}\n\n",
        );
        usage.feed(b"data: [DONE]\n\n");
        assert_eq!(usage.settle(1), (5, 9));
    }

    #[test]
    fn stream_without_usage_is_estimated() {
        let mut usage = StreamUsage::default();
        usage.feed(b"data: {\"choices\":[{\"delta\":{\"content\":\"abcdefgh\"}}]}\n\n");
        assert_eq!(usage.settle(40), (40, 2));
    }

    #[test]
    fn upstream_status_is_classified() {
        assert_eq!(
            classify_status(StatusCode::UNAUTHORIZED),
            UpstreamCategory::AuthFailed
        );
        assert_eq!(
            classify_status(StatusCode::NOT_FOUND),
            UpstreamCategory::Rejected
        );
        assert_eq!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR),
            UpstreamCategory::Error
        );
    }

    #[test]
    fn defaults_are_two_hundred_per_day_and_two_concurrent() {
        let defaults = AiSettings::defaults();
        assert_eq!(defaults.daily_request_limit, 200);
        assert_eq!(defaults.max_concurrent_per_user, 2);
        assert!(!defaults.enabled);
    }

    #[test]
    fn in_flight_requests_are_capped_per_user() {
        let runtime = Arc::new(AiRuntime::default());
        let first = runtime.acquire(1, 2).expect("第一条应放行");
        let second = runtime.acquire(1, 2).expect("第二条应放行");
        assert!(runtime.acquire(1, 2).is_none(), "超过并发上限要拒绝");
        assert!(runtime.acquire(2, 2).is_some(), "别的用户不受影响");
        drop(first);
        assert!(runtime.acquire(1, 2).is_some(), "归还名额后可以再进");
        drop(second);
    }

    #[test]
    fn clamping_respects_the_configured_output_cap() {
        let settings = AiSettings {
            max_output_tokens: 100,
            ..AiSettings::defaults()
        };
        let mut payload = json!({"max_tokens": 9000});
        let Value::Object(ref mut payload) = payload else {
            unreachable!()
        };
        assert_eq!(clamp_output(payload, &settings), 100);
        assert_eq!(payload.get("max_tokens"), Some(&json!(100)));
    }

    #[test]
    fn sampling_keeps_only_one_knob_by_default() {
        let settings = AiSettings {
            temperature: Some(0.2),
            top_p: Some(0.9),
            ..AiSettings::defaults()
        };
        let Value::Object(mut payload) = json!({}) else {
            unreachable!()
        };
        apply_sampling(&mut payload, &settings).unwrap();
        assert_eq!(payload.get("temperature"), Some(&json!(0.2)));
        assert!(
            !payload.contains_key("top_p"),
            "默认不同时传 top_p，避免模型拒绝这个组合"
        );

        let both = AiSettings {
            sampling_both: true,
            ..settings
        };
        let Value::Object(mut payload) = json!({}) else {
            unreachable!()
        };
        apply_sampling(&mut payload, &both).unwrap();
        assert_eq!(payload.get("temperature"), Some(&json!(0.2)));
        assert_eq!(payload.get("top_p"), Some(&json!(0.9)));
    }

    #[test]
    fn sampling_rejects_out_of_range_values() {
        let settings = AiSettings::defaults();
        let Value::Object(mut payload) = json!({"temperature": 3}) else {
            unreachable!()
        };
        assert!(apply_sampling(&mut payload, &settings).is_err());
        let Value::Object(mut payload) = json!({"top_p": 1.5}) else {
            unreachable!()
        };
        assert!(apply_sampling(&mut payload, &settings).is_err());
    }

    #[test]
    fn custom_headers_allow_plain_but_never_authorization() {
        let headers = parse_custom_headers(r#"{"X-Title":"ShellAs","X-Trace":"on"}"#).unwrap();
        assert_eq!(headers.len(), 2);

        for denied in [
            r#"{"Authorization":"Bearer leaked"}"#,
            r#"{"X-Api-Key":"leaked"}"#,
            r#"{"Cookie":"a=b"}"#,
            r#"{"X-Bad":"a\r\nb"}"#,
        ] {
            assert!(parse_custom_headers(denied).is_err(), "{denied} 应被拒绝");
        }
        assert!(parse_custom_headers("[]").is_err());
    }

    #[test]
    fn retry_backoff_grows_and_is_capped() {
        let settings = AiSettings {
            retry_backoff_ms: 300,
            ..AiSettings::defaults()
        };
        assert_eq!(retry_backoff(&settings, 1), Duration::from_millis(300));
        assert_eq!(retry_backoff(&settings, 2), Duration::from_millis(600));
        assert_eq!(retry_backoff(&settings, 3), Duration::from_millis(1200));
        assert_eq!(retry_backoff(&settings, 9), Duration::from_millis(9_600));
    }

    #[test]
    fn defaults_use_a_128k_context() {
        assert_eq!(AiSettings::defaults().max_context_tokens, 131_072);
        assert_eq!(AiSettings::defaults().max_input_tokens, 122_880);
        assert_eq!(AiSettings::defaults().min_interval_seconds, 1);
    }
}
