# ShellAs 服务端（shellas.agree.host）

[返回 README](../README.zh-CN.md)

一个进程，同时提供官网、自建更新服务、注册登录与端到端加密的云同步。
工程在仓库根目录的 `server/`，不参与根 workspace 的构建与测试。

## 1. 目标与范围

| 部分 | 要做什么 | 状态 |
| --- | --- | --- |
| 官网 | 产品介绍、下载页、更新日志，说明 macOS 未签名的安装方式 | 已完成 |
| 更新服务 | 取代 GitHub Releases：已签名的清单 + 安装包下载（支持断点续传） | 已完成 |
| 云同步 | 用户名 + 口令注册登录，上传/下载加密快照与分文档同步，有配额，可回滚旧版本 | 已完成 |
| 匿名统计 | 接收客户端的匿名使用事件，客户端默认上报、可关 | 已完成 |
| 管理接口 | 发布安装包与清单，只对持有管理员令牌的人开放 | 已完成 |
| 后台 | `/admin` 管理端：发布、升级记录、撤下/补发、更新日志、使用看板、用户与管理员账号 | 已完成 |
| 客户端 | 注册登录界面、同步引擎、同步设置里「同步服务器密码」默认不勾选 | 待开始 |

不在 v1 范围：邮箱/短信验证、邮件找回口令（接口位置已留，返回 501）、支付、多节点、对象存储、ARM 安装包。

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
- **用户与管理员彻底分开**：两套表、两套会话、两套路由、两个 cookie、两套
  限流与失败锁定。拿客户端入口爆破口令只会锁住那一个用户名，碰不到后台账号；
  反过来后台登录失败也锁不到用户。接口层连函数都不共用。
- **同步按文档分链**：`settings` 和 `hosts` 各一条独立版本链，上传必须带
  `X-Base-Version`，基线不是最新就返回 409 并附上最新文档，客户端合并后再传。
  服务端只存密文和 SHA-256，做不了字段级合并，所以把冲突判定交给客户端。
- **秘密不上云**：客户端只上传加密后的密文；主机里的密码、私钥口令、凭据秘密
  永远留在本地钥匙串。服务端既没有密钥，也没有明文入口。
- **安装包以磁盘为准**：`data/releases/<version>/` 下的文件是事实来源，数据库
  `release_packages` 只补大小、SHA-256、平台、架构和下载次数；发布记录
  `release_versions` 记录每次发布的清单参数，用于撤下、补发和审计。

## 3. 结构

```text
server/
  Cargo.toml            独立 Cargo 项目（根 Cargo.toml 已 exclude）
  src/
    main.rs             子命令：serve（默认）、publish
    config.rs           全部配置来自环境变量
    db.rs               SQLite：用户、会话、快照、清单、更新记录、后台账号
    auth.rs             Argon2id 口令、令牌摘要、字段校验
    api.rs              账号、同步、更新、命令行管理接口
    admin.rs            后台登录、RBAC、发布、更新记录、使用记录、静态页面托管
    release.rs          安装包命名与清单 JSON 组装（命令行与后台共用）
    signing.rs          minisign 清单签名
    site.rs             官网三个页面与静态资源
    error.rs            统一的 JSON 错误
    util.rs             时间、sha256、随机、HTML 转义
  admin-ui/             后台前端（pure-admin-thin，Vue 3 + Element Plus）
  web/site.css          官网样式（编译时嵌入二进制）
  Dockerfile           Rust + Node 两段构建
  docker-compose.yml
  README.md             部署与运维
```

一个二进制内嵌所有页面与样式，部署只需要它、后台静态文件目录和 `data/`
目录。后台界面在镜像里固定在 `/app/admin-ui`。

## 4. 数据模型

