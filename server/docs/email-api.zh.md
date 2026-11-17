# 邮件与账号验证接口契约（v1）

面向客户端的「注册需验证邮箱」「找回密码」，以及后台的 SMTP 配置、模板编辑、测试发送和发送记录。本文件是契约：客户端按这里的字段名与错误码对接，后台页面按这里的管理接口实现。

## 0. 范围

- 后台可配置 SMTP（主机、端口、加密方式、用户名、口令、发件人），配置存数据库，保存后**下一条邮件立即生效**，不需要重启服务。
- 两套可编辑模板：注册邮箱验证码、找回密码验证码；后台可预览。
- 注册是否要求邮箱验证由后台开关控制，**默认关闭**。关闭时注册流程与现有客户端完全一致。
- 找回密码：请求验证码 → 邮件收到 6 位数字 → 凭验证码和新口令重置；成功后该用户所有会话立即失效。
- 验证码 6 位数字、10 分钟有效、同一邮箱每分钟最多 1 封、每小时最多 5 封、连续输错 5 次作废、重发即作废旧码。
- 服务端只存验证码摘要，不存明文；发送记录也不存验证码。

不在本轮范围：换绑邮箱的验证流程（现有 `PUT /api/v1/me/email` 行为不变）、短信、邮件退信回调、客户端界面实现。

## 1. 数据库与迁移

数据库版本 `PRAGMA user_version` 由 **9 → 10**。v10 迁移内容：

```sql
ALTER TABLE users ADD COLUMN email_verified INTEGER NOT NULL DEFAULT 0;

CREATE TABLE email_settings (
  id                   INTEGER PRIMARY KEY CHECK (id = 1),
  enabled              INTEGER NOT NULL DEFAULT 0,
  host                 TEXT NOT NULL DEFAULT '',
  port                 INTEGER NOT NULL DEFAULT 587,
  encryption           TEXT NOT NULL DEFAULT 'starttls',
  username             TEXT NOT NULL DEFAULT '',
  password             TEXT NOT NULL DEFAULT '',
  from_address         TEXT NOT NULL DEFAULT '',
  from_name            TEXT NOT NULL DEFAULT '',
  require_verification INTEGER NOT NULL DEFAULT 0,
  updated_at           TEXT NOT NULL
);

CREATE TABLE email_templates (
  purpose    TEXT PRIMARY KEY,
  subject    TEXT NOT NULL,
  body       TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

CREATE TABLE email_codes (
  id             INTEGER PRIMARY KEY,
  purpose        TEXT NOT NULL,
  email          TEXT NOT NULL COLLATE NOCASE,
  code_hash      TEXT NOT NULL,
  attempts       INTEGER NOT NULL DEFAULT 0,
  max_attempts   INTEGER NOT NULL DEFAULT 5,
  created_at     TEXT NOT NULL,
  expires_at     TEXT NOT NULL,
  consumed_at    TEXT,
  invalidated_at TEXT
);
CREATE INDEX email_codes_lookup ON email_codes(purpose, email, created_at);

CREATE TABLE email_sends (
  id         INTEGER PRIMARY KEY,
  email      TEXT NOT NULL,
  purpose    TEXT NOT NULL,
  result     TEXT NOT NULL,
  detail     TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL
);
```

- `email_settings` 只有一行（`id = 1`）。口令只进不出：任何接口都不回传，只回 `hasPassword`。
- 迁移同时写入两套默认模板，管理员可直接编辑；如果模板后来被清空或占位符写错，发信前会拒绝发送并给出明确错误。
- `email_verified` 只描述「邮箱归属是否通过验证码确认」。已有账号迁移后为 `0`，不会被停用或要求补验证，见第 5 节。
- `.env` 只在 `email_settings` 行不存在时播种一次，之后以库里保存的值为准。播种变量：

