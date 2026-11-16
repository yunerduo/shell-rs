# shellas-server

Agree ShellAS 的服务端：官网、自建更新、注册登录与端到端加密的云同步。
一个 Rust 二进制，SQLite 存元数据，快照和安装包都是磁盘文件。

设计与接口见 [docs/shellas-service.zh.md](../docs/shellas-service.zh.md)。

## 配置

全部来自环境变量：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `SHELLAS_BIND` | `127.0.0.1:8080` | 监听地址 |
| `SHELLAS_DATA_DIR` | `data` | 数据库、快照、安装包所在目录 |
| `SHELLAS_PUBLIC_URL` | `https://shellas.agree.host` | 清单里的下载地址前缀 |
| `SHELLAS_ADMIN_TOKEN` | 空 | 管理接口令牌；不设则管理接口关闭 |
| `SHELLAS_DEFAULT_QUOTA_MB` | `50` | 新用户的同步配额 |
| `SHELLAS_MAX_SNAPSHOTS` | `20` | 每人保留的快照份数 |
| `SHELLAS_SESSION_DAYS` | `30` | 登录有效期 |
| `SHELLAS_MAX_SNAPSHOT_MB` | `100` | 单次同步上传上限 |
| `SHELLAS_MAX_PACKAGE_MB` | `400` | 单个安装包上限 |

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

## 更新签名密钥

清单由发布机器在本地签名，私钥不进服务器。第一次发布前生成一对：

```bash
minisign -G -p shellas-update.pub -s shellas-update.key
```

- `shellas-update.key`（私钥）只放在发布机器或 CI 的 secret 里，离线备份；
- `shellas-update.pub` 的第二行公钥要写进客户端
  `src/update/build_info.rs` 的 `TRUSTED_KEYS`，否则客户端会拒绝新清单；
- 换钥匙时，先发一版带新公钥的客户端，再用新私钥签名。

## 发布一个版本

安装包命名与仓库现有 `packaging/` 一致，放在一个目录里：

```text
ShellRS-0.1.19-windows-x86_64-setup.exe
ShellRS-0.1.19-windows-x86_64-portable.zip
ShellRS-0.1.19-macos-aarch64.dmg
ShellRS-0.1.19-macos-aarch64.app.zip
ShellRS-0.1.19-macos-x86_64.dmg
ShellRS-0.1.19-macos-x86_64.app.zip
ShellRS-0.1.19-linux-x86_64.AppImage
```

缺哪个就跳过哪个，清单里只列存在的平台。然后：

```bash
SHELLAS_ADMIN_TOKEN=... \
MINISIGN_PASSWORD=... \
shellas-server publish \
  --dist dist \
  --version 0.1.19 \
  --channel stable \
  --notes release-notes.md \
  --minisign-key shellas-update.key
```

`publish` 会算大小与 SHA-256、生成清单、在本地签名，再依次上传安装包和
签名信封。加 `--dry-run` 只生成 `dist/stable.json`，不上传。

### 各平台怎么出包

- **Windows**：本机 `cargo build --release --bins`，用 Inno Setup 6.5+
  打安装程序（`packaging/windows/shellrs.iss`），再用 7-Zip 打便携版 zip。
- **Linux**：Docker 里用 `packaging/linux/appimage.sh`，见
  `.github/workflows/release.yml` 的 `linux` 任务。
- **macOS**：只能在 macOS 上构建。用 GitHub Actions 的 `macos-15`
  runner 出两种架构的 `.app.zip` 和 `.dmg`。因为没有 Apple 开发者证书，
  构建时**跳过签名和公证**；下载页已经写明右键打开和 `xattr -dr` 的办法。

## 接口与加密约定

- 账号：注册、登录、退出、`/api/v1/me`；
- 同步：`GET/PUT /api/v1/sync`、`GET/DELETE /api/v1/sync/{version}`；
- 更新：`/update/v1/stable.json`、`/update/v1/beta.json`、`/releases/...`；
- 匿名统计：`POST /api/v0/events`（Aptabase 协议，按 IP 限流，不存原始地址）；
- 管理：`PUT /admin/v1/releases/{version}/{file}`、`PUT /admin/v1/manifest/{channel}`。

同步内容由客户端加密，服务器只看得到密文：

1. 客户端用登录口令 + `sync_salt` 做 Argon2id（64 MiB、3 轮）得到 32 字节密钥；
2. AES-256-GCM 加密快照，随机 12 字节 nonce，附加数据 `shellas-sync-v1`；
3. 上传 `nonce || ciphertext`，`X-Snapshot-Sha256` 是整段内容的 SHA-256。

「同步服务器密码」是客户端设置里的勾选项，默认关闭；服务器无法分辨密文
里有没有密码。
