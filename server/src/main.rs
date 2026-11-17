//! Agree ShellAs 服务端：官网、自建更新、端到端加密的云同步。
//!
//!   shellas-server                    启动服务（同 serve）
//!   shellas-server publish ...        发布一个版本
//!
//! 配置全部来自环境变量，见 README.md。

mod admin;
mod ai;
mod api;
mod auth;
mod config;
mod db;
mod error;
mod publish;
mod release;
mod signing;
mod site;
mod util;

use std::net::SocketAddr;
use std::time::Instant;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::{get, post, put};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::config::Config;
use crate::db::Db;

#[derive(Parser)]
#[command(
    name = "shellas-server",
    version,
    about = "Agree ShellAs 的官网、更新与云同步服务"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
// 命令行只解析一次，参数大一点不值得为它加一层间接。
#[allow(clippy::large_enum_variant)]
enum Command {
    /// 启动官网、更新与同步服务
    Serve,
    /// 生成清单、本地签名并上传一个版本
    Publish(publish::PublishArgs),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command.unwrap_or(Command::Serve) {
        Command::Serve => serve(),
        Command::Publish(args) => publish::run(args),
    }
}

fn serve() -> anyhow::Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("shellas_server=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    let config = Config::from_env()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve_async(config))
}

async fn serve_async(config: Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(config.sync_dir())?;
    std::fs::create_dir_all(config.releases_dir())?;
    std::fs::create_dir_all(config.manifests_dir())?;
    let bind = config.bind;
    let data_dir = config.data_dir.clone();
    let db = Db::open(&config.database_path())?;
    bootstrap_admin(&config, &db)?;
    seed_ai(&config, &db)?;
    let state = AppState::new(config, db);
    if state.config.admin_token.is_none() {
        tracing::warn!("没有设置 SHELLAS_ADMIN_TOKEN，管理接口已关闭");
    }
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(
        "监听 http://{bind}，数据目录 {}，对外地址 {}",
        data_dir.display(),
        state.config.public_url
    );
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown())
    .await?;
    Ok(())
}

/// 首次启动时按环境变量建一个后台管理员；库里已有账号就不再动它，
/// 免得重启把管理员自己改过的口令覆盖回去。
fn bootstrap_admin(config: &Config, db: &Db) -> anyhow::Result<()> {
    let (Some(username), Some(password)) = (&config.admin_user, &config.admin_password) else {
        return Ok(());
    };
    if db.admin_count()? > 0 {
        return Ok(());
    }
    let hash = auth::hash_password(password)
        .map_err(|error| anyhow::anyhow!("后台口令散列失败：{error}"))?;
    db.create_admin(username, &hash, admin::ROLE_OWNER)?;
    tracing::info!(admin = %username, "已创建后台管理员账号");
    Ok(())
}