| 变量 | 含义 |
| --- | --- |
| `SHELLAS_SMTP_HOST` | 主机 |
| `SHELLAS_SMTP_PORT` | 端口，默认 587 |
| `SHELLAS_SMTP_ENCRYPTION` | `starttls` / `tls` / `plain` |
| `SHELLAS_SMTP_USERNAME` | 用户名，可空 |
| `SHELLAS_SMTP_PASSWORD` | 口令，可空；只写日志以外的任何地方都不出现 |
| `SHELLAS_SMTP_FROM` | 发件人地址 |
| `SHELLAS_SMTP_FROM_NAME` | 发件人显示名 |

## 2. 后台：SMTP 配置

权限：`email:read` 读取，`email:write` 修改（当前只有 `owner` 拥有）。

### 2.1 读取

`GET /admin/api/email/settings`

```json
{
  "success": true,
  "data": {
    "enabled": true,
    "host": "smtp.example.com",
    "port": 587,
    "encryption": "starttls",
    "username": "no-reply@example.com",
    "hasPassword": true,
    "fromAddress": "no-reply@example.com",
    "fromName": "Agree ShellAs",
    "requireVerification": false,
    "configured": true,
    "warning": null,
    "updatedAt": "2026-11-17T00:00:00Z"
  }
}
```

- `hasPassword` 只表示「库里存了口令」，口令本身永不下发。
- `configured` 表示主机、发件人地址、端口、加密方式已经齐全，可以尝试发信。
- `warning`：`encryption = "plain"` 时固定为「明文连接会让口令与邮件内容在网络中以明文传输，仅限本机或可信内网使用」，后台必须显著展示；其他情况为 `null`。
- 加密方式取值：`starttls`（先明文连接再升级）、`tls`（隐式 TLS，通常 465 端口）、`plain`（全程明文，需显式警告）。

### 2.2 保存

`PUT /admin/api/email/settings`

```json
{
  "enabled": true,
  "host": "smtp.example.com",
  "port": 587,
  "encryption": "starttls",
  "username": "no-reply@example.com",
  "password": "只在新填时提交",
  "clearPassword": false,
  "fromAddress": "no-reply@example.com",
  "fromName": "Agree ShellAs",
  "requireVerification": true
}
```

- 字段都可选，只改提交上来的字段；`password` 留空表示不修改，`clearPassword = true` 表示删除已存口令。
- 校验：`port` 1–65535；`encryption` 只能是上述三个值；`fromAddress` 必须是合法邮箱；填了 `username` 就必须有口令（本次提交或库里已有）；`enabled = true` 时主机和发件人地址不能为空；`requireVerification = true` 时 SMTP 必须已经配置完整并启用，否则拒绝保存。
- 成功返回与 2.1 相同的 `data`。
- 校验失败：400 `bad_request`，`message` 指明哪个字段。

### 2.3 发送测试邮件

`POST /admin/api/email/test`

```json
{ "to": "you@example.com" }
```

始终返回 200，用 `category` 区分结果，不把几种失败混成一个错误：

```json
{
  "success": true,
  "data": {
    "ok": false,
    "category": "auth_failed",
    "detail": "服务器拒绝了认证（535），请检查用户名和口令",
    "to": "you@example.com",
    "latencyMs": 812,
    "sentAt": "2026-11-17T00:00:00Z"
  }
}
```

| `category` | 含义 |
| --- | --- |
| `ok` | 服务器接受并投递了这封信 |
| `unreachable` | DNS、TCP 连接、TLS 握手失败，或连接超时 |
| `auth_failed` | 连上了，但认证被拒绝（如 535） |
| `rejected` | 连上且认证通过，但发件人、收件人或正文被拒（如 550、554） |
| `not_configured` | 主机、发件人、端口或加密方式不全，未尝试发送 |

- `detail` 是给人看的说明，不含口令、不含完整服务器会话。
- 每次测试都写入发送记录，用途为 `test`。

## 3. 后台：邮件模板

### 3.1 读取

`GET /admin/api/email/templates`

