# Agree ShellAS 服务端（shellas.agree.host）

[返回 README](../README.zh-CN.md)

一个进程，同时提供官网、自建更新服务、注册登录与端到端加密的云同步。
工程在仓库根目录的 `server/`，不参与根 workspace 的构建与测试。

## 1. 目标与范围

| 部分 | 要做什么 | 状态 |
| --- | --- | --- |
| 官网 | 产品介绍、下载页、更新日志，说明 macOS 未签名的安装方式 | 已完成 |
| 更新服务 | 取代 GitHub Releases：已签名的清单 + 安装包下载（支持断点续传） | 已完成 |
| 云同步 | 用户名 + 口令注册登录，上传/下载加密快照，有配额，可回滚旧版本 | 已完成 |
| 匿名统计 | 接收客户端的匿名使用事件，客户端默认上报、可关 | 已完成 |
| 管理接口 | 发布安装包与清单，只对持有管理员令牌的人开放 | 已完成 |
| 客户端 | 注册登录界面、同步引擎、同步设置里「同步服务器密码」默认不勾选 | 待开始 |

不在 v1 范围：邮箱/短信验证、找回口令、支付、多节点、对象存储、ARM 安装包。

## 2. 已定决策

- **一个域名走路径**：`https://shellas.agree.host/` 是官网，`/update/v1/*` 是更新，
  `/releases/*` 是安装包，`/api/v1/*` 是账号与同步。不拆子域。
- **SQLite**（WAL 模式），单实例部署。库里只放元数据，快照和安装包都是磁盘文件。
  理由：写入都是毫秒级小事务，数据量在几十万行以内；单文件备份简单。
- **苹果证书不买**：macOS 包不签名、不公证，下载页写明首次打开的办法
  （右键 → 打开；或 `xattr -dr com.apple.quarantine`）。
- **macOS 包用 GitHub Actions 构建**：本机是 Windows，macOS 目标只能在
  `macos-15` runner 上构建。
- **ARM 先不做**：x86_64 的 Windows / Linux 和两种 macOS 先跑通，ARM 后续再加。
- **服务器上不出现明文**：口令只存 Argon2id 的 PHC 串；同步快照在客户端加密，
  服务器只存密文、盐和摘要。
- **默认不同步服务器密码**：这是客户端的同步选项，默认关闭；服务端只是原样
  保存客户端给的密文，无法分辨里面有没有密码。

## 3. 结构

```text
server/
  Cargo.toml            独立 Cargo 项目（根 Cargo.toml 已 exclude）
  src/
    main.rs             子命令：serve（默认）、publish
    config.rs           全部配置来自环境变量
    db.rs               SQLite：用户、会话、快照、清单
    auth.rs             Argon2id 口令、令牌摘要、字段校验
    api.rs              账号、同步、更新、管理接口
    site.rs             官网三个页面与静态资源
    error.rs            统一的 JSON 错误
    util.rs             时间、sha256、随机、HTML 转义
  web/site.css          官网样式（编译时嵌入二进制）
  Dockerfile
  docker-compose.yml
  README.md             部署与运维
```

一个二进制内嵌所有页面与样式，部署只需要它和 `data/` 目录。

## 4. 数据模型

```sql
users(id, username UNIQUE, email UNIQUE NULL, password_hash,
      quota_bytes, sync_salt, created_at)

sessions(token_hash PRIMARY KEY, user_id, created_at,
         last_used_at, expires_at, device)

snapshots(id, user_id, version, size, sha256, created_at, device,
          UNIQUE(user_id, version))

manifests(channel, version, envelope, published_at,
          PRIMARY KEY(channel, version))
```

磁盘布局：

```text
data/
  shellas.db
  sync/<user_id>/<version>.bin        加密快照，只存密文
  releases/<version>/<文件名>          安装包
  update/v1/<channel>.json            清单的落盘副本（对外服务读数据库）
```

- 每个用户保留最近 20 份快照（`SHELLAS_MAX_SNAPSHOTS`），默认配额 50 MB
  （`SHELLAS_DEFAULT_QUOTA_MB`）。
- 版本号按用户递增；删除或淘汰只删文件，不影响其他用户。

## 5. 接口

### 账号

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/api/v1/auth/register` | `{username, password, email?}`，成功即返回令牌 |
| POST | `/api/v1/auth/login` | `{username, password}`，返回令牌与同步盐 |
| POST | `/api/v1/auth/logout` | 作废当前令牌 |
| GET | `/api/v1/me` | 用户名、邮箱、配额、已用空间、`sync_salt` |

- 用户名 3–32 位，字母数字与 `_`、`-`；口令 8–128 位；邮箱可选，只做格式检查。
- 令牌 32 字节随机数，base64url；库里只存 SHA-256 摘要，默认 30 天过期。
- 注册按 IP 限流（5 次/小时），登录限流（20 次/10 分钟）。

### 同步

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/v1/sync` | `{used_bytes, quota_bytes, latest, versions[]}` |
| PUT | `/api/v1/sync` | 请求体是密文；必需 `X-Snapshot-Sha256`，可选 `X-Device` |
| GET | `/api/v1/sync/{version}` | 下载指定版本，返回 `X-Snapshot-Sha256` |
| DELETE | `/api/v1/sync/{version}` | 删除指定版本 |