/// 库里还没有内置 AI 配置时，用环境变量播种地址、Key 与模型。
/// 总开关保持关闭，等管理员在后台确认无误后手动打开。
fn seed_ai(config: &Config, db: &Db) -> anyhow::Result<()> {
    if config.ai_base_url.is_none() && config.ai_api_key.is_none() && config.ai_model.is_none() {
        return Ok(());
    }
    let inserted = db.seed_ai_settings(
        config.ai_base_url.as_deref().unwrap_or_default(),
        config.ai_api_key.as_deref().unwrap_or_default(),
        config.ai_model.as_deref().unwrap_or_default(),
    )?;
    if inserted {
        tracing::info!("已用环境变量播种内置 AI 配置（总开关保持关闭）");
    }
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(site::home))
        .route("/features", get(site::features))
        .route("/download", get(site::download))
        .route("/changelog", get(site::changelog))
        .route("/healthz", get(api::healthz))
        .route("/assets/site.css", get(site::css))
        .route("/assets/logo.svg", get(site::logo))
        .route("/assets/app.png", get(site::app_image))
        .route("/favicon.svg", get(site::logo))
        .route("/favicon.ico", get(site::favicon_ico))
        .route("/favicon-16.png", get(site::favicon_16))
        .route("/favicon-32.png", get(site::favicon_32))
        .route("/favicon-180.png", get(site::favicon_180))
        .route("/favicon-192.png", get(site::favicon_192))
        .route("/favicon-512.png", get(site::favicon_512))
        .route("/site.webmanifest", get(site::manifest))
        .route("/robots.txt", get(site::robots))
        .route("/api/v1/auth/register", post(api::register))
        .route("/api/v1/auth/login", post(api::login))
        .route("/api/v1/auth/logout", post(api::logout))
        // 邮件找回接口位置先占住，现在返回 501。
        .route("/api/v1/auth/password/forgot", post(api::forgot_password))
        .route("/api/v1/auth/password/reset", post(api::reset_password))
        .route("/api/v1/me", get(api::me))
        // 账号自助：改口令会让所有会话失效；换邮箱先记待验证。
        .route("/api/v1/me/password", put(api::change_password))
        .route("/api/v1/me/email", put(api::change_email))
        // 内置 AI · 试用：用户只拿自己的登录态调这里，模型由服务端锁定。
        .route("/api/v1/ai/status", get(ai::status))
        .route(
            "/api/v1/ai/chat/completions",
            post(ai::chat).layer(DefaultBodyLimit::max(1024 * 1024 + 64 * 1024)),
        )
        .route(
            "/api/v0/events",
            post(api::analytics_events).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/v1/sync",
            get(api::list_sync_versions).put(api::upload_snapshot),
        )
        .route(
            "/api/v1/sync/{version}",
            get(api::download_snapshot).delete(api::delete_snapshot),
        )
        // 新的按文档增量同步：settings 与 hosts 各一条版本链。
        .route("/api/v1/sync/documents/{kind}", put(api::upload_document))
        .route(
            "/api/v1/sync/documents/{kind}/{version}",
            get(api::download_document).delete(api::delete_document),
        )
        .route("/update/v1/stable.json", get(api::stable_manifest))
        .route("/update/v1/beta.json", get(api::beta_manifest))
        .route(
            "/admin/v1/releases/{version}/{file}",
            put(api::admin_upload),
        )
        .route(
            "/admin/v1/manifest/{channel}",
            put(api::admin_manifest).layer(DefaultBodyLimit::max(1024 * 1024)),
        )
        // 后台前端（pure-admin-thin）用的 JSON 接口。
        .route("/admin/api/auth/login", post(admin::login))
        .route("/admin/api/auth/refresh", post(admin::refresh))
        .route("/admin/api/auth/logout", post(admin::logout))
        .route("/admin/api/routes", get(admin::routes))
        .route("/admin/api/me", get(admin::me))
        .route("/admin/api/releases", get(admin::releases))
        .route("/admin/api/releases/packages", get(admin::packages))
        .route(
            "/admin/api/releases/{version}/hashes",
            get(admin::release_hashes),
        )
        .route(
            "/admin/api/releases/{version}/{file}",
            put(api::admin_upload),
        )
        .route("/admin/api/release", post(admin::publish_release))
        .route(
            "/admin/api/releases/{version}/withdraw",
            post(admin::withdraw_release),
        )
        .route(
            "/admin/api/releases/{version}/republish",
            post(admin::republish_release),
        )
        .route(
            "/admin/api/changelog",
            get(admin::changelog).post(admin::create_changelog),
        )
        .route(
            "/admin/api/changelog/{id}",
            put(admin::update_changelog).delete(admin::delete_changelog),
        )
        .route("/admin/api/usage", get(admin::usage))
        .route(
            "/admin/api/ai/settings",
            get(ai::admin_settings_get).put(ai::admin_settings_put),
        )
        .route("/admin/api/ai/test", post(ai::admin_test))
        .route("/admin/api/ai/usage", get(ai::admin_usage))
        .route("/admin/api/ai/users/{id}", put(ai::admin_user_switch))
        .route(
            "/admin/api/admins",
            get(admin::admins).post(admin::create_admin),
        )
        .route(
            "/admin/api/admins/{id}",
            put(admin::update_admin).delete(admin::delete_admin),
        )
        .route("/admin/api/users", get(admin::users))
        .route(
            "/admin/api/users/{id}",
            put(admin::update_user).delete(admin::delete_user),
        )
        .route(
            "/admin/api/users/{id}/password",
            post(admin::reset_user_password),
        )
        .route("/admin/api/account/password", post(admin::change_password))
        // 后台静态页面：hash 路由，只有 /admin 和 /admin/ 需要回退到 index.html。
        .route("/admin", get(admin::spa_index))
        .route("/admin/", get(admin::spa_index))
        .route("/admin/{*path}", get(admin::spa))
        // 安装包下载走这里，顺便记下载次数；撤下的包不再提供。
        .route("/releases/{version}/{file}", get(api::download_release))
        .fallback(site::not_found)
        .layer(axum::middleware::from_fn(log_request))
        .with_state(state)
}

async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let started = Instant::now();
    let response = next.run(request).await;
    tracing::info!(
        %method,
        path,
        status = response.status().as_u16(),
        ms = started.elapsed().as_millis(),
        "request"
    );
    response
}

async fn shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("SIGTERM 监听失败");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
    tracing::info!("收到退出信号，正在关闭");
}
