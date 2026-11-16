//! 官网：首页、下载页、更新日志。页面与样式编译进二进制。

use std::collections::BTreeMap;

use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::api::AppState;
use crate::util;

const CSS: &str = include_str!("../web/site.css");
const LOGO: &str = include_str!("../web/logo.svg");

/// 页面里的 HTML 一律走这里，带上 CSP：没有脚本，外部资源一个都不加载。
fn html(body: String) -> Response {
    let mut response = body.into_response();
    let head = response.headers_mut();
    head.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    head.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; style-src 'self'; img-src 'self' data:; \
             script-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ),
    );
    head.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    head.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

pub async fn home() -> Response {
    html(layout(
        "SSH 主机管理与文件传输",
        "Agree ShellAS 是一个免费开源的 SSH 客户端：真终端、主机管理、SFTP 双栏，可选端到端加密的云同步。",
        "",
        HOME,
    ))
}

pub async fn download(State(state): State<AppState>) -> Response {
    html(layout(
        "下载",
        "下载 Agree ShellAS：Windows、macOS、Linux。",
        "download",
        &download_page(&state),
    ))
}

pub async fn changelog(State(state): State<AppState>) -> Response {
    html(layout(
        "更新日志",
        "Agree ShellAS 每个版本改了什么。",
        "changelog",
        &changelog_page(&state),
    ))
}

pub async fn not_found() -> Response {
    let body = layout(
        "页面不存在",
        "没有找到这个页面。",
        "",
        r#"<section class="narrow">
  <h1>页面不存在</h1>
  <p class="lede">地址可能写错了，或者这个页面已经搬走。</p>
  <p><a class="button" href="/">回到首页</a></p>
</section>"#,
    );
    (StatusCode::NOT_FOUND, html(body)).into_response()
}

pub async fn css() -> Response {
    let mut response = CSS.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/css; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    response
}

pub async fn logo() -> Response {
    let mut response = LOGO.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    response
}

pub async fn robots() -> Response {
    let mut response =
        "User-agent: *\nDisallow: /api/\nDisallow: /admin/\nDisallow: /releases/\n".into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn layout(title: &str, description: &str, active: &str, body: &str) -> String {
    let nav = |href: &str, key: &str, text: &str| {
        let class = if active == key { " class=\"on\"" } else { "" };
        format!("<a href=\"{href}\"{class}>{text}</a>")
    };
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · Agree ShellAS</title>
<meta name="description" content="{description}">
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="stylesheet" href="/assets/site.css">
</head>
<body>
<header class="top">
  <a class="brand" href="/"><img src="/assets/logo.svg" alt="" width="30" height="30">Agree ShellAS</a>
  <nav>
    {}
    {}
    {}
    <a href="https://github.com/yunerduo/shell-rs" rel="noopener">GitHub</a>
  </nav>
</header>
<main>
{body}
</main>
<footer class="bottom">
  <div>
    <strong>Agree ShellAS</strong>
    <span>免费开源，基于 ShellRS，GPL-3.0。</span>
  </div>
  <div class="foot-links">
    <a href="/download">下载</a>
    <a href="/changelog">更新日志</a>
    <a href="https://github.com/yunerduo/shell-rs" rel="noopener">源代码</a>
  </div>
</footer>
</body>
</html>"#,
        nav("/#features", "features", "功能"),
        nav("/download", "download", "下载"),
        nav("/changelog", "changelog", "更新日志"),
    )
}

