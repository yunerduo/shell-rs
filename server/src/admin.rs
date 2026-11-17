//! 官网后台的 JSON 接口。
//!
//! 界面是 `server/admin-ui`（pure-admin-thin，Vue 3 + Element Plus），
//! 编译后的静态文件由本模块托管在 `/admin`。管理员账号与客户端用户完全
//! 分开：登录后拿一个不透明会话令牌，角色决定能调哪些接口，服务端逐接口
//! 校验，不依赖前端隐藏菜单。

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Component, Path as FsPath, PathBuf};
use std::time::Duration;

use axum::Json;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{self, AppState};
use crate::auth;
use crate::db::{AdminUser, ChangelogEntry, ReleaseVersion, UsageRow, UserRow};
use crate::error::{ApiError, ApiResult};
use crate::release::{self, FileInfo};
use crate::signing;
use crate::util;

pub const ROLE_OWNER: &str = "owner";
pub const ROLE_PUBLISHER: &str = "publisher";
pub const ROLE_VIEWER: &str = "viewer";

const OWNER_PERMISSIONS: &[&str] = &[
    "release:read",
    "release:publish",
    "release:upload",
    "changelog:read",
    "changelog:write",
    "usage:read",
    "admin:read",
    "admin:write",
    "user:read",
    "user:write",
    "ai:read",
    "ai:write",
];

const PUBLISHER_PERMISSIONS: &[&str] = &[
    "release:read",
    "release:publish",
    "release:upload",
    "changelog:read",
    "changelog:write",
    "usage:read",
];

const VIEWER_PERMISSIONS: &[&str] = &["release:read", "changelog:read", "usage:read"];

pub fn valid_role(role: &str) -> bool {
    matches!(role, ROLE_OWNER | ROLE_PUBLISHER | ROLE_VIEWER)
}

pub fn permissions_for(role: &str) -> &'static [&'static str] {
    match role {
        ROLE_OWNER => OWNER_PERMISSIONS,
        ROLE_PUBLISHER => PUBLISHER_PERMISSIONS,
        _ => VIEWER_PERMISSIONS,
    }
}

pub fn has_permission(admin: &AdminUser, permission: &str) -> bool {
    permissions_for(&admin.role).contains(&permission)
}

/// 当前请求携带的后台会话。供 `api.rs` 的 CLI 管理接口复用来接受后台令牌。
pub fn current_admin(state: &AppState, headers: &HeaderMap) -> Option<AdminUser> {
    let token = api::bearer_or_cookie(headers, api::ADMIN_COOKIE)?;
    state
        .db
        .admin_session(&auth::token_hash(token))
        .ok()
        .flatten()
}

fn require(state: &AppState, headers: &HeaderMap, permission: &str) -> ApiResult<AdminUser> {
    let admin = current_admin(state, headers).ok_or(ApiError::Unauthorized)?;
    if has_permission(&admin, permission) {
        Ok(admin)
    } else {
        Err(ApiError::Forbidden("当前角色没有这个操作的权限".into()))
    }
}

/// 后台会话 cookie 与用户会话 cookie 名字不同、Path 不同；这里只认后台那个。
fn admin_cookie_response(response: &mut Response, token: &str, days: i64) {
    if let Some(cookie) = api::session_cookie(api::ADMIN_COOKIE, token, days, "/admin") {
        response.headers_mut().append(header::SET_COOKIE, cookie);
    }
}

// ---- 登录 ----

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshForm {
    refresh_token: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionData {
    avatar: String,
    username: String,
    nickname: String,
    roles: Vec<String>,
    permissions: Vec<String>,
    access_token: String,
    refresh_token: String,
    expires: String,
}

fn session_data(admin: &AdminUser, token: &str, expires: &str) -> SessionData {
    SessionData {
        avatar: String::new(),
        username: admin.username.clone(),
        nickname: admin.username.clone(),
        roles: vec![admin.role.clone()],
        permissions: permissions_for(&admin.role)
            .iter()
            .map(|permission| permission.to_string())
            .collect(),
        access_token: token.to_string(),
        refresh_token: token.to_string(),
        expires: expires.to_string(),
    }
}

pub async fn login(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(form): Json<LoginForm>,
) -> ApiResult<Response> {
    let ip = api::client_ip(&headers, addr);
    // 后台有自己的限流器与失败记录，用户入口刷不动这里。
    if !state.admin_limiter.allow(
        format!("admin-login:{ip}"),
        10,
        Duration::from_secs(10 * 60),
    ) {
        return Err(ApiError::TooMany("尝试太频繁，请十分钟后再试".into()));
    }
    let username = form.username.trim().to_string();
    let key = username.to_ascii_lowercase();
    if let Some(until) = state.db.login_locked_at("admin", &key)? {
        return Err(ApiError::Locked(format!(
            "口令错误次数太多，请在 {until} 之后再试"
        )));
    }
    let found = state.db.admin_by_name(&username)?;
    let hash = found.as_ref().map(|admin| admin.password_hash.clone());
    let password = form.password;
    let verified = tokio::task::spawn_blocking(move || match hash {
        Some(hash) => auth::verify_password(&password, &hash),
        None => {
            let _ = auth::hash_password(&password);
            false
        }
    })
    .await
    .unwrap_or(false);
    if !verified {
        let until = state.db.record_login_failure(
            "admin",
            &key,
            state.config.login_max_failures,
            state.config.login_lock_minutes,
        )?;
        if !until.is_empty() {
            return Err(ApiError::Locked(format!(
                "口令错误次数太多，请在 {until} 之后再试"
            )));
        }
        tracing::warn!(username, "后台登录失败");
        return Err(ApiError::Credentials("账号或口令不对".into()));
    }
    let Some(admin) = found else {
        return Err(ApiError::Credentials("账号或口令不对".into()));
    };
    state.db.clear_login_failures("admin", &key)?;
    let token = util::random_token();
    let expires = util::later(state.config.admin_session_days);
    state
        .db
        .create_admin_session(admin.id, &auth::token_hash(&token), &expires)?;
    tracing::info!(admin = %admin.username, role = %admin.role, "后台登录成功");
    let mut response = Json(json!({
        "success": true,
        "data": session_data(&admin, &token, &expires),
    }))
    .into_response();
    admin_cookie_response(&mut response, &token, state.config.admin_session_days);
    Ok(response)
}

pub async fn refresh(
    State(state): State<AppState>,
    Json(form): Json<RefreshForm>,
) -> ApiResult<Response> {
    let token = form.refresh_token.trim();
    if token.is_empty() {
        return Err(ApiError::Unauthorized);
    }
    let token_hash = auth::token_hash(token);
    let Some(_admin) = state.db.admin_session(&token_hash)? else {
        return Err(ApiError::Unauthorized);
    };
    let expires = util::later(state.config.admin_session_days);
    if !state.db.refresh_admin_session(&token_hash, &expires)? {
        return Err(ApiError::Unauthorized);
    }
    let mut response = Json(json!({
        "success": true,
        "data": {
            "accessToken": token,
            "refreshToken": token,
            "expires": expires,
        },
    }))
    .into_response();
    admin_cookie_response(&mut response, token, state.config.admin_session_days);
    Ok(response)
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = api::bearer_or_cookie(&headers, api::ADMIN_COOKIE) {
        state.db.delete_admin_session(&auth::token_hash(token))?;
    }
    let mut response = Json(json!({ "success": true })).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        api::clear_cookie(api::ADMIN_COOKIE, "/admin"),
    );
    Ok(response)
}

