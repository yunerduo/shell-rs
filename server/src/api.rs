//! 账号、云同步、更新清单与管理接口。
//!
//! 同步快照是密文：服务器只验大小、摘要和配额，从不解析内容。清单的
//! 签名在客户端校验，服务器只负责原样保存和分发。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio_util::io::ReaderStream;

use crate::auth;
use crate::config::Config;
use crate::db::{AnalyticsEvent, Db, DocumentError, Snapshot, SyncDocument, User};
use crate::error::{ApiError, ApiResult};
use crate::util;

/// 用户会话与管理员会话用不同的 cookie 名、不同的 Path，互不串用。
pub const USER_COOKIE: &str = "shellas_user_session";
pub const ADMIN_COOKIE: &str = "shellas_admin_session";

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Db>,
    /// 以下三个限流器互不共享计数：拿用户入口刷接口不会把管理后台一起限死。
    pub user_limiter: Arc<Limiter>,
    pub admin_limiter: Arc<Limiter>,
    pub analytics_limiter: Arc<Limiter>,
    /// 内置 AI 的 HTTP 客户端与按用户的在途计数。
    pub ai: Arc<crate::ai::AiRuntime>,
}

impl AppState {
    pub fn new(config: Config, db: Db) -> Self {
        Self {
            config: Arc::new(config),
            db: Arc::new(db),
            user_limiter: Arc::new(Limiter::default()),
            admin_limiter: Arc::new(Limiter::default()),
            analytics_limiter: Arc::new(Limiter::default()),
            ai: Arc::new(crate::ai::AiRuntime::default()),
        }
    }
}

/// 按 IP 的简单限流：只保留窗口内的次数，内存里一张表。
#[derive(Default)]
pub struct Limiter {
    hits: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl Limiter {
    pub fn allow(&self, key: String, limit: usize, window: Duration) -> bool {
        let now = Instant::now();
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if hits.len() > 10_000 {
            hits.retain(|_, times| {
                times
                    .back()
                    .is_some_and(|at| now.duration_since(*at) < window)
            });
        }
        let times = hits.entry(key).or_default();
        while times
            .front()
            .is_some_and(|at| now.duration_since(*at) > window)
        {
            times.pop_front();
        }
        if times.len() >= limit {
            return false;
        }
        times.push_back(now);
        true
    }
}

pub async fn healthz() -> &'static str {
    "ok"
}

// ---- 账号 ----

#[derive(Deserialize)]
pub struct RegisterRequest {
    username: String,
    password: String,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Serialize)]
pub struct MeResponse {
    username: String,
    email: Option<String>,
    /// 待验证的新邮箱；验证通过前 `email` 不变。
    pending_email: Option<String>,
    quota_bytes: i64,
    used_bytes: i64,
    sync_salt: String,
    created_at: String,
}

#[derive(Serialize)]
pub struct AuthResponse {
    token: String,
    expires_at: String,
    user: MeResponse,
}

pub async fn register(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<RegisterRequest>,
) -> ApiResult<Response> {
    let ip = client_ip(&headers, addr);
    if !state
        .user_limiter
        .allow(format!("register:{ip}"), 5, Duration::from_secs(60 * 60))
    {
        return Err(ApiError::TooLarge("注册太频繁，请稍后再试".into()));
    }
    let username = request.username.trim().to_string();
    if !auth::valid_username(&username) {
        return Err(ApiError::BadRequest(
            "用户名要 3–32 位，只能用字母、数字、下划线和短横线".into(),
        ));
    }
    if !auth::valid_password(&request.password) {
        return Err(ApiError::BadRequest(auth::PASSWORD_RULE.into()));
    }
    let email = request
        .email
        .as_deref()
        .map(str::trim)
        .filter(|email| !email.is_empty());
    if let Some(email) = email
        && !auth::valid_email(email)
    {
        return Err(ApiError::BadRequest("邮箱格式不对".into()));
    }
    let password = request.password;
    let password_hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    let quota = state.config.default_quota_bytes;
    let salt = util::random_salt();
    let user = match state
        .db
        .create_user(&username, email, &password_hash, &salt, quota)
    {
        Ok(user) => user,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            return Err(ApiError::Conflict("用户名或邮箱已被注册".into()));
        }
        Err(error) => return Err(error.into()),
    };
    Ok(session_response(
        issue_session(&state, &headers, user)?,
        state.config.session_days,
    ))
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Response> {
    let ip = client_ip(&headers, addr);
    if !state
        .user_limiter
        .allow(format!("login:{ip}"), 20, Duration::from_secs(10 * 60))
    {
        return Err(ApiError::TooLarge("尝试太频繁，请稍后再试".into()));
    }
    let username = request.username.trim().to_string();
    let key = username.to_ascii_lowercase();
    if let Some(until) = state.db.login_locked_at("user", &key)? {
        return Err(ApiError::Locked(format!(
            "口令错误次数太多，请在 {until} 之后再试"
        )));
    }
    let found = state.db.user_by_name(&username)?;
    // 账号不存在时也照样算一遍口令散列，响应时间不泄露账号是否存在。
    let hash = found.as_ref().map(|user| user.password_hash.clone());
    let password = request.password;
    let valid = tokio::task::spawn_blocking(move || match hash {
        Some(hash) => auth::verify_password(&password, &hash),
        None => {
            let _ = auth::hash_password(&password);
            false
        }
    })
    .await
    .map_err(ApiError::internal)?;
    if !valid {
        let until = state.db.record_login_failure(
            "user",
            &key,
            state.config.login_max_failures,
            state.config.login_lock_minutes,
        )?;
        if !until.is_empty() {
            return Err(ApiError::Locked(format!(
                "口令错误次数太多，请在 {until} 之后再试"
            )));
        }
        return Err(ApiError::Credentials("用户名或口令不对".into()));
    }
    let Some(user) = found else {
        return Err(ApiError::Credentials("用户名或口令不对".into()));
    };
    if user.disabled {
        return Err(ApiError::Forbidden("账号已被停用，请联系管理员".into()));
    }
    state.db.clear_login_failures("user", &key)?;
    Ok(session_response(
        issue_session(&state, &headers, user)?,
        state.config.session_days,
    ))
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    let token = bearer_or_cookie(&headers, USER_COOKIE).ok_or(ApiError::Unauthorized)?;
    state.db.delete_session(&auth::token_hash(token))?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, clear_cookie(USER_COOKIE, "/api"));
    Ok(response)
}

