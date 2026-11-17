# shellas-server

Agree ShellAs 的服务端：官网、自建更新、注册登录与端到端加密的云同步。
一个 Rust 二进制，SQLite 存元数据，快照和安装包都是磁盘文件。

设计与接口见 [docs/shellas-service.zh.md](../docs/shellas-service.zh.md)。

## 配置

全部来自环境变量：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `SHELLAS_BIND` | `127.0.0.1:8080` | 监听地址 |
| `SHELLAS_DATA_DIR` | `data` | 数据库、快照、安装包所在目录 |
| `SHELLAS_PUBLIC_URL` | `https://shellas.agree.host` | 清单里的下载地址前缀 |
| `SHELLAS_ADMIN_TOKEN` | 空 | 管理接口令牌；不设则命令行管理接口关闭 |
| `SHELLAS_ADMIN_USER` | 空 | 首次启动创建的后台 owner 账号；与口令同时设置 |
| `SHELLAS_ADMIN_PASSWORD` | 空 | 后台 owner 口令，至少 8 个字符 |
| `SHELLAS_ADMIN_SESSION_DAYS` | `7` | 后台登录有效期（天） |
| `SHELLAS_ADMIN_UI_DIR` | `admin-ui/dist` | 后台前端构建产物目录 |
| `SHELLAS_MINISIGN_KEY` | 空 | 服务端签名私钥路径；不设则后台发布被拒绝，命令行 publish 仍可用 |
| `SHELLAS_MINISIGN_KEY_PASSWORD` | 空 | 签名私钥口令 |
| `SHELLAS_DEFAULT_QUOTA_MB` | `50` | 新用户的同步配额 |
| `SHELLAS_MAX_SNAPSHOTS` | `20` | 每人保留的快照份数 |
| `SHELLAS_SESSION_DAYS` | `30` | 登录有效期 |
| `SHELLAS_MAX_SNAPSHOT_MB` | `100` | 单次同步上传上限 |
| `SHELLAS_MAX_PACKAGE_MB` | `400` | 单个安装包上限 |
| `SHELLAS_LOGIN_MAX_FAILURES` | `5` | 连续登录失败多少次后锁定；用户与管理员各自计数 |
| `SHELLAS_LOGIN_LOCK_MINUTES` | `15` | 登录锁定时长（分钟） |

## 本机试跑

```bash
cd server
cargo run                      # 默认监听 127.0.0.1:8080
curl http://127.0.0.1:8080/healthz
```

注册一个账号：

```bash
curl -s http://127.0.0.1:8080/api/v1/auth/register \
  -H 'Content-Type: application/json' \
  -d '{"username":"alice","password":"a-long-password"}'
```

## 后台管理端

后台界面是 `server/admin-ui/`（pure-admin-thin，Vue 3 + Element Plus），
构建产物由同一个二进制托管在 `/admin`。后台账号与客户端账号完全分开。

- 角色：`owner`（全部）、`publisher`（版本发布、更新记录）、`viewer`（只读）。
- 登录：`/admin`，账号来自 `SHELLAS_ADMIN_USER` / `SHELLAS_ADMIN_PASSWORD`；
  首次启动建库时创建，之后改口令在后台「系统 → 管理员账号」里操作。
- 版本发布：上传安装包、填版本号与更新说明、选建议更新/强制更新；
  清单字段是 `severity`（`recommended` / `required`），发布后立即生效。
- 更新记录：后台增删改，直接反映到官网 `/changelog`。
- 使用记录：`/api/v0/events` 的看板，口径是「设备·天」。

后台发布需要服务器上有签名私钥（`SHELLAS_MINISIGN_KEY`）。不想把私钥
放到服务器时，不设这个变量，继续用下面的命令行 `publish` 本地签名上传。

本机开发：

```bash
# 后端
cd server && cargo run
# 前端，另开一个终端；Vite 把 /admin/api 代理到 127.0.0.1:8080
cd server/admin-ui && pnpm install && pnpm dev
```

构建后台静态文件：

```bash
cd server/admin-ui && pnpm build   # 产物在 admin-ui/dist
```

`Dockerfile` 已经包含 Node 构建阶段，`docker compose up -d --build` 会一起构建。

## 部署（1Panel + Docker）

1. **DNS**：`shellas.agree.host` 的 A 记录指向服务器。
2. **令牌**：生成一个管理员令牌，写进 `server/.env`：

   ```bash
   openssl rand -base64 36
   # .env
   SHELLAS_ADMIN_TOKEN=上面那串
   ```

3. **启动**：

   ```bash
    cd server
    docker compose up -d --build
    curl http://127.0.0.1:8080/healthz
   ```

4. **反向代理**：1Panel 建网站 `shellas.agree.host`，代理到
   `http://127.0.0.1:8080`，申请 Let's Encrypt 证书并开启 HTTPS。
   客户端与网页都走 HTTPS，不要只开 HTTP。
5. **开机自启**：compose 已经 `restart: unless-stopped`。

数据都在 `server/data/`，备份时连同 `shellas.db` 一起打包即可。
数据库开了 WAL，热备份请先 `docker compose stop`，或者用
`sqlite3 data/shellas.db ".backup data/backup.db"`。

### 源码替换与重建的硬性顺序

不要只替换单个 `.rs` 文件。`site.rs` 依赖 `release.rs`，`admin.rs`、
`signing.rs` 和 `main.rs` 的模块声明也必须一起在，单文件替换会让镜像
构建失败，容器起不来、站点直接 502。

正确顺序：

