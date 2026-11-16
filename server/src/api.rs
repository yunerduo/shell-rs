//! 账号、云同步、更新清单与管理接口。
//!
//! 同步快照是密文：服务器只验大小、摘要和配额，从不解析内容。清单的
//! 签名在客户端校验，服务器只负责原样保存和分发。

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{SecondsFormat, Utc};
use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt as _;
use tokio_util::io::ReaderStream;

use crate::auth;
use crate::config::Config;
use crate::db::{AnalyticsEvent, Db, Snapshot, User};
use crate::error::{ApiError, ApiResult};
use crate::util;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Db>,
    pub limiter: Arc<Limiter>,
}

impl AppState {
    pub fn new(config: Config, db: Db) -> Self {
        Self {
            config: Arc::new(config),
            db: Arc::new(db),
            limiter: Arc::new(Limiter::default()),
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
) -> ApiResult<Json<AuthResponse>> {
    let ip = client_ip(&headers, addr);
    if !state
        .limiter
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
        return Err(ApiError::BadRequest(format!(
            "口令至少要 {} 个字符",
            auth::PASSWORD_MIN
        )));
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
    issue_session(&state, &headers, user).map(Json)
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(request): Json<LoginRequest>,
) -> ApiResult<Json<AuthResponse>> {
    let ip = client_ip(&headers, addr);
    if !state
        .limiter
        .allow(format!("login:{ip}"), 20, Duration::from_secs(10 * 60))
    {
        return Err(ApiError::TooLarge("尝试太频繁，请稍后再试".into()));
    }
    let username = request.username.trim().to_string();
    let Some(user) = state.db.user_by_name(&username)? else {
        return Err(ApiError::Unauthorized);
    };
    let password = request.password;
    let hash = user.password_hash.clone();
    let valid = tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .map_err(ApiError::internal)?;
    if !valid {
        return Err(ApiError::Unauthorized);
    }
    issue_session(&state, &headers, user).map(Json)
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<StatusCode> {
    let token = bearer(&headers).ok_or(ApiError::Unauthorized)?;
    state.db.delete_session(&auth::token_hash(token))?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<MeResponse>> {
    let user = require_user(&state, &headers)?;
    Ok(Json(me_response(&state, &user)?))
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
    latest: Option<Snapshot>,
    versions: Vec<Snapshot>,
}

pub async fn list_sync_versions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<SyncList>> {
    let user = require_user(&state, &headers)?;
    let versions = state.db.snapshots(user.id)?;
    Ok(Json(SyncList {
        used_bytes: versions.iter().map(|s| s.size).sum(),
        quota_bytes: user.quota_bytes,
        latest: versions.first().cloned(),
        versions,
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
    if !state.limiter.allow(
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
    require_admin(&state, &headers)?;
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
    Ok(Json(serde_json::json!({
        "ok": true,
        "version": version,
        "file": file,
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
    require_admin(&state, &headers)?;
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

fn require_user(state: &AppState, headers: &HeaderMap) -> ApiResult<User> {
    let token = bearer(headers).ok_or(ApiError::Unauthorized)?;
    state
        .db
        .session_user(&auth::token_hash(token))?
        .ok_or(ApiError::Unauthorized)
}

fn require_admin(state: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    let expected = state
        .config
        .admin_token
        .as_deref()
        .ok_or(ApiError::NotFound)?;
    let given = bearer(headers).ok_or(ApiError::Unauthorized)?;
    if util::ct_eq(given, expected) {
        Ok(())
    } else {
        Err(ApiError::Unauthorized)
    }
}

/// 反向代理后面拿不到真实对端地址，优先看 `X-Forwarded-For` 的第一段。
fn client_ip(headers: &HeaderMap, addr: SocketAddr) -> String {
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

fn validate_version(version: &str) -> ApiResult<()> {
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
        assert!(validate_file_name("ShellRS-0.1.19-windows-x86_64-setup.exe").is_ok());
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
                             "appVersion":"0.1.19","sdkVersion":"shellrs@0.1.19"},
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
}