/// pure-admin 用这个接口取动态菜单。后台页面是静态路由，这里返回空表即可；
/// 菜单与按钮权限由各自路由的 `meta.roles` / `meta.auths` 控制。
pub async fn routes(headers: HeaderMap, State(state): State<AppState>) -> ApiResult<Json<Value>> {
    require(&state, &headers, "release:read")?;
    Ok(Json(json!({ "success": true, "data": [] })))
}

pub async fn me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let admin = current_admin(&state, &headers).ok_or(ApiError::Unauthorized)?;
    Ok(Json(json!({
        "success": true,
        "data": {
            "username": admin.username,
            "role": admin.role,
            "permissions": permissions_for(&admin.role),
        },
    })))
}

// ---- 版本发布 ----

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct ChannelInfo {
    version: String,
    severity: String,
    notes: String,
    notes_url: String,
    published_at: String,
}

fn manifest_info(envelope: &str) -> Option<ChannelInfo> {
    #[derive(Deserialize)]
    struct Envelope {
        manifest: String,
    }
    let envelope: Envelope = serde_json::from_str(envelope).ok()?;
    let manifest: Value = serde_json::from_str(&envelope.manifest).ok()?;
    // 新清单用 severity；老清单只有 level，统一按两个值读出来。
    let severity = manifest
        .get("severity")
        .and_then(Value::as_str)
        .or_else(|| manifest.get("level").and_then(Value::as_str))
        .map(|value| {
            if value == release::SEVERITY_REQUIRED {
                release::SEVERITY_REQUIRED
            } else {
                release::SEVERITY_RECOMMENDED
            }
        })
        .unwrap_or(release::SEVERITY_RECOMMENDED)
        .to_string();
    Some(ChannelInfo {
        version: manifest
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        severity,
        notes: manifest
            .get("notes")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        notes_url: manifest
            .get("notes_url")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        published_at: manifest
            .get("published_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn channel_info(state: &AppState, channel: &str) -> Option<ChannelInfo> {
    state
        .db
        .current_manifest(channel)
        .ok()
        .flatten()
        .and_then(|envelope| manifest_info(&envelope))
}

pub async fn releases(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require(&state, &headers, "release:read")?;

    // 磁盘是安装包的事实来源；库里的记录负责大小、摘要和下载次数。
    // 这里顺手把还没登记过的文件补上，老包第一次出现在后台就能看到。
    let mut versions: BTreeSet<String> = BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(state.config.releases_dir()) {
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let version = entry.file_name().to_string_lossy().to_string();
            if api::validate_version(&version).is_err() {
                continue;
            }
            versions.insert(version.clone());
            if let Ok(inner) = std::fs::read_dir(entry.path()) {
                for file in inner.flatten() {
                    let name = file.file_name().to_string_lossy().to_string();
                    let Ok(meta) = file.metadata() else {
                        continue;
                    };
                    if !meta.is_file() {
                        continue;
                    }
                    let known = state.db.package(&version, &name)?;
                    let changed = known
                        .as_ref()
                        .map(|package| package.size != meta.len() as i64)
                        .unwrap_or(true);
                    if changed {
                        let (platform, arch) = release::file_platform(&version, &name)
                            .map(release::split_platform)
                            .unwrap_or(("", ""));
                        state.db.upsert_package(
                            &version,
                            &name,
                            platform,
                            arch,
                            meta.len() as i64,
                            "",
                        )?;
                    }
                }
            }
        }
    }

    let records = state.db.release_versions(100)?;
    for record in &records {
        versions.insert(record.version.clone());
    }

    let mut releases = Vec::new();
    for version in versions {
        let packages = state.db.packages(&version)?;
        let total_size: i64 = packages.iter().map(|package| package.size).sum();
        let downloads: i64 = packages.iter().map(|package| package.downloads).sum();
        let files: Vec<Value> = packages
            .iter()
            .map(|package| {
                json!({
                    "name": package.file,
                    "platform": package.platform,
                    "arch": package.arch,
                    "size": package.size,
                    "sha256": package.sha256,
                    "downloads": package.downloads,
                    "uploadedAt": package.uploaded_at,
                    "withdrawn": package.withdrawn,
                })
            })
            .collect();
        releases.push(json!({
            "version": version,
            "files": files,
            "totalSize": total_size,
            "downloads": downloads,
        }));
    }

    let mut manifests = Vec::new();
    for row in state.db.manifest_rows(100)? {
        let info = manifest_info(&row.envelope);
        manifests.push(json!({
            "channel": row.channel,
            "version": row.version,
            "severity": info.as_ref().map(|info| info.severity.clone()).unwrap_or_else(|| release::SEVERITY_RECOMMENDED.to_string()),
            "publishedAt": row.published_at,
            "withdrawn": row.withdrawn,
        }));
    }

    Ok(Json(json!({
        "success": true,
        "data": {
            "stable": channel_info(&state, "stable"),
            "beta": channel_info(&state, "beta"),
            "manifests": manifests,
            "releases": releases,
            "records": records.iter().map(release_record_json).collect::<Vec<_>>(),
        },
    })))
}

fn release_record_json(record: &ReleaseVersion) -> Value {
    json!({
        "version": record.version,
        "channel": record.channel,
        "severity": record.severity,
        "notes": record.notes,
        "notesUrl": record.notes_url,
        "minimumVersion": record.minimum_version,
        "rollout": record.rollout,
        "publishedAt": record.published_at,
        "withdrawn": record.withdrawn,
        "packageCount": record.package_count,
        "totalSize": record.total_size,
        "downloads": record.downloads,
    })
}

#[derive(Deserialize)]
pub struct ChannelForm {
    channel: String,
}

/// 撤下某个通道上的某个版本：清单立刻不再下发，包也停发（除非还有别的
/// 通道在发同一版本）。发布记录保留，随时可以补发。
pub async fn withdraw_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(version): Path<String>,
    Json(form): Json<ChannelForm>,
) -> ApiResult<Json<Value>> {
    let admin = require(&state, &headers, "release:publish")?;
    if !release::valid_channel(&form.channel) {
        return Err(ApiError::BadRequest("通道只有 stable 和 beta".into()));
    }
    api::validate_version(&version)?;
    if state.db.release_version(&version, &form.channel)?.is_none() {
        return Err(ApiError::NotFound);
    }
    state
        .db
        .set_release_withdrawn(&version, &form.channel, true)?;
    state
        .db
        .set_manifest_withdrawn(&form.channel, &version, true)?;
    if state.db.active_release_channels(&version)? == 0 {
        state.db.set_packages_withdrawn(&version, true)?;
    }
    tracing::info!(admin = %admin.username, channel = %form.channel, %version, "后台撤下版本");
    Ok(Json(json!({ "success": true })))
}

/// 补发一个撤下过的版本：用保存的发布记录重新生成、签名清单，
/// 发布时间更新为现在，并恢复安装包下载。
pub async fn republish_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(version): Path<String>,
    Json(form): Json<ChannelForm>,
) -> ApiResult<Json<Value>> {
    let admin = require(&state, &headers, "release:publish")?;
    if !release::valid_channel(&form.channel) {
        return Err(ApiError::BadRequest("通道只有 stable 和 beta".into()));
    }
    api::validate_version(&version)?;
    let record = state
        .db
        .release_version(&version, &form.channel)?
        .ok_or(ApiError::NotFound)?;
    let notes_url = if record.notes_url.trim().is_empty() {
        format!("{}/changelog#v{}", state.config.public_url, version)
    } else {
        record.notes_url.clone()
    };
    let published_at = publish_version(
        &state,
        &PublishInput {
            channel: form.channel.clone(),
            version: version.clone(),
            severity: record.severity.clone(),
            notes: record.notes.clone(),
            notes_url,
            minimum_version: record.minimum_version.clone(),
            rollout: record.rollout,
        },
    )
    .await?;
    tracing::info!(admin = %admin.username, channel = %form.channel, %version, "后台补发版本");
    Ok(Json(json!({
        "success": true,
        "data": { "channel": form.channel, "version": version, "publishedAt": published_at },
    })))
}