1. 先确认当前站点健康：`curl -fsS http://127.0.0.1:8080/healthz`。
2. 把整套 `server/` 源码（或至少 `src/` 全部、`web/`、`admin-ui/`、
   `Cargo.toml`、`Cargo.lock`、`Dockerfile`）一起上传，不要挑文件。
3. 在服务器上先编译再重建：`cd server && docker compose build`。
   构建失败就停在这里，旧容器继续跑，不要 `up -d`。
4. 构建成功后再 `docker compose up -d`，随后复核 `/healthz`、
   `/admin`、`/update/v1/stable.json` 和一份安装包下载。
5. 出问题回滚到上一版镜像并确认站点恢复，再排查。

`site.rs` 曾因为单独替换、引用了服务器上不存在的 `crate::release`
导致容器崩溃；以后一律按上面的整套源码流程走。

## 更新签名密钥

清单由发布机器在本地签名，私钥默认不进服务器。第一次发布前生成一对：
```bash
minisign -G -p shellas-update.pub -s shellas-update.key
```

- `shellas-update.key`（私钥）只放在发布机器或 CI 的 secret 里，离线备份；
- `shellas-update.pub` 的第二行公钥要写进客户端
  `src/update/build_info.rs` 的 `TRUSTED_KEYS`，否则客户端会拒绝新清单；
- 换钥匙时，先发一版带新公钥的客户端，再用新私钥签名。

两种发布方式：

- **命令行（推荐，私钥不离开发布机器）**：`shellas-server publish` 在本地
  算摘要、组装清单、签名，再上传安装包和信封。
- **后台网页发布**：需要把私钥放到服务器数据卷（例如
  `server/data/update.key`），并设置 `SHELLAS_MINISIGN_KEY`。后台发布时
  服务器自己签名，管理员不需要接触私钥，但私钥落在服务器上，安全边界
  要自行评估。没有私钥时后台的「上传并发布」会被拒绝，命令行方式不受影响。

## 发布一个版本

安装包命名与仓库现有 `packaging/` 一致，放在一个目录里：

```text
ShellAs-0.1.19-windows-x86_64-setup.exe
ShellAs-0.1.19-windows-x86_64-portable.zip
ShellAs-0.1.19-macos-aarch64.dmg
ShellAs-0.1.19-macos-aarch64.app.zip
ShellAs-0.1.19-macos-x86_64.dmg
ShellAs-0.1.19-macos-x86_64.app.zip
ShellAs-0.1.19-linux-x86_64.AppImage
```

缺哪个就跳过哪个，清单里只列存在的平台。然后：

```bash
SHELLAS_ADMIN_TOKEN=... \
MINISIGN_PASSWORD=... \
shellas-server publish \
  --dist dist \
  --version 0.1.19 \
  --channel stable \
  --severity recommended \
  --notes release-notes.md \
  --minisign-key shellas-update.key
```

`--severity` 只有 `recommended`（建议更新，默认）和 `required`（强制更新）
两个值；清单顶层字段是 `severity`。

`publish` 会算大小与 SHA-256、生成清单、在本地签名，再依次上传安装包和
签名信封。加 `--dry-run` 只生成 `dist/stable.json`，不上传。

### 各平台怎么出包

- **Windows**：本机 `cargo build --release --bins`，用 Inno Setup 6.5+
  打安装程序（`packaging/windows/shellas.iss`），再用 7-Zip 打便携版 zip。
- **Linux**：Docker 里用 `packaging/linux/appimage.sh`，见
  `.github/workflows/release.yml` 的 `linux` 任务。
- **macOS**：只能在 macOS 上构建。用 GitHub Actions 的 `macos-15`
  runner 出两种架构的 `.app.zip` 和 `.dmg`。因为没有 Apple 开发者证书，
  构建时**跳过签名和公证**；下载页已经写明右键打开和 `xattr -dr` 的办法。

## 接口与加密约定

- 账号：注册、登录、退出、`/api/v1/me`；找回口令接口已占位（501）；
- 同步：`GET/PUT /api/v1/sync`、`GET/DELETE /api/v1/sync/{version}`；
  分文档同步 `PUT /api/v1/sync/documents/{kind}`（`settings` / `hosts`，
  带 `X-Base-Version`，冲突返回 409 和最新元数据）；
- 更新：`/update/v1/stable.json`、`/update/v1/beta.json`、`/releases/...`；
- 匿名统计：`POST /api/v0/events`（Aptabase 协议，按 IP 限流，不存原始地址）；
- 管理：`PUT /admin/v1/releases/{version}/{file}`、`PUT /admin/v1/manifest/{channel}`；
- 后台：`/admin`（页面）、`/admin/api/*`（登录、版本发布、升级记录撤下/补发、
  更新记录、使用记录、用户账号、管理员）。

用户与管理员完全隔离：`users` / `sessions` 与 `admin_users` / `admin_sessions`
是两套表，登录路由、cookie（`shellas_user_session` / `shellas_admin_session`）、
限流器和失败锁定（`login_lockouts.scope`）都不共用。客户端入口被爆破只会锁住
那个用户名，不会牵连后台账号。

同步内容由客户端加密，服务器只看得到密文：

1. 客户端用登录口令 + `sync_salt` 做 Argon2id（64 MiB、3 轮）得到 32 字节密钥；
2. AES-256-GCM 加密快照，随机 12 字节 nonce，附加数据 `shellas-sync-v1`；
3. 上传 `nonce || ciphertext`，`X-Snapshot-Sha256` 是整段内容的 SHA-256。

「同步服务器密码」是客户端设置里的勾选项，默认关闭；服务器无法分辨密文
里有没有密码。
