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
const APP_PNG: &[u8] = include_bytes!("../web/app.png");
const FAVICON_ICO: &[u8] = include_bytes!("../web/favicon.ico");
const FAVICON_16: &[u8] = include_bytes!("../web/favicon-16.png");
const FAVICON_32: &[u8] = include_bytes!("../web/favicon-32.png");
const FAVICON_180: &[u8] = include_bytes!("../web/favicon-180.png");
const FAVICON_192: &[u8] = include_bytes!("../web/favicon-192.png");
const FAVICON_512: &[u8] = include_bytes!("../web/favicon-512.png");
const MANIFEST: &str = include_str!("../web/site.webmanifest");

/// 页面里唯一的脚本：背景光斑跟随指针，截图点开看大图。
/// 只做这两件事，不读页面内容，也不改文案。
const SITE_JS: &str = r#"
(() => {
  const reduce = window.matchMedia('(prefers-reduced-motion: reduce)');
  const glow = document.querySelector('.pointer-glow');
  if (glow && !reduce.matches) {
    let tx = window.innerWidth / 2;
    let ty = window.innerHeight * 0.32;
    let x = tx;
    let y = ty;
    let frame = 0;
    let idle = 0;
    const paint = () => {
      x += (tx - x) * 0.12;
      y += (ty - y) * 0.12;
      glow.style.transform =
        'translate3d(' + x.toFixed(1) + 'px,' + y.toFixed(1) + 'px,0) translate(-50%,-50%)';
      frame = Math.abs(tx - x) + Math.abs(ty - y) > 1 ? requestAnimationFrame(paint) : 0;
    };
    window.addEventListener('pointermove', (event) => {
      const moved = Math.hypot(event.clientX - tx, event.clientY - ty);
      tx = event.clientX;
      ty = event.clientY;
      glow.style.opacity = String(Math.min(0.6, 0.38 + moved / 500));
      clearTimeout(idle);
      idle = setTimeout(() => { glow.style.opacity = '0.38'; }, 240);
      if (!frame) frame = requestAnimationFrame(paint);
    }, { passive: true });
  }

  const shot = document.querySelector('.shot img');
  if (shot) {
    let box = null;
    const close = () => {
      if (!box) return;
      const closing = box;
      box = null;
      closing.classList.remove('open');
      document.body.classList.remove('lightbox-open');
      document.body.style.paddingRight = '';
      setTimeout(() => closing.remove(), 180);
    };
    const open = () => {
      if (box) return;
      box = document.createElement('div');
      box.className = 'lightbox';
      const image = document.createElement('img');
      image.src = shot.currentSrc || shot.src;
      image.alt = shot.alt;
      box.appendChild(image);
      box.addEventListener('click', (event) => {
        if (event.target === box || event.target === image) close();
      });
      document.body.appendChild(box);
      const gap = window.innerWidth - document.documentElement.clientWidth;
      if (gap > 0) document.body.style.paddingRight = gap + 'px';
      document.body.classList.add('lightbox-open');
      requestAnimationFrame(() => box.classList.add('open'));
    };
    shot.addEventListener('click', () => {
      if (box) close();
      else open();
    });
    window.addEventListener('keydown', (event) => {
      if (event.key === 'Escape') close();
    });
  }
})();
"#;

/// 页面里的 HTML 一律走这里：脚本只允许带本次响应 nonce 的那一段，
/// 外部资源一个都不加载。
fn html(body: String) -> Response {
    let nonce = util::random_token();
    let body = body.replace("__CSP_NONCE__", &nonce);
    let csp = format!(
        "default-src 'self'; style-src 'self'; img-src 'self' data:; \
         script-src 'nonce-{nonce}'; base-uri 'none'; form-action 'none'; \
         frame-ancestors 'none'"
    );
    let mut response = body.into_response();
    let head = response.headers_mut();
    head.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    head.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&csp)
            .unwrap_or_else(|_| HeaderValue::from_static("default-src 'none'")),
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
    let body = format!("{HOME_INTRO}{FEATURE_SECTIONS}{DOWNLOAD_CTA}");
    html(layout(
        "SSH 主机管理与文件传输",
        "ShellAs 是一个免费开源的 SSH 客户端，支持终端、主机管理和 SFTP 双栏文件传输，可选端到端加密的云同步。",
        "home",
        &body,
    ))
}

