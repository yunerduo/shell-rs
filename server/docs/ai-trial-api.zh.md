# 内置 AI · 试用 接口契约（v1）

面向已登录客户端用户的「内置 AI · 试用」。服务端保存唯一一套 OpenAI 兼容上游配置并做代理与计量；客户端只拿自己的登录态，永远接触不到上游地址与 Key。

本文件是契约。第 8 节四项已由客户端确认，按本文件实现。

## 0. 范围

- 一个上游、一个模型，全局开关 + 单用户开关。
- 按用户粗略计量：请求次数、输入/输出 token、最近使用时间。
- v1 支持非流式与 SSE 流式；`messages` / `tools` / `tool_choice` / `response_format` 原样透传。
- 不承诺：多模型、多上游、按用户计费、对话历史、提示词过滤。

## 1. 上游配置

存服务端数据库（单行配置），**运行时唯一事实来源**。后台改完下一次请求立即生效，不重启容器、不重建镜像。

| 字段 | JSON 名 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- | --- |
| 总开关 | `enabled` | bool | `false` | 关闭时所有用户请求 403 |
| 上游根地址 | `baseUrl` | string | — | 形如 `https://host/v1`，服务端拼 `/chat/completions`；只允许 `http`/`https` |
| 上游 Key | `apiKey` | string | — | 只存服务端；读取接口不回传，只回 `hasApiKey` |
| 模型 id | `model` | string | — | 用户请求里的 `model` 必须为空或等于它 |
| 最大上下文 | `maxContextTokens` | int | `131072` | 128k；输入估算 + `max_tokens` 不得超过它 |
| 输出上限 | `maxOutputTokens` | int | `4096` | 夹紧客户端传的 `max_tokens` |
| 单请求输入上限 | `maxInputTokens` | int | `122880` | 输入估算超过它直接 400 `input_too_long` |
| 温度 | `temperature` | float? | `null` | 客户端不传时用；`null` 表示用上游默认 |
| Top P | `topP` | float? | `null` | 0–1；客户端不传时用；`null` 表示不透传 |
| 同时传两个采样参数 | `samplingBoth` | bool | `false` | 关闭时两者都在只保留 `temperature` |
| 透传 include_usage | `streamIncludeUsage` | bool | `true` | 流式请求补 `stream_options.include_usage` |
| 上游超时 | `timeoutSeconds` | int | `60` | 单次上游请求的总预算（含读取响应体） |
| 连接超时 | `connectTimeoutSeconds` | int | `10` | 建立 TCP/TLS 连接的预算，必须 ≤ 整体超时 |
| 失败重试 | `retryCount` | int | `1` | 仅连接失败/5xx，且尚未向客户端写出字节时 |
| 重试退避 | `retryBackoffMs` | int | `300` | 第 n 次重试等待 `base × 2^(n-1)`，最多 10 秒 |
| 自定义请求头 | `customHeaders` | object | `{}` | 只能是非敏感头；拒绝 `Authorization`、`Cookie` 及名称含 key/token/secret 的头 |
| 每日请求限额 | `dailyRequestLimit` | int | `200` | 0 = 不限 |
| 每日 token 限额 | `dailyTokenLimit` | int | `0` | 0 = 不限；输入+输出合计 |
| 全站每日 token 上限 | `globalDailyTokenLimit` | int | `0` | 0 = 不限；所有用户当日合计 |
| 单用户并发 | `maxConcurrentPerUser` | int | `2` | 超过返回 429 |
| 请求体上限 | `maxRequestBytes` | int | `262144` | 超过返回 413 |
| 最小请求间隔 | `minIntervalSeconds` | int | `1` | 0 = 不限；超限 429 `too_frequent` |
| 间隔计数口径 | `minIntervalScope` | string | `user` | `user` 或 `email`；按邮箱时同邮箱多账号共用节流 |
| 可读上游提示 | `readableUpstreamHint` | bool | `true` | 失败时把上游一句话原因脱敏、截断后带给客户端 |
| 更新时间 | `updatedAt` | string | — | RFC 3339 |