```sql
-- 客户端账号（与 admin_* 完全无关）
users(id, username UNIQUE, email UNIQUE NULL, password_hash,
      quota_bytes, sync_salt, created_at, disabled)

sessions(token_hash PRIMARY KEY, user_id, created_at,
         last_used_at, expires_at, device)

snapshots(id, user_id, version, size, sha256, created_at, device,
          UNIQUE(user_id, version))

-- 按文档增量同步：settings / hosts 各一条版本链
sync_documents(id, user_id, kind, version, size, sha256, created_at, device,
               UNIQUE(user_id, kind, version))

manifests(channel, version, envelope, published_at, withdrawn,
          PRIMARY KEY(channel, version))

release_packages(version, file, platform, arch, size, sha256,
                 downloads, uploaded_at, withdrawn,
                 PRIMARY KEY(version, file))

release_versions(version, channel, severity, notes, notes_url,
                 minimum_version, rollout, published_at, withdrawn,
                 package_count, total_size, downloads,
                 PRIMARY KEY(version, channel))

login_lockouts(scope, name, failures, locked_until, updated_at,
               PRIMARY KEY(scope, name))

analytics_events(id, name, session_id, device, occurred_at, received_at,
                 app_version, os_name, os_version, sdk_version, is_debug, props)
analytics_salts(day PRIMARY KEY, salt)

-- 后台账号（与服务端 users 完全无关）
admin_users(id, username UNIQUE, password_hash, role, created_at)
admin_sessions(token_hash PRIMARY KEY, admin_id, created_at, expires_at)

changelog(id, version UNIQUE, channel, severity, title, body,
          published_at, updated_at)
```

磁盘布局：

```text
data/
  shellas.db
  sync/<user_id>/<version>.bin        加密快照，只存密文
  sync/<user_id>/documents/<kind>/<version>.bin
  releases/<version>/<文件名>          安装包
  update/v1/<channel>.json            清单的落盘副本（对外服务读数据库）
```

- 每个用户保留最近 20 份快照（`SHELLAS_MAX_SNAPSHOTS`），`settings` 与
  `hosts` 各保留最近 20 份文档；默认配额 50 MB（`SHELLAS_DEFAULT_QUOTA_MB`），
  快照与文档合并计算。
- 版本号按用户、按文档分别递增；删除或淘汰只删文件，不影响其他用户。
- `manifests` 与 `release_versions` 的 `withdrawn` 为 1 时不再下发；后台
  撤下清单后，`current_manifest` 自动回退到该通道上一个未撤下的版本。
- `login_lockouts` 用 `scope` 区分 `user` / `admin`，两者互不影响；连续失败
  达到 `SHELLAS_LOGIN_MAX_FAILURES`（默认 5）后锁定
  `SHELLAS_LOGIN_LOCK_MINUTES`（默认 15）分钟，30 天没动过的记录自动清理。

## 5. 接口

### 账号

客户端账号（`/api/v1/auth/*`）与后台管理员（`/admin/api/auth/*`）是两套独立
体系，互不复用表、会话或登录逻辑：

| 维度 | 客户端账号 | 后台管理员 |
| --- | --- | --- |
| 表 | `users` / `sessions` | `admin_users` / `admin_sessions` |
| 登录路由 | `/api/v1/auth/login` | `/admin/api/auth/login` |
| 会话 cookie | `shellas_user_session`，`Path=/api`，HttpOnly | `shellas_admin_session`，`Path=/admin`，HttpOnly |
| 令牌通道 | `Authorization: Bearer`，桌面客户端用 | `Authorization: Bearer`，浏览器后台主要用 cookie |
| 限流器 | `user_limiter`（注册 5 次/小时，登录 20 次/10 分钟） | `admin_limiter`（更严） |
| 失败锁定 | `login_lockouts(scope='user')` | `login_lockouts(scope='admin')` |
| 权限 | 只有自己的数据 | `owner` / `publisher` / `viewer`，逐接口校验 |

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/api/v1/auth/register` | `{username, password, email?}`，成功即返回令牌并下发 cookie |
| POST | `/api/v1/auth/login` | `{username, password}`，返回令牌与同步盐，下发 cookie |
| POST | `/api/v1/auth/logout` | 作废当前令牌（Bearer 或 cookie），并清 cookie |
| POST | `/api/v1/auth/password/forgot` | 占位接口，当前返回 501；将来接邮件找回 |
| POST | `/api/v1/auth/password/reset` | 占位接口，当前返回 501；将来接邮件里的重置令牌 |
| GET | `/api/v1/me` | 用户名、邮箱、配额、已用空间、`sync_salt` |

- 用户名 3–32 位，字母数字与 `_`、`-`；口令 8–128 位；邮箱可选，只做格式检查。
- 令牌 32 字节随机数，base64url；库里只存 SHA-256 摘要，默认 30 天过期。
- 登录失败达到阈值返回 429（`error=locked`），成功后清零；账号不存在时也做一次
  Argon2 校验，避免用响应时间猜用户名。停用账号登录返回 403，且其会话立即失效。
- 管理员被停用、改口令或删除时，其后台会话一并作废。

### 同步

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/api/v1/sync` | `{used_bytes, quota_bytes, latest, versions[], documents, latest_documents}` |
| PUT | `/api/v1/sync` | 整包快照；请求体是密文；必需 `X-Snapshot-Sha256`，可选 `X-Device` |
| GET | `/api/v1/sync/{version}` | 下载指定快照版本，返回 `X-Snapshot-Sha256` |
| DELETE | `/api/v1/sync/{version}` | 删除指定快照版本 |
| PUT | `/api/v1/sync/documents/{kind}` | 上传一份文档；`kind` 只有 `settings` / `hosts`；必需 `X-Snapshot-Sha256`，`X-Base-Version` 见下 |
| GET | `/api/v1/sync/documents/{kind}/{version}` | 下载指定文档版本 |
| DELETE | `/api/v1/sync/documents/{kind}/{version}` | 删除指定文档版本 |