/// 邮件找回这轮不做，但接口位置先占住：客户端可以调，拿到的是一个
/// 稳定的 501，而不是 404。
#[derive(Deserialize, Default)]
#[allow(dead_code)] // 接口占位：字段先按将来发信的入参留着。
pub struct ForgotPasswordRequest {
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub email: String,
}

pub async fn forgot_password(
    State(_state): State<AppState>,
    Json(_request): Json<ForgotPasswordRequest>,
) -> ApiResult<StatusCode> {
    Err(ApiError::NotImplemented(
        "邮件找回还没有开放，请联系管理员重置口令".into(),
    ))
}

#[derive(Deserialize, Default)]
#[allow(dead_code)] // 接口占位：字段先按将来校验收口令的入参留着。
pub struct ResetPasswordRequest {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub password: String,
}

pub async fn reset_password(
    State(_state): State<AppState>,
    Json(_request): Json<ResetPasswordRequest>,
) -> ApiResult<StatusCode> {
    Err(ApiError::NotImplemented(
        "邮件找回还没有开放，请联系管理员重置口令".into(),
    ))
}

fn session_response(body: AuthResponse, days: i64) -> Response {
    let token = body.token.clone();
    let mut response = Json(body).into_response();
    if let Some(cookie) = session_cookie(USER_COOKIE, &token, days, "/api") {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
    response
}

pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<MeResponse>> {
    let user = require_user(&state, &headers)?;
    Ok(Json(me_response(&state, &user)?))
}

// ---- 账号自助 ----

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// 用户自己改口令：先验旧口令，再按与注册一致的规则校验新口令，
/// 改完删掉该用户的全部会话（含当前这条），客户端据此回到未登录。
pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChangePasswordRequest>,
) -> ApiResult<Response> {
    let user = require_user(&state, &headers)?;
    if !state.user_limiter.allow(
        format!("password:{}", user.id),
        10,
        Duration::from_secs(60 * 60),
    ) {
        return Err(ApiError::TooMany("尝试太频繁，请稍后再试".into()));
    }
    let current = request.current_password;
    let hash = user.password_hash.clone();
    let verified = tokio::task::spawn_blocking(move || auth::verify_password(&current, &hash))
        .await
        .map_err(ApiError::internal)?;
    if !verified {
        return Err(ApiError::BadRequest("当前口令不对".into()));
    }
    if !auth::valid_password(&request.new_password) {
        return Err(ApiError::BadRequest(auth::PASSWORD_RULE.into()));
    }
    let password = request.new_password;
    let hash = tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
    state.db.set_user_password(user.id, &hash)?;
    let mut response = Json(serde_json::json!({
        "success": true,
        "message": "口令已修改，所有已登录会话已失效，请重新登录"
    }))
    .into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, clear_cookie(USER_COOKIE, "/api"));
    Ok(response)
}

#[derive(Deserialize)]
pub struct ChangeEmailRequest {
    email: String,
}

/// 换邮箱先记成「待验证」：服务端还没有发信能力，没有办法确认新邮箱
/// 归属，所以在验证通过前不改 `users.email`，只返回 pending。
pub async fn change_email(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChangeEmailRequest>,
) -> ApiResult<Json<Value>> {
    let user = require_user(&state, &headers)?;
    if !state.user_limiter.allow(
        format!("email:{}", user.id),
        5,
        Duration::from_secs(60 * 60),
    ) {
        return Err(ApiError::TooMany("尝试太频繁，请稍后再试".into()));
    }
    let email = request.email.trim();
    if !auth::valid_email(email) {
        return Err(ApiError::BadRequest("邮箱格式不对".into()));
    }
    if user
        .email
        .as_deref()
        .is_some_and(|current| current.eq_ignore_ascii_case(email))
    {
        return Err(ApiError::BadRequest("新邮箱与当前邮箱相同".into()));
    }
    if state.db.email_taken_by_other(user.id, email)? {
        return Err(ApiError::Conflict("该邮箱已被其他账号使用".into()));
    }
    state.db.set_pending_email(user.id, email)?;
    Ok(Json(serde_json::json!({
        "success": true,
        "pending": true,
        "email": email,
        "verification": "unavailable",
        "message": "服务端还没有邮件通道，新邮箱已记为待验证；验证通过前主邮箱保持不变"
    })))
}