初始播种（可选，仅当数据库没有配置时读一次）：

- `SHELLAS_AI_BASE_URL`
- `SHELLAS_AI_API_KEY`
- `SHELLAS_AI_MODEL`

播种后以数据库为准；环境变量不再参与运行期决策。**任何环境变量、日志、仓库文件都不得出现上游 Key 的明文。**

## 2. 鉴权

- 用户接口：`Authorization: Bearer <用户会话令牌>`，也接受 cookie `shellas_user_session`（与现有 `/api/v1/me` 一致）。
  - 未登录/令牌失效 → 401 `unauthorized`。
  - 用户账号被停用 → 403 `account_disabled`（沿用现有语义）。
- 后台接口：后台会话（`shellas_admin_session`）或 `SHELLAS_ADMIN_TOKEN`。
  - 读取配置与用量需要权限 `ai:read`，修改需要 `ai:write`；v1 只授予 `owner`。
  - 权限不足 → 403。

## 3. 用户接口

### 3.1 可用性探测

`GET /api/v1/ai/status`

```json
{
  "available": true,
  "reason": null,
  "model": "<后台配置的模型 id>",
  "maxContextTokens": 131072,
  "maxOutputTokens": 4096,
  "maxInputTokens": 122880,
  "dailyRequestLimit": 200,
  "dailyTokenLimit": 0,
  "requestsUsedToday": 3,
  "requestsRemainingToday": 197,
  "tokensUsedToday": 5120,
  "tokensRemainingToday": null
}
```

- `available=false` 时 `reason` 取值：`disabled`（全局关）、`user_disabled`（该用户被关）、`not_configured`（缺地址/Key/模型）、`daily_limit_reached`、`tokens_exhausted`、`global_limit_reached`。
- `dailyTokenLimit` 为 0 时 `tokensRemainingToday` 为 `null`（表示不限）；有上限时给出剩余值（不小于 0）。
- `tokensUsedToday` 始终给出当日已消耗 token（输入 + 输出，按第 5 节的真实值或估算）。
- 客户端据此决定是否展示「内置 AI · 试用」入口；`available=false` 仍返回 200。
- 不返回上游地址、Key、任何上游响应片段。

### 3.2 对话

`POST /api/v1/ai/chat/completions`

请求体是 OpenAI Chat Completions 的子集：

| 字段 | 规则 |
| --- | --- |
| `model` | 可省略；非空时必须等于后台配置的模型，否则 400 `model_not_allowed`。服务端始终用配置值覆盖上游请求里的 `model` |
| `messages` | 必填，非空数组；`content` 允许字符串或 parts 数组（文本、`image_url` 原样透传） |
| `stream` | 可选，默认 `false`；`true` 时响应 `text/event-stream` |
| `max_tokens` | 可选；夹紧到 `maxOutputTokens`，默认 `maxOutputTokens` |
| `temperature` | 可选；未传用后台配置，后台也没配则不透传；必须在 0–2，否则 400 `invalid_sampling` |
| `top_p` | 可选；未传用后台配置；必须在 0–1，否则 400 `invalid_sampling`。默认不与 `temperature` 同时发给上游（详见 5.4） |
| `stop` / `tools` / `tool_choice` / `response_format` | 原样透传 |
| `n` | 只允许 1（缺省或 `1`），其他值 400 `unsupported_parameter` |
| 其余字段 | 忽略 |

响应：

- `stream=false`：返回上游 200 的 JSON 原样（`id` / `choices` / `usage` 等不做改写）。
- `stream=true`：SSE 原样转发；`streamIncludeUsage=true` 时服务端向上游补 `stream_options: {"include_usage": true}`，若上游在最后一个 chunk 给了 `usage` 就采信；关闭则不改写，缺 `usage` 时按文本估算。
- 无论流式与否，错误体统一为 OpenAI 形状：