const HOME: &str = r##"
<section class="hero">
  <div class="hero-copy">
    <p class="eyebrow">SSH 客户端 · 主机管理 · SFTP</p>
    <h1>把服务器<br>装进一个窗口</h1>
    <p class="lede">
      Agree ShellAS 是一个免费开源的 SSH 工具。连接、文件、密钥放在一处，
      本地优先，云同步可选且端到端加密。
    </p>
    <div class="actions">
      <a class="button" href="/download">下载</a>
      <a class="button ghost" href="#features">看看能做什么</a>
    </div>
    <p class="small">Windows · macOS · Linux，x86_64。ARM 版本稍后提供。</p>
  </div>
  <div class="hero-art" aria-hidden="true">
    <div class="window">
      <div class="window-bar">
        <span class="dot red"></span><span class="dot yellow"></span><span class="dot green"></span>
        <span class="window-title">web-01 · 终端</span>
      </div>
      <div class="window-body">
        <div class="terminal">
          <p><span class="user">deploy@web-01</span><span class="muted">:~$</span> ssh prod-db</p>
          <p class="muted">Connected · Ubuntu 24.04.1 LTS · 12 ms</p>
          <p><span class="user">deploy@prod-db</span><span class="muted">:~$</span> systemctl status nginx</p>
          <p class="ok">● nginx.service - A high performance web server</p>
          <p class="ok">     Active: active (running)</p>
          <p><span class="user">deploy@prod-db</span><span class="muted">:~$</span> <span class="caret"></span></p>
        </div>
        <aside class="sftp">
          <div class="sftp-head">远程 · /var/www</div>
          <div class="sftp-row folder">app/</div>
          <div class="sftp-row folder">static/</div>
          <div class="sftp-row">deploy.sh</div>
          <div class="sftp-row">nginx.conf</div>
          <div class="sftp-tag">上传中 · 62%</div>
        </aside>
      </div>
    </div>
  </div>
</section>

<section id="features" class="section">
  <h2>一件事做完整</h2>
  <p class="lede">不是把十种协议都塞进来，而是把 SSH 和文件传输做顺。</p>
  <div class="grid">
    <article>
      <h3>真终端</h3>
      <p>完整的终端模拟，多标签、选区、缩放和搜索。本地终端和远程会话是同一套体验。</p>
    </article>
    <article>
      <h3>主机管理</h3>
      <p>分组可以嵌套，主机可以不属于任何分组。搜索、最近连接、系统标记，找一台机器不用翻笔记。</p>
    </article>
    <article>
      <h3>SFTP 双栏</h3>
      <p>左边本地、右边远程，像 WinSCP 一样对着看。上传支持断点续传和冲突确认。</p>
    </article>
    <article>
      <h3>秘密进钥匙串</h3>
      <p>密码和私钥口令保存在系统钥匙串，数据库里永远不出现秘密。</p>
    </article>
    <article>
      <h3>本地优先</h3>
      <p>主机和分组存在本机 SQLite 里，离线也能用。云同步是可选项，不是前提。</p>
    </article>
    <article>
      <h3>可选云同步</h3>
      <p>换一台电脑，登录后自动拉回主机、分组和 AI 配置。加密在客户端完成。</p>
    </article>
  </div>
</section>

<section class="section split">
  <div>
    <h2>同步是加密的，服务器解不开</h2>
    <p class="lede">
      登录口令在你的电脑上用 Argon2id 派生出同步密钥，快照用 AES-256-GCM
      加密后才上传。服务器只保存密文和摘要，所以即使服务器被拿走，
      里面的内容也读不出来。
    </p>
    <p class="muted">
      代价是：忘记登录口令，云上的备份也就解不开了。请把口令记在自己的密码管理器里。
    </p>
  </div>
  <div class="note-card">
    <h3>服务器密码默认不同步</h3>
    <p>
      同步设置里有一项「同步服务器密码」，默认不勾选。只有你手动勾上，
      密码才会随快照一起加密上传。
    </p>
    <p class="small">主机地址、端口、用户名、分组和 AI 配置默认同步。</p>
  </div>
</section>

<section class="section cta">
  <h2>现在就可以用</h2>
  <p class="lede">下载安装包，不需要注册。要跨设备同步时再注册账号。</p>
  <a class="button" href="/download">前往下载</a>
</section>
"##;