fn issue_session(state: &AppState, headers: &HeaderMap, user: User) -> ApiResult<AuthResponse> {
    let token = util::random_token();
    let expires_at = util::later(state.config.session_days);
    let device = headers.get("x-device").and_then(|v| v.to_str().ok());
    state
        .db
        .create_session(user.id, &auth::token_hash(&token), &expires_at, device)?;
    Ok(AuthResponse {
        token,
        expires_at,
        user: me_response(state, &user)?,
    })
}

fn me_response(state: &AppState, user: &User) -> ApiResult<MeResponse> {
    Ok(MeResponse {
        username: user.username.clone(),
        email: user.email.clone(),
        pending_email: state.db.pending_email(user.id)?,
        quota_bytes: user.quota_bytes,
        used_bytes: state.db.used_bytes(user.id)?,
        sync_salt: user.sync_salt.clone(),
        created_at: user.created_at.clone(),
    })
}

// ---- 云同步 ----

#[derive(Serialize)]
pub struct SyncList {
    used_bytes: i64,
    quota_bytes: i64,
    /// 老的单一快照流，保留给还没有升级的客户端。
    latest: Option<Snapshot>,
    versions: Vec<Snapshot>,
    /// 按 `settings` / `hosts` 分开的文档流，是新的增量同步接口。
    documents: Vec<SyncDocument>,
    latest_documents: BTreeMap<String, SyncDocument>,
}

pub async fn list_sync_versions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<SyncList>> {
    let user = require_user(&state, &headers)?;
    let versions = state.db.snapshots(user.id)?;
    let documents = state.db.documents(user.id)?;
    let mut latest_documents = BTreeMap::new();
    for document in &documents {
        latest_documents
            .entry(document.kind.clone())
            .or_insert_with(|| document.clone());
    }
    Ok(Json(SyncList {
        used_bytes: state.db.used_bytes(user.id)?,
        quota_bytes: user.quota_bytes,
        latest: versions.first().cloned(),
        versions,
        documents,
        latest_documents,
    }))
}

pub async fn upload_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<Snapshot>> {
    let user = require_user(&state, &headers)?;
    let limit = (user.quota_bytes as u64).min(state.config.max_snapshot_bytes);
    if let Some(length) = content_length(&headers)
        && length > limit
    {
        return Err(ApiError::TooLarge(format!(
            "单个快照最多 {}",
            util::format_bytes(limit)
        )));
    }
    let expected = headers
        .get("x-snapshot-sha256")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    if expected.is_none() {
        return Err(ApiError::BadRequest("缺少 X-Snapshot-Sha256 请求头".into()));
    }
    let dir = state.config.sync_dir().join(user.id.to_string());
    tokio::fs::create_dir_all(&dir).await?;
    let temporary = dir.join(format!(".upload-{}", util::random_token()));
    let saved = save_stream(body, &temporary, limit, expected.as_deref()).await;
    let (size, sha256) = match saved {
        Ok(saved) => saved,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
    };
    if size == 0 {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(ApiError::BadRequest("快照是空的".into()));
    }
    let (snapshot, evicted) = match state.db.add_snapshot(
        user.id,
        size as i64,
        &sha256,
        headers.get("x-device").and_then(|v| v.to_str().ok()),
        user.quota_bytes,
        state.config.max_snapshots,
    ) {
        Ok(added) => added,
        Err(crate::db::SnapshotError::Quota { used, quota }) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(ApiError::Quota { used, quota });
        }
        Err(crate::db::SnapshotError::Sql(error)) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error.into());
        }
    };
    let final_path = snapshot_path(&state, user.id, snapshot.version);
    if let Err(error) = tokio::fs::rename(&temporary, &final_path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        let _ = state.db.delete_snapshot(user.id, snapshot.version);
        return Err(error.into());
    }
    for old in evicted {
        let _ = tokio::fs::remove_file(snapshot_path(&state, user.id, old)).await;
    }
    Ok(Json(snapshot))
}

pub async fn download_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(version): Path<i64>,
) -> ApiResult<Response> {
    let user = require_user(&state, &headers)?;
    let Some(snapshot) = state.db.snapshot(user.id, version)? else {
        return Err(ApiError::NotFound);
    };
    let file = tokio::fs::File::open(snapshot_path(&state, user.id, version)).await?;
    let body = Body::from_stream(ReaderStream::new(file));
    let mut response = Response::new(body);
    let head = response.headers_mut();
    head.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    head.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&snapshot.size.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    if let Ok(value) = HeaderValue::from_str(&snapshot.sha256) {
        head.insert("x-snapshot-sha256", value);
    }
    Ok(response)
}

pub async fn delete_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(version): Path<i64>,
) -> ApiResult<StatusCode> {
    let user = require_user(&state, &headers)?;
    if state.db.delete_snapshot(user.id, version)?.is_none() {
        return Err(ApiError::NotFound);
    }
    let _ = tokio::fs::remove_file(snapshot_path(&state, user.id, version)).await;
    Ok(StatusCode::NO_CONTENT)
}

fn snapshot_path(state: &AppState, user_id: i64, version: i64) -> PathBuf {
    state
        .config
        .sync_dir()
        .join(user_id.to_string())
        .join(format!("{version}.bin"))
}