```json
{"error": {"message": "内置 AI 试用已关闭，请联系管理员", "type": "ai_disabled", "code": "ai_trial_disabled"}}
```

（`type` 供程序判断，`code` 供文案映射；`message` 是可直接展示的中文。）

## 4. 后台接口

响应统一沿用后台现有形状 `{"success": true, "data": ...}`，错误沿用 `{"error": "<code>", "message": "..."}`。

### 4.1 读取配置

`GET /admin/api/ai/settings` →

```json
{
  "enabled": false,
  "baseUrl": "https://api.example.com/v1",
  "model": "<后台配置的模型 id>",
  "hasApiKey": true,
  "maxContextTokens": 131072,
  "maxOutputTokens": 4096,
  "maxInputTokens": 122880,
  "temperature": null,
  "topP": null,
  "samplingBoth": false,
  "streamIncludeUsage": true,
  "timeoutSeconds": 60,
  "connectTimeoutSeconds": 10,
  "retryCount": 1,
  "retryBackoffMs": 300,
  "customHeaders": {},
  "dailyRequestLimit": 200,
  "dailyTokenLimit": 0,
  "globalDailyTokenLimit": 0,
  "maxConcurrentPerUser": 2,
  "maxRequestBytes": 262144,
  "minIntervalSeconds": 1,
  "minIntervalScope": "user",
  "readableUpstreamHint": true,
  "updatedAt": "2026-11-17T08:00:00Z"
}
```

### 4.2 修改配置

`PUT /admin/api/ai/settings`，请求体为上面的字段（`hasApiKey`/`updatedAt` 只读）。

- `apiKey` 省略或空字符串 = 不修改；非空 = 覆盖。
- `clearApiKey: true` = 清空（与 `apiKey` 同时出现时以 `clearApiKey` 为准）。
- 写入前校验：`baseUrl` 只允许 http/https 且无查询串；`maxContextTokens` ≥ 1024；`maxOutputTokens` / `maxInputTokens` 均 ≥ 1 且 ≤ `maxContextTokens`；`temperature` 0–2；`topP` 0–1；`timeoutSeconds` 5–600；`connectTimeoutSeconds` 1–60 且 ≤ `timeoutSeconds`；`retryCount` 0–3；`retryBackoffMs` 0–5000；`customHeaders` 必须是字符串到字符串的 JSON 对象，且不含敏感头；限额 ≥ 0；`maxConcurrentPerUser` 1–100；`maxRequestBytes` 4 KiB–1 MiB；`minIntervalSeconds` 0–3600；`minIntervalScope` 只能是 `user` / `email`。
- `customHeaders` 保存时就会被拒绝的典型值：`Authorization`、`Proxy-Authorization`、`Cookie`、`Set-Cookie`、`Host`、`Content-Type`、`Content-Length`、逐跳头，以及名称含 `token` / `key` / `secret` / `password` / `session` / `auth` 的头。发送前还会再过滤一次。
- 成功返回 200 + 新配置（同样不含 Key）。失败 400 `bad_request`，`message` 指出具体字段。

### 4.3 连通性自检

`POST /admin/api/ai/test` → 200 始终返回诊断结果（不抛 5xx）：

```json
{
  "ok": false,
  "category": "unreachable",
  "detail": "连接上游失败：Connection refused",
  "latencyMs": 12,
  "checkedAt": "2026-11-17T08:01:00Z"
}
```

`category` 取值：

| 值 | 含义 |
| --- | --- |
| `ok` | 上游接受配置与 Key |
| `unreachable` | DNS 失败、连接被拒、TLS 失败——**网络层问题，不是鉴权失败** |
| `timeout` | 已连上但整体请求超时 |
| `auth_failed` | 上游明确返回 401/403（Key 不对或无权限） |
| `rejected` | 上游返回其他 4xx（如模型不存在） |
| `upstream_error` | 上游 5xx |
| `not_configured` | 还没有保存上游配置，或缺地址/模型 id |