```json
{
  "success": true,
  "data": [
    {
      "purpose": "register_verification",
      "subject": "【Agree ShellAs】邮箱验证码",
      "body": "你好 {username}：\n\n你的 ShellAs 邮箱验证码是 {code}，{minutes} 分钟内有效。",
      "placeholders": ["username", "code", "minutes"],
      "configured": true,
      "updatedAt": "2026-11-17T00:00:00Z"
    },
    {
      "purpose": "password_reset",
      "subject": "【Agree ShellAs】重置口令验证码",
      "body": "…",
      "placeholders": ["username", "code", "minutes"],
      "configured": true,
      "updatedAt": "2026-11-17T00:00:00Z"
    }
  ]
}
```

- `purpose` 固定两种：`register_verification`、`password_reset`。
- `configured = false` 表示该模板不存在或主题/正文为空；后台要显示「未配置」并给出提示。

### 3.2 保存

`PUT /admin/api/email/templates/{purpose}`

```json
{ "subject": "【Agree ShellAs】邮箱验证码", "body": "你好 {username}：…{code}…{minutes}…" }
```

- 主题和正文都不能为空。
- 占位符只允许第 8 节列出的三个；出现未知占位符、`{` 未闭合、`}` 多余时返回 400 `template_invalid`，`message` 指出是哪个占位符。
- 保存成功后立即生效；已发出的验证码不受影响。

### 3.3 预览

`POST /admin/api/email/templates/{purpose}/preview`

```json
{ "subject": "可选，不传用已保存的", "body": "可选，不传用已保存的" }
```

返回渲染后的内容，示例值固定：`username = "alice"`、`code = "123456"`、`minutes = "10"`。

```json
{
  "success": true,
  "data": {
    "subject": "【Agree ShellAs】邮箱验证码",
    "body": "你好 alice：\n\n你的 ShellAs 邮箱验证码是 123456，10 分钟内有效。",
    "variables": { "username": "alice", "code": "123456", "minutes": "10" }
  }
}
```

- 模板未配置：503 `email_template_missing`。
- 占位符写错：400 `template_invalid`。

### 3.4 发送记录

`GET /admin/api/email/sends?limit=50&offset=0`

```json
{
  "success": true,
  "data": {
    "total": 12,
    "items": [
      {
        "id": 12,
        "email": "alice@example.com",
        "purpose": "password_reset",
        "result": "ok",
        "detail": "",
        "createdAt": "2026-11-17T00:00:00Z"
      }
    ]
  }
}
```

- `purpose`：`register_verification` / `password_reset` / `test`。
- `result`：`ok` / `unreachable` / `auth_failed` / `rejected` / `not_configured`。
- 只记时间、收件邮箱、用途、结果和一句不含敏感信息的说明；**不存验证码、不存邮件正文**。
- `limit` 默认 50，最大 200；按时间倒序。

## 4. 验证码发放

`POST /api/v1/auth/email/code`

用于注册流程。不需要登录。

```json
{ "email": "alice@example.com", "username": "alice" }
```

| 字段 | 必填 | 说明 |
| --- | --- | --- |
| `email` | 是 | 合法邮箱，去掉首尾空白；不区分大小写 |
| `username` | 否 | 只用于模板里的 `{username}`；不传时用邮箱 `@` 前的部分 |

成功：200

```json
{
  "success": true,
  "expires_in": 600,
  "resend_after": 60,
  "message": "验证码已发送"
}
```

- `expires_in` 是验证码有效期（秒），`resend_after` 是同一邮箱再次请求的最短间隔（秒）。客户端据此显示倒计时。
- 只有注册开关打开时才可用；开关关闭时返回 400 `email_verification_disabled`，客户端不应显示「获取验证码」。
- 该邮箱已被注册：409 `conflict`（注册接口本来就会返回同样的冲突，这里不额外隐藏）。

失败：

