# 发布运维手册

[返回开发文档](./development.md)

这份手册面向负责发版和站点运维的人。命令里的 `<site-domain>`、`<admin-token>`、`<minisign-key>` 等一律是占位符，实际值只在发布机器和部署环境里，不写进仓库。

## 1. 服务划分

`server/` 是一个 Rust 二进制（`shellas-server`），一个进程同时提供下面几组能力。没有拆成多个微服务。

| 能力 | 路由 | 实现 | 说明 |
| --- | --- | --- | --- |
| 官网页面 | `/`、`/features`、`/download`、`/changelog` | `server/src/site.rs` | HTML 在编译期嵌入二进制；`/features` 重定向到首页的功能区 |
| 站点静态资源 | `/assets/*`、`/favicon*`、`/site.webmanifest` | `server/src/site.rs` | CSS、logo、应用截图、图标 |
| 更新清单 | `/update/v1/stable.json`、`/update/v1/beta.json` | `server/src/api.rs` | 返回 `{manifest, signature}`；对外读数据库，`data/update/v1/` 是落盘副本 |
| 安装包下载 | `/releases/{version}/{file}` | `server/src/api.rs` | 支持 Range（206），供断点续传；每次成功返回记一次下载，撤下的包返回 404 |
| 匿名统计 | `POST /api/v0/events` | `server/src/api.rs` | Aptabase 协议；不存原始 IP |
| 注册登录 | `/api/v1/auth/*`、`/api/v1/me` | `server/src/api.rs` | 令牌只存 SHA-256 摘要；与后台账号完全隔离 |
| 找回口令（占位） | `/api/v1/auth/password/forgot`、`/reset` | `server/src/api.rs` | 当前返回 501，等邮件通道 |
| 云同步 | `/api/v1/sync*`、`/api/v1/sync/documents/{kind}` | `server/src/api.rs` | 只存密文；`settings` / `hosts` 各一条版本链 |
| 管理接口（命令行） | `/admin/v1/releases/*`、`/admin/v1/manifest/*` | `server/src/api.rs` | Bearer `<admin-token>`；未设置令牌且没有后台会话时 404 |
| 后台界面 | `/admin` | `server/admin-ui/` + `server/src/admin.rs` | Vue 3 + Element Plus，构建产物由同一个二进制托管 |
| 后台接口 | `/admin/api/*` | `server/src/admin.rs` | 后台会话 cookie 或 Bearer；owner / publisher / viewer 三种角色 |
| 升级记录 | `/admin/api/releases`、`/withdraw`、`/republish` | `server/src/admin.rs` | 版本、平台、架构、大小、SHA-256、下载次数、分级、撤下/补发 |
| 更新记录 | `/changelog` | `server/src/site.rs` | 读 `changelog` 表，后台可增删改；发布版本时自动写入一条 |
| 健康检查 | `/healthz` | `server/src/api.rs` | 返回 `ok` |

用户与管理员是两套独立体系，运维上要分清：

| 维度 | 客户端用户 | 后台管理员 |
| --- | --- | --- |
| 表 | `users` / `sessions` | `admin_users` / `admin_sessions` |
| 登录入口 | `/api/v1/auth/login` | `/admin/api/auth/login` |
| cookie | `shellas_user_session`（`Path=/api`） | `shellas_admin_session`（`Path=/admin`） |
| 限流 | 注册 5 次/小时、登录 20 次/10 分钟 | 更严的后台限流器 |
| 失败锁定 | `login_lockouts(scope='user')` | `login_lockouts(scope='admin')` |

两边的失败计数互不影响：有人在客户端入口爆破口令，只会锁住那个用户名，
不会牵连后台账号。锁定阈值由 `SHELLAS_LOGIN_MAX_FAILURES`（默认 5）和
`SHELLAS_LOGIN_LOCK_MINUTES`（默认 15）控制。

后台账号页在「系统 → 用户账号」（owner 可见），可以改配额、停用/启用、
重置口令和删除；停用或重置口令会立刻清掉该用户的会话。

运行方式：

- 二进制：`shellas-server serve`（默认子命令就是 `serve`）。
- 容器：`server/` 下的 compose，进程监听 `127.0.0.1:8080`，由站点反向代理到公网，证书在代理层。
- 数据目录由 `SHELLAS_DATA_DIR` 决定，容器里挂一个持久卷；数据库是 SQLite。

服务启动时会自动创建 `sync/`、`releases/`、`update/v1/` 三个目录，不需要手工建。

## 2. 上线要用的 skill

skill 本体放在仓库外的本机目录（例如 `~/.trae-cn/skills/`），**不提交到
GitHub**：里面通常带服务器地址、面板入口和操作路径，属于涉密资产。仓库里
只记录名字和用途。