const MACOS_HELP: &str = r#"
<section class="section help">
  <h2>macOS：首次打开需要手动放行</h2>
  <p class="lede">
    这是一个公益项目，没有购买 Apple 开发者证书，所以安装包没有签名和公证。
    系统会提示「无法验证开发者」或「已损坏」，按下面的任一方式打开即可。
  </p>
  <div class="grid two">
    <article>
      <h3>方式一：右键打开</h3>
      <ol>
        <li>打开 DMG，把 Agree ShellAS 拖进「应用程序」。</li>
        <li>在「应用程序」里按住 Control 点按图标，选「打开」。</li>
        <li>弹窗里再点一次「打开」，之后就能正常双击启动。</li>
      </ol>
    </article>
    <article>
      <h3>方式二：去掉隔离标记</h3>
      <p>如果提示「已损坏，无法打开」，打开「终端」执行：</p>
      <pre><code>xattr -dr com.apple.quarantine "/Applications/ShellRS.app"</code></pre>
      <p>然后再打开应用。也可以到「系统设置 › 隐私与安全性」底部点「仍要打开」。</p>
    </article>
  </div>
</section>
"#;

const WINDOWS_HELP: &str = r#"
<section class="section help">
  <h2>Windows：SmartScreen 提示</h2>
  <p class="lede">
    安装包没有购买代码签名证书。首次运行时 Windows 可能提示「已保护你的电脑」，
    点「更多信息」→「仍要运行」即可。
  </p>
</section>
"#;

const LINUX_HELP: &str = r#"
<section class="section help">
  <h2>Linux：AppImage</h2>
  <p class="lede">下载后给它执行权限，然后直接运行。系统里需要 Vulkan 或 OpenGL 的图形驱动。</p>
  <pre><code>chmod +x ShellRS-*-linux-x86_64.AppImage
./ShellRS-*-linux-x86_64.AppImage</code></pre>
  <p class="small">没有 FUSE 的系统可以加 <code>--appimage-extract-and-run</code>。</p>
</section>
"#;

#[derive(Deserialize)]
struct Envelope {
    manifest: String,
}