| HTTP | `error` | 场景 |
| --- | --- | --- |
| 400 | `bad_request` | 邮箱格式不对 |
| 400 | `email_verification_disabled` | 后台没有开启「注册需验证邮箱」 |
| 400 | `email_template_invalid` | 模板占位符写错，未发送 |
| 409 | `conflict` | 该邮箱已被注册 |
| 429 | `too_many` | 同一邮箱 1 分钟内已发过，或 1 小时内已发满 5 封 |
| 503 | `email_not_configured` | SMTP 配置不全，未发送 |
| 503 | `email_template_missing` | 模板缺失，未发送 |
| 502 | `email_unreachable` / `email_auth_failed` / `email_rejected` | SMTP 发送失败，验证码已作废 |

发送失败时不会留下可用的验证码，用户重试即可。

## 5. 注册需验证邮箱

后台开关 `requireVerification`，默认 `false`。

### 5.1 关闭时

`POST /api/v1/auth/register` 行为与现在完全一致；请求里带 `verification_code` 会被忽略。新账号的 `email_verified` 为 `0`（没有验证就是没有验证，不伪造）。

### 5.2 打开时

注册请求必须带邮箱和验证码：

```json
{
  "username": "alice",
  "password": "correct horse",
  "email": "alice@example.com",
  "verification_code": "123456",
  "phone": "13800138000"
}
```

校验顺序：

1. 用户名、口令、邮箱、手机号格式（与现在一致）。
2. `email` 必填；缺失 → 400 `email_required`。
3. `verification_code` 必填；缺失 → 400 `email_verification_required`。
4. 验证码必须是为 `register_verification` 用途、该邮箱签发、未过期、未使用、未作废、未输错 5 次。
5. 通过后才创建账号，`email_verified = 1`；创建成功后该验证码立即作废，不能重放。

验证码失败：

| HTTP | `error` | 场景 |
| --- | --- | --- |
| 400 | `email_code_invalid` | 验证码不对 |
| 400 | `email_code_expired` | 超过 10 分钟 |
| 400 | `email_code_locked` | 已连续输错 5 次，验证码作废，需要重新获取 |

第 5 次输错时本次返回 `email_code_locked`；作废后即使再输入正确验证码也不能通过。

注册成功返回与现在相同的登录态，`user` 里新增 `email_verified`：

```json
{
  "token": "…",
  "expires_at": "2026-12-17T00:00:00Z",
  "user": {
    "username": "alice",
    "email": "alice@example.com",
    "email_verified": true,
    "phone": "13800138000",
    "pending_email": null,
    "quota_bytes": 52428800,
    "used_bytes": 0,
    "sync_salt": "…",
    "created_at": "2026-11-17T00:00:00Z"
  }
}
```

其他失败（用户名/邮箱重复、手机号重复、限流、服务端错误）与现有契约一致。

### 5.3 已有未验证账号的策略

- 开关只约束**新注册**，不会追溯停用、锁定或要求补验证已有账号。
- 迁移后已有账号 `email_verified = 0`；它们可以照常登录、同步、改口令。
- `GET /api/v1/me` 和后台用户列表新增 `email_verified` / `emailVerified`，后台显示「未验证」标签，但没有任何强制动作。
- 已有账号通过邮件验证码成功重置口令后，主邮箱被视为已验证，`email_verified` 置为 `1`。
- 本轮不提供「登录后重新发送验证邮件」的接口。

## 6. 找回密码

### 6.1 请求验证码

`POST /api/v1/auth/password/forgot`

```json
{ "email": "alice@example.com" }
```

成功：200，**无论邮箱是否存在、是否发送成功，响应都一样**：

```json
{
  "success": true,
  "expires_in": 600,
  "resend_after": 60,
  "message": "如果该邮箱已注册，验证码已发送"
}
```