#[derive(Deserialize)]
pub struct VersionQuery {
    version: String,
}

pub async fn packages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<VersionQuery>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "release:read")?;
    api::validate_version(&query.version)?;
    let rows: Vec<Value> = release::packages(&query.version)
        .into_iter()
        .map(|package| {
            json!({
                "platform": package.platform,
                "file": package.file,
                "asset": package.asset,
                "installer": package.installer,
            })
        })
        .collect();
    Ok(Json(json!({ "success": true, "data": rows })))
}

/// 按需计算某个版本已上传安装包的大小与 SHA-256，供后台核对。
pub async fn release_hashes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(version): Path<String>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "release:read")?;
    api::validate_version(&version)?;
    let dir = state.config.releases_dir().join(&version);
    let mut rows = Vec::new();
    for package in release::packages(&version) {
        let path = dir.join(&package.file);
        if !path.is_file() {
            continue;
        }
        let size = path.metadata().map_err(ApiError::internal)?.len();
        let sha256 = tokio::task::spawn_blocking(move || hash_file(&path))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
        // 算一次就存下来，之后版本列表直接显示，不用反复读大文件。
        let (platform, arch) = release::split_platform(package.platform);
        if let Err(error) = state.db.upsert_package(
            &version,
            &package.file,
            platform,
            arch,
            size as i64,
            &sha256,
        ) {
            tracing::warn!(error = %error, %version, file = %package.file, "摘要已算出，但写入失败");
        }
        rows.push(json!({
            "name": package.file,
            "platform": platform,
            "arch": arch,
            "size": size,
            "sha256": sha256,
        }));
    }
    Ok(Json(json!({ "success": true, "data": rows })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReleaseForm {
    channel: String,
    version: String,
    severity: String,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    notes_url: String,
    #[serde(default)]
    minimum_version: String,
    #[serde(default = "everyone")]
    rollout: f64,
}

fn everyone() -> f64 {
    1.0
}

pub async fn publish_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<ReleaseForm>,
) -> ApiResult<Json<Value>> {
    let admin = require(&state, &headers, "release:publish")?;
    if !release::valid_channel(&form.channel) {
        return Err(ApiError::BadRequest("通道只有 stable 和 beta".into()));
    }
    api::validate_version(&form.version)?;
    if !release::valid_severity(&form.severity) {
        return Err(ApiError::BadRequest(
            "更新分级只有 recommended 和 required".into(),
        ));
    }
    if !form.rollout.is_finite() || !(0.0..=1.0).contains(&form.rollout) {
        return Err(ApiError::BadRequest("灰度比例要在 0 到 1 之间".into()));
    }
    if form.notes.len() > 64 * 1024 {
        return Err(ApiError::BadRequest("更新说明太长了".into()));
    }
    let minimum_version = form.minimum_version.trim();
    if !minimum_version.is_empty() {
        api::validate_version(minimum_version)?;
    }
    let notes_url = if form.notes_url.trim().is_empty() {
        format!("{}/changelog#v{}", state.config.public_url, form.version)
    } else {
        let url = form.notes_url.trim().to_string();
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err(ApiError::BadRequest(
                "说明链接要以 http:// 或 https:// 开头".into(),
            ));
        }
        url
    };

    let (published_at, files) = publish_version(
        &state,
        &PublishInput {
            channel: form.channel.clone(),
            version: form.version.clone(),
            severity: form.severity.clone(),
            notes: form.notes.clone(),
            notes_url,
            minimum_version: (!minimum_version.is_empty()).then(|| minimum_version.to_string()),
            rollout: form.rollout,
        },
    )
    .await?;
    tracing::info!(
        admin = %admin.username,
        channel = %form.channel,
        version = %form.version,
        severity = %form.severity,
        "后台发布完成"
    );
    Ok(Json(json!({
        "success": true,
        "data": {
            "channel": form.channel,
            "version": form.version,
            "severity": form.severity,
            "publishedAt": published_at,
            "files": files,
        },
    })))
}