上传成功后返回 `{kind, version, size, sha256, created_at}`。超过配额返回 413，
`{"error":"quota_exceeded", used_bytes, quota_bytes}`，客户端据此提示用户。

增量与冲突规则：

1. 每个用户、每个 `kind` 各有一条独立递增的版本链，互不影响。
2. 上传必须带 `X-Base-Version`：首次上传留空；之后填客户端手里最新的版本号。
3. 如果 `X-Base-Version` 不等于服务端最新版本，返回 409：
   `{"error":"conflict","message":"服务端已有更新的版本，请先合并再上传","latest":{...}}`，
   `latest` 是最新文档的元数据（不含正文）。
4. 客户端拿到 409 后先下载最新版本、在本地解密合并，再用新的版本号作基线重传。
5. 服务端只按 `kind` 保留最近 20 份文档，超出的按版本从旧到新淘汰（返回
   `evicted`，由接口删文件）。
6. 服务端不解析密文，不做字段级合并，也不保存任何密钥；秘密本身永远不上云。

### 更新

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| GET | `/update/v1/stable.json` | 稳定通道的签名信封 `{manifest, signature}` |
| GET | `/update/v1/beta.json` | 测试通道的签名信封 |
| GET | `/releases/{version}/{file}` | 安装包，支持 Range（206），供断点续传；每次 GET 记一次下载 |

清单格式与客户端 `src/update/manifest.rs` 完全一致；`signature` 是 minisign
签名，trusted comment 为 `shellas-manifest <channel> <version>`。

清单顶层有 `severity`，取值只有 `recommended`（建议更新，缺省）和
`required`（强制更新）；`notes` 是 Markdown 更新说明。强更时后台默认把
`rollout` 设为 1.0，否则只有抽中灰度的机器会被阻断。

安装包与升级记录：

- 下载路由只服务 `release_packages` 里登记过、且未撤下的文件；未登记的文件
  首次被请求时自动补登记（大小、平台、架构，摘要留空，后台「校验摘要」会补算）。
- 撤下某个版本时，清单不再下发；如果没有任何通道还在发同一版本，相关安装包
  也标记为撤下并停止下载。撤下只改状态，不删文件，发布记录保留。
- 补发用 `release_versions` 里保存的清单参数重新签名并发布，发布时间更新为现在。