#[derive(Deserialize, Default)]
struct ManifestInfo {
    #[serde(default)]
    channel: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    notes: String,
    #[serde(default)]
    assets: BTreeMap<String, AssetInfo>,
    #[serde(default)]
    installers: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct AssetInfo {
    #[serde(default)]
    size: u64,
}

fn manifest_of(envelope: &str) -> Option<ManifestInfo> {
    let envelope: Envelope = serde_json::from_str(envelope).ok()?;
    serde_json::from_str(&envelope.manifest).ok()
}

fn latest_stable(state: &AppState) -> Option<ManifestInfo> {
    // 读库失败等同于「还没有发布」，页面照常显示。
    let envelope = state.db.current_manifest("stable").ok().flatten()?;
    let manifest = manifest_of(&envelope)?;
    (manifest.channel == "stable").then_some(manifest)
}

fn download_page(state: &AppState) -> String {
    let Some(release) = latest_stable(state) else {
        return format!(
            r#"<section class="narrow">
  <h1>下载</h1>
  <p class="lede">第一个版本还在准备中。发布后这里会出现各平台的安装包。</p>
</section>
{MACOS_HELP}
{WINDOWS_HELP}
{LINUX_HELP}"#
        );
    };
    let version = util::escape_html(&release.version);
    let published = release
        .published_at
        .as_deref()
        .and_then(|at| at.get(..10))
        .unwrap_or("");
    let version_line = if published.is_empty() {
        format!("当前版本 {version}")
    } else {
        format!("当前版本 {version} · {published}")
    };

    let row = |title: &str, note: &str, key: &str, installer: Option<&str>| {
        let Some(installer) = installer else {
            return format!(
                r#"<div class="download-row"><div><strong>{title}</strong><span>{note}</span></div><span class="muted">此版本未提供</span></div>"#
            );
        };
        let Some(file) = installer.rsplit('/').next() else {
            return String::new();
        };
        let size = release
            .assets
            .get(key)
            .map(|asset| util::format_bytes(asset.size))
            .unwrap_or_default();
        let href = format!("/releases/{}/{file}", util::escape_html(&release.version));
        format!(
            r#"<div class="download-row"><div><strong>{title}</strong><span>{note}</span></div><span class="size">{size}</span><a class="button small-button" href="{href}">下载</a></div>"#
        )
    };

    let installers = &release.installers;
    let rows = [
        row(
            "Windows 10 / 11（x64）",
            "安装程序，当前用户安装，不需要管理员权限",
            "windows-x86_64",
            installers.get("windows-x86_64").map(String::as_str),
        ),
        row(
            "Windows 便携版（x64）",
            "解压即用，配置放在同目录的 data 文件夹",
            "windows-x86_64-portable",
            installers
                .get("windows-x86_64-portable")
                .map(String::as_str),
        ),
        row(
            "macOS · Apple Silicon",
            "适用于 M1 / M2 / M3 / M4，DMG 安装包",
            "macos-aarch64",
            installers.get("macos-aarch64").map(String::as_str),
        ),
        row(
            "macOS · Intel",
            "适用于 x86_64 的 Mac，DMG 安装包",
            "macos-x86_64",
            installers.get("macos-x86_64").map(String::as_str),
        ),
        row(
            "Linux（x86_64）",
            "AppImage，下载后 chmod +x 直接运行",
            "linux-x86_64",
            installers.get("linux-x86_64").map(String::as_str),
        ),
    ]
    .concat();

    format!(
        r#"<section class="narrow">
  <p class="eyebrow">下载</p>
  <h1>Agree ShellAS</h1>
  <p class="lede">{version_line}</p>
  <div class="download-list">{rows}</div>
  <p class="small">ARM 版本（Linux aarch64、Windows on ARM）暂未提供。</p>
</section>
{MACOS_HELP}
{WINDOWS_HELP}
{LINUX_HELP}"#
    )
}

fn changelog_page(state: &AppState) -> String {
    let history = match state.db.manifest_history() {
        Ok(history) => history,
        Err(error) => {
            tracing::error!(error = %error, "读取更新日志失败");
            return "<section class=\"narrow\"><h1>更新日志</h1><p class=\"lede\">暂时读不到发布记录。</p></section>".to_string();
        }
    };
    let mut seen = std::collections::HashSet::new();
    let mut entries = String::new();
    for (channel, envelope) in history {
        let Some(manifest) = manifest_of(&envelope) else {
            continue;
        };
        if manifest.version.is_empty() || !seen.insert(manifest.version.clone()) {
            continue;
        }
        let version = util::escape_html(&manifest.version);
        let date = manifest
            .published_at
            .as_deref()
            .and_then(|at| at.get(..10))
            .unwrap_or("");
        let badge = if channel == "beta" {
            "<span class=\"badge\">测试版</span>"
        } else {
            ""
        };
        let notes = if manifest.notes.trim().is_empty() {
            "<p class=\"muted\">这个版本没有写更新说明。</p>".to_string()
        } else {
            markdown(&manifest.notes)
        };
        entries.push_str(&format!(
            r#"<article class="release" id="v{version}">
  <h2>{version} {badge}</h2>
  <p class="muted small">{date}</p>
  {notes}
</article>"#
        ));
    }
    if entries.is_empty() {
        return "<section class=\"narrow\"><h1>更新日志</h1><p class=\"lede\">还没有发布记录。</p></section>".to_string();
    }
    format!(
        r#"<section class="narrow">
  <p class="eyebrow">更新日志</p>
  <h1>每个版本改了什么</h1>
  <p class="lede">清单里的更新说明，按发布时间排列。</p>
</section>
<section class="section narrow">{entries}</section>"#
    )
}

/// Markdown 转 HTML。原始 HTML 丢掉：更新说明由我们自己写，但没必要
/// 让一段粘贴进来的标签改变页面结构。
fn markdown(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, html};

    let parser = Parser::new_ext(text, Options::ENABLE_TABLES).filter_map(|event| match event {
        Event::Html(_) | Event::InlineHtml(_) => None,
        other => Some(other),
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_renders_and_raw_html_is_dropped() {
        let out = markdown("### 新增\n\n- 同步\n\n<script>alert(1)</script>");
        assert!(out.contains("<h3>新增</h3>"));
        assert!(out.contains("<li>同步</li>"));
        assert!(!out.contains("<script>"));
    }

    #[test]
    fn a_manifest_is_read_from_its_envelope() {
        let envelope =
            r#"{"manifest":"{\"channel\":\"stable\",\"version\":\"0.1.19\"}","signature":"x"}"#;
        let manifest = manifest_of(envelope).unwrap();
        assert_eq!(manifest.version, "0.1.19");
        assert_eq!(manifest.channel, "stable");
    }
}