struct PublishInput {
    channel: String,
    version: String,
    severity: String,
    notes: String,
    notes_url: String,
    minimum_version: Option<String>,
    rollout: f64,
}

/// 扫描安装包、生成清单、签名并落库。首次发布和补发走同一条路。
async fn publish_version(
    state: &AppState,
    input: &PublishInput,
) -> ApiResult<(String, Vec<String>)> {
    // 签名私钥是这条路的前置条件，必须先确认它，再扫包、再写库。补发失败时
    // 绝不能留下「清单已撤下、安装包却恢复可下载」的半截状态。
    let Some(key) = state.config.minisign_key.clone() else {
        return Err(ApiError::BadRequest(
            "服务器没有配置签名私钥（SHELLAS_MINISIGN_KEY），请改用命令行 publish".into(),
        ));
    };
    let dir = state.config.releases_dir().join(&input.version);
    let mut found = BTreeMap::new();
    let mut scanned = Vec::new();
    for package in release::packages(&input.version) {
        let path = dir.join(&package.file);
        if !path.is_file() {
            continue;
        }
        let size = path.metadata().map_err(ApiError::internal)?.len();
        let sha256 = tokio::task::spawn_blocking(move || hash_file(&path))
            .await
            .map_err(ApiError::internal)?
            .map_err(ApiError::internal)?;
        let (platform, arch) = release::split_platform(package.platform);
        scanned.push((
            package.file.clone(),
            platform,
            arch,
            size as i64,
            sha256.clone(),
        ));
        found.insert(package.file, FileInfo { size, sha256 });
    }
    let has_asset = release::packages(&input.version)
        .iter()
        .any(|package| package.asset && found.contains_key(&package.file));
    if !has_asset {
        return Err(ApiError::BadRequest(
            "这个版本还没有可用的安装包，请先上传至少一个平台的包".into(),
        ));
    }

    let published_at = util::now();
    let manifest = release::build_manifest(
        &state.config.public_url,
        &input.channel,
        &input.version,
        &input.severity,
        &input.notes,
        Some(&input.notes_url),
        input.minimum_version.as_deref(),
        input.rollout,
        &published_at,
        &found,
    );
    let manifest_text = serde_json::to_string_pretty(&manifest).map_err(ApiError::internal)?;
    let envelope = {
        let channel = input.channel.clone();
        let version = input.version.clone();
        let text = manifest_text.clone();
        let password = state.config.minisign_password.clone();
        let signature = tokio::task::spawn_blocking(move || {
            signing::sign(&key, password.as_deref(), &channel, &version, &text)
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
        serde_json::to_string_pretty(&json!({
            "manifest": manifest_text,
            "signature": signature,
        }))
        .map_err(ApiError::internal)?
    };

    // 签名成功之后才动库：清单签不出来时，已撤下的安装包记录必须原样保留。
    for (file, platform, arch, size, sha256) in &scanned {
        if let Err(error) =
            state
                .db
                .upsert_package(&input.version, file, platform, arch, *size, sha256)
        {
            tracing::warn!(error = %error, version = %input.version, "安装包元数据写入失败");
        }
    }

    state
        .db
        .put_manifest(&input.channel, &input.version, &envelope, &published_at)?;
    if let Err(error) = state.db.put_release_version(
        &input.version,
        &input.channel,
        &input.severity,
        &input.notes,
        &input.notes_url,
        input.minimum_version.as_deref(),
        input.rollout,
        &published_at,
    ) {
        tracing::warn!(error = %error, version = %input.version, "发布记录写入失败");
    }
    if let Err(error) = state.db.set_packages_withdrawn(&input.version, false) {
        tracing::warn!(error = %error, version = %input.version, "安装包状态恢复失败");
    }
    let dir = state.config.manifests_dir();
    if let Err(error) = tokio::fs::create_dir_all(&dir).await {
        tracing::warn!(error = %error, "清单落盘目录创建失败");
    } else if let Err(error) = tokio::fs::write(
        dir.join(format!("{}.json", input.channel)),
        envelope.as_bytes(),
    )
    .await
    {
        tracing::warn!(error = %error, channel = %input.channel, "清单已入库，但落盘副本写失败");
    }
    if let Err(error) = state.db.changelog_from_publish(
        &input.version,
        &input.channel,
        &input.severity,
        &input.notes,
        &published_at,
    ) {
        tracing::warn!(error = %error, version = %input.version, "更新记录同步失败");
    }
    Ok((published_at, found.keys().cloned().collect::<Vec<String>>()))
}

fn hash_file(path: &FsPath) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

// ---- 更新记录 ----

fn changelog_json(entry: &ChangelogEntry) -> Value {
    json!({
        "id": entry.id,
        "version": entry.version,
        "channel": entry.channel,
        "severity": entry.severity,
        "title": entry.title,
        "body": entry.body,
        "publishedAt": entry.published_at,
        "updatedAt": entry.updated_at,
    })
}

pub async fn changelog(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "changelog:read")?;
    let rows: Vec<Value> = state
        .db
        .changelog_entries()?
        .iter()
        .map(changelog_json)
        .collect();
    Ok(Json(json!({ "success": true, "data": rows })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangelogForm {
    version: String,
    channel: String,
    severity: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    published_at: String,
}

struct ChangelogValues {
    version: String,
    channel: String,
    severity: String,
    title: String,
    body: String,
    published_at: String,
}

fn changelog_values(form: ChangelogForm) -> ApiResult<ChangelogValues> {
    let version = form.version.trim().to_string();
    api::validate_version(&version)?;
    if !release::valid_channel(form.channel.trim()) {
        return Err(ApiError::BadRequest("通道只有 stable 和 beta".into()));
    }
    let severity = form.severity.trim().to_string();
    if !release::valid_severity(&severity) {
        return Err(ApiError::BadRequest(
            "更新分级只有 recommended 和 required".into(),
        ));
    }
    if form.body.len() > 256 * 1024 {
        return Err(ApiError::BadRequest("正文太长了".into()));
    }
    Ok(ChangelogValues {
        version,
        channel: form.channel.trim().to_string(),
        severity,
        title: form.title.trim().to_string(),
        body: form.body,
        published_at: normalize_date(&form.published_at)?,
    })
}

fn normalize_date(text: &str) -> ApiResult<String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(util::now());
    }
    if text.len() == 10 {
        return Ok(format!("{text}T00:00:00Z"));
    }
    DateTime::parse_from_rfc3339(text)
        .map(|at| {
            at.to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .map_err(|_| ApiError::BadRequest("日期格式不对".into()))
}

pub async fn create_changelog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<ChangelogForm>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "changelog:write")?;
    let values = changelog_values(form)?;
    let id = state.db.create_changelog(
        &values.version,
        &values.channel,
        &values.severity,
        &values.title,
        &values.body,
        &values.published_at,
    )?;
    let entry = state
        .db
        .changelog_entry(id)?
        .ok_or(ApiError::Internal("更新记录写入后读不到".into()))?;
    Ok(Json(
        json!({ "success": true, "data": changelog_json(&entry) }),
    ))
}

pub async fn update_changelog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(form): Json<ChangelogForm>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "changelog:write")?;
    let values = changelog_values(form)?;
    state.db.update_changelog(
        id,
        &values.version,
        &values.channel,
        &values.severity,
        &values.title,
        &values.body,
        &values.published_at,
    )?;
    let entry = state.db.changelog_entry(id)?.ok_or(ApiError::NotFound)?;
    Ok(Json(
        json!({ "success": true, "data": changelog_json(&entry) }),
    ))
}