pub async fn features() -> Response {
    axum::response::Redirect::permanent("/#why").into_response()
}

pub async fn download(State(state): State<AppState>) -> Response {
    html(layout(
        "下载",
        "下载 ShellAs：Windows、macOS、Linux。",
        "download",
        &download_page(&state),
    ))
}

pub async fn changelog(State(state): State<AppState>) -> Response {
    html(layout(
        "更新日志",
        "ShellAs 的版本更新记录。",
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

pub async fn app_image() -> Response {
    let mut response = APP_PNG.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    response
}

/// 站点图标与 manifest：编译进二进制的静态字节，一天缓存。
fn site_asset(bytes: &'static [u8], content_type: &'static str) -> Response {
    let mut response = bytes.into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    response
}

pub async fn favicon_ico() -> Response {
    site_asset(FAVICON_ICO, "image/x-icon")
}

pub async fn favicon_16() -> Response {
    site_asset(FAVICON_16, "image/png")
}

pub async fn favicon_32() -> Response {
    site_asset(FAVICON_32, "image/png")
}

pub async fn favicon_180() -> Response {
    site_asset(FAVICON_180, "image/png")
}

pub async fn favicon_192() -> Response {
    site_asset(FAVICON_192, "image/png")
}

pub async fn favicon_512() -> Response {
    site_asset(FAVICON_512, "image/png")
}

pub async fn manifest() -> Response {
    site_asset(MANIFEST.as_bytes(), "application/manifest+json")
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
<title>{title} · ShellAs</title>
<meta name="description" content="{description}">
<link rel="icon" href="/favicon.ico" sizes="any">
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="icon" href="/favicon-32.png" type="image/png" sizes="32x32">
<link rel="icon" href="/favicon-16.png" type="image/png" sizes="16x16">
<link rel="apple-touch-icon" href="/favicon-180.png">
<link rel="manifest" href="/site.webmanifest">
<link rel="stylesheet" href="/assets/site.css?v=20261117183407">
</head>
<body>
<div class="pointer-glow" aria-hidden="true"></div>
<header class="top">
  <a class="brand" href="/"><img src="/assets/logo.svg" alt="" width="30" height="30">ShellAs</a>
  <nav>
    {}
    {}
    {}
    {}
  </nav>
</header>
<main>
{body}
</main>
<footer class="bottom">
  <div class="foot-main">
    <p class="foot-brand"><strong>ShellAs</strong> 免费开源，GPL-3.0。</p>
    <p class="credit">基于 <a href="https://shellrs.com/zh" rel="noopener">ShellRS</a></p>
  </div>
  <p class="copyright">© 2026 ShellAs</p>
</footer>
<script nonce="__CSP_NONCE__">{}</script>
</body>
</html>"#,
        nav("/", "home", "首页"),
        nav("/#why", "", "功能"),
        nav("/download", "download", "下载"),
        nav("/changelog", "changelog", "更新日志"),
        SITE_JS,
    )
}

const FEATURE_SECTIONS: &str = r##"
<section id="why" class="section">
  <p class="kicker">为什么选 ShellAs</p>
  <h2>Xshell 和 WinSCP，合进一个原生窗口</h2>
  <div class="grid">
    <article>
      <h3>原生，GPU 绘制</h3>
      <p>Rust 编写，界面基于 GPUI，终端也由 GPU 绘制。</p>
    </article>
    <article>
      <h3>一个窗口干完</h3>
      <p>终端、SFTP 和端口转发并排，标签可以拖成分栏。</p>
    </article>
    <article>
      <h3>秘密只进钥匙串</h3>
      <p>密码和私钥口令存系统钥匙串，数据库里没有。</p>
    </article>
  </div>
</section>

<section class="section">
  <p class="kicker">SFTP</p>
  <h2>像 WinSCP 一样的 SFTP</h2>
  <p class="lede">左边本地、右边远程，沿用熟悉的多选和拖放；上传断了能接着传。</p>
  <div class="grid two">
    <article>
      <h3>断点续传</h3>
      <p>断线或重启之后，从已经传完的位置继续。</p>
    </article>
    <article>
      <h3>删除进废纸篓</h3>
      <p>本地删除走系统废纸篓，不跟随符号链接。</p>
    </article>
  </div>
</section>

<section class="section">
  <p class="kicker">右侧栏</p>
  <h2>终端旁边，就是服务器的状态</h2>
  <p class="lede">系统监控、进程、systemd 服务、Docker、GPU 和网络连接跟着当前终端显示，读取都走这条终端的 SSH 连接。</p>
  <div class="grid">
    <article>
      <h3>系统监控</h3>
      <p>CPU、内存和网速定时刷新。</p>
    </article>
    <article>
      <h3>进程和服务</h3>
      <p>结束进程，启停、重启和设置 systemd 服务。</p>
    </article>
    <article>
      <h3>Docker 和 GPU</h3>
      <p>查看容器、镜像、卷和网络，也能看 GPU 占用。</p>
    </article>
  </div>
</section>

<section class="section">
  <p class="kicker">端口转发</p>
  <h2>看得懂的端口转发</h2>
  <p class="lede">本地、远程和动态转发，每条都有连接示意图，一句话说清从哪进、到哪去。</p>
  <div class="grid two">
    <article>
      <h3>-L / -R / -D</h3>
      <p>本地、远程和动态转发，动态转发支持 SOCKS5 和 SOCKS4。</p>
    </article>
    <article>
      <h3>断线自动重连</h3>
      <p>连接掉了自动重试，重试次数用完再报失败。</p>
    </article>
  </div>
</section>

<section class="section">
  <p class="kicker">分栏</p>
  <h2>终端和文件，并排查看</h2>
  <p class="lede">把标签拖到任意一边，左右或上下分栏：上面看部署输出，下面看文件传到哪。</p>
</section>

<section class="section">
  <p class="kicker">还有这些</p>
  <h2>为服务器太多的人准备</h2>
  <div class="grid">
    <article>
      <h3>跳板和代理</h3>
      <p>把已保存的主机串成多级跳板，也可以走 HTTP、SOCKS5 代理。</p>
    </article>
    <article>
      <h3>共用凭据</h3>
      <p>密码和密钥凭据多台主机共用一条，也能当场生成 Ed25519 密钥。</p>
    </article>
    <article>
      <h3>从堡垒机打开</h3>
      <p>JumpServer 等堡垒机的 ssh://、sftp:// 链接直接打开终端或 SFTP，连接不保存。</p>
    </article>
    <article>
      <h3>认得你的服务器</h3>
      <p>连上后识别远程系统并标出徽章；分组可以多层嵌套，也能搜索。</p>
    </article>
    <article>
      <h3>签名的在线更新</h3>
      <p>更新清单经 minisign 签名，安装包按 SHA-256 校验，后台下载，重启时安装。</p>
    </article>
    <article>
      <h3>独立的 known_hosts</h3>
      <p>信任新主机密钥前先问你，记录和 ~/.ssh 分开保存。</p>
    </article>
  </div>
</section>

<section class="section">
  <p class="kicker">AI 助手</p>
  <h2>多出来的 AI 助手</h2>
  <p class="lede">连接 OpenAI 或 Anthropic 兼容的模型，在绑定的终端里读日志、查进程、执行命令。遇到报错可以直接把输出交给它，改什么、为什么改都写在对话里。</p>
  <div class="grid two">
    <article>
      <h3>逐条确认</h3>
      <p>受限模式下每条命令批准后才执行；完全访问跳过确认，黑名单仍拦截。</p>
    </article>
    <article>
      <h3>只动绑定的终端</h3>
      <p>只能使用你绑定的终端；命令、输出和待审项记入对话和审计记录。</p>
    </article>
    <article>
      <h3>角色和定时任务</h3>
      <p>角色在对话里用 @ 载入；定时任务无人值守时，写操作进待审。</p>
    </article>
    <article>
      <h3>API Key 存钥匙串</h3>
      <p>模型的 API Key 存在系统钥匙串，不写进配置、数据库和日志。</p>
    </article>
  </div>
</section>

<section class="section">
  <div class="split">
    <div>
      <p class="kicker">给 AI Agent 用</p>
      <h2>让 Agent 用上你保存的主机</h2>
      <p class="lede">启用外部 CLI 后，Claude Code、Codex、OpenCode、WorkBuddy 可以通过 shellas 使用你保存的主机。</p>
      <p class="muted">ShellAs 代为登录，Agent 拿不到密码；陌生主机或缺密码时直接返回错误码，不弹框。</p>
    </div>
    <pre><code><span class="c"># 列出保存的主机</span>
shellas hosts list

<span class="c"># 执行命令，退出码就是远程命令的退出码</span>
shellas exec &lt;ID&gt; "uptime &amp;&amp; df -h"

<span class="c"># 传文件，不用交出密码</span>
shellas upload &lt;ID&gt; ./dist /opt/app</code></pre>
  </div>
</section>

<section class="section split">
  <div>
    <p class="kicker">同步</p>
    <h2>云同步的加密方式</h2>
    <p class="lede">同步密钥由登录口令经 Argon2id 派生，快照用 AES-256-GCM 加密后上传，服务器只存密文。</p>
    <p class="muted">忘记登录口令，云上的备份无法恢复。</p>
  </div>
  <div class="note-card">
    <h3>服务器密码默认不同步</h3>
    <p>勾选「同步服务器密码」后才随快照加密上传。</p>
    <p class="small">主机地址、端口、用户名、分组、凭据和代码片段默认同步。</p>
  </div>
</section>
"##;

const HOME_INTRO: &str = r##"
<section class="hero">
  <p class="eyebrow">&gt; 开源 · GPL-3.0</p>
  <h1>Agree ShellAs</h1>
  <p class="lede">用 Rust 编写、由 GPU 渲染的原生跨平台 SSH 客户端：Xshell 式主机管理、WinSCP 式 SFTP、端口转发，外加一个能执行命令的 AI 助手。</p>
  <div class="actions">
    <a class="button" href="/download">下载</a>
    <a class="button ghost" href="#why">功能说明</a>
  </div>
  <p class="version">v0.1.20 · Windows x64 · Linux x64</p>
</section>

<figure class="shot">
  <img src="/assets/app.png?v=20261117183407" width="1589" height="797"
       alt="ShellAs 主界面：左侧是主机列表，中间是终端，右侧是 AI 助手面板">
  <figcaption>ShellAs 主界面：主机列表、终端和 AI 助手</figcaption>
</figure>
"##;

const DOWNLOAD_CTA: &str = r##"
<section class="section cta">
  <p class="kicker">下载</p>
  <h2>免费开源，Windows 和 Linux 都能用</h2>
  <p class="lede">安装包不需要注册。</p>
  <a class="button" href="/download">下载安装包</a>
</section>
"##;

const MACOS_HELP: &str = r#"
<section class="section help">
  <h2>macOS：首次打开需要手动放行</h2>
  <p class="lede">
    如果 macOS 提示「无法验证开发者」或「已损坏」，按下面任一方式打开即可。
  </p>
  <div class="grid two">
    <article>
      <h3>方式一：右键打开</h3>
      <ol>
        <li>打开 DMG，把 ShellAs 拖进「应用程序」。</li>
        <li>在「应用程序」里按住 Control 点按图标，选「打开」。</li>
        <li>弹窗里再点一次「打开」，之后就能正常双击启动。</li>
      </ol>
    </article>
    <article>
      <h3>方式二：去掉隔离标记</h3>
      <p>如果提示「已损坏，无法打开」，打开「终端」执行：</p>
      <pre><code>xattr -dr com.apple.quarantine "/Applications/ShellAs.app"</code></pre>
      <p>然后在「系统设置 › 隐私与安全性」底部点「仍要打开」。</p>
    </article>
  </div>
</section>
"#;

const WINDOWS_HELP: &str = r#"
<section class="section help">
  <h2>Windows：SmartScreen 提示</h2>
  <p class="lede">
    如果 Windows 提示「已保护你的电脑」，点「更多信息」→「仍要运行」即可。
  </p>
</section>
"#;

const LINUX_HELP: &str = r#"
<section class="section help">
  <h2>Linux：AppImage</h2>
  <p class="lede">下载后赋予执行权限，然后直接运行。系统需要 Vulkan 或 OpenGL 图形驱动。</p>
  <pre><code>chmod +x ShellAs-*-linux-x86_64.AppImage
./ShellAs-*-linux-x86_64.AppImage</code></pre>
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
  <p class="lede">还没有发布版本。发布后这里会显示安装包。</p>
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
            "安装程序，按当前用户安装，不需要管理员权限",
            "windows-x86_64",
            installers.get("windows-x86_64").map(String::as_str),
        ),
        row(
            "Windows 便携版（x64）",
            "解压即用，配置保存在程序旁的 data 目录",
            "windows-x86_64-portable",
            installers
                .get("windows-x86_64-portable")
                .map(String::as_str),
        ),
        row(
            "macOS · Apple Silicon",
            "M1 及更新机型，DMG 安装包",
            "macos-aarch64",
            installers.get("macos-aarch64").map(String::as_str),
        ),
        row(
            "macOS · Intel",
            "Intel 机型，DMG 安装包",
            "macos-x86_64",
            installers.get("macos-x86_64").map(String::as_str),
        ),
        row(
            "Linux（x86_64）",
            "AppImage，chmod +x 后直接运行",
            "linux-x86_64",
            installers.get("linux-x86_64").map(String::as_str),
        ),
    ]
    .concat();

    format!(
        r#"<section class="narrow">
  <h1>下载 ShellAs</h1>
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
    let entries = match state.db.changelog_entries() {
        Ok(entries) => entries,
        Err(error) => {
            tracing::error!(error = %error, "读取更新日志失败");
            return "<section class=\"narrow\"><h1>更新日志</h1><p class=\"lede\">暂时读不到发布记录。</p></section>".to_string();
        }
    };
    let mut rendered = String::new();
    for entry in entries {
        let version = util::escape_html(&entry.version);
        let date = entry.published_at.get(..10).unwrap_or("");
        let mut badges = String::new();
        if entry.channel == "beta" {
            badges.push_str("<span class=\"badge\">测试版</span>");
        }
        if entry.severity == crate::release::SEVERITY_REQUIRED {
            badges.push_str("<span class=\"badge warn\">强制更新</span>");
        }
        let title = if entry.title.trim().is_empty() {
            String::new()
        } else {
            format!(
                "<p class=\"release-title\"><strong>{}</strong></p>",
                util::escape_html(&entry.title)
            )
        };
        let notes = if entry.body.trim().is_empty() {
            "<p class=\"muted\">这个版本没有写更新说明。</p>".to_string()
        } else {
            markdown(&entry.body)
        };
        rendered.push_str(&format!(
            r#"<article class="release" id="v{version}">
  <h2>{version} {badges}</h2>
  {title}
  <p class="muted small">{date}</p>
  {notes}
</article>"#
        ));
    }
    if rendered.is_empty() {
        return "<section class=\"narrow\"><h1>更新日志</h1><p class=\"lede\">还没有发布记录。</p></section>".to_string();
    }
    format!(
        r#"<section class="narrow">
  <h1>更新日志</h1>
  <p class="lede">按发布时间从新到旧排列。</p>
</section>
<section class="section narrow">{rendered}</section>"#
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