| skill | 用途 |
| --- | --- |
| `shellas-release` | 构建产物、生成并签名清单、上传安装包、发布清单、公网验签与下载复核 |
| `shellas-deploy` | 只发布官网页面和静态资源（改版、文案、截图），不动安装包和清单 |

官网发版和客户端发版是两条线：改官网只走 `shellas-deploy`；发新版本走
`shellas-release`，最后再按需重发官网的下载页和更新日志。

后台网页发布是第三条路：私钥放到服务器数据卷后，管理员在 `/admin` 里
上传并发布，不再依赖本机 skill。两种方式共用同一套清单与签名逻辑。

## 3. 从打包到发布

### 3.1 版本准备

1. `CHANGELOG.md`（英文）和 `CHANGELOG.zh.md`（中文）各写一节 `## [x.y.z]`。
2. 根 `Cargo.toml` 的 `version` 改成 `x.y.z`。
3. 提交后打 tag `vx.y.z`，推送 tag。
4. 判定标准：tag 与 `Cargo.toml` 一致；两份更新日志都有这一节；`src/update/build_info.rs` 的 `TRUSTED_KEYS` 非空且包含当前签名公钥。

### 3.2 三个平台的产物

所有产物统一放在一个 `dist/` 目录，文件名必须和下面完全一致，`publish` 按名字找包，缺哪个就跳过哪个。

| 平台 | 文件 | 怎么出 |
| --- | --- | --- |
| Windows | `ShellAs-<版本>-windows-x86_64-setup.exe` | `cargo build --release --bins`，用 Inno Setup 6.5+ 跑 `packaging/windows/shellas.iss` |
| Windows 便携版 | `ShellAs-<版本>-windows-x86_64-portable.zip` | 根目录放 `shellas.exe`、`shellas-cli.exe` 和空的 `data/`，用 7-Zip 打包；打完核对正好是这三项 |
| macOS Apple Silicon | `ShellAs-<版本>-macos-aarch64.app.zip`、`ShellAs-<版本>-macos-aarch64.dmg` | 在 macOS 上 `packaging/macos/bundle.sh` 出 `.app`，再 codesign、notarytool、staple，`ditto` 出 zip，`hdiutil` 出 dmg |
| macOS Intel | `ShellAs-<版本>-macos-x86_64.app.zip`、`ShellAs-<版本>-macos-x86_64.dmg` | 同上，按 x86_64 构建 |
| Linux | `ShellAs-<版本>-linux-x86_64.AppImage` | WSL Ubuntu 22.04 里构建，再跑 `packaging/linux/appimage.sh`；产物要求 glibc 2.35+ |

说明：

- `.app.zip` 是给自动更新用的资产，`.dmg` 是给人首次安装用的；Windows 的 setup 和 portable zip 两者兼用；Linux 的 AppImage 两者兼用。
- macOS 没有签名证书时，构建会跳过公证；下载页要写明右键打开和 `xattr -dr com.apple.quarantine` 的办法。
- 只发布部分平台是允许的，清单里只列存在的包；但至少要有一种可升级资产。

### 3.3 生成清单、签名、上传

两条路，二选一：

**A. 命令行（私钥不离开发布机器，推荐）**

```bash
SHELLAS_ADMIN_TOKEN=<admin-token> \
MINISIGN_PASSWORD=<key-password> \
shellas-server publish \
  --dist dist \
  --version <版本> \
  --channel stable \
  --severity recommended \
  --notes release-notes.md \
  --minisign-key <minisign-key> \
  --base-url https://<site-domain>
```

**B. 后台网页（私钥放在服务器）**

1. 把私钥放到服务器数据卷，例如 `server/data/update.key`；
2. 在 `server/.env` 设置 `SHELLAS_MINISIGN_KEY=/data/update.key`（带口令再加
   `SHELLAS_MINISIGN_KEY_PASSWORD`）；
3. 打开 `https://<site-domain>/admin`，用后台账号登录；
4. 「版本发布 → 发布新版本」，填版本号、更新说明、建议更新/强制更新，
   选择各平台安装包，提交；
5. 服务器算 SHA-256、组装清单、签名并立即生效；页面上的「校验摘要」可以
   随时重算并核对。

清单分级字段是 `severity`，取值只有：

- `recommended`：建议更新，客户端提示但不阻断；缺省就是它。
- `required`：强制更新；后台表单默认把灰度比例设为 1.0，否则只有抽中
  灰度的机器会被阻断。

更新说明写在 `notes`（Markdown），客户端直接渲染。不要用 `mandatory`、
`force` 之类的别名。