pub async fn delete_changelog(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "changelog:write")?;
    if !state.db.delete_changelog(id)? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(json!({ "success": true })))
}

// ---- 使用记录 ----

#[derive(Deserialize, Default)]
pub struct UsageQuery {
    #[serde(default)]
    days: Option<i64>,
}

fn usage_row_json(row: &UsageRow) -> Value {
    json!({
        "key": if row.key.trim().is_empty() { "（未知）" } else { row.key.as_str() },
        "deviceDays": row.device_days,
        "events": row.events,
    })
}

pub async fn usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UsageQuery>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "usage:read")?;
    let days = query.days.unwrap_or(30).clamp(1, 365);
    let since = (Utc::now() - chrono::Duration::days(days))
        .format("%Y-%m-%d")
        .to_string();
    let totals = state.db.usage_totals(&since)?;
    let daily: Vec<Value> = state
        .db
        .usage_daily(&since)?
        .iter()
        .map(|row| {
            json!({
                "date": row.key,
                "deviceDays": row.device_days,
                "events": row.events,
            })
        })
        .collect();
    let versions: Vec<Value> = state
        .db
        .usage_versions(&since)?
        .iter()
        .map(usage_row_json)
        .collect();
    let platforms: Vec<Value> = state
        .db
        .usage_platforms(&since)?
        .iter()
        .map(usage_row_json)
        .collect();
    let top_events: Vec<Value> = state
        .db
        .usage_top_events(&since, 15)?
        .iter()
        .map(usage_row_json)
        .collect();
    let recent: Vec<Value> = state
        .db
        .usage_recent(20)?
        .iter()
        .map(|event| {
            json!({
                "name": event.name,
                "appVersion": event.app_version,
                "osName": event.os_name,
                "occurredAt": event.occurred_at,
                "isDebug": event.is_debug,
            })
        })
        .collect();
    Ok(Json(json!({
        "success": true,
        "data": {
            "days": days,
            "since": since,
            "totals": {
                "events": totals.events,
                "deviceDays": totals.device_days,
                "sessions": totals.sessions,
            },
            "daily": daily,
            "versions": versions,
            "platforms": platforms,
            "topEvents": top_events,
            "recent": recent,
        },
    })))
}

