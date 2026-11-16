//! Agree ShellAS 服务端：官网、自建更新、端到端加密的云同步。
//!
//!   shellas-server                    启动服务（同 serve）
//!   shellas-server publish ...        发布一个版本
//!
//! 配置全部来自环境变量，见 README.md。

mod api;
mod auth;
mod config;
mod db;
mod error;
mod publish;
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
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

use crate::api::AppState;
use crate::config::Config;
use crate::db::Db;

#[derive(Parser)]
#[command(
    name = "shellas-server",
    version,
    about = "Agree ShellAS 的官网、更新与云同步服务"
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

fn router(state: AppState) -> Router {
    let releases = ServeDir::new(state.config.releases_dir());
    Router::new()
        .route("/", get(site::home))
        .route("/download", get(site::download))
        .route("/changelog", get(site::changelog))
        .route("/healthz", get(api::healthz))
        .route("/assets/site.css", get(site::css))
        .route("/assets/logo.svg", get(site::logo))
        .route("/favicon.svg", get(site::logo))
        .route("/robots.txt", get(site::robots))
        .route("/api/v1/auth/register", post(api::register))
        .route("/api/v1/auth/login", post(api::login))
        .route("/api/v1/auth/logout", post(api::logout))
        .route("/api/v1/me", get(api::me))
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
        .nest_service("/releases", releases)
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
