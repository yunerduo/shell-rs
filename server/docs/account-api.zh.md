# 账号接口契约（v1）

面向客户端注册、登录与账号自助（改口令、改邮箱），以及后台的用户账号列表。本文件是契约：客户端按这里的字段名与错误码对接。

## 0. 范围

- 注册可选填中国大陆手机号，只校验格式，不做短信验证。
- 手机号存 `users.phone`，唯一；老用户没有手机号是正常状态。
- `GET /api/v1/me` 与后台用户列表都返回 `phone`。
- 改口令（校验旧口令、作废全部会话）与改邮箱（无邮件通道，先记待验证）。

## 1. 数据库与迁移

数据库版本 `PRAGMA user_version` 由 **8 → 9**。v9 迁移内容：

```sql
ALTER TABLE users ADD COLUMN phone TEXT;
ALTER TABLE users ADD COLUMN phone_verified INTEGER NOT NULL DEFAULT 0;
CREATE UNIQUE INDEX IF NOT EXISTS users_phone ON users(phone);
```

- `phone` 可空；SQLite 唯一索引允许多个 `NULL`，所以老用户没有手机号不受影响。
- `phone_verified` 是为将来短信/邮件验证预留的状态位，目前**恒为 0**，没有任何接口会把它置 1。
- 迁移在启动时按版本号顺序执行，已有数据原地升级，不重建表。

## 2. 鉴权与错误体

- 用户接口：`Authorization: Bearer <用户会话令牌>`，或 cookie `shellas_user_session`。
- 未登录或令牌失效：401 `unauthorized`。
- 账号被停用：403 `forbidden`。
- 错误体统一为：

```json
{ "error": "invalid_phone", "message": "手机号要 11 位数字，且以 1 开头" }
```

`error` 是给客户端分支判断的稳定错误码，`message` 是给人看的提示，文案可能调整，不要用于判断。

## 3. 注册

`POST /api/v1/auth/register`

```json
{
  "username": "alice",
  "password": "correct horse",
  "email": "alice@example.com",
  "phone": "13800138000"
}
```

| 字段 | 必填 | 规则 |
| --- | --- | --- |
| `username` | 是 | 3–32 位，字母、数字、`_`、`-` |
| `password` | 是 | 8–128 位，且至少包含小写字母、大写字母、数字、其他符号中的两类 |
| `email` | 否 | 格式合法即可；唯一 |
| `phone` | 否 | 去掉首尾空白后必须是 11 位 ASCII 数字、以 `1` 开头；空串视同未填 |

成功：200，返回登录态（同时下发 `shellas_user_session` cookie）：

```json
{
  "token": "…",
  "expires_at": "2026-12-17T00:00:00Z",
  "user": {
    "username": "alice",
    "email": "alice@example.com",
    "phone": "13800138000",
    "pending_email": null,
    "quota_bytes": 52428800,
    "used_bytes": 0,
    "sync_salt": "…",
    "created_at": "2026-11-17T00:00:00Z"
  }
}
```

失败：

| HTTP | `error` | 场景 |
| --- | --- | --- |
| 400 | `bad_request` | 用户名不合规、口令不合规、邮箱格式错 |
| 400 | `invalid_phone` | 手机号格式错（非 11 位、不以 `1` 开头、含非数字） |
| 409 | `conflict` | 用户名或邮箱已被注册 |
| 409 | `phone_taken` | 手机号已被其他账号注册 |
| 413 | `too_large` | 同 IP 注册过于频繁 |
| 500 | `internal` | 服务端错误 |

注意：

- 手机号格式错用独立的 `invalid_phone`，不要按泛化的 `bad_request` 处理；客户端可据此把焦点落到手机号输入框。
- 手机号重复用 `phone_taken`，与用户名/邮箱重复的 `conflict` 分开，便于分别提示。
- 预检与写库之间仍撞上唯一索引（并发注册）时，服务端按索引名区分，同样返回 `phone_taken`；其余冲突仍返回 `conflict`。

## 4. 登录与登出

`POST /api/v1/auth/login`：`{ "username", "password" }`，成功返回与注册相同形状的登录态。总失败次数过多时 429 `locked`。

`POST /api/v1/auth/logout`：204，清除 cookie 并删除当前会话。

## 5. 当前账号

`GET /api/v1/me`