// ---- 用户账号（与管理员完全分开） ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UserDto {
    id: i64,
    username: String,
    email: Option<String>,
    quota_bytes: i64,
    used_bytes: i64,
    versions: i64,
    created_at: String,
    disabled: bool,
}

fn user_dto(row: &UserRow) -> UserDto {
    UserDto {
        id: row.id,
        username: row.username.clone(),
        email: row.email.clone(),
        quota_bytes: row.quota_bytes,
        used_bytes: row.used_bytes,
        versions: row.versions,
        created_at: row.created_at.clone(),
        disabled: row.disabled,
    }
}

fn user_dto_by_id(state: &AppState, id: i64) -> ApiResult<UserDto> {
    state
        .db
        .users()?
        .iter()
        .find(|row| row.id == id)
        .map(user_dto)
        .ok_or(ApiError::NotFound)
}

pub async fn users(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require(&state, &headers, "user:read")?;
    let rows: Vec<UserDto> = state.db.users()?.iter().map(user_dto).collect();
    Ok(Json(json!({ "success": true, "data": rows })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateUserForm {
    #[serde(default)]
    quota_mb: Option<i64>,
    #[serde(default)]
    disabled: Option<bool>,
}

/// 调整云同步配额或停用账号。停用会立刻作废该用户的所有会话。
pub async fn update_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(form): Json<UpdateUserForm>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "user:write")?;
    let Some(user) = state.db.user_by_id(id)? else {
        return Err(ApiError::NotFound);
    };
    if let Some(quota_mb) = form.quota_mb {
        if !(1..=10 * 1024).contains(&quota_mb) {
            return Err(ApiError::BadRequest("配额要在 1 MB 到 10 GB 之间".into()));
        }
        state.db.set_user_quota(id, quota_mb * 1024 * 1024)?;
    }
    if let Some(disabled) = form.disabled {
        state.db.set_user_disabled(id, disabled)?;
        tracing::info!(user = %user.username, disabled, "后台调整了用户状态");
    }
    Ok(Json(json!({
        "success": true,
        "data": user_dto_by_id(&state, id)?,
    })))
}

#[derive(Deserialize)]
pub struct ResetUserPasswordForm {
    password: String,
}

/// 邮件找回还没接，后台先给管理员一个重置用户口令的入口。
pub async fn reset_user_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(form): Json<ResetUserPasswordForm>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "user:write")?;
    let Some(user) = state.db.user_by_id(id)? else {
        return Err(ApiError::NotFound);
    };
    if !auth::valid_password(&form.password) {
        return Err(ApiError::BadRequest(auth::PASSWORD_RULE.into()));
    }
    let hash = tokio::task::spawn_blocking({
        let password = form.password.clone();
        move || auth::hash_password(&password)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    state.db.set_user_password(id, &hash)?;
    tracing::info!(user = %user.username, "后台重置了用户口令");
    Ok(Json(json!({ "success": true })))
}

pub async fn delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "user:write")?;
    let Some(user) = state.db.user_by_id(id)? else {
        return Err(ApiError::NotFound);
    };
    state.db.delete_user(id)?;
    // 快照和同步文档的元数据已级联删除，密文文件也一起清掉。
    let dir = state.config.sync_dir().join(id.to_string());
    if let Err(error) = tokio::fs::remove_dir_all(&dir).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(error = %error, user = %user.username, "用户已删除，但同步目录清理失败");
    }
    tracing::info!(user = %user.username, "后台删除了用户");
    Ok(Json(json!({ "success": true })))
}

// ---- 管理员账号 ----

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AdminDto {
    id: i64,
    username: String,
    role: String,
    created_at: String,
}