`detail` 只给可读的一句话原因，不含上游响应体、Key、完整堆栈。

### 4.4 用量列表

`GET /admin/api/ai/usage?page=1&pageSize=20&search=`

```json
{
  "total": 12,
  "items": [
    {
      "userId": 3,
      "username": "alice",
      "accountDisabled": false,
      "aiEnabled": true,
      "requestsToday": 4,
      "tokensToday": 1830,
      "totalRequests": 140,
      "totalInputTokens": 51230,
      "totalOutputTokens": 20112,
      "lastUsedAt": "2026-11-17T07:59:00Z"
    }
  ]
}
```

只含计数与时间，不含任何对话内容。

### 4.5 单用户开关

`PUT /admin/api/ai/users/{id}`，请求体 `{"enabled": false}`。

- 只影响内置 AI；不影响该用户的登录、同步等其它功能。
- 关闭后该用户请求立即返回 403 `ai_trial_disabled`。
- 用户不存在 → 404；非用户账号（如管理员）→ 400。

## 5. 限额与计量语义

- **日界**：服务器本地时区（Asia/Shanghai）每天 00:00 归零；`retryAfterSeconds` 给到下一个 00:00。
- **请求次数**：通过鉴权、开关、并发、日限额检查后、调用上游前 +1；上游失败也计入（防止免费重试）。
- **token**：
  - 上游返回 `usage.prompt_tokens` / `completion_tokens` 时用真实值；
  - 否则按字符估算：`ceil(字符数 / 4)`，流式输出累计文本同样估算；一条 `image_url` 记 1000 token；
  - 输入、输出、合计三者都记录。
- **并发**：按用户的在途请求计数（单实例内存）；超限 429 `too_concurrent`，`Retry-After: 1`。
- **输入上限**：按第 5 节的估算，`inputTokens > maxInputTokens` → 400 `input_too_long`，附 `used` / `limit`；不动用上游额度。
- **每日请求限额**：超限 429 `daily_limit_reached`，`retryAfterSeconds` + `used`/`limit`。
- **每日 token 限额**：调用前按已用判断，超限 429 `tokens_exhausted`；本次请求的 token 在完成后补记（可能略微超出，属「大概」范围）。
- **全站每日 token 上限**：按当天所有用户的 `input+output` 合计判断，超限 429 `global_tokens_exhausted`，附全站 `used`/`limit` 与 `retryAfterSeconds`。
- **最小请求间隔**：按 `minIntervalScope` 取 `user:<id>` 或 `email:<小写邮箱>` 作为节流键，键的最近一次请求时间存在 `ai_throttle` 表；间隔不足 429 `too_frequent`，`Retry-After` 给剩余秒数。
- **关闭语义**：
  - 全局关 → 403 `ai_disabled`；
  - 单用户关 → 403 `ai_trial_disabled`；
  - 都发生在调用上游之前，不产生用量。

### 采样参数与上游边界

- 客户端传了 `temperature` / `top_p` 就先用客户端的；没传才补后台配置值；后台为 `null` 时不透传。
- `temperature` 超出 0–2、`top_p` 超出 0–1、或类型不是数字 → 400 `invalid_sampling`。
- `samplingBoth=false`（默认）时，如果最终两个参数都在，只发 `temperature`、去掉 `top_p`；只有 `top_p` 时才发 `top_p`。这是为了避免部分模型同时收到两者直接 400。
- 上游仍可能因为模型自身的参数边界拒绝请求（例如只接受其中一个、或温度上限更低）：此时按第 6 节返回 `upstream_rejected`；开启 `readableUpstreamHint` 时，`message` 末尾附上游的一句话原因（已脱敏、截断到 200 字）。
- 改动这些配置不需要重启：保存后下一条请求就按新值构造 payload。

### 用户侧错误码一览