`publish` 的顺序固定：

1. 扫描 `dist/`，对每个存在的包计算 size 和 SHA-256；
2. 组装清单 JSON（`assets`、`installers`、`notes`、`rollout` 等）；
3. 用 minisign 在本地签名，trusted comment 为 `shellas-manifest <channel> <version>`；
4. 写出 `dist/<channel>.json`（`{manifest, signature}` 信封）；
5. 依次 `PUT /admin/v1/releases/{version}/{file}` 上传每个安装包；
6. 最后 `PUT /admin/v1/manifest/{channel}` 上传签名信封，服务端校验通道和版本后入库。

发布顺序不能反：先传包，最后传清单。清单一旦生效，客户端就会按里面的地址和摘要去下载。

演练：加 `--dry-run` 只生成 `dist/<channel>.json`，不上传。

### 3.4 判定标准

一次发版算完成，必须同时满足：

- `GET https://<site-domain>/update/v1/<channel>.json` 返回 200，且是 `{manifest, signature}` 信封。
- 用受信任公钥能验过签名，trusted comment 的通道和版本与清单一致。
- 清单里每个资产的 `size` 和 `sha256` 与实际下载到的文件一致。
- 每个资产 URL 返回 200，断点续传返回 206。
- 官网 `/download` 和 `/changelog` 已显示新版本（若下载页从清单生成，需重新构建站点）。
- 旧客户端检查更新能拿到新版本；只接受比自己新的版本，发错的版本撤不回来，只能用更新的补丁版覆盖，或把清单改回上一版阻止继续升级。

`stable` 与 `beta` 的规则：

- 带 `-` 的版本号（如 `0.3.0-beta.1`）只进 beta。
- 正式版同时成为 beta 的最新版。
- beta 客户端能收到 stable 的版本，反之不行。

### 3.5 升级记录：撤下与补发

后台「版本发布 → 升级记录」列出每次发布的版本、通道、更新分级、包数、
体积、下载次数、发布时间和状态；展开「安装包」还能看到每个文件的平台、
架构、大小、SHA-256、下载次数、上传时间和是否可下载。

- **撤下**：对一条未撤下的记录点「撤下」。该通道的清单立刻不再下发，
  `current_manifest` 自动回退到该通道上一个未撤下的版本；如果没有任何通道
  还在发同一版本，相关安装包也标记为撤下并停止下载。撤下不删文件，记录保留。
- **补发**：对一条已撤下的记录点「补发」。服务器用保存的清单参数重新组装、
  签名并发布，发布时间更新为现在，安装包恢复可下载。
- **下载次数**：`/releases/{version}/{file}` 每次成功返回（200 或 206）计一次；
  `HEAD` 和 416 不计。只用于展示趋势，不是精确的装机量。
- 安装包以磁盘为准：`data/releases/<version>/` 下的文件在后台列表里会自动
  补登记；摘要第一次算出来后写回数据库，之后直接显示。

## 4. 服务器目录约定

```text
<data-dir>/
  shellas.db                         SQLite：账号、令牌、同步元数据、清单记录、更新记录、后台账号
  update.key                         可选：后台发布用的 minisign 私钥（不要进 Git）
  releases/<version>/<文件名>         安装包，对外走 /releases/{version}/{file}
  update/v1/<channel>.json           清单落盘副本（对外读数据库，这里是给人看和备份的）
  sync/<user-id>/<version>.bin       加密快照，只存密文
  sync/<user-id>/documents/<kind>/<version>.bin   分文档同步（settings / hosts）
```

环境变量（与登录锁定相关）：

| 变量 | 默认 | 说明 |
| --- | --- | --- |
| `SHELLAS_LOGIN_MAX_FAILURES` | 5 | 连续失败多少次后锁定，用户与管理员各自计数 |
| `SHELLAS_LOGIN_LOCK_MINUTES` | 15 | 锁定时长（分钟） |

后台界面构建产物不在数据目录：镜像里固定在 `/app/admin-ui`，
`SHELLAS_ADMIN_UI_DIR` 指向它。

约定：

- 安装包按版本分目录，文件名与 `packaging/` 和 `release.rs` 里的命名一致。
- 不手工改 `update/v1/*.json` 来发布：对外服务读数据库，改文件不生效。要换清单就走管理接口重新发布。
- 手工替换某个版本的安装包：先 `PUT /admin/v1/releases/{version}/{file}` 覆盖，再重新发布同版本清单（摘要会重新计算）。只换文件不换清单，客户端会因 SHA-256 不匹配报损坏。
- 清理旧版本：先确认没有客户端还在按旧清单下载，再删 `releases/<version>/`；数据库里的旧清单记录不会因此自动消失。
- 备份：`shellas.db` + `releases/` + `update/v1/` + `sync/`。数据库开了 WAL，热备份用 `sqlite3 shellas.db ".backup backup.db"`，或先停服务。
- 官网页面、CSS、截图都编译进二进制，改这些要重新构建镜像；它们不占数据目录。

