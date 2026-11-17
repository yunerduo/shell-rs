# ShellAs 后台界面

基于 [pure-admin-thin](https://github.com/pure-admin/pure-admin-thin)（MIT），
Vue 3 + Vite + TypeScript + Element Plus。只保留后台需要的布局、登录、
固定路由和角色/按钮权限，业务页面在 `src/views/` 下自己写。

## 与 Rust 服务的关系

- 页面地址：`/admin`（hash 路由，构建 base 是 `/admin/`）。
- 接口前缀：`/admin/api`，由 `server/src/admin.rs` 提供。
- 登录后令牌放在 `Authorization: Bearer <token>`，沿用模板的 `src/utils/http`。
- 菜单是静态路由（`src/router/modules/`），`meta.roles` 控制角色可见性；
  按钮用 `v-perms` 按登录接口返回的 `permissions` 控制。服务端逐接口再校验。
- 角色：`owner`、`publisher`、`viewer`。
- 开发时 Vite 把 `/admin/api` 代理到 `http://127.0.0.1:8080`。

## 开发与构建

```bash
pnpm install
pnpm dev        # http://127.0.0.1:8848/admin/
pnpm typecheck
pnpm build      # 产物 dist/，由 Rust 服务托管
```

`dist/` 不进 Git；`server/Dockerfile` 的 `ui` 阶段会自动构建并复制到
`/app/admin-ui`，运行时的 `SHELLAS_ADMIN_UI_DIR` 指向那里。

## 页面

| 路径 | 文件 | 作用 |
| --- | --- | --- |
| `/release/releases` | `src/views/releases/index.vue` | 查看清单、上传安装包、发布、核对 SHA-256 |
| `/release/changelog` | `src/views/changelog/index.vue` | 维护官网 `/changelog` |
| `/data/usage` | `src/views/usage/index.vue` | 使用记录看板 |
| `/system/admins` | `src/views/admins/index.vue` | 管理员与角色（仅 owner） |

清单分级字段是 `severity`（`recommended` / `required`），发布表单里
「建议更新 / 强制更新」直接对应这两个值；强更时灰度比例默认 1.0。

上游项目的文档：<https://pure-admin.cn>。升级上游时优先保留本目录的
`src/api/admin.ts`、`src/router/modules/`、`src/views/` 与 `.env.*` 的
`VITE_PUBLIC_PATH=/admin/`。