### 匿名统计

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/api/v0/events` | 请求体是 Aptabase 协议的事件数组，`App-Key` 头必须与客户端一致 |

- 不需要账号。App-Key 是公开的，只用来挡住随手乱发的请求；每个地址每小时最多 120 次请求。
- 一批最多 25 个事件；名字不是短标识符、时间解析不了、属性超过 4 KB 的事件安静丢掉，不影响同批其它事件。
- 来源地址和 `User-Agent` **不落库**：两者加当天的盐算成 SHA-256 摘要存进 `device`，第二天换盐，跨天接不上同一台设备。
- 事件落在 `analytics_events` 表：名字、会话 id、设备摘要、事件时间、收到时间、版本、系统、架构（在属性里）和各功能次数。

### 管理

命令行管理接口需要 `Authorization: Bearer <SHELLAS_ADMIN_TOKEN>`。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| PUT | `/admin/v1/releases/{version}/{file}` | 上传安装包（流式，上限 400 MB） |
| PUT | `/admin/v1/manifest/{channel}` | 上传签名信封，校验版本与通道后入库 |
| GET | `/healthz` | 健康检查，返回 `ok` |

未设置管理员令牌且没有后台会话时，命令行管理接口一律 404（保持老行为）。

后台界面（`/admin`，Vue 3 + Element Plus）使用独立的 `admin_users` 账号和
不透明会话令牌，与客户端账号无关。角色为 `owner` / `publisher` / `viewer`，
服务端逐接口校验权限。

| 方法 | 路径 | 说明 |
| --- | --- | --- |
| POST | `/admin/api/auth/login` | 后台登录，返回 roles / permissions / accessToken，下发 `shellas_admin_session` |
| POST | `/admin/api/auth/refresh` | 续期会话并重设 cookie |
| POST | `/admin/api/auth/logout` | 作废当前会话并清 cookie |
| GET | `/admin/api/releases` | 当前 stable/beta 清单、历史清单（含撤下状态）、已上传版本与文件（平台/架构/大小/SHA-256/下载次数/撤下状态）、升级记录 |
| GET | `/admin/api/releases/packages?version=` | 这个版本需要的平台文件名 |
| GET | `/admin/api/releases/{version}/hashes` | 按需计算已上传包的 SHA-256，并写回 `release_packages` |
| PUT | `/admin/api/releases/{version}/{file}` | 上传安装包（也接受后台会话），同步登记平台、架构、摘要 |
| POST | `/admin/api/release` | 组装、签名并发布清单，写入升级记录 |
| POST | `/admin/api/releases/{version}/withdraw` | `{channel}`；撤下该通道的该版本，必要时停发安装包 |
| POST | `/admin/api/releases/{version}/republish` | `{channel}`；用保存的记录重新签名并补发 |
| GET/POST | `/admin/api/changelog` | 更新记录列表 / 新增 |
| PUT/DELETE | `/admin/api/changelog/{id}` | 修改 / 删除更新记录 |
| GET | `/admin/api/usage?days=30` | 使用记录汇总 |
| GET | `/admin/api/users` | 客户端用户列表（owner） |
| PUT | `/admin/api/users/{id}` | 改配额或停用/启用（owner），停用会清掉其会话 |
| POST | `/admin/api/users/{id}/password` | 重置用户口令（owner），同时清会话 |
| DELETE | `/admin/api/users/{id}` | 删除用户及其同步目录（owner） |
| GET/POST | `/admin/api/admins` | 管理员列表 / 新增（owner） |
| PUT/DELETE | `/admin/api/admins/{id}` | 改角色或重置口令 / 删除（owner） |
| POST | `/admin/api/account/password` | 修改自己的口令 |

后台发布若要走服务器签名，需要配置 `SHELLAS_MINISIGN_KEY`；不配置时后台
发布会返回明确错误，命令行 `publish` 不受影响。

后台登录同样受失败锁定约束（scope=`admin`），阈值与用户一样由
`SHELLAS_LOGIN_MAX_FAILURES` / `SHELLAS_LOGIN_LOCK_MINUTES` 控制，但计数
与用户完全分开。

## 6. 同步加密方案（客户端职责）

服务器只看到密文。客户端（桌面端）第一次登录后：

1. 用登录口令 + 服务端给的 `sync_salt`，本地做 Argon2id（64 MiB、3 轮），
   派生 32 字节密钥；
2. 用 AES-256-GCM 加密快照 JSON，随机 12 字节 nonce，附加数据固定为
   `shellas-sync-v1`；
3. 上传 `nonce || ciphertext`，并把 SHA-256 放在 `X-Snapshot-Sha256`；
4. 新设备登录后拉最新版本解密，本地已有数据时询问「合并还是覆盖」。

分文档同步时，`settings` 和 `hosts` 各自加密成一份密文后按上一节规则上传，
并带 `X-Base-Version`；409 时先拉最新、合并、再传。`hosts` 文档里只包含
主机、分组、凭据引用等可同步字段，密码、私钥口令和凭据秘密不进入明文，
也就不会进入密文。

因此：忘记登录口令 = 云上的数据解不开。这是有意的取舍，界面上要写明。

## 7. 发布流程

安装包从哪来：

- **Windows**：`cargo build --release --bins`，Inno Setup 6.5+ 打安装程序
  （`packaging/windows/shellas.iss`），便携版 zip 的根上是两个 exe 和空的 `data`；
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
- [x] 后台管理端：管理员登录与角色、版本发布、更新记录维护、使用记录看板
- [x] 清单 `severity`（`recommended` / `required`）与后台发布
- [x] `publish` 子命令
- [x] 用户与管理员彻底隔离：独立表、会话、路由、cookie、限流、失败锁定
- [x] 登录失败锁定与用户停用（会话立即失效）
- [x] 找回口令接口占位（501），等邮件通道
- [x] 按文档增量同步（`settings` / `hosts`，`X-Base-Version` 冲突检测）
- [x] 升级记录：平台、架构、大小、SHA-256、下载次数、分级、发布时间
- [x] 撤下与补发版本（清单回退、安装包状态同步）
- [x] 后台用户账号页：搜索、配额、停用/启用、重置口令、删除
- [x] Dockerfile（Rust + Node 两段构建）、compose、部署说明
- [x] 编译、单元测试、clippy
- [ ] 客户端：注册登录、分文档同步、同步设置（密码默认不勾）、更新源指向新域名
- [ ] 邮件找回口令（接口已占位，需 SMTP 与重置令牌表）
- [x] GitHub Actions：macOS 未签名构建 workflow 已写好并推送，待手动运行
- [ ] 把 0.1.19 的包与签名清单发布到新服务

## 10. 验证记录

本机冒烟测试（`127.0.0.1:8099`，临时数据目录）：

- 首页、样式、图标、下载页、更新日志都返回 200；
- 注册 → 上传 17 字节快照 → 列表 → 下载，内容一致；未带令牌返回 401；
- 管理接口上传安装包与清单，`/update/v1/stable.json` 能取回信封；
- 安装包 `Range: bytes=0-4` 返回 206 和 5 字节；
- 未知路径返回 404。

后台管理端冒烟（`127.0.0.1:18080` / `:18081`，临时数据目录）：

- `/admin` 返回后台页面，构建产物的入口 JS 与静态资源 200；
- owner 登录拿到权限列表，错误口令 401，匿名访问后台接口 401；
- 更新记录新增 → 官网 `/changelog` 出现「强制更新」与正文 → 修改 → 删除；
- 上传安装包返回 SHA-256，`/admin/api/releases/{version}/hashes` 重算一致；
- 未配置私钥时发布返回 400 和明确提示；
- 用临时 minisign 私钥发布 `3.0.0`（`severity=required`，`rollout=1.0`）后，
  `/update/v1/stable.json` 的清单含 `severity`、无 `level`，资产大小与
  SHA-256 与本地文件一致，下载内容一致；
- viewer 角色可以看使用记录，但新增更新记录返回 403；
- 退出登录后同一令牌再访问返回 401。

本轮新增功能冒烟（`127.0.0.1:8124` / `:8125`，临时数据目录，重新构建的二进制）：

- 注册与登录返回 200，登录响应带一行 `Set-Cookie`（`shellas_user_session`）；
- 文档同步：`settings` 首次上传 200（`version=1`），错误基线 409，
  非法 `kind` 400，下载 200，删除 204；`hosts` 单独一条版本链；
- `POST /api/v1/auth/password/forgot` 返回 501（占位）；
- 连续错误口令第 5 次返回 429，锁定后再用正确口令仍 429；同一时刻后台
  `admin` 登录仍 200，证明锁定不串；
- 安装包上传 200，完整下载 200、`Range: bytes=0-4` 返回 206/5 字节、
  越界 `Range` 返回 416、`HEAD` 200，下载计数只加 2（HEAD 与 416 不计）；
- 撤下一个没有发布记录的版本返回 404，匿名访问 `/admin/api/users` 返回 401。

`cargo test` 41 个（含 5 个新增：文档基线与版本链、用户/管理员锁定隔离、
安装包元数据与下载计数、清单撤下后回退、安装包 Range 续传解析）、
`cargo clippy --all-targets -- -D warnings`、
`server/admin-ui` 的 `pnpm typecheck` 与 `pnpm build` 均通过。Docker 镜像
构建未在本机验证（Docker 守护进程未运行），部署时按
`docs/release-ops-manual.zh.md` 的整套源码流程先在服务器上 `docker compose build`。