fn admin_dto(admin: &AdminUser) -> AdminDto {
    AdminDto {
        id: admin.id,
        username: admin.username.clone(),
        role: admin.role.clone(),
        created_at: admin.created_at.clone(),
    }
}

pub async fn admins(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    require(&state, &headers, "admin:read")?;
    let rows: Vec<AdminDto> = state.db.admins()?.iter().map(admin_dto).collect();
    Ok(Json(json!({ "success": true, "data": rows })))
}

#[derive(Deserialize)]
pub struct NewAdminForm {
    username: String,
    password: String,
    role: String,
}

pub async fn create_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<NewAdminForm>,
) -> ApiResult<Json<Value>> {
    require(&state, &headers, "admin:write")?;
    let username = form.username.trim();
    if !auth::valid_username(username) {
        return Err(ApiError::BadRequest(
            "账号 3–32 个字符，只能是字母、数字、下划线和短横线".into(),
        ));
    }
    if !auth::valid_password(&form.password) {
        return Err(ApiError::BadRequest(auth::PASSWORD_RULE.into()));
    }
    if !valid_role(&form.role) {
        return Err(ApiError::BadRequest(
            "角色只有 owner、publisher、viewer".into(),
        ));
    }
    let hash = tokio::task::spawn_blocking({
        let password = form.password.clone();
        move || auth::hash_password(&password)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    let id = state.db.create_admin(username, &hash, &form.role)?;
    let admin = state.db.admin_by_id(id)?.ok_or(ApiError::NotFound)?;
    Ok(Json(json!({ "success": true, "data": admin_dto(&admin) })))
}

#[derive(Deserialize)]
pub struct UpdateAdminForm {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

pub async fn update_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Json(form): Json<UpdateAdminForm>,
) -> ApiResult<Json<Value>> {
    let actor = require(&state, &headers, "admin:write")?;
    let Some(admin) = state.db.admin_by_id(id)? else {
        return Err(ApiError::NotFound);
    };
    if let Some(role) = form.role.as_deref() {
        if !valid_role(role) {
            return Err(ApiError::BadRequest(
                "角色只有 owner、publisher、viewer".into(),
            ));
        }
        if admin.role == ROLE_OWNER && role != ROLE_OWNER && state.db.owner_count()? <= 1 {
            return Err(ApiError::BadRequest(
                "至少要留一个 owner，不能把最后一个 owner 降级".into(),
            ));
        }
        state.db.update_admin_role(id, role)?;
        if admin.id == actor.id && role != ROLE_OWNER {
            // 自己把自己降级后，旧会话仍按新角色校验，不需要额外处理。
            tracing::info!(admin = %admin.username, role, "管理员降级了自己");
        }
    }
    if let Some(password) = form.password.as_deref().filter(|value| !value.is_empty()) {
        if !auth::valid_password(password) {
            return Err(ApiError::BadRequest(auth::PASSWORD_RULE.into()));
        }
        let hash = tokio::task::spawn_blocking({
            let password = password.to_string();
            move || auth::hash_password(&password)
        })
        .await
        .map_err(ApiError::internal)?
        .map_err(ApiError::internal)?;
        state.db.set_admin_password(id, &hash)?;
        state.db.delete_admin_sessions(id)?;
    }
    let admin = state.db.admin_by_id(id)?.ok_or(ApiError::NotFound)?;
    Ok(Json(json!({ "success": true, "data": admin_dto(&admin) })))
}

pub async fn delete_admin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Json<Value>> {
    let actor = require(&state, &headers, "admin:write")?;
    if actor.id == id {
        return Err(ApiError::BadRequest("不能删除当前登录的账号".into()));
    }
    let Some(admin) = state.db.admin_by_id(id)? else {
        return Err(ApiError::NotFound);
    };
    if admin.role == ROLE_OWNER && state.db.owner_count()? <= 1 {
        return Err(ApiError::BadRequest("至少要留一个 owner".into()));
    }
    state.db.delete_admin(id)?;
    state.db.delete_admin_sessions(id)?;
    Ok(Json(json!({ "success": true })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordForm {
    current_password: String,
    new_password: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(form): Json<PasswordForm>,
) -> ApiResult<Json<Value>> {
    let admin = current_admin(&state, &headers).ok_or(ApiError::Unauthorized)?;
    let hash = admin.password_hash.clone();
    let current = form.current_password;
    let verified = tokio::task::spawn_blocking(move || auth::verify_password(&current, &hash))
        .await
        .unwrap_or(false);
    if !verified {
        return Err(ApiError::BadRequest("当前口令不对".into()));
    }
    if !auth::valid_password(&form.new_password) {
        return Err(ApiError::BadRequest(auth::PASSWORD_RULE.into()));
    }
    let hash = tokio::task::spawn_blocking({
        let password = form.new_password.clone();
        move || auth::hash_password(&password)
    })
    .await
    .map_err(ApiError::internal)?
    .map_err(ApiError::internal)?;
    state.db.set_admin_password(admin.id, &hash)?;
    state.db.delete_admin_sessions(admin.id)?;
    Ok(Json(json!({ "success": true })))
}

// ---- 托管后台静态文件 ----

pub async fn spa_index(State(state): State<AppState>) -> Response {
    serve_admin_file(&state, "")
}

pub async fn spa(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    serve_admin_file(&state, &path)
}

fn serve_admin_file(state: &AppState, relative: &str) -> Response {
    let Some(relative) = safe_relative(relative) else {
        return (StatusCode::NOT_FOUND, "没有找到").into_response();
    };
    let root = &state.config.admin_ui_dir;
    let requested = if relative.is_empty() {
        root.join("index.html")
    } else {
        root.join(&relative)
    };
    if requested.is_file() {
        return file_response(&requested, relative.starts_with("static/"));
    }
    // 没有扩展名的是前端路由（hash 模式一般用不到），回退到 index.html。
    if relative.to_string_lossy().contains('.') {
        return (StatusCode::NOT_FOUND, "没有找到").into_response();
    }
    let index = root.join("index.html");
    if index.is_file() {
        file_response(&index, false)
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "后台界面还没有构建。请在 server/admin-ui 下执行 pnpm install && pnpm build。",
        )
            .into_response()
    }
}

fn safe_relative(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in FsPath::new(path).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn file_response(path: &FsPath, immutable: bool) -> Response {
    let Ok(bytes) = std::fs::read(path) else {
        return (StatusCode::NOT_FOUND, "没有找到").into_response();
    };
    let mut response = bytes.into_response();
    let mime = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
    {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "webmanifest" => "application/manifest+json",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "map" => "application/json",
        _ => "application/octet-stream",
    };
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; \
             style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; \
             font-src 'self' data:; connect-src 'self'; base-uri 'none'; \
             form-action 'self'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_map_to_permissions() {
        let viewer = AdminUser {
            id: 1,
            username: "v".into(),
            password_hash: String::new(),
            role: ROLE_VIEWER.into(),
            created_at: String::new(),
        };
        assert!(has_permission(&viewer, "usage:read"));
        assert!(!has_permission(&viewer, "release:publish"));
        assert!(!has_permission(&viewer, "admin:write"));
        assert!(valid_role(ROLE_PUBLISHER));
        assert!(!valid_role("root"));
    }

    #[test]
    fn dates_normalize_to_utc_seconds() {
        assert_eq!(
            normalize_date("2026-11-16").unwrap(),
            "2026-11-16T00:00:00Z"
        );
        assert_eq!(
            normalize_date("2026-11-16T08:00:00+08:00").unwrap(),
            "2026-11-16T00:00:00Z"
        );
        assert!(normalize_date("昨天").is_err());
    }

    #[test]
    fn a_manifest_without_a_severity_reads_as_recommended() {
        let info = manifest_info(
            r#"{"manifest":"{\"channel\":\"stable\",\"version\":\"0.1.9\",\"notes\":\"x\"}","signature":"s"}"#,
        )
        .unwrap();
        assert_eq!(info.severity, release::SEVERITY_RECOMMENDED);
        assert_eq!(info.version, "0.1.9");
    }

    #[test]
    fn the_old_level_field_still_upgrades_to_severity() {
        let info = manifest_info(
            r#"{"manifest":"{\"channel\":\"stable\",\"version\":\"0.1.9\",\"level\":\"required\"}","signature":"s"}"#,
        )
        .unwrap();
        assert_eq!(info.severity, release::SEVERITY_REQUIRED);
    }

    #[test]
    fn paths_outside_the_admin_ui_are_rejected() {
        assert!(safe_relative("../secrets").is_none());
        assert!(safe_relative("/etc/passwd").is_none());
        assert_eq!(
            safe_relative("static/js/index.js").unwrap(),
            PathBuf::from("static/js/index.js")
        );
    }

    /// 缺签名私钥时补发必须整体失败：不能在失败前把安装包恢复成可下载。
    #[tokio::test]
    async fn a_republish_without_a_signing_key_leaves_packages_withdrawn() {
        use crate::db::Db;

        let version = "9.9.9";
        let root = std::env::temp_dir().join(format!("shellas-republish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let releases = root.join("releases").join(version);
        std::fs::create_dir_all(&releases).unwrap();
        let file = format!("ShellAs-{version}-linux-x86_64.AppImage");
        std::fs::write(releases.join(&file), b"package").unwrap();

        let config = test_config(root.clone());
        let db = Db::open(&config.database_path()).unwrap();
        db.upsert_package(version, &file, "linux", "x86_64", 7, "")
            .unwrap();
        db.set_packages_withdrawn(version, true).unwrap();
        let state = AppState::new(config, db);

        let error = publish_version(
            &state,
            &PublishInput {
                channel: "stable".into(),
                version: version.into(),
                severity: release::SEVERITY_RECOMMENDED.into(),
                notes: "撤下后补发".into(),
                notes_url: String::new(),
                minimum_version: None,
                rollout: 1.0,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(error, ApiError::BadRequest(_)), "缺私钥应当是 400");
        let packages = state.db.packages(version).unwrap();
        assert_eq!(packages.len(), 1);
        assert!(packages[0].withdrawn, "补发失败后安装包不能恢复可下载");
        assert!(
            state.db.current_manifest("stable").unwrap().is_none(),
            "补发失败后清单不能重新下发"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    fn test_config(data_dir: PathBuf) -> crate::config::Config {
        crate::config::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            data_dir,
            public_url: "https://example.test".into(),
            admin_token: None,
            admin_user: None,
            admin_password: None,
            admin_session_days: 7,
            minisign_key: None,
            minisign_password: None,
            admin_ui_dir: PathBuf::from("admin-ui/dist"),
            login_max_failures: 5,
            login_lock_minutes: 15,
            default_quota_bytes: 50 * 1024 * 1024,
            max_snapshots: 20,
            session_days: 30,
            max_snapshot_bytes: 100 * 1024 * 1024,
            max_package_bytes: 400 * 1024 * 1024,
            ai_base_url: None,
            ai_api_key: None,
            ai_model: None,
        }
    }
}