/// 新同步接口只有两种文档：应用设置、主机（含分组与凭据引用）。
/// 凭据的密码、私钥和口令永远不进这两个文档，客户端只同步引用。
fn valid_sync_kind(kind: &str) -> bool {
    matches!(kind, "settings" | "hosts")
}

fn document_path(state: &AppState, user_id: i64, kind: &str, version: i64) -> PathBuf {
    state
        .config
        .sync_dir()
        .join(user_id.to_string())
        .join(format!("{kind}-{version}.bin"))
}

/// `PUT /api/v1/sync/documents/{kind}`。
///
/// 请求头 `X-Base-Version` 是客户端基于哪一版改的：服务端该文档的最新版
/// 已经不是它时返回 409，并在 `latest` 里带上服务端最新一份，客户端合并
/// 后重新上传。不传 `X-Base-Version` 只允许首次上传。
pub async fn upload_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    body: Body,
) -> ApiResult<Response> {
    let user = require_user(&state, &headers)?;
    if !valid_sync_kind(&kind) {
        return Err(ApiError::BadRequest(
            "同步文档只有 settings 和 hosts".into(),
        ));
    }
    let limit = (user.quota_bytes as u64).min(state.config.max_snapshot_bytes);
    if let Some(length) = content_length(&headers)
        && length > limit
    {
        return Err(ApiError::TooLarge(format!(
            "单个同步文档最多 {}",
            util::format_bytes(limit)
        )));
    }
    let expected = headers
        .get("x-snapshot-sha256")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());
    let Some(expected) = expected else {
        return Err(ApiError::BadRequest("缺少 X-Snapshot-Sha256 请求头".into()));
    };
    let base_version = headers
        .get("x-base-version")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| ApiError::BadRequest("X-Base-Version 必须是整数".into()))
        })
        .transpose()?;
    let dir = state.config.sync_dir().join(user.id.to_string());
    tokio::fs::create_dir_all(&dir).await?;
    let temporary = dir.join(format!(".upload-{}", util::random_token()));
    let (size, sha256) = match save_stream(body, &temporary, limit, Some(&expected)).await {
        Ok(saved) => saved,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
    };
    if size == 0 {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(ApiError::BadRequest("同步文档是空的".into()));
    }
    let (document, evicted) = match state.db.add_document(
        user.id,
        &kind,
        size as i64,
        &sha256,
        base_version,
        headers
            .get("x-device")
            .and_then(|value| value.to_str().ok()),
        user.quota_bytes,
        state.config.max_snapshots,
    ) {
        Ok(added) => added,
        Err(DocumentError::Conflict { latest }) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Ok((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "conflict",
                    "message": "服务端已有更新的版本，请先合并再上传",
                    "latest": latest,
                })),
            )
                .into_response());
        }
        Err(DocumentError::Quota { used, quota }) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(ApiError::Quota { used, quota });
        }
        Err(DocumentError::Sql(error)) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error.into());
        }
    };
    let final_path = document_path(&state, user.id, &kind, document.version);
    if let Err(error) = tokio::fs::rename(&temporary, &final_path).await {
        let _ = tokio::fs::remove_file(&temporary).await;
        let _ = state.db.delete_document(user.id, &kind, document.version);
        return Err(error.into());
    }
    for old in evicted {
        let _ = tokio::fs::remove_file(document_path(&state, user.id, &kind, old)).await;
    }
    Ok(Json(document).into_response())
}

pub async fn download_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, version)): Path<(String, i64)>,
) -> ApiResult<Response> {
    let user = require_user(&state, &headers)?;
    if !valid_sync_kind(&kind) {
        return Err(ApiError::NotFound);
    }
    let Some(document) = state.db.document(user.id, &kind, version)? else {
        return Err(ApiError::NotFound);
    };
    let file = tokio::fs::File::open(document_path(&state, user.id, &kind, version)).await?;
    let body = Body::from_stream(ReaderStream::new(file));
    let mut response = Response::new(body);
    let head = response.headers_mut();
    head.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    head.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&document.size.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    if let Ok(value) = HeaderValue::from_str(&document.sha256) {
        head.insert("x-snapshot-sha256", value);
    }
    if let Ok(value) = HeaderValue::from_str(&document.kind) {
        head.insert("x-document-kind", value);
    }
    Ok(response)
}