| HTTP | `code` | 场景 |
| --- | --- | --- |
| 400 | `invalid_request` | JSON 不合法、`messages` 为空等 |
| 400 | `model_not_allowed` | 客户端指定的模型与后台不一致 |
| 400 | `unsupported_parameter` | `n` 不为 1 等不支持的参数 |
| 400 | `invalid_sampling` | `temperature` / `top_p` 类型或范围不合法 |
| 400 | `input_too_long` | 输入估算超过 `maxInputTokens` |
| 400 | `context_too_long` | 输入 + 输出上限超过 `maxContextTokens` |
| 403 | `ai_disabled` / `ai_trial_disabled` | 全局 / 单用户开关关闭 |
| 413 | `request_too_large` | 请求体超过 `maxRequestBytes` |
| 429 | `too_concurrent` | 超过单用户并发 |
| 429 | `too_frequent` | 小于最小请求间隔 |
| 429 | `daily_limit_reached` | 每用户日请求次数用完 |
| 429 | `tokens_exhausted` | 每用户日 token 用完 |
| 429 | `global_tokens_exhausted` | 全站日 token 用完 |
| 502 | `upstream_auth_failed` / `upstream_rejected` / `upstream_rate_limited` | 上游拒绝 |
| 504 | `upstream_unreachable` / `upstream_timeout` | 连不上 / 超时 |
| 503 | `not_configured` | 后台还没配置好 |

## 6. 上游错误的映射与脱敏

| 上游情况 | 返回客户端 | 是否重试 |
| --- | --- | --- |
| DNS 失败 / 连接被拒 / 连接超时 / TLS 失败 | 504 `upstream_unreachable` | 是（连接失败） |
| 总超时（`timeoutSeconds`） | 504 `upstream_timeout` | 否 |
| 401 / 403 | 502 `upstream_auth_failed`（提示「服务端上游密钥无效」，不暗示用户口令问题） | 否 |
| 429 | 502 `upstream_rejected`（附 `retryAfterSeconds`，若上游给了） | 否 |
| 其他 4xx | 502 `upstream_rejected` | 否 |
| 5xx | 502 `upstream_error` | 是（`retryCount` 次，退避 `retryBackoffMs × 2^(n-1)`，最多 10 秒） |

- 超时拆成两段：`connectTimeoutSeconds` 管建连，`timeoutSeconds` 管整次请求（含读响应体）。连接超时按连接失败处理并参与重试；整体超时不重试。
- 开启 `readableUpstreamHint` 时，上游 4xx/5xx 的 `error.message`（或 `message`）会被截断到 200 字、替换掉 API Key 后附在客户端 `message` 末尾；关闭时只给通用文案。后台自检的 `detail` 走同一套脱敏规则。
- 一律**不**把上游响应体、`Authorization`、真实 `baseUrl` 回给客户端或写进日志。
- 连接失败与鉴权失败必须分开：前者 `upstream_unreachable`，后者 `upstream_auth_failed`。后台自检同样按第 4.3 节分类。
- 重试只在尚未向客户端写出任何响应字节时进行；流式一旦开始写就不再重试，改为发送一个 `data: {"error": ...}` 结束帧（OpenAI 兼容客户端能识别）。

## 7. 隐私

- 不持久化 `messages`、模型输出、工具参数。
- 日志只记：用户 id、请求 id、模型、耗时、状态码/错误码、token 估算值；不记正文、不记 Key。
- 用量表只存计数与 `last_used_at`。

## 8. 已确认的决定

1. **流式必须支持**：`stream=true` 是 v1 的必备能力，按第 3.2 节透传 SSE。
2. **错误体采用 OpenAI 形状**：`{"error": {"message", "type", "code"}}`，客户端现有解析器按此编写。
3. **默认额度**：每用户 `200 次/天`、`2 并发`，两者都能在后台修改，保存后下一次请求立即生效。
4. **`/status` 额外给出今日 token**：`tokensUsedToday` 始终返回；配置了每日 token 上限时同时返回 `dailyTokenLimit` 与 `tokensRemainingToday`。