### 4.1 整套源码部署（不要单文件替换）

曾经发生过：只把 `site.rs` 传到服务器，而它引用了 `crate::release`，服务器
上没有 `release.rs`、`main.rs` 也没声明 `mod release`，重新构建后容器起不
来，站点 502，只能回滚。以后按下面顺序走：

1. 先确认当前站点健康：`curl -fsS http://127.0.0.1:8080/healthz`。
2. 上传**整套 `server/` 源码**，至少包括 `src/` 全部、`web/`、`admin-ui/`、
   `Cargo.toml`、`Cargo.lock`、`Dockerfile`、`docker-compose.yml`；
   不要挑单个 `.rs` 文件替换。
3. 在服务器上先构建，不碰正在跑的容器：`cd server && docker compose build`。
4. 构建成功后才 `docker compose up -d`；构建失败就停在这里，旧容器继续服务。
5. 重建后复核：`/healthz`、`/admin`、`/update/v1/stable.json`、
   `/changelog`，再下载一个安装包核对 SHA-256。
6. 出问题立即回滚到上一版镜像，确认站点恢复后再排查。

## 5. 每个产物交付什么 hash 与签名

| 产物 | 交付的校验信息 | 在哪一步产生 | 在哪一步校验 |
| --- | --- | --- | --- |
| 每个安装包 / 升级包 | SHA-256、字节数 | `publish` 扫描 `dist/` 时计算，写进清单 `assets`；后台发布时服务器计算 | 客户端下载后按清单核对；后台「版本发布 → 校验摘要」可重算；运维也可手工 `sha256sum` |
| 更新清单 | minisign 签名，trusted comment `shellas-manifest <channel> <version>` | 发布机器本地签名，服务器只收已签名信封 | 客户端先验签名和 trusted comment，再信清单 |
| 签名身份 | key id | 公钥文件第二行；当前在用 `9A7DE51998612035`，离线备用 `F3B9F08DE80AF399`（见 `src/update/build_info.rs`） | 客户端用 `TRUSTED_KEYS` 里的公钥验签；任一受信任公钥通过即可 |
| 清单信封 | 无额外 hash | `publish` 写出 `dist/<channel>.json` | 服务端校验通道/版本后入库；客户端验签后解析 |

客户端的校验顺序（`src/update/verify.rs`）：

1. 解析信封，用受信任公钥验 minisign 签名；
2. 从 trusted comment 取通道和版本，必须与请求的通道一致；
3. 解析清单，清单里的 `channel`/`version` 必须与 trusted comment 一致；
4. 下载资产后先比字节数，再比 SHA-256；任一不符按损坏处理。

换密钥的规则：先把新公钥发进一版客户端，再用新私钥签名。私钥丢失后，已发布的客户端再也收不到更新。

公网复核命令：

```bash
curl -fsS https://<site-domain>/update/v1/stable.json -o envelope.json
jq -r .manifest envelope.json > manifest.json
jq -r .signature envelope.json > manifest.json.minisig
minisign -Vm manifest.json -x manifest.json.minisig -p <public-key>.pub
jq -r '.assets | to_entries[] | "\(.key) \(.value.size) \(.value.sha256) \(.value.urls[0])"' manifest.json
# 对每个 URL：curl -fL -o file <url> && sha256sum file
```

## 6. `.gitignore` 约定

仓库根目录已经忽略 `/dist/`、`.env*`、`/target/`。发布运维还要保证下面这些
不进 Git：

```gitignore
# 本机 skill（含服务器地址、面板入口与操作路径）与凭据
/.trae/
/local-skills/
/.claude/skills/shellas-*/
/.agents/skills/shellas-*/
/.secrets/
*.key
!*.key.example

# 本地发布配置与临时产物
/deploy.local.*
/release-tmp/
*.minisig.tmp
```

`server/.gitignore` 忽略 `server/data/`、`server/target/`、`server/dist/`、
`server/.preview/` 和 `check.log`。`server/admin-ui/.gitignore` 忽略
`node_modules/`、`dist/`、`.eslintcache`、`report.html` 和 Vite 临时文件。

发布用的私钥、管理员令牌、后台口令、面板凭据一律放在仓库外；`.env` 只提交
`.env.example`。`server/data/update.key` 虽然可能在服务器上存在，但
`server/data/` 已被忽略，不会被误提交。