- 只有邮箱命中且发送成功时才会真的发信；未命中、发送失败、被限流都返回同一个 200，不暴露邮箱是否存在。
- 唯一的例外是**全局**不可用：SMTP 配置不全时所有请求（命中与否）都返回 503 `email_not_configured`，模板缺失时统一返回 503 `email_template_missing`。这不泄露具体账号是否存在。
- 邮件发送失败会记入后台发送记录，便于管理员发现。
- 不要求用户名，只用邮箱；邮箱不区分大小写。

失败：

| HTTP | `error` | 场景 |
| --- | --- | --- |
| 400 | `bad_request` | 邮箱格式不对 |
| 429 | `too_many` | 同一 IP 请求过于频繁（与邮箱是否存在无关） |
| 503 | `email_not_configured` / `email_template_missing` | 全局邮件能力不可用 |

### 6.2 凭验证码重置

`POST /api/v1/auth/password/reset`

```json
{ "email": "alice@example.com", "code": "123456", "password": "new correct horse" }
```

- 新口令规则与注册一致（8–128 位，至少两类字符）。
- 验证码必须是为 `password_reset` 用途、该邮箱签发、未过期、未使用、未作废、未输错 5 次。
- 成功后：更新口令散列、删除该用户全部会话（含 cookie 会话）、该验证码作废、该邮箱其余未用重置码一并作废、`email_verified` 置为 `1`。

成功：200

```json
{
  "success": true,
  "message": "口令已重置，所有已登录会话已失效，请重新登录"
}
```

失败：

| HTTP | `error` | 场景 |
| --- | --- | --- |
| 400 | `bad_request` | 邮箱格式错；或新口令不合规（`message` 含规则说明） |
| 400 | `email_code_invalid` | 验证码不对，或不属于该邮箱 |
| 400 | `email_code_expired` | 超过 10 分钟 |
| 400 | `email_code_locked` | 已连续输错 5 次，验证码作废 |
| 429 | `too_many` | 同一 IP 尝试过于频繁 |

### 6.3 会话失效

重置成功后服务端删除该用户全部会话令牌，现有客户端下一次请求即 401 `unauthorized`。客户端应清除本地令牌并回到登录页。

## 7. 错误码汇总

用户接口新增：

| `error` | HTTP | 含义 |
| --- | --- | --- |
| `email_required` | 400 | 开关打开时注册没填邮箱 |
| `email_verification_required` | 400 | 开关打开时注册没填验证码 |
| `email_verification_disabled` | 400 | 开关关闭时调用了获取注册验证码 |
| `email_code_invalid` | 400 | 验证码不对 |
| `email_code_expired` | 400 | 验证码过期 |
| `email_code_locked` | 400 | 输错 5 次已作废 |
| `email_template_invalid` | 400 / 502 | 模板占位符写错，拒绝发送 |
| `email_not_configured` | 503 | SMTP 配置不全 |
| `email_template_missing` | 503 | 模板缺失或为空 |
| `email_unreachable` | 502 | 连不上 SMTP 服务器 |
| `email_auth_failed` | 502 | SMTP 认证失败 |
| `email_rejected` | 502 | SMTP 服务器拒收 |

后台接口沿用 `bad_request`、`unauthorized`、`forbidden`、`not_found`，外加 `template_invalid`（保存或预览模板时占位符写错）。

`message` 是给人看的提示，文案可以调整；客户端只用 `error` 做分支。

## 8. 模板变量

只允许三个占位符，写成 `{name}`，区分大小写：

| 占位符 | 注册验证码邮件 | 找回密码邮件 | 说明 |
| --- | --- | --- | --- |
| `{username}` | 请求里的 `username`，没传时用邮箱 `@` 前部分 | 账号用户名 | 纯文本 |
| `{code}` | 6 位数字 | 6 位数字 | 纯文本，可能有前导 0 |
| `{minutes}` | `10` | `10` | 有效期分钟数 |