```json
{
  "username": "alice",
  "email": "alice@example.com",
  "phone": "13800138000",
  "pending_email": null,
  "quota_bytes": 52428800,
  "used_bytes": 0,
  "sync_salt": "…",
  "created_at": "2026-11-17T00:00:00Z"
}
```

- `phone`：注册时填的手机号；没有就是 `null`。
- `pending_email`：待验证的新邮箱；没有就是 `null`。验证通过前 `email` 不变。
- `phone` 存在**不代表已验证**；当前没有短信通道，验证状态见第 9 节。

## 6. 修改口令

`PUT /api/v1/me/password`

```json
{ "current_password": "旧口令", "new_password": "新口令" }
```

成功：200。

```json
{ "success": true, "message": "口令已修改，所有已登录会话已失效，请重新登录" }
```

同时清除当前 cookie，并删除该用户的全部会话（含当前这条）。客户端应回到未登录状态并提示重新登录。

失败：

| HTTP | `error` | 场景 |
| --- | --- | --- |
| 400 | `bad_request` | 当前口令不对；或新口令不合规（`message` 含规则说明） |
| 401 | `unauthorized` | 未登录 |
| 429 | `too_many` | 每用户每小时 10 次 |

## 7. 修改邮箱

`PUT /api/v1/me/email`

```json
{ "email": "new@example.com" }
```

服务端还没有邮件通道，无法确认新邮箱归属，因此**不改主邮箱**，只记一条待验证记录。

成功：200。

```json
{
  "success": true,
  "pending": true,
  "email": "new@example.com",
  "verification": "unavailable",
  "message": "服务端还没有邮件通道，新邮箱已记为待验证；验证通过前主邮箱保持不变"
}
```

失败：

| HTTP | `error` | 场景 |
| --- | --- | --- |
| 400 | `bad_request` | 邮箱格式错，或与当前邮箱相同 |
| 409 | `conflict` | 邮箱已被其他账号使用 |
| 401 | `unauthorized` | 未登录 |
| 429 | `too_many` | 每用户每小时 5 次 |

## 8. 后台用户列表

`GET /admin/api/users`（权限 `user:read`）

```json
{
  "success": true,
  "data": [
    {
      "id": 1,
      "username": "alice",
      "email": "alice@example.com",
      "phone": "13800138000",
      "phoneVerified": false,
      "quotaBytes": 52428800,
      "usedBytes": 0,
      "versions": 0,
      "createdAt": "2026-11-17T00:00:00Z",
      "disabled": false
    }
  ]
}
```

- 列表接口一次返回全部用户，没有 `search` 参数。后台用户页在浏览器里按账号、邮箱、手机号做本地过滤；数据量变大后再加服务端搜索。
- 手机号列在没有值时显示 `—`；有值但未验证时显示「未验证」标签。

## 9. 手机号验证的预留状态

当前**没有短信通道**，因此：

- 注册只校验手机号格式，不发送验证码，也不把手机号标记为已验证。
- `users.phone_verified` 与后台字段 `phoneVerified` 目前恒为 `0` / `false`；客户端不得把这个字段当作「已启用」或「已验证」的依据。
- 手机号本身也不代表账号归属已验证；不要用它做登录凭据或找回口令的依据。

将来接入短信时，按下面这份预留演进，不需要再改表结构：

| 位置 | 预留 | 将来含义 |
| --- | --- | --- |
| `users.phone` | 已有，可空 | 注册时填写的号码 |
| `users.phone_verified` | 已有，默认 0 | `1` 表示号码归属已通过验证 |
| `GET /api/v1/me` | 暂不返回验证状态 | 增加 `phone_verified`（或在需要时增加 `pending_phone`） |
| 后台 `UserDto` | 已有 `phoneVerified` | 直接反映验证结果 |
| 新增接口 | 未定义 | 发送验证码、校验验证码、换绑手机号 |

在验证能力落地前，客户端只应展示手机号，不应展示「已验证」。

## 10. 已确认的决定

- 手机号字段名固定为 `phone`，客户端已按此对接。
- 手机号可选；中国大陆 11 位、`1` 开头；不要求短信验证。
- 格式错 → 400 `invalid_phone`；重复 → 409 `phone_taken`；与邮箱/用户名重复的 `conflict` 区分。
- 老用户没有手机号是正常状态，不做强制补填。
- 后台用户页支持按手机号过滤。