pub async fn delete_document(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, version)): Path<(String, i64)>,
) -> ApiResult<StatusCode> {
    let user = require_user(&state, &headers)?;
    if !valid_sync_kind(&kind) {
        return Err(ApiError::NotFound);
    }
    if state.db.delete_document(user.id, &kind, version)?.is_none() {
        return Err(ApiError::NotFound);
    }
    let _ = tokio::fs::remove_file(document_path(&state, user.id, &kind, version)).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---- 更新 ----

pub async fn stable_manifest(State(state): State<AppState>) -> ApiResult<Response> {
    manifest_response(&state, "stable")
}

pub async fn beta_manifest(State(state): State<AppState>) -> ApiResult<Response> {
    manifest_response(&state, "beta")
}

fn manifest_response(state: &AppState, channel: &str) -> ApiResult<Response> {
    let Some(envelope) = state.db.current_manifest(channel)? else {
        return Err(ApiError::NotFound);
    };
    let mut response = envelope.into_response();
    let head = response.headers_mut();
    head.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    head.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    Ok(response)
}

// ---- 匿名统计 ----

/// 客户端上报时带的 App-Key，与 `src/analytics/mod.rs` 的 `APP_KEY` 一致。
/// 不是秘密：每个发布版都带着它，它只允许写统计事件。
pub const ANALYTICS_APP_KEY: &str = "A-US-1582708162";
/// 一次请求最多的事件数，与客户端的批次一致。
const ANALYTICS_BATCH_LIMIT: usize = 25;
/// 每个地址每小时的请求上限：正常一天也就几个批次。
const ANALYTICS_REQUESTS_PER_HOUR: usize = 120;
/// 一条事件属性的上限，超过的整条丢掉。
const ANALYTICS_PROPS_LIMIT: usize = 4 * 1024;

/// 客户端发来的一条事件，字段与 Aptabase 的 REST 协议一致。
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct AnalyticsEventIn {
    #[serde(default)]
    event_name: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    timestamp: String,
    #[serde(default)]
    system_props: SystemPropsIn,
    #[serde(default)]
    props: Map<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct SystemPropsIn {
    #[serde(default)]
    is_debug: bool,
    #[serde(default)]
    os_name: String,
    #[serde(default)]
    os_version: String,
    #[serde(default)]
    app_version: String,
    #[serde(default)]
    sdk_version: String,
}

/// `POST /api/v0/events`：收下匿名统计。没有账号，只有 App-Key；来源地址
/// 和 User-Agent 不落库，只留下当天的摘要，用来区分设备。
pub async fn analytics_events(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<StatusCode> {
    let key = headers
        .get("app-key")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    if !util::ct_eq(key, ANALYTICS_APP_KEY) {
        return Err(ApiError::Unauthorized);
    }
    let ip = client_ip(&headers, addr);
    if !state.analytics_limiter.allow(
        format!("analytics:{ip}"),
        ANALYTICS_REQUESTS_PER_HOUR,
        Duration::from_secs(60 * 60),
    ) {
        return Err(ApiError::TooMany("上报太频繁，请稍后再试".into()));
    }
    let batch: Vec<AnalyticsEventIn> = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("事件不是合法的 JSON 数组".into()))?;
    if batch.len() > ANALYTICS_BATCH_LIMIT {
        return Err(ApiError::BadRequest(format!(
            "一次最多 {ANALYTICS_BATCH_LIMIT} 个事件"
        )));
    }
    let events = prepare_events(batch, &device_id(&state, &headers, &ip)?, &util::now());
    if !events.is_empty() {
        state.db.insert_analytics_events(&events)?;
    }
    Ok(StatusCode::OK)
}

/// 来源地址与 User-Agent 在当天的盐下算出的摘要：同一天里认得出同一台
/// 设备，认不出它是谁；第二天换了盐就接不上了。
fn device_id(state: &AppState, headers: &HeaderMap, ip: &str) -> ApiResult<String> {
    let day = Utc::now().format("%Y-%m-%d").to_string();
    let salt = state.db.analytics_salt(&day)?;
    let agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    Ok(util::sha256_hex(
        format!("{salt}\n{ip}\n{agent}").as_bytes(),
    ))
}

/// 只留下形状对的事件：名字是短的标识符、时间能解析、属性是 JSON 对象
/// 且不过大。其余安静地丢掉，不让一条坏数据拖垮整批。
fn prepare_events(
    batch: Vec<AnalyticsEventIn>,
    device: &str,
    received_at: &str,
) -> Vec<AnalyticsEvent> {
    batch
        .into_iter()
        .filter_map(|event| {
            let name = event.event_name.trim();
            let valid_name = !name.is_empty()
                && name.len() <= 40
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            if !valid_name {
                return None;
            }
            let occurred_at = chrono::DateTime::parse_from_rfc3339(event.timestamp.trim()).ok()?;
            let props = serde_json::to_string(&event.props).ok()?;
            if props.len() > ANALYTICS_PROPS_LIMIT {
                return None;
            }
            Some(AnalyticsEvent {
                name: name.to_string(),
                session_id: clip(&event.session_id, 64),
                device: device.to_string(),
                occurred_at: occurred_at
                    .to_utc()
                    .to_rfc3339_opts(SecondsFormat::Secs, true),
                received_at: received_at.to_string(),
                app_version: clip(&event.system_props.app_version, 50),
                os_name: clip(&event.system_props.os_name, 30),
                os_version: clip(&event.system_props.os_version, 100),
                sdk_version: clip(&event.system_props.sdk_version, 50),
                is_debug: event.system_props.is_debug,
                props,
            })
        })
        .collect()
}

fn clip(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

// ---- 管理 ----

pub async fn admin_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((version, file)): Path<(String, String)>,
    body: Body,
) -> ApiResult<Json<Value>> {
    require_admin(&state, &headers, "release:upload")?;
    validate_version(&version)?;
    validate_file_name(&file)?;
    let dir = state.config.releases_dir().join(&version);
    tokio::fs::create_dir_all(&dir).await?;
    let temporary = dir.join(format!(".upload-{}", util::random_token()));
    let saved = save_stream(body, &temporary, state.config.max_package_bytes, None).await;
    let (size, sha256) = match saved {
        Ok(saved) => saved,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temporary).await;
            return Err(error);
        }
    };
    if size == 0 {
        let _ = tokio::fs::remove_file(&temporary).await;
        return Err(ApiError::BadRequest("安装包是空的".into()));
    }
    tokio::fs::rename(&temporary, dir.join(&file)).await?;
    // 上传成功就把元数据记下来；写库失败只记日志，后台列版本时会再补。
    let (platform, arch) = crate::release::file_platform(&version, &file)
        .map(crate::release::split_platform)
        .unwrap_or(("", ""));
    if let Err(error) =
        state
            .db
            .upsert_package(&version, &file, platform, arch, size as i64, &sha256)
    {
        tracing::warn!(error = %error, version, file, "安装包已落盘，但元数据写入失败");
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "version": version,
        "file": file,
        "platform": platform,
        "arch": arch,
        "size": size,
        "sha256": sha256,
    })))
}