- 未知占位符（如 `{user}`、`{expire}`）、未闭合的 `{`、多余的 `}` 都拒绝保存，返回 `template_invalid` 并指出位置。
- 渲染是纯文本替换，不解释 HTML；正文按纯文本邮件发送。
- 模板缺失、主题或正文为空时，发信前直接失败，不会发出半成品邮件。

## 9. 限流与防重放

| 项 | 规则 |
| --- | --- |
| 验证码长度 | 6 位数字，允许前导 0 |
| 有效期 | 10 分钟 |
| 同一邮箱发送频率 | 任意用途合计：每分钟最多 1 封，每小时最多 5 封 |
| 重发 | 新验证码签发时，同用途、同邮箱的旧验证码立即作废 |
| 输错次数 | 连续 5 次错误作废，之后即使输入正确也不通过 |
| 使用次数 | 一次性；验证成功立即标记已使用，并发提交只有一个成功 |
| 存储 | 只存 `SHA-256(purpose + email + code)` 摘要，不存明文 |
| 注册验证码接口 | 同一 IP 每小时最多 10 次（内存限流） |
| 找回请求接口 | 同一 IP 每小时最多 10 次（内存限流）；命中邮箱后受上面的邮箱频率限制，被限流时仍返回同一个 200 |
| 重置接口 | 同一 IP 每小时最多 20 次 |

- 邮箱频率限制来自数据库，重启后仍然有效。
- 发送记录只写时间、邮箱、用途、结果，绝不写验证码。

## 10. 客户端要展示的状态

### 10.1 注册页

1. 后台开关关闭：不显示验证码相关控件，请求体不带 `verification_code`，行为与现在一致。
2. 开关打开：
   - 邮箱变为必填；填完后显示「获取验证码」。
   - 点击后调用 `POST /api/v1/auth/email/code`，成功按 `resend_after` 显示倒计时（60 秒）。
   - `too_many` 时提示「发送太频繁，请稍后再试」，不要清空已输入的验证码。
   - `email_unreachable` / `email_auth_failed` / `email_rejected` 提示「验证码邮件发送失败，请稍后重试」；`email_not_configured` / `email_template_missing` 提示「邮件服务暂不可用，请联系管理员」。
   - 验证码输入 6 位数字；提交注册时带上 `verification_code`。
   - `email_code_invalid` 把焦点放到验证码输入框；`email_code_expired` 提示重新获取；`email_code_locked` 要求重新获取。
3. 注册成功后按返回的 `user.email_verified` 展示状态。

### 10.2 找回密码页

1. 输入邮箱，调用 `POST /api/v1/auth/password/forgot`。
2. 无论响应如何，都显示「如果该邮箱已注册，验证码已发送」；不要根据响应判断邮箱是否存在。
3. 第二步输入验证码和新口令，调用 `POST /api/v1/auth/password/reset`。
4. 成功后清除本地令牌、回到登录页，提示「口令已重置，请重新登录」。
5. `email_code_invalid` / `expired` / `locked` 分别提示输错、过期、重新获取。

### 10.3 已登录账号

- `GET /api/v1/me` 返回 `email_verified`；为 `false` 时只显示「未验证」提示，不影响使用。
- 后台用户列表返回 `emailVerified`，同样只做展示。

## 11. 已确认的决定

- SMTP 配置与模板都存数据库，保存后立即生效；`.env` 只在库为空时播种一次。
- 口令只进不出；后台只能看到「已配置」。
- 测试发送用四个结果分类：连不上、认证失败、被拒收、成功；配置不全单独标为 `not_configured`。
- 注册验证开关默认关闭；关闭时老客户端行为不变。
- 已有未验证账号不追溯停用，只展示状态；重置口令成功后标记为已验证。
- 找回密码请求接口对邮箱是否存在保持沉默，统一 200；只有全局邮件能力不可用时才统一报 503。
- 验证码 6 位、10 分钟、每分钟 1 封、每小时 5 封、5 次错误作废、重发作废旧码、一次性使用、只存摘要。
- 本轮不实现换绑邮箱的验证流程。
