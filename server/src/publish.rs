//! `publish`：给一个 `dist/` 目录里的安装包生成清单、本地签名并上传。
//!
//! 私钥只在这台发布机器上，服务器拿到的是已经签好的信封。清单格式与
//! 客户端 `src/update/manifest.rs` 一致，trusted comment 是
//! `shellas-manifest <channel> <version>`。

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::release::{self, FileInfo, SEVERITY_RECOMMENDED};
use crate::signing;

#[derive(Args)]
pub struct PublishArgs {
    /// 安装包所在目录
    #[arg(long, default_value = "dist")]
    pub dist: PathBuf,
    /// 版本号，与安装包文件名一致
    #[arg(long)]
    pub version: String,
    /// 通道：stable 或 beta
    #[arg(long, default_value = "stable")]
    pub channel: String,
    /// 更新分级：recommended（建议）或 required（强制）
    #[arg(long, default_value = SEVERITY_RECOMMENDED)]
    pub severity: String,
    /// 更新说明的 Markdown 文件
    #[arg(long)]
    pub notes: Option<PathBuf>,
    /// 对外地址，清单里的下载链接以它开头
    #[arg(long, default_value = "https://shellas.agree.host")]
    pub base_url: String,
    /// 管理接口地址，默认与对外地址相同
    #[arg(long)]
    pub server: Option<String>,
    /// 管理令牌
    #[arg(long, env = "SHELLAS_ADMIN_TOKEN")]
    pub admin_token: String,
    /// minisign 私钥文件
    #[arg(long)]
    pub minisign_key: PathBuf,
    /// 私钥口令，未设置时从标准输入读
    #[arg(long, env = "MINISIGN_PASSWORD")]
    pub minisign_password: Option<String>,
    /// 只生成清单和签名，不上传
    #[arg(long)]
    pub dry_run: bool,
}

pub fn run(args: PublishArgs) -> Result<()> {
    if !release::valid_channel(&args.channel) {
        bail!("通道只有 stable 和 beta");
    }
    if !release::valid_severity(&args.severity) {
        bail!("更新分级只有 recommended 和 required");
    }
    if !args.dist.is_dir() {
        bail!("找不到安装包目录 {}", args.dist.display());
    }
    let base = args.base_url.trim_end_matches('/').to_string();
    let server = args
        .server
        .as_deref()
        .unwrap_or(&args.base_url)
        .trim_end_matches('/')
        .to_string();

    let mut found = BTreeMap::new();
    let mut uploads = Vec::new();
    let packages = release::packages(&args.version);
    for package in &packages {
        let path = args.dist.join(&package.file);
        if !path.is_file() {
            println!("跳过 {}：dist 里没有", package.file);
            continue;
        }
        let size = path.metadata()?.len();
        let sha256 = sha256_of(&path)?;
        println!(
            "{}  {}  {}",
            package.file,
            crate::util::format_bytes(size),
            &sha256[..16]
        );
        found.insert(package.file.clone(), FileInfo { size, sha256 });
        uploads.push(path);
    }
    if !packages
        .iter()
        .any(|package| package.asset && found.contains_key(&package.file))
    {
        bail!("dist 里没有可发布的安装包");
    }

    let notes = match &args.notes {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("读不到更新说明 {}", path.display()))?,
        None => String::new(),
    };
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let notes_url = format!("{base}/changelog#v{}", args.version);
    let manifest = release::build_manifest(
        &base,
        &args.channel,
        &args.version,
        &args.severity,
        &notes,
        Some(&notes_url),
        None,
        1.0,
        &now,
        &found,
    );
    let manifest_text = serde_json::to_string_pretty(&manifest)?;
    let signature = sign(&args, &manifest_text)?;
    let envelope = serde_json::to_string_pretty(&json!({
        "manifest": manifest_text,
        "signature": signature,
    }))?;

    let envelope_path = args.dist.join(format!("{}.json", args.channel));
    std::fs::write(&envelope_path, &envelope)?;
    println!("写入 {}", envelope_path.display());

    if args.dry_run {
        println!("--dry-run：没有上传");
        return Ok(());
    }

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .build()?;
    for path in &uploads {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("package");
        println!("上传 {name}");
        let body = File::open(path)?;
        let response = client
            .put(format!(
                "{server}/admin/v1/releases/{}/{}",
                args.version, name
            ))
            .bearer_auth(&args.admin_token)
            .body(reqwest::blocking::Body::new(body))
            .send()?;
        check(response, &format!("上传 {name}"))?;
    }
    println!("发布 {} 通道清单", args.channel);
    let response = client
        .put(format!("{server}/admin/v1/manifest/{}", args.channel))
        .bearer_auth(&args.admin_token)
        .body(envelope)
        .send()?;
    check(response, "发布清单")?;
    println!("完成：{base}/update/v1/{}.json", args.channel);
    Ok(())
}

fn sha256_of(path: &Path) -> Result<String> {
    let mut file = BufReader::new(File::open(path)?);
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

fn sign(args: &PublishArgs, manifest: &str) -> Result<String> {
    match signing::sign(
        &args.minisign_key,
        args.minisign_password.as_deref(),
        &args.channel,
        &args.version,
        manifest,
    ) {
        Ok(signature) => Ok(signature),
        Err(_) if args.minisign_password.is_none() => {
            // 私钥带口令：交互式发布时问一次，CI 里用 MINISIGN_PASSWORD。
            print!("minisign 私钥口令：");
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let password = line.trim_end().to_string();
            signing::sign(
                &args.minisign_key,
                Some(&password),
                &args.channel,
                &args.version,
                manifest,
            )
            .context("私钥口令不对")
        }
        Err(error) => Err(error),
    }
}

fn check(response: reqwest::blocking::Response, what: &str) -> Result<()> {
    if response.status().is_success() {
        return Ok(());
    }
    let status = response.status();
    let body = response.text().unwrap_or_default();
    bail!("{what}失败：HTTP {status} {body}");
}