#[derive(Deserialize)]
struct EnvelopeIn {
    manifest: String,
    signature: String,
}

pub async fn admin_manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    require_admin(&state, &headers, "release:publish")?;
    if channel != "stable" && channel != "beta" {
        return Err(ApiError::BadRequest("通道只有 stable 和 beta".into()));
    }
    let envelope: EnvelopeIn = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("信封格式不对：应该是 {manifest, signature}".into()))?;
    if envelope.signature.trim().is_empty() || envelope.manifest.trim().is_empty() {
        return Err(ApiError::BadRequest("清单或签名是空的".into()));
    }
    let manifest: Value = serde_json::from_str(&envelope.manifest)
        .map_err(|_| ApiError::BadRequest("清单不是合法的 JSON".into()))?;
    if manifest.get("channel").and_then(Value::as_str) != Some(channel.as_str()) {
        return Err(ApiError::BadRequest("清单里的通道和上传路径不一致".into()));
    }
    let Some(version) = manifest.get("version").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest("清单里没有版本号".into()));
    };
    validate_version(version)?;
    if manifest.get("schema").and_then(Value::as_u64).is_none() {
        return Err(ApiError::BadRequest("清单里没有 schema".into()));
    }
    let published_at = manifest
        .get("published_at")
        .and_then(Value::as_str)
        .filter(|at| !at.trim().is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let text = String::from_utf8(body.to_vec())
        .map_err(|_| ApiError::BadRequest("信封不是 UTF-8".into()))?;
    state
        .db
        .put_manifest(&channel, version, &text, &published_at)?;
    record_manifest_release(&state, &channel, version, &manifest, &published_at);
    // 落盘一份，备份和排障时不用开库。
    let dir = state.config.manifests_dir();
    let copy = async {
        tokio::fs::create_dir_all(&dir).await?;
        tokio::fs::write(dir.join(format!("{channel}.json")), text.as_bytes()).await
    }
    .await;
    if let Err(error) = copy {
        tracing::warn!(error = %error, channel, "清单已入库，但落盘副本写失败");
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "channel": channel,
        "version": version,
    })))
}

/// 命令行 publish 上传清单后也补一条发布记录，后台的升级记录才完整。
fn record_manifest_release(
    state: &AppState,
    channel: &str,
    version: &str,
    manifest: &Value,
    published_at: &str,
) {
    let severity = manifest
        .get("severity")
        .or_else(|| manifest.get("level"))
        .and_then(Value::as_str)
        .map(|severity| {
            if severity == crate::release::SEVERITY_REQUIRED {
                crate::release::SEVERITY_REQUIRED
            } else {
                crate::release::SEVERITY_RECOMMENDED
            }
        })
        .unwrap_or(crate::release::SEVERITY_RECOMMENDED);
    let notes = manifest.get("notes").and_then(Value::as_str).unwrap_or("");
    let notes_url = manifest
        .get("notes_url")
        .and_then(Value::as_str)
        .unwrap_or("");
    let minimum_version = manifest
        .get("minimum_version")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty());
    let rollout = manifest
        .get("rollout")
        .and_then(Value::as_f64)
        .unwrap_or(1.0);
    if let Err(error) = state.db.put_release_version(
        version,
        channel,
        severity,
        notes,
        notes_url,
        minimum_version,
        rollout,
        published_at,
    ) {
        tracing::warn!(error = %error, version, channel, "发布记录写入失败");
    }
    // 重新发布一个撤下过的版本时，安装包也跟着恢复。
    if let Err(error) = state.db.set_packages_withdrawn(version, false) {
        tracing::warn!(error = %error, version, "安装包状态恢复失败");
    }
}