上传成功后返回 `{version, size, sha256, created_at}`。超过配额返回 413，
`{"error":"quota_exceeded", used_bytes, quota_bytes}`，客户端据此提示用户。

### 更新

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/update/v1/stable.json` | 稳定通道的签名信封 `{manifest, signature}` |
| GET | `/update/v1/beta.json` | 测试通道的签名信封 |
| GET | `/releases/{version}/{file}` | 安装包，支持 Range（206），供断点续传 |

清单格式与客户端 `src/update/manifest.rs` 完全一致；`signature` 是 minisign
签名，trusted comment 为 `shellrs-manifest <channel> <version>`。

### 匿名统计

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/api/v0/events` | 请求体是 Aptabase 协议的事件数组，`App-Key` 头必须与客户端一致 |

- 不需要账号。App-Key 是公开的，只用来挡住随手乱发的请求；每个地址每小时最多 120 次请求。
- 一批最多 25 个事件；名字不是短标识符、时间解析不了、属性超过 4 KB 的事件安静丢掉，不影响同批其它事件。
- 来源地址和 `User-Agent` **不落库**：两者加当天的盐算成 SHA-256 摘要存进 `device`，第二天换盐，跨天接不上同一台设备。
- 事件落在 `analytics_events` 表：名字、会话 id、设备摘要、事件时间、收到时间、版本、系统、架构（在属性里）和各功能次数。

### 管理

需要 `Authorization: Bearer <SHELLAS_ADMIN_TOKEN>`。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| PUT | `/admin/v1/releases/{version}/{file}` | 上传安装包（流式，上限 400 MB） |
| PUT | `/admin/v1/manifest/{channel}` | 上传签名信封，校验版本与通道后入库 |
| GET | `/healthz` | 健康检查，返回 `ok` |

未设置管理员令牌时，管理接口一律 404。

## 6. 同步加密方案（客户端职责）

服务器只看到密文。客户端（桌面端）第一次登录后：

1. 用登录口令 + 服务端给的 `sync_salt`，本地做 Argon2id（64 MiB、3 轮），
   派生 32 字节密钥；
2. 用 AES-256-GCM 加密快照 JSON，随机 12 字节 nonce，附加数据固定为
   `shellas-sync-v1`；
3. 上传 `nonce || ciphertext`，并把 SHA-256 放在 `X-Snapshot-Sha256`；
4. 新设备登录后拉最新版本解密，本地已有数据时询问「合并还是覆盖」。

因此：忘记登录口令 = 云上的数据解不开。这是有意的取舍，界面上要写明。

## 7. 发布流程

安装包从哪来：

- **Windows**：`cargo build --release --bins`，Inno Setup 6.5+ 打安装程序
  （`packaging/windows/shellrs.iss`），便携版 zip 的根上是两个 exe 和空的 `data`；
- **Linux**：`packaging/linux/build-in-wsl.sh` 在 WSL 的 Ubuntu 22.04 里构建
  AppImage。这样出的包要求 glibc 2.35+（Ubuntu 22.04、Debian 12）；
  要兼容 Ubuntu 20.04，得在 `ubuntu:20.04` 容器里构建；
- **macOS**：`.github/workflows/package.yml`，在 Actions 里手动触发，
  不签名、不公证，产物在本次运行的 Artifacts 里。

然后把包装进 `dist/`，执行：

1. `shellas-server publish` 计算大小与 SHA-256，生成清单，在**本地**用
   minisign 私钥签名；
2. 依次上传安装包与签名信封；
3. 客户端下一次检查更新时拿到新清单并校验签名。

私钥只存在于发布机器（或 CI 的 secret）上，服务器不保存。

## 8. 部署（1Panel）

- DNS：`shellas.agree.host` A 记录指向服务器；
- 1Panel 建反向代理站点，转发到 `127.0.0.1:8080`，申请 Let's Encrypt 证书；
- `docker compose up -d`，数据卷挂到 `./data`；
- 备份：`shellas.db` + `sync/` + `releases/`，定时打包即可。

详细命令见 `server/README.md`。

## 9. 任务清单

- [x] `server/` 独立项目，根 workspace 排除
- [x] 配置、时间/摘要/随机工具
- [x] SQLite 表结构与用户、会话、快照、清单读写
- [x] Argon2id 口令与令牌
- [x] 账号接口与限流
- [x] 同步接口（流式上传、下载、删除、配额）
- [x] 更新接口（清单、Range 下载）与管理上传
- [x] 匿名统计接收（`/api/v0/events`，日盐设备摘要）
- [x] 官网首页、下载页、更新日志
- [x] `publish` 子命令
- [x] Dockerfile、compose、部署说明
- [x] 编译、单元测试（27 个）、clippy
- [ ] 客户端：注册登录、同步设置（密码默认不勾）、更新源指向新域名
- [x] GitHub Actions：macOS 未签名构建 workflow 已写好并推送，待手动运行
- [ ] 把 0.1.19 的包与签名清单发布到新服务

## 10. 验证记录

本机冒烟测试（`127.0.0.1:8099`，临时数据目录）：

- 首页、样式、图标、下载页、更新日志都返回 200；
- 注册 → 上传 17 字节快照 → 列表 → 下载，内容一致；未带令牌返回 401；
- 管理接口上传安装包与清单，`/update/v1/stable.json` 能取回信封；
- 安装包 `Range: bytes=0-4` 返回 206 和 5 字节；
- 未知路径返回 404。