/// `GET /releases/{version}/{file}`：发安装包，同时记一次下载。
/// 被撤下且没有任何通道还在发的包不再提供下载。
pub async fn download_release(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path((version, file)): Path<(String, String)>,
) -> ApiResult<Response> {
    validate_version(&version)?;
    validate_file_name(&file)?;
    let path = state.config.releases_dir().join(&version).join(&file);
    let meta = match tokio::fs::metadata(&path).await {
        Ok(meta) if meta.is_file() => meta,
        _ => return Err(ApiError::NotFound),
    };
    if let Some(package) = state.db.package(&version, &file)? {
        if package.withdrawn {
            return Err(ApiError::NotFound);
        }
    } else {
        // 老版本的文件还没登记过，第一次下载时补上，之后就能计数。
        let (platform, arch) = crate::release::file_platform(&version, &file)
            .map(crate::release::split_platform)
            .unwrap_or(("", ""));
        let _ = state
            .db
            .upsert_package(&version, &file, platform, arch, meta.len() as i64, "");
    }
    let total = meta.len();
    let range = release_range(
        headers
            .get(header::RANGE)
            .and_then(|value| value.to_str().ok()),
        total,
    );
    if matches!(range, ReleaseRange::Invalid) {
        let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        if let Ok(value) = HeaderValue::from_str(&format!("bytes */{total}")) {
            response.headers_mut().insert(header::CONTENT_RANGE, value);
        }
        return Ok(response);
    }
    // 只有真正开始发文件才记一次下载；416 和 HEAD 不算。
    if method == Method::GET
        && let Err(error) = state.db.increment_download(&version, &file)
    {
        tracing::warn!(error = %error, version, file, "下载次数记录失败");
    }
    let mut handle = tokio::fs::File::open(&path).await?;
    let (status, start, length, content_range) = match range {
        ReleaseRange::Partial { start, end } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{total}")),
        ),
        _ => (StatusCode::OK, 0, total, None),
    };
    if start > 0 {
        handle.seek(std::io::SeekFrom::Start(start)).await?;
    }
    let body = Body::from_stream(ReaderStream::new(handle.take(length)));
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let head = response.headers_mut();
    head.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    head.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    head.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).unwrap_or(HeaderValue::from_static("0")),
    );
    if let Some(value) = content_range
        && let Ok(value) = HeaderValue::from_str(&value)
    {
        head.insert(header::CONTENT_RANGE, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!("attachment; filename=\"{file}\"")) {
        head.insert(header::CONTENT_DISPOSITION, value);
    }
    head.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    Ok(response)
}

/// 客户端断点续传只用到单段 Range；多段或越界一律当无效，回 416。
enum ReleaseRange {
    Full,
    Partial { start: u64, end: u64 },
    Invalid,
}

fn release_range(value: Option<&str>, total: u64) -> ReleaseRange {
    let Some(value) = value else {
        return ReleaseRange::Full;
    };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return ReleaseRange::Invalid;
    };
    if spec.contains(',') {
        return ReleaseRange::Invalid;
    }
    let Some((start, end)) = spec.trim().split_once('-') else {
        return ReleaseRange::Invalid;
    };
    let (start, end) = if start.trim().is_empty() {
        let Ok(suffix) = end.trim().parse::<u64>() else {
            return ReleaseRange::Invalid;
        };
        if suffix == 0 || total == 0 {
            return ReleaseRange::Invalid;
        }
        (total.saturating_sub(suffix), total - 1)
    } else {
        let Ok(start) = start.trim().parse::<u64>() else {
            return ReleaseRange::Invalid;
        };
        if start >= total {
            return ReleaseRange::Invalid;
        }
        let end = if end.trim().is_empty() {
            total - 1
        } else {
            let Ok(end) = end.trim().parse::<u64>() else {
                return ReleaseRange::Invalid;
            };
            end.min(total - 1)
        };
        if end < start {
            return ReleaseRange::Invalid;
        }
        (start, end)
    };
    ReleaseRange::Partial { start, end }
}

// ---- 共用 ----

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name && !value.is_empty()).then_some(value)
        })
}

/// 桌面客户端用 `Authorization: Bearer`，浏览器后台用 HttpOnly cookie；
/// 两种都认，但各自只认自己那个 cookie 名。
pub(crate) fn bearer_or_cookie<'a>(headers: &'a HeaderMap, cookie: &str) -> Option<&'a str> {
    bearer(headers).or_else(|| cookie_value(headers, cookie))
}

pub(crate) fn session_cookie(
    name: &str,
    token: &str,
    days: i64,
    path: &str,
) -> Option<HeaderValue> {
    HeaderValue::from_str(&format!(
        "{name}={token}; Path={path}; Max-Age={}; HttpOnly; Secure; SameSite=Lax",
        days.max(1) * 86_400
    ))
    .ok()
}

pub(crate) fn clear_cookie(name: &str, path: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}=; Path={path}; Max-Age=0; HttpOnly; Secure; SameSite=Lax"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("expired="))
}

pub(crate) fn require_user(state: &AppState, headers: &HeaderMap) -> ApiResult<User> {
    let token = bearer_or_cookie(headers, USER_COOKIE).ok_or(ApiError::Unauthorized)?;
    let user = state
        .db
        .session_user(&auth::token_hash(token))?
        .ok_or(ApiError::Unauthorized)?;
    if user.disabled {
        return Err(ApiError::Forbidden("账号已被停用".into()));
    }
    Ok(user)
}

/// 管理接口认证：`SHELLAS_ADMIN_TOKEN`（发布命令行）或后台登录会话。
/// 命令行令牌视为全权；后台会话按角色校验 `permission`。
pub(crate) fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
    permission: &str,
) -> ApiResult<()> {
    if let Some(expected) = state.config.admin_token.as_deref()
        && let Some(given) = bearer(headers)
        && util::ct_eq(given, expected)
    {
        return Ok(());
    }
    if let Some(admin) = crate::admin::current_admin(state, headers) {
        return if crate::admin::has_permission(&admin, permission) {
            Ok(())
        } else {
            Err(ApiError::Forbidden("当前角色没有这个操作的权限".into()))
        };
    }
    // 没配令牌又没登录过后台时保持老行为：当作接口不存在。
    if state.config.admin_token.is_none() {
        return Err(ApiError::NotFound);
    }
    Err(ApiError::Unauthorized)
}

/// 反向代理后面拿不到真实对端地址，优先看 `X-Forwarded-For` 的第一段。
pub(crate) fn client_ip(headers: &HeaderMap, addr: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| addr.ip().to_string())
}

fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

/// 把请求体流式写到 `path`，边写边算摘要；超过 `max` 立刻停。
async fn save_stream(
    body: Body,
    path: &FsPath,
    max: u64,
    expected_sha256: Option<&str>,
) -> ApiResult<(u64, String)> {
    let mut file = tokio::fs::File::create(path).await?;
    let mut hasher = Sha256::new();
    let mut size = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ApiError::internal)?;
        size += chunk.len() as u64;
        if size > max {
            drop(file);
            return Err(ApiError::TooLarge(format!(
                "请求体超过上限 {}",
                util::format_bytes(max)
            )));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    let sha256 = hex::encode(hasher.finalize());
    if let Some(expected) = expected_sha256
        && !util::ct_eq(&sha256, expected)
    {
        let _ = tokio::fs::remove_file(path).await;
        return Err(ApiError::BadRequest("SHA-256 对不上，上传不完整".into()));
    }
    Ok((size, sha256))
}

pub(crate) fn validate_version(version: &str) -> ApiResult<()> {
    let valid = !version.is_empty()
        && version.len() <= 64
        && version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'));
    if valid {
        Ok(())
    } else {
        Err(ApiError::BadRequest("版本号不合法".into()))
    }
}

fn validate_file_name(file: &str) -> ApiResult<()> {
    let valid = !file.is_empty()
        && file.len() <= 128
        && file != "."
        && file != ".."
        && file
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'));
    if valid {
        Ok(())
    } else {
        Err(ApiError::BadRequest("文件名不合法".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limiter_counts_inside_its_window() {
        let limiter = Limiter::default();
        assert!(limiter.allow("a".into(), 2, Duration::from_secs(60)));
        assert!(limiter.allow("a".into(), 2, Duration::from_secs(60)));
        assert!(!limiter.allow("a".into(), 2, Duration::from_secs(60)));
        assert!(limiter.allow("b".into(), 2, Duration::from_secs(60)));
    }

    #[test]
    fn version_and_file_names_stay_plain() {
        assert!(validate_version("0.1.19").is_ok());
        assert!(validate_version("0.2.0-beta.1").is_ok());
        assert!(validate_version("../x").is_err());
        assert!(validate_file_name("ShellAs-0.1.19-windows-x86_64-setup.exe").is_ok());
        assert!(validate_file_name("..").is_err());
        assert!(validate_file_name("a/b").is_err());
    }

    #[test]
    fn statistics_keep_only_well_formed_events() {
        let batch: Vec<AnalyticsEventIn> = serde_json::from_str(
            r#"[
             {"timestamp":"2026-11-16T08:00:00.000Z","sessionId":"176000000012345678",
              "eventName":"app_started",
              "systemProps":{"isDebug":false,"osName":"Windows","osVersion":"10.0.22631",
                             "appVersion":"0.1.19","sdkVersion":"shellas@0.1.19"},
              "props":{"arch":"x86_64","hosts":"1-5"}},
             {"timestamp":"yesterday","eventName":"app_started"},
             {"timestamp":"2026-11-16T08:00:00Z","eventName":"has spaces"},
             {"timestamp":"2026-11-16T08:00:00Z","eventName":"app_active",
              "systemProps":{"osName":"macOS"}}
            ]"#,
        )
        .unwrap();
        let events = prepare_events(batch, "device", "2026-11-16T09:00:00Z");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "app_started");
        assert_eq!(events[0].occurred_at, "2026-11-16T08:00:00Z");
        assert_eq!(events[0].device, "device");
        assert_eq!(events[0].os_name, "Windows");
        assert!(events[0].props.contains(r#""arch":"x86_64""#));
        assert_eq!(events[1].name, "app_active");
        assert_eq!(events[1].os_name, "macOS");
        assert_eq!(events[1].props, "{}");
    }

    #[test]
    fn the_forwarded_for_header_wins() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.9, 127.0.0.1".parse().unwrap());
        let addr: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        assert_eq!(client_ip(&headers, addr), "203.0.113.9");
    }

    #[test]
    fn release_ranges_cover_resume_and_reject_nonsense() {
        assert!(matches!(release_range(None, 100), ReleaseRange::Full));
        assert!(matches!(
            release_range(Some("bytes=0-4"), 100),
            ReleaseRange::Partial { start: 0, end: 4 }
        ));
        assert!(matches!(
            release_range(Some("bytes=90-"), 100),
            ReleaseRange::Partial { start: 90, end: 99 }
        ));
        assert!(matches!(
            release_range(Some("bytes=-10"), 100),
            ReleaseRange::Partial { start: 90, end: 99 }
        ));
        assert!(matches!(
            release_range(Some("bytes=50-200"), 100),
            ReleaseRange::Partial { start: 50, end: 99 }
        ));
        // 越界、倒序、多段、空文件都不接。
        assert!(matches!(
            release_range(Some("bytes=100-"), 100),
            ReleaseRange::Invalid
        ));
        assert!(matches!(
            release_range(Some("bytes=9-2"), 100),
            ReleaseRange::Invalid
        ));
        assert!(matches!(
            release_range(Some("bytes=0-1,4-5"), 100),
            ReleaseRange::Invalid
        ));
        assert!(matches!(
            release_range(Some("bytes=0-0"), 0),
            ReleaseRange::Invalid
        ));
    }
}
