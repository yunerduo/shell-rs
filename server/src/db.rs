//! SQLite：用户、会话、同步快照的元数据和更新清单。
//!
//! 快照本体和安装包都在磁盘上，库里只有元数据。连接只有一个，包在
//! `Mutex` 里：这台服务的写入都是毫秒级的小事务，串行反而简单可靠；
//! 文件读写不占着这把锁。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use thiserror::Error;

use crate::util;

/// 正在升级的库版本，见 [`migrate`]。
const SCHEMA_VERSION: i64 = 11;

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    /// 注册时填的手机号，可以没有；没有短信通道，不做验证。
    pub phone: Option<String>,
    /// 邮箱是否通过验证码确认过归属。开关关闭时注册的账号为 false，
    /// 但仍然可以正常使用；重置口令成功后会置为 true。
    pub email_verified: bool,
    pub password_hash: String,
    pub quota_bytes: i64,
    pub sync_salt: String,
    pub created_at: String,
    /// 停用后不能登录、已有会话也立即失效（`session_user` 仍会查出来，
    /// 由接口层统一拒绝）。
    pub disabled: bool,
}

/// 后台用户管理列表里的一行，配额与用量一起带出来。
#[derive(Clone, Debug)]
pub struct UserRow {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    /// 手机号是否验证过。目前没有短信通道，恒为 false，留给以后。
    pub phone_verified: bool,
    /// 邮箱是否通过验证码确认过归属。
    pub email_verified: bool,
    pub quota_bytes: i64,
    /// 快照与同步文档占用的总字节数。
    pub used_bytes: i64,
    /// 快照与同步文档的总个数。
    pub versions: i64,
    pub created_at: String,
    pub disabled: bool,
}

/// 云端同步的一个文档：`settings` 是应用设置，`hosts` 是主机、分组和
/// 凭据引用（秘密本身不上云）。内容是客户端加密后的密文，服务端不解析。
#[derive(Clone, Debug, serde::Serialize)]
pub struct SyncDocument {
    pub kind: String,
    pub version: i64,
    pub size: i64,
    pub sha256: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_version: Option<i64>,
}

/// 一个已经上传的安装包。`sha256` 在后台按需计算后回填。
#[derive(Clone, Debug)]
pub struct ReleasePackage {
    #[allow(dead_code)] // 由调用方按版本查询，字段保留便于以后按包反查。
    pub version: String,
    pub file: String,
    pub platform: String,
    pub arch: String,
    pub size: i64,
    pub sha256: String,
    pub downloads: i64,
    pub uploaded_at: String,
    pub withdrawn: bool,
}

/// 一次发布记录：同一个版本可以在 stable 和 beta 各发一次。
#[derive(Clone, Debug)]
pub struct ReleaseVersion {
    pub version: String,
    pub channel: String,
    pub severity: String,
    pub notes: String,
    pub notes_url: String,
    pub minimum_version: Option<String>,
    pub rollout: f64,
    pub published_at: String,
    pub withdrawn: bool,
    pub package_count: i64,
    pub total_size: i64,
    pub downloads: i64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct Snapshot {
    pub version: i64,
    pub size: i64,
    pub sha256: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// 一条匿名统计事件，见 `api::analytics_events`。
#[derive(Clone, Debug)]
pub struct AnalyticsEvent {
    pub name: String,
    pub session_id: String,
    /// 来源地址与 User-Agent 在当天的盐下算出的摘要：原始值不落库。
    pub device: String,
    pub occurred_at: String,
    pub received_at: String,
    pub app_version: String,
    pub os_name: String,
    pub os_version: String,
    pub sdk_version: String,
    pub is_debug: bool,
    /// 事件的属性，原样存 JSON 文本。
    pub props: String,
}

/// 后台管理员。与客户端用户完全分开：管理员不参与同步，也不走注册接口。
#[derive(Clone, Debug)]
pub struct AdminUser {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
    /// `owner` / `publisher` / `viewer`，见 `admin.rs`。
    pub role: String,
    pub created_at: String,
}

/// 官网 `/changelog` 上的一条更新记录，后台可增删改。
#[derive(Clone, Debug)]
pub struct ChangelogEntry {
    pub id: i64,
    pub version: String,
    pub channel: String,
    pub severity: String,
    pub title: String,
    pub body: String,
    pub published_at: String,
    pub updated_at: String,
}

/// 一条已发布的清单，后台列表用。
#[derive(Clone, Debug)]
pub struct ManifestRow {
    pub channel: String,
    pub version: String,
    pub envelope: String,
    pub published_at: String,
    pub withdrawn: bool,
}

/// 使用记录按某个维度聚合的一行。
#[derive(Clone, Debug)]
pub struct UsageRow {
    pub key: String,
    pub device_days: i64,
    pub events: i64,
}

/// 内置 AI 的唯一一套上游配置。`api_key` 只存服务端，任何接口都不回传。
#[derive(Clone, Debug)]
pub struct AiSettings {
    pub enabled: bool,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_context_tokens: i64,
    pub max_output_tokens: i64,
    pub max_input_tokens: i64,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    /// 同时把 temperature 和 top_p 传给上游。默认只传一个：有些模型
    /// 拒绝这两个参数同时出现。
    pub sampling_both: bool,
    /// 流式请求是否向上游补 `stream_options.include_usage`。
    pub stream_include_usage: bool,
    pub timeout_seconds: i64,
    pub connect_timeout_seconds: i64,
    pub retry_count: i64,
    /// 重试的基础退避毫秒数，按 2 的幂递增。
    pub retry_backoff_ms: i64,
    /// 自定义请求头，JSON 对象文本；保存前校验，禁止敏感头。
    pub custom_headers: String,
    pub daily_request_limit: i64,
    pub daily_token_limit: i64,
    pub global_daily_token_limit: i64,
    pub max_concurrent_per_user: i64,
    pub max_request_bytes: i64,
    /// 两次请求之间的最小间隔秒数，0 表示不限。
    pub min_interval_seconds: i64,
    /// 最小间隔按 `user` 还是 `email` 计。
    pub min_interval_scope: String,
    /// 上游失败时是否把上游的一句话原因带给客户端。
    pub readable_upstream_hint: bool,
    pub updated_at: String,
}

impl AiSettings {
    /// 没有任何配置时的默认值；`enabled` 默认关闭，等管理员在后台打开。
    pub fn defaults() -> Self {
        Self {
            enabled: false,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            // 128k 上下文，输入默认留 120k，其余给输出与误差。
            max_context_tokens: 131_072,
            max_output_tokens: 4_096,
            max_input_tokens: 122_880,
            temperature: None,
            top_p: None,
            sampling_both: false,
            stream_include_usage: true,
            timeout_seconds: 60,
            connect_timeout_seconds: 10,
            retry_count: 1,
            retry_backoff_ms: 300,
            custom_headers: "{}".into(),
            daily_request_limit: 200,
            daily_token_limit: 0,
            global_daily_token_limit: 0,
            max_concurrent_per_user: 2,
            max_request_bytes: 256 * 1024,
            min_interval_seconds: 1,
            min_interval_scope: "user".into(),
            readable_upstream_hint: true,
            updated_at: String::new(),
        }
    }
}

/// 一个用户的今日用量：只有计数，没有内容。
#[derive(Clone, Debug, Default)]
pub struct AiUsage {
    pub requests_today: i64,
    pub tokens_today: i64,
}

/// 后台内置 AI 用量列表里的一行。
#[derive(Clone, Debug)]
pub struct AiUsageRow {
    pub id: i64,
    pub username: String,
    pub account_disabled: bool,
    pub ai_enabled: bool,
    pub requests_today: i64,
    pub tokens_today: i64,
    pub requests_total: i64,
    pub input_tokens_total: i64,
    pub output_tokens_total: i64,
    pub last_used_at: Option<String>,
}

/// SMTP 配置。口令只存服务端，任何接口都不回传。
#[derive(Clone, Debug)]
pub struct EmailSettings {
    pub enabled: bool,
    pub host: String,
    pub port: i64,
    /// `starttls` / `tls` / `plain`。
    pub encryption: String,
    pub username: String,
    pub password: String,
    pub from_address: String,
    pub from_name: String,
    /// 注册是否要求邮箱验证。只影响新注册，不追溯已有账号。
    pub require_verification: bool,
    pub updated_at: String,
}

impl EmailSettings {
    /// 库为空时的默认值：未配置、未启用、不要求验证。
    pub fn defaults() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: 587,
            encryption: "starttls".into(),
            username: String::new(),
            password: String::new(),
            from_address: String::new(),
            from_name: "Agree ShellAs".into(),
            require_verification: false,
            updated_at: String::new(),
        }
    }
}

/// 一套邮件模板，`purpose` 是 `register_verification` 或 `password_reset`。
#[derive(Clone, Debug)]
pub struct EmailTemplate {
    pub purpose: String,
    pub subject: String,
    pub body: String,
    pub updated_at: String,
}

/// 发送记录的一行。只留元数据，验证码和正文不在这里。
#[derive(Clone, Debug)]
pub struct EmailSend {
    pub id: i64,
    pub email: String,
    pub purpose: String,
    pub result: String,
    pub detail: String,
    pub created_at: String,
}

/// 校验验证码的结果。输错次数已在库里累加。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailCodeCheck {
    Ok,
    Invalid,
    Expired,
    Locked,
}

/// 每日限额挡住请求时的原因，接口层据此给 429 与可读文案。
#[derive(Debug, Error)]
pub enum AiQuotaError {
    #[error("今日请求次数已用完")]
    Requests { used: i64, limit: i64 },
    #[error("今日 token 已用完")]
    Tokens { used: i64, limit: i64 },
    #[error("全局 token 配额已用完")]
    GlobalTokens { used: i64, limit: i64 },
    #[error("请求太频繁")]
    Interval { retry_after: i64 },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

#[derive(Clone, Debug, Default)]
pub struct UsageTotals {
    pub events: i64,
    /// 设备按天匿名，跨天认不出同一台，所以记「设备·天」。
    pub device_days: i64,
    pub sessions: i64,
}

#[derive(Clone, Debug)]
pub struct RecentUsage {
    pub name: String,
    pub app_version: String,
    pub os_name: String,
    pub occurred_at: String,
    pub is_debug: bool,
}

#[derive(Debug, Error)]
pub enum SnapshotError {
    #[error("配额已满")]
    Quota { used: i64, quota: i64 },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

/// 同步文档的写入结果。冲突时把服务端最新的那份带回去，客户端据此合并。
#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("版本冲突")]
    Conflict { latest: Option<Box<SyncDocument>> },
    #[error("配额已满")]
    Quota { used: i64, quota: i64 },
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::prepare(conn)
    }

    /// 只在测试里用：库在内存里，不用临时目录。
    #[cfg(test)]
    fn in_memory() -> Self {
        let conn = Connection::open_in_memory().unwrap();
        Self::prepare(conn).unwrap()
    }

    fn prepare(conn: Connection) -> anyhow::Result<Self> {
        // WAL：读写互不阻塞，掉电最多丢最后一个事务。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        // 一个线程 panic 不该让整个服务再也开不了库。
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[allow(clippy::too_many_arguments)] // 建号要一次写全，调用方都是明确的字面量。
    pub fn create_user(
        &self,
        username: &str,
        email: Option<&str>,
        phone: Option<&str>,
        password_hash: &str,
        sync_salt: &str,
        quota_bytes: i64,
        email_verified: bool,
    ) -> rusqlite::Result<User> {
        let created_at = util::now();
        self.conn().execute(
            "INSERT INTO users (username, email, phone, password_hash, quota_bytes, sync_salt, created_at, email_verified)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                username,
                email,
                phone,
                password_hash,
                quota_bytes,
                sync_salt,
                created_at,
                email_verified
            ],
        )?;
        Ok(User {
            id: self.conn().last_insert_rowid(),
            username: username.to_string(),
            email: email.map(str::to_string),
            phone: phone.map(str::to_string),
            email_verified,
            password_hash: password_hash.to_string(),
            quota_bytes,
            sync_salt: sync_salt.to_string(),
            created_at,
            disabled: false,
        })
    }

    /// 手机号是否已被占用。
    pub fn phone_taken(&self, phone: &str) -> rusqlite::Result<bool> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM users WHERE phone = ?1",
            [phone],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn user_by_name(&self, username: &str) -> rusqlite::Result<Option<User>> {
        self.conn()
            .query_row(
                "SELECT id, username, email, password_hash, quota_bytes, sync_salt, created_at, disabled, phone, email_verified
                 FROM users WHERE username = ?1 COLLATE NOCASE",
                [username],
                user_row,
            )
            .optional()
    }

    /// 按邮箱查账号，不区分大小写。找回密码用。
    pub fn user_by_email(&self, email: &str) -> rusqlite::Result<Option<User>> {
        self.conn()
            .query_row(
                "SELECT id, username, email, password_hash, quota_bytes, sync_salt, created_at, disabled, phone, email_verified
                 FROM users WHERE email = ?1 COLLATE NOCASE",
                [email],
                user_row,
            )
            .optional()
    }

    pub fn user_by_id(&self, id: i64) -> rusqlite::Result<Option<User>> {
        self.conn()
            .query_row(
                "SELECT id, username, email, password_hash, quota_bytes, sync_salt, created_at, disabled, phone, email_verified
                 FROM users WHERE id = ?1",
                [id],
                user_row,
            )
            .optional()
    }

    /// 后台用户列表：带上配额、用量和版本数。
    pub fn users(&self) -> rusqlite::Result<Vec<UserRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT u.id, u.username, u.email, u.phone, u.phone_verified, u.quota_bytes,
                    u.created_at, u.disabled,
                    COALESCE((SELECT SUM(size) FROM snapshots WHERE user_id = u.id), 0)
                      + COALESCE((SELECT SUM(size) FROM sync_documents WHERE user_id = u.id), 0),
                    (SELECT COUNT(*) FROM snapshots WHERE user_id = u.id)
                      + (SELECT COUNT(*) FROM sync_documents WHERE user_id = u.id),
                    u.email_verified
             FROM users u ORDER BY u.id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(UserRow {
                id: row.get(0)?,
                username: row.get(1)?,
                email: row.get(2)?,
                phone: row.get(3)?,
                phone_verified: row.get(4)?,
                quota_bytes: row.get(5)?,
                created_at: row.get(6)?,
                disabled: row.get(7)?,
                used_bytes: row.get(8)?,
                versions: row.get(9)?,
                email_verified: row.get(10)?,
            })
        })?;
        rows.collect()
    }

    pub fn set_user_quota(&self, id: i64, quota_bytes: i64) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE users SET quota_bytes = ?2 WHERE id = ?1",
            params![id, quota_bytes],
        )?;
        Ok(())
    }

    pub fn set_user_disabled(&self, id: i64, disabled: bool) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE users SET disabled = ?2 WHERE id = ?1",
            params![id, disabled],
        )?;
        if disabled {
            // 停用后已有令牌立刻失效，不用等过期。
            self.conn()
                .execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
        }
        Ok(())
    }

    /// 改口令并让该用户所有已登录会话立即失效。
    pub fn set_user_password(&self, id: i64, password_hash: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE users SET password_hash = ?2 WHERE id = ?1",
            params![id, password_hash],
        )?;
        self.conn()
            .execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
        Ok(())
    }

    pub fn set_email_verified(&self, id: i64, verified: bool) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE users SET email_verified = ?2 WHERE id = ?1",
            params![id, verified],
        )?;
        Ok(())
    }

    // ---- 邮件配置、模板与验证码 ----

    pub fn email_settings(&self) -> rusqlite::Result<Option<EmailSettings>> {
        self.conn()
            .query_row(
                "SELECT enabled, host, port, encryption, username, password,
                        from_address, from_name, require_verification, updated_at
                 FROM email_settings WHERE id = 1",
                [],
                |row| {
                    Ok(EmailSettings {
                        enabled: row.get(0)?,
                        host: row.get(1)?,
                        port: row.get(2)?,
                        encryption: row.get(3)?,
                        username: row.get(4)?,
                        password: row.get(5)?,
                        from_address: row.get(6)?,
                        from_name: row.get(7)?,
                        require_verification: row.get(8)?,
                        updated_at: row.get(9)?,
                    })
                },
            )
            .optional()
    }

    /// 只在库为空时播种 `.env` 里的配置，已有记录时保持后台改过的值。
    pub fn seed_email_settings(&self, settings: &EmailSettings) -> rusqlite::Result<bool> {
        let inserted = self.conn().execute(
            "INSERT OR IGNORE INTO email_settings
                (id, enabled, host, port, encryption, username, password,
                 from_address, from_name, require_verification, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                settings.enabled,
                settings.host,
                settings.port,
                settings.encryption,
                settings.username,
                settings.password,
                settings.from_address,
                settings.from_name,
                settings.require_verification,
                settings.updated_at
            ],
        )?;
        Ok(inserted > 0)
    }

    pub fn save_email_settings(&self, settings: &EmailSettings) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO email_settings
                (id, enabled, host, port, encryption, username, password,
                 from_address, from_name, require_verification, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                enabled = excluded.enabled,
                host = excluded.host,
                port = excluded.port,
                encryption = excluded.encryption,
                username = excluded.username,
                password = excluded.password,
                from_address = excluded.from_address,
                from_name = excluded.from_name,
                require_verification = excluded.require_verification,
                updated_at = excluded.updated_at",
            params![
                settings.enabled,
                settings.host,
                settings.port,
                settings.encryption,
                settings.username,
                settings.password,
                settings.from_address,
                settings.from_name,
                settings.require_verification,
                settings.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn email_template(&self, purpose: &str) -> rusqlite::Result<Option<EmailTemplate>> {
        self.conn()
            .query_row(
                "SELECT purpose, subject, body, updated_at FROM email_templates WHERE purpose = ?1",
                [purpose],
                |row| {
                    Ok(EmailTemplate {
                        purpose: row.get(0)?,
                        subject: row.get(1)?,
                        body: row.get(2)?,
                        updated_at: row.get(3)?,
                    })
                },
            )
            .optional()
    }

    pub fn save_email_template(
        &self,
        purpose: &str,
        subject: &str,
        body: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO email_templates (purpose, subject, body, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(purpose) DO UPDATE SET
                subject = excluded.subject,
                body = excluded.body,
                updated_at = excluded.updated_at",
            params![purpose, subject, body, util::now()],
        )?;
        Ok(())
    }

    pub fn insert_email_send(
        &self,
        email: &str,
        purpose: &str,
        result: &str,
        detail: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO email_sends (email, purpose, result, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![email, purpose, result, detail, util::now()],
        )?;
        Ok(())
    }

    pub fn email_sends(&self, limit: i64, offset: i64) -> rusqlite::Result<(i64, Vec<EmailSend>)> {
        let conn = self.conn();
        let total: i64 =
            conn.query_row("SELECT COUNT(*) FROM email_sends", [], |row| row.get(0))?;
        let mut statement = conn.prepare(
            "SELECT id, email, purpose, result, detail, created_at
             FROM email_sends ORDER BY id DESC LIMIT ?1 OFFSET ?2",
        )?;
        let rows = statement.query_map(params![limit, offset], |row| {
            Ok(EmailSend {
                id: row.get(0)?,
                email: row.get(1)?,
                purpose: row.get(2)?,
                result: row.get(3)?,
                detail: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok((total, rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    /// 该邮箱最近一次签发验证码的时间，用于每分钟 1 封的判断。
    pub fn last_email_code_at(&self, email: &str) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT created_at FROM email_codes
                 WHERE email = ?1 ORDER BY id DESC LIMIT 1",
                [email],
                |row| row.get(0),
            )
            .optional()
    }

    /// 该邮箱在 `since` 之后签发过多少封验证码，用于每小时 5 封的判断。
    pub fn email_code_count_since(&self, email: &str, since: &str) -> rusqlite::Result<i64> {
        self.conn().query_row(
            "SELECT COUNT(*) FROM email_codes WHERE email = ?1 AND created_at >= ?2",
            params![email, since],
            |row| row.get(0),
        )
    }

    /// 作废旧码并写入新码。调用方已经算好摘要与过期时间。
    pub fn issue_email_code(
        &self,
        purpose: &str,
        email: &str,
        code_hash: &str,
        expires_at: &str,
        max_attempts: i64,
    ) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE email_codes SET invalidated_at = ?3
             WHERE purpose = ?1 AND email = ?2
               AND consumed_at IS NULL AND invalidated_at IS NULL",
            params![purpose, email, util::now()],
        )?;
        tx.execute(
            "INSERT INTO email_codes
                (purpose, email, code_hash, attempts, max_attempts, created_at, expires_at)
             VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6)",
            params![
                purpose,
                email,
                code_hash,
                max_attempts,
                util::now(),
                expires_at
            ],
        )?;
        tx.commit()
    }

    /// 校验并消费验证码。整个判断和写入在一个事务里，并发提交只有一个成功。
    pub fn check_email_code(
        &self,
        purpose: &str,
        email: &str,
        code_hash: &str,
    ) -> rusqlite::Result<EmailCodeCheck> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let found = tx
            .query_row(
                "SELECT id, code_hash, attempts, max_attempts, expires_at, consumed_at, invalidated_at
                 FROM email_codes WHERE purpose = ?1 AND email = ?2
                 ORDER BY id DESC LIMIT 1",
                params![purpose, email],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, Option<String>>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, stored, attempts, max_attempts, expires_at, consumed_at, invalidated_at)) =
            found
        else {
            return Ok(EmailCodeCheck::Invalid);
        };
        if consumed_at.is_some() || invalidated_at.is_some() {
            return Ok(if attempts >= max_attempts {
                EmailCodeCheck::Locked
            } else {
                EmailCodeCheck::Invalid
            });
        }
        if util::is_past(&expires_at) {
            tx.execute(
                "UPDATE email_codes SET invalidated_at = ?2 WHERE id = ?1",
                params![id, util::now()],
            )?;
            tx.commit()?;
            return Ok(EmailCodeCheck::Expired);
        }
        if attempts >= max_attempts {
            tx.execute(
                "UPDATE email_codes SET invalidated_at = ?2 WHERE id = ?1",
                params![id, util::now()],
            )?;
            tx.commit()?;
            return Ok(EmailCodeCheck::Locked);
        }
        if stored != code_hash {
            let attempts = attempts + 1;
            if attempts >= max_attempts {
                tx.execute(
                    "UPDATE email_codes SET attempts = ?2, invalidated_at = ?3 WHERE id = ?1",
                    params![id, attempts, util::now()],
                )?;
                tx.commit()?;
                return Ok(EmailCodeCheck::Locked);
            }
            tx.execute(
                "UPDATE email_codes SET attempts = ?2 WHERE id = ?1",
                params![id, attempts],
            )?;
            tx.commit()?;
            return Ok(EmailCodeCheck::Invalid);
        }
        tx.execute(
            "UPDATE email_codes SET consumed_at = ?2 WHERE id = ?1",
            params![id, util::now()],
        )?;
        tx.commit()?;
        Ok(EmailCodeCheck::Ok)
    }

    pub fn invalidate_email_codes(&self, purpose: &str, email: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE email_codes SET invalidated_at = ?3
             WHERE purpose = ?1 AND email = ?2
               AND consumed_at IS NULL AND invalidated_at IS NULL",
            params![purpose, email, util::now()],
        )?;
        Ok(())
    }

    /// 该用户待验证的新邮箱；没有则为 `None`。
    pub fn pending_email(&self, user_id: i64) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT email FROM user_email_changes WHERE user_id = ?1",
                [user_id],
                |row| row.get(0),
            )
            .optional()
    }

    /// 记下待验证的新邮箱，同一用户只保留最新的一条。
    pub fn set_pending_email(&self, user_id: i64, email: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO user_email_changes (user_id, email, created_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET
               email = excluded.email,
               created_at = excluded.created_at",
            params![user_id, email, util::now()],
        )?;
        Ok(())
    }

    /// 邮箱是否已被别的用户占用（不区分大小写）。
    pub fn email_taken_by_other(&self, user_id: i64, email: &str) -> rusqlite::Result<bool> {
        let count: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM users
             WHERE email = ?1 COLLATE NOCASE AND id != ?2",
            params![email, user_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// 删除用户；快照和文档的元数据由外键级联删掉，文件由调用方清理。
    pub fn delete_user(&self, id: i64) -> rusqlite::Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM users WHERE id = ?1", [id])?
            > 0)
    }

    /// 令牌摘要对应的用户；顺手记一次使用时间，过期会话当场删掉。
    pub fn session_user(&self, token_hash: &str) -> rusqlite::Result<Option<User>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                "SELECT u.id, u.username, u.email, u.password_hash, u.quota_bytes,
                        u.sync_salt, u.created_at, u.disabled, u.phone, u.email_verified,
                        s.expires_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ?1",
                [token_hash],
                |row| {
                    let expires_at: String = row.get(10)?;
                    Ok((user_row(row)?, expires_at))
                },
            )
            .optional()?;
        let Some((user, expires_at)) = found else {
            return Ok(None);
        };
        if util::is_past(&expires_at) {
            conn.execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
            return Ok(None);
        }
        conn.execute(
            "UPDATE sessions SET last_used_at = ?2 WHERE token_hash = ?1",
            params![token_hash, util::now()],
        )?;
        Ok(Some(user))
    }

    pub fn create_session(
        &self,
        user_id: i64,
        token_hash: &str,
        expires_at: &str,
        device: Option<&str>,
    ) -> rusqlite::Result<()> {
        let now = util::now();
        self.conn().execute(
            "INSERT INTO sessions (token_hash, user_id, created_at, last_used_at, expires_at, device)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
            params![token_hash, user_id, now, expires_at, device],
        )?;
        Ok(())
    }

    pub fn delete_session(&self, token_hash: &str) -> rusqlite::Result<()> {
        self.conn()
            .execute("DELETE FROM sessions WHERE token_hash = ?1", [token_hash])?;
        Ok(())
    }

    pub fn snapshots(&self, user_id: i64) -> rusqlite::Result<Vec<Snapshot>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT version, size, sha256, created_at, device FROM snapshots
             WHERE user_id = ?1 ORDER BY version DESC",
        )?;
        let rows = statement.query_map([user_id], |row| {
            Ok(Snapshot {
                version: row.get(0)?,
                size: row.get(1)?,
                sha256: row.get(2)?,
                created_at: row.get(3)?,
                device: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    pub fn snapshot(&self, user_id: i64, version: i64) -> rusqlite::Result<Option<Snapshot>> {
        self.conn()
            .query_row(
                "SELECT version, size, sha256, created_at, device FROM snapshots
                 WHERE user_id = ?1 AND version = ?2",
                params![user_id, version],
                |row| {
                    Ok(Snapshot {
                        version: row.get(0)?,
                        size: row.get(1)?,
                        sha256: row.get(2)?,
                        created_at: row.get(3)?,
                        device: row.get(4)?,
                    })
                },
            )
            .optional()
    }

    /// 快照与同步文档一共占了多少字节。
    pub fn used_bytes(&self, user_id: i64) -> rusqlite::Result<i64> {
        self.conn().query_row(
            "SELECT COALESCE((SELECT SUM(size) FROM snapshots WHERE user_id = ?1), 0)
                  + COALESCE((SELECT SUM(size) FROM sync_documents WHERE user_id = ?1), 0)",
            [user_id],
            |row| row.get(0),
        )
    }

    /// 记一份新快照：版本号按用户递增，超过配额的直接拒绝，只保留最新
    /// 的 `keep` 份。返回新快照与被挤掉的旧版本，旧文件由调用方删。
    pub fn add_snapshot(
        &self,
        user_id: i64,
        size: i64,
        sha256: &str,
        device: Option<&str>,
        quota: i64,
        keep: i64,
    ) -> Result<(Snapshot, Vec<i64>), SnapshotError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let used: i64 = tx.query_row(
            "SELECT COALESCE((SELECT SUM(size) FROM snapshots WHERE user_id = ?1), 0)
                  + COALESCE((SELECT SUM(size) FROM sync_documents WHERE user_id = ?1), 0)",
            [user_id],
            |row| row.get(0),
        )?;
        if used + size > quota {
            return Err(SnapshotError::Quota { used, quota });
        }
        let version: i64 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) + 1 FROM snapshots WHERE user_id = ?1",
            [user_id],
            |row| row.get(0),
        )?;
        let created_at = util::now();
        tx.execute(
            "INSERT INTO snapshots (user_id, version, size, sha256, created_at, device)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![user_id, version, size, sha256, created_at, device],
        )?;
        let evicted: Vec<i64> = {
            let mut statement = tx.prepare(
                "SELECT version FROM snapshots WHERE user_id = ?1
                 ORDER BY version DESC LIMIT -1 OFFSET ?2",
            )?;
            let rows = statement.query_map(params![user_id, keep.max(1)], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<i64>>>()?
        };
        for version in &evicted {
            tx.execute(
                "DELETE FROM snapshots WHERE user_id = ?1 AND version = ?2",
                params![user_id, version],
            )?;
        }
        tx.commit()?;
        Ok((
            Snapshot {
                version,
                size,
                sha256: sha256.to_string(),
                created_at,
                device: device.map(str::to_string),
            },
            evicted,
        ))
    }

    pub fn delete_snapshot(
        &self,
        user_id: i64,
        version: i64,
    ) -> rusqlite::Result<Option<Snapshot>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let snapshot = tx
            .query_row(
                "SELECT version, size, sha256, created_at, device FROM snapshots
                 WHERE user_id = ?1 AND version = ?2",
                params![user_id, version],
                |row| {
                    Ok(Snapshot {
                        version: row.get(0)?,
                        size: row.get(1)?,
                        sha256: row.get(2)?,
                        created_at: row.get(3)?,
                        device: row.get(4)?,
                    })
                },
            )
            .optional()?;
        if snapshot.is_some() {
            tx.execute(
                "DELETE FROM snapshots WHERE user_id = ?1 AND version = ?2",
                params![user_id, version],
            )?;
        }
        tx.commit()?;
        Ok(snapshot)
    }

    // ---- 同步文档 ----

    pub fn documents(&self, user_id: i64) -> rusqlite::Result<Vec<SyncDocument>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT kind, version, size, sha256, created_at, device, base_version
             FROM sync_documents WHERE user_id = ?1
             ORDER BY kind, version DESC",
        )?;
        let rows = statement.query_map([user_id], document_row)?;
        rows.collect()
    }

    pub fn document(
        &self,
        user_id: i64,
        kind: &str,
        version: i64,
    ) -> rusqlite::Result<Option<SyncDocument>> {
        self.conn()
            .query_row(
                "SELECT kind, version, size, sha256, created_at, device, base_version
                 FROM sync_documents WHERE user_id = ?1 AND kind = ?2 AND version = ?3",
                params![user_id, kind, version],
                document_row,
            )
            .optional()
    }

    /// 记一份新的同步文档。`base_version` 是客户端基于哪一版改的：
    /// 服务端最新版变了就返回冲突，让客户端先合并再传。版本号按
    /// `(用户, 文档)` 递增，只保留最新的 `keep` 份。
    #[allow(clippy::too_many_arguments)] // 参数都是这次写入直接需要的，拆结构体反而绕。
    pub fn add_document(
        &self,
        user_id: i64,
        kind: &str,
        size: i64,
        sha256: &str,
        base_version: Option<i64>,
        device: Option<&str>,
        quota: i64,
        keep: i64,
    ) -> Result<(SyncDocument, Vec<i64>), DocumentError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let latest = tx
            .query_row(
                "SELECT kind, version, size, sha256, created_at, device, base_version
                 FROM sync_documents WHERE user_id = ?1 AND kind = ?2
                 ORDER BY version DESC LIMIT 1",
                params![user_id, kind],
                document_row,
            )
            .optional()?;
        let base_ok = match (&latest, base_version) {
            (None, None) => true,
            (Some(latest), Some(base)) => latest.version == base,
            _ => false,
        };
        if !base_ok {
            return Err(DocumentError::Conflict {
                latest: latest.map(Box::new),
            });
        }
        let used: i64 = tx.query_row(
            "SELECT COALESCE((SELECT SUM(size) FROM snapshots WHERE user_id = ?1), 0)
                  + COALESCE((SELECT SUM(size) FROM sync_documents WHERE user_id = ?1), 0)",
            [user_id],
            |row| row.get(0),
        )?;
        if used + size > quota {
            return Err(DocumentError::Quota { used, quota });
        }
        let version = latest
            .as_ref()
            .map(|latest| latest.version + 1)
            .unwrap_or(1);
        let created_at = util::now();
        tx.execute(
            "INSERT INTO sync_documents
               (user_id, kind, version, size, sha256, base_version, created_at, device)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                user_id,
                kind,
                version,
                size,
                sha256,
                base_version,
                created_at,
                device
            ],
        )?;
        let evicted: Vec<i64> = {
            let mut statement = tx.prepare(
                "SELECT version FROM sync_documents WHERE user_id = ?1 AND kind = ?2
                 ORDER BY version DESC LIMIT -1 OFFSET ?3",
            )?;
            let rows =
                statement.query_map(params![user_id, kind, keep.max(1)], |row| row.get(0))?;
            rows.collect::<rusqlite::Result<Vec<i64>>>()?
        };
        for old in &evicted {
            tx.execute(
                "DELETE FROM sync_documents WHERE user_id = ?1 AND kind = ?2 AND version = ?3",
                params![user_id, kind, old],
            )?;
        }
        tx.commit()?;
        Ok((
            SyncDocument {
                kind: kind.to_string(),
                version,
                size,
                sha256: sha256.to_string(),
                created_at,
                device: device.map(str::to_string),
                base_version,
            },
            evicted,
        ))
    }

    pub fn delete_document(
        &self,
        user_id: i64,
        kind: &str,
        version: i64,
    ) -> rusqlite::Result<Option<SyncDocument>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let found = tx
            .query_row(
                "SELECT kind, version, size, sha256, created_at, device, base_version
                 FROM sync_documents WHERE user_id = ?1 AND kind = ?2 AND version = ?3",
                params![user_id, kind, version],
                document_row,
            )
            .optional()?;
        if found.is_some() {
            tx.execute(
                "DELETE FROM sync_documents WHERE user_id = ?1 AND kind = ?2 AND version = ?3",
                params![user_id, kind, version],
            )?;
        }
        tx.commit()?;
        Ok(found)
    }

    // ---- 登录失败锁定 ----
    //
    // 用户与管理员各用各的 scope（`user` / `admin`），两种登录互不影响：
    // 拿用户入口刷口令只会锁住用户名，不会牵连后台账号。

    /// 还在锁定中时返回解锁时间。
    pub fn login_locked_at(&self, scope: &str, name: &str) -> rusqlite::Result<Option<String>> {
        let locked_until: Option<String> = self
            .conn()
            .query_row(
                "SELECT locked_until FROM login_lockouts WHERE scope = ?1 AND name = ?2",
                params![scope, name],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(locked_until.filter(|until| !util::is_past(until)))
    }

    /// 记一次失败；达到 `max_failures` 就锁定 `lock_minutes` 分钟，
    /// 返回解锁时间。
    pub fn record_login_failure(
        &self,
        scope: &str,
        name: &str,
        max_failures: i64,
        lock_minutes: i64,
    ) -> rusqlite::Result<String> {
        let conn = self.conn();
        let now = util::now();
        conn.execute(
            "INSERT INTO login_lockouts (scope, name, failures, locked_until, updated_at)
             VALUES (?1, ?2, 1, NULL, ?3)
             ON CONFLICT(scope, name) DO UPDATE SET
               failures = CASE
                 WHEN login_lockouts.locked_until IS NOT NULL
                   AND login_lockouts.locked_until > ?3
                 THEN login_lockouts.failures
                 ELSE login_lockouts.failures + 1
               END,
               updated_at = excluded.updated_at",
            params![scope, name, now],
        )?;
        let failures: i64 = conn.query_row(
            "SELECT failures FROM login_lockouts WHERE scope = ?1 AND name = ?2",
            params![scope, name],
            |row| row.get(0),
        )?;
        let locked_until = if failures >= max_failures {
            let until = util::later_minutes(lock_minutes);
            conn.execute(
                "UPDATE login_lockouts SET locked_until = ?3, failures = 0
                 WHERE scope = ?1 AND name = ?2",
                params![scope, name, until],
            )?;
            until
        } else {
            String::new()
        };
        // 顺手清掉一个月没动过的记录，免得被任意用户名撑大。
        let stale = util::days_ago(30);
        let _ = conn.execute("DELETE FROM login_lockouts WHERE updated_at < ?1", [stale]);
        Ok(locked_until)
    }

    pub fn clear_login_failures(&self, scope: &str, name: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "DELETE FROM login_lockouts WHERE scope = ?1 AND name = ?2",
            params![scope, name],
        )?;
        Ok(())
    }

    /// 存一个通道的签名清单（信封：清单原文 + minisign 签名）。
    pub fn put_manifest(
        &self,
        channel: &str,
        version: &str,
        envelope: &str,
        published_at: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO manifests (channel, version, envelope, published_at, withdrawn)
             VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(channel, version) DO UPDATE SET
               envelope = excluded.envelope,
               published_at = excluded.published_at,
               withdrawn = 0",
            params![channel, version, envelope, published_at],
        )?;
        Ok(())
    }

    /// 一个通道当前的清单信封，最后发布的那个。
    pub fn current_manifest(&self, channel: &str) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT envelope FROM manifests WHERE channel = ?1 AND withdrawn = 0
                 ORDER BY published_at DESC, rowid DESC LIMIT 1",
                [channel],
                |row| row.get(0),
            )
            .optional()
    }

    /// 当天给设备摘要用的盐：第一次用时生成，第二天换一个，于是同一个
    /// 地址跨天就认不回同一台设备。
    pub fn analytics_salt(&self, day: &str) -> rusqlite::Result<String> {
        let conn = self.conn();
        let existing = conn
            .query_row(
                "SELECT salt FROM analytics_salts WHERE day = ?1",
                [day],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(salt) = existing {
            return Ok(salt);
        }
        let salt = util::random_salt();
        conn.execute(
            "INSERT INTO analytics_salts (day, salt) VALUES (?1, ?2)",
            params![day, salt],
        )?;
        Ok(salt)
    }

    /// 一批统计事件一个事务写进去。
    pub fn insert_analytics_events(&self, events: &[AnalyticsEvent]) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut statement = tx.prepare(
                "INSERT INTO analytics_events
                   (name, session_id, device, occurred_at, received_at, app_version,
                    os_name, os_version, sdk_version, is_debug, props)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for event in events {
                statement.execute(params![
                    event.name,
                    event.session_id,
                    event.device,
                    event.occurred_at,
                    event.received_at,
                    event.app_version,
                    event.os_name,
                    event.os_version,
                    event.sdk_version,
                    event.is_debug,
                    event.props,
                ])?;
            }
        }
        tx.commit()
    }

    // ---- 后台管理员 ----

    pub fn admin_count(&self) -> rusqlite::Result<i64> {
        self.conn()
            .query_row("SELECT COUNT(*) FROM admin_users", [], |row| row.get(0))
    }

    pub fn create_admin(
        &self,
        username: &str,
        password_hash: &str,
        role: &str,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO admin_users (username, password_hash, role, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![username, password_hash, role, util::now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn admin_by_name(&self, username: &str) -> rusqlite::Result<Option<AdminUser>> {
        self.conn()
            .query_row(
                "SELECT id, username, password_hash, role, created_at
                 FROM admin_users WHERE username = ?1 COLLATE NOCASE",
                [username],
                admin_row,
            )
            .optional()
    }

    pub fn admin_by_id(&self, id: i64) -> rusqlite::Result<Option<AdminUser>> {
        self.conn()
            .query_row(
                "SELECT id, username, password_hash, role, created_at
                 FROM admin_users WHERE id = ?1",
                [id],
                admin_row,
            )
            .optional()
    }

    pub fn admins(&self) -> rusqlite::Result<Vec<AdminUser>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, username, password_hash, role, created_at
             FROM admin_users ORDER BY id",
        )?;
        let rows = statement.query_map([], admin_row)?;
        rows.collect()
    }

    pub fn owner_count(&self) -> rusqlite::Result<i64> {
        self.conn().query_row(
            "SELECT COUNT(*) FROM admin_users WHERE role = 'owner'",
            [],
            |row| row.get(0),
        )
    }

    pub fn update_admin_role(&self, id: i64, role: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE admin_users SET role = ?2 WHERE id = ?1",
            params![id, role],
        )?;
        Ok(())
    }

    pub fn delete_admin(&self, id: i64) -> rusqlite::Result<bool> {
        let removed = self
            .conn()
            .execute("DELETE FROM admin_users WHERE id = ?1", [id])?;
        Ok(removed > 0)
    }

    pub fn create_admin_session(
        &self,
        admin_id: i64,
        token_hash: &str,
        expires_at: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO admin_sessions (token_hash, admin_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![token_hash, admin_id, util::now(), expires_at],
        )?;
        Ok(())
    }

    /// 续期一个仍然有效的会话，返回是否续上。
    pub fn refresh_admin_session(
        &self,
        token_hash: &str,
        expires_at: &str,
    ) -> rusqlite::Result<bool> {
        let updated = self.conn().execute(
            "UPDATE admin_sessions SET expires_at = ?2
             WHERE token_hash = ?1 AND expires_at > ?3",
            params![token_hash, expires_at, util::now()],
        )?;
        Ok(updated > 0)
    }

    /// 令牌摘要对应的管理员；过期会话当场删掉。
    pub fn admin_session(&self, token_hash: &str) -> rusqlite::Result<Option<AdminUser>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                "SELECT a.id, a.username, a.password_hash, a.role, a.created_at, s.expires_at
                 FROM admin_sessions s JOIN admin_users a ON a.id = s.admin_id
                 WHERE s.token_hash = ?1",
                [token_hash],
                |row| {
                    let expires_at: String = row.get(5)?;
                    Ok((admin_row(row)?, expires_at))
                },
            )
            .optional()?;
        let Some((admin, expires_at)) = found else {
            return Ok(None);
        };
        if util::is_past(&expires_at) {
            conn.execute(
                "DELETE FROM admin_sessions WHERE token_hash = ?1",
                [token_hash],
            )?;
            return Ok(None);
        }
        Ok(Some(admin))
    }

    pub fn delete_admin_session(&self, token_hash: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "DELETE FROM admin_sessions WHERE token_hash = ?1",
            [token_hash],
        )?;
        Ok(())
    }

    pub fn delete_admin_sessions(&self, admin_id: i64) -> rusqlite::Result<()> {
        self.conn()
            .execute("DELETE FROM admin_sessions WHERE admin_id = ?1", [admin_id])?;
        Ok(())
    }

    pub fn set_admin_password(&self, admin_id: i64, password_hash: &str) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE admin_users SET password_hash = ?2 WHERE id = ?1",
            params![admin_id, password_hash],
        )?;
        Ok(())
    }

    // ---- 更新记录 ----

    pub fn manifest_rows(&self, limit: i64) -> rusqlite::Result<Vec<ManifestRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT channel, version, envelope, published_at, withdrawn FROM manifests
             ORDER BY published_at DESC, rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(ManifestRow {
                channel: row.get(0)?,
                version: row.get(1)?,
                envelope: row.get(2)?,
                published_at: row.get(3)?,
                withdrawn: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    // ---- 安装包与发布记录 ----

    /// 登记一个安装包。`sha256` 为空表示还没算，保留库里已有的摘要。
    pub fn upsert_package(
        &self,
        version: &str,
        file: &str,
        platform: &str,
        arch: &str,
        size: i64,
        sha256: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO release_packages
               (version, file, platform, arch, size, sha256, downloads, uploaded_at, withdrawn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, 0)
             ON CONFLICT(version, file) DO UPDATE SET
               platform = excluded.platform,
               arch = excluded.arch,
               size = excluded.size,
               sha256 = CASE WHEN excluded.sha256 <> ''
                             THEN excluded.sha256
                             ELSE release_packages.sha256 END,
               uploaded_at = excluded.uploaded_at,
               withdrawn = 0",
            params![version, file, platform, arch, size, sha256, util::now()],
        )?;
        Ok(())
    }

    pub fn package(&self, version: &str, file: &str) -> rusqlite::Result<Option<ReleasePackage>> {
        self.conn()
            .query_row(
                "SELECT version, file, platform, arch, size, sha256, downloads,
                        uploaded_at, withdrawn
                 FROM release_packages WHERE version = ?1 AND file = ?2",
                params![version, file],
                package_row,
            )
            .optional()
    }

    pub fn packages(&self, version: &str) -> rusqlite::Result<Vec<ReleasePackage>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT version, file, platform, arch, size, sha256, downloads,
                    uploaded_at, withdrawn
             FROM release_packages WHERE version = ?1 ORDER BY file",
        )?;
        let rows = statement.query_map([version], package_row)?;
        rows.collect()
    }

    /// 下载次数加一；返回是否确实有过这条记录。
    pub fn increment_download(&self, version: &str, file: &str) -> rusqlite::Result<bool> {
        let updated = self.conn().execute(
            "UPDATE release_packages SET downloads = downloads + 1
             WHERE version = ?1 AND file = ?2",
            params![version, file],
        )?;
        Ok(updated > 0)
    }

    pub fn set_packages_withdrawn(&self, version: &str, withdrawn: bool) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE release_packages SET withdrawn = ?2 WHERE version = ?1",
            params![version, withdrawn],
        )?;
        Ok(())
    }

    pub fn set_manifest_withdrawn(
        &self,
        channel: &str,
        version: &str,
        withdrawn: bool,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE manifests SET withdrawn = ?3 WHERE channel = ?1 AND version = ?2",
            params![channel, version, withdrawn],
        )?;
        Ok(())
    }

    /// 记一次发布（同一版本可以在不同通道各记一条），发布即恢复为可用。
    #[allow(clippy::too_many_arguments)]
    pub fn put_release_version(
        &self,
        version: &str,
        channel: &str,
        severity: &str,
        notes: &str,
        notes_url: &str,
        minimum_version: Option<&str>,
        rollout: f64,
        published_at: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO release_versions
               (version, channel, severity, notes, notes_url, minimum_version,
                rollout, published_at, withdrawn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
             ON CONFLICT(version, channel) DO UPDATE SET
               severity = excluded.severity,
               notes = excluded.notes,
               notes_url = excluded.notes_url,
               minimum_version = excluded.minimum_version,
               rollout = excluded.rollout,
               published_at = excluded.published_at,
               withdrawn = 0",
            params![
                version,
                channel,
                severity,
                notes,
                notes_url,
                minimum_version,
                rollout,
                published_at
            ],
        )?;
        Ok(())
    }

    pub fn release_version(
        &self,
        version: &str,
        channel: &str,
    ) -> rusqlite::Result<Option<ReleaseVersion>> {
        self.conn()
            .query_row(
                "SELECT rv.version, rv.channel, rv.severity, rv.notes, rv.notes_url,
                        rv.minimum_version, rv.rollout, rv.published_at, rv.withdrawn,
                        (SELECT COUNT(*) FROM release_packages p WHERE p.version = rv.version),
                        (SELECT COALESCE(SUM(size), 0) FROM release_packages p
                          WHERE p.version = rv.version),
                        (SELECT COALESCE(SUM(downloads), 0) FROM release_packages p
                          WHERE p.version = rv.version)
                 FROM release_versions rv
                 WHERE rv.version = ?1 AND rv.channel = ?2",
                params![version, channel],
                release_version_row,
            )
            .optional()
    }

    pub fn release_versions(&self, limit: i64) -> rusqlite::Result<Vec<ReleaseVersion>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT rv.version, rv.channel, rv.severity, rv.notes, rv.notes_url,
                    rv.minimum_version, rv.rollout, rv.published_at, rv.withdrawn,
                    (SELECT COUNT(*) FROM release_packages p WHERE p.version = rv.version),
                    (SELECT COALESCE(SUM(size), 0) FROM release_packages p
                      WHERE p.version = rv.version),
                    (SELECT COALESCE(SUM(downloads), 0) FROM release_packages p
                      WHERE p.version = rv.version)
             FROM release_versions rv
             ORDER BY rv.published_at DESC, rv.rowid DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], release_version_row)?;
        rows.collect()
    }

    pub fn set_release_withdrawn(
        &self,
        version: &str,
        channel: &str,
        withdrawn: bool,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE release_versions SET withdrawn = ?3
             WHERE version = ?1 AND channel = ?2",
            params![version, channel, withdrawn],
        )?;
        Ok(())
    }

    /// 这个版本还有几个通道在发（用来决定安装包是否跟着一起撤下）。
    pub fn active_release_channels(&self, version: &str) -> rusqlite::Result<i64> {
        self.conn().query_row(
            "SELECT COUNT(*) FROM release_versions
             WHERE version = ?1 AND withdrawn = 0",
            [version],
            |row| row.get(0),
        )
    }

    pub fn changelog_entries(&self) -> rusqlite::Result<Vec<ChangelogEntry>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, version, channel, severity, title, body, published_at, updated_at
             FROM changelog ORDER BY published_at DESC, id DESC",
        )?;
        let rows = statement.query_map([], changelog_row)?;
        rows.collect()
    }

    pub fn changelog_entry(&self, id: i64) -> rusqlite::Result<Option<ChangelogEntry>> {
        self.conn()
            .query_row(
                "SELECT id, version, channel, severity, title, body, published_at, updated_at
                 FROM changelog WHERE id = ?1",
                [id],
                changelog_row,
            )
            .optional()
    }

    /// 发布时写入更新记录：已有版本保留后台编辑过的正文，只同步通道与分级。
    pub fn changelog_from_publish(
        &self,
        version: &str,
        channel: &str,
        severity: &str,
        body: &str,
        published_at: &str,
    ) -> rusqlite::Result<()> {
        let now = util::now();
        self.conn().execute(
            "INSERT INTO changelog (version, channel, severity, title, body, published_at, updated_at)
             VALUES (?1, ?2, ?3, '', ?4, ?5, ?6)
             ON CONFLICT(version) DO UPDATE SET
               channel = excluded.channel,
               severity = excluded.severity,
               published_at = excluded.published_at,
               updated_at = excluded.updated_at",
            params![version, channel, severity, body, published_at, now],
        )?;
        Ok(())
    }

    pub fn create_changelog(
        &self,
        version: &str,
        channel: &str,
        severity: &str,
        title: &str,
        body: &str,
        published_at: &str,
    ) -> rusqlite::Result<i64> {
        let conn = self.conn();
        let now = util::now();
        conn.execute(
            "INSERT INTO changelog (version, channel, severity, title, body, published_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![version, channel, severity, title, body, published_at, now],
        )?;
        Ok(conn.last_insert_rowid())
    }

    // 一条更新记录就是这些字段，拆成结构体反而多一层搬运。
    #[allow(clippy::too_many_arguments)]
    pub fn update_changelog(
        &self,
        id: i64,
        version: &str,
        channel: &str,
        severity: &str,
        title: &str,
        body: &str,
        published_at: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "UPDATE changelog SET version = ?2, channel = ?3, severity = ?4, title = ?5,
                    body = ?6, published_at = ?7, updated_at = ?8
             WHERE id = ?1",
            params![
                id,
                version,
                channel,
                severity,
                title,
                body,
                published_at,
                util::now()
            ],
        )?;
        Ok(())
    }

    pub fn delete_changelog(&self, id: i64) -> rusqlite::Result<bool> {
        let removed = self
            .conn()
            .execute("DELETE FROM changelog WHERE id = ?1", [id])?;
        Ok(removed > 0)
    }

    // ---- 使用记录聚合 ----

    pub fn usage_totals(&self, since: &str) -> rusqlite::Result<UsageTotals> {
        self.conn().query_row(
            "SELECT COUNT(*),
                    COUNT(DISTINCT device || '|' || substr(occurred_at, 1, 10)),
                    COUNT(DISTINCT session_id)
             FROM analytics_events WHERE substr(occurred_at, 1, 10) >= ?1",
            [since],
            |row| {
                Ok(UsageTotals {
                    events: row.get(0)?,
                    device_days: row.get(1)?,
                    sessions: row.get(2)?,
                })
            },
        )
    }

    /// 每天的事件数与「设备·天」数。
    pub fn usage_daily(&self, since: &str) -> rusqlite::Result<Vec<UsageRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT substr(occurred_at, 1, 10) AS day,
                    COUNT(DISTINCT device) AS devices, COUNT(*) AS events
             FROM analytics_events WHERE substr(occurred_at, 1, 10) >= ?1
             GROUP BY day ORDER BY day",
        )?;
        let rows = statement.query_map([since], |row| {
            Ok(UsageRow {
                key: row.get(0)?,
                device_days: row.get(1)?,
                events: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    fn usage_breakdown(&self, since: &str, column: &str) -> rusqlite::Result<Vec<UsageRow>> {
        let conn = self.conn();
        // 列名只来自本文件的常量，不接外部输入。
        let sql = format!(
            "SELECT {column} AS key, COUNT(DISTINCT device) AS devices, COUNT(*) AS events
             FROM analytics_events WHERE substr(occurred_at, 1, 10) >= ?1
             GROUP BY key ORDER BY devices DESC, events DESC LIMIT 50"
        );
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([since], |row| {
            Ok(UsageRow {
                key: row.get(0)?,
                device_days: row.get(1)?,
                events: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    pub fn usage_versions(&self, since: &str) -> rusqlite::Result<Vec<UsageRow>> {
        self.usage_breakdown(since, "app_version")
    }

    pub fn usage_platforms(&self, since: &str) -> rusqlite::Result<Vec<UsageRow>> {
        self.usage_breakdown(since, "os_name")
    }

    pub fn usage_top_events(&self, since: &str, limit: i64) -> rusqlite::Result<Vec<UsageRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT name, 0, COUNT(*) AS events FROM analytics_events
             WHERE substr(occurred_at, 1, 10) >= ?1
             GROUP BY name ORDER BY events DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![since, limit], |row| {
            Ok(UsageRow {
                key: row.get(0)?,
                device_days: row.get(1)?,
                events: row.get(2)?,
            })
        })?;
        rows.collect()
    }

    pub fn usage_recent(&self, limit: i64) -> rusqlite::Result<Vec<RecentUsage>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT name, app_version, os_name, occurred_at, is_debug
             FROM analytics_events ORDER BY occurred_at DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(RecentUsage {
                name: row.get(0)?,
                app_version: row.get(1)?,
                os_name: row.get(2)?,
                occurred_at: row.get(3)?,
                is_debug: row.get(4)?,
            })
        })?;
        rows.collect()
    }
}

fn admin_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AdminUser> {
    Ok(AdminUser {
        id: row.get(0)?,
        username: row.get(1)?,
        password_hash: row.get(2)?,
        role: row.get(3)?,
        created_at: row.get(4)?,
    })
}

fn changelog_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChangelogEntry> {
    Ok(ChangelogEntry {
        id: row.get(0)?,
        version: row.get(1)?,
        channel: row.get(2)?,
        severity: row.get(3)?,
        title: row.get(4)?,
        body: row.get(5)?,
        published_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

fn user_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        email: row.get(2)?,
        password_hash: row.get(3)?,
        quota_bytes: row.get(4)?,
        sync_salt: row.get(5)?,
        created_at: row.get(6)?,
        disabled: row.get(7)?,
        phone: row.get(8)?,
        email_verified: row.get(9)?,
    })
}

fn document_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncDocument> {
    Ok(SyncDocument {
        kind: row.get(0)?,
        version: row.get(1)?,
        size: row.get(2)?,
        sha256: row.get(3)?,
        created_at: row.get(4)?,
        device: row.get(5)?,
        base_version: row.get(6)?,
    })
}

fn package_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReleasePackage> {
    Ok(ReleasePackage {
        version: row.get(0)?,
        file: row.get(1)?,
        platform: row.get(2)?,
        arch: row.get(3)?,
        size: row.get(4)?,
        sha256: row.get(5)?,
        downloads: row.get(6)?,
        uploaded_at: row.get(7)?,
        withdrawn: row.get(8)?,
    })
}

fn release_version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReleaseVersion> {
    Ok(ReleaseVersion {
        version: row.get(0)?,
        channel: row.get(1)?,
        severity: row.get(2)?,
        notes: row.get(3)?,
        notes_url: row.get(4)?,
        minimum_version: row.get(5)?,
        rollout: row.get(6)?,
        published_at: row.get(7)?,
        withdrawn: row.get(8)?,
        package_count: row.get(9)?,
        total_size: row.get(10)?,
        downloads: row.get(11)?,
    })
}

// ---- 内置 AI ----

impl Db {
    pub fn ai_settings(&self) -> rusqlite::Result<Option<AiSettings>> {
        self.conn()
            .query_row(
                "SELECT enabled, base_url, api_key, model, max_context_tokens,
                        max_output_tokens, temperature, timeout_seconds, retry_count,
                        daily_request_limit, daily_token_limit, max_concurrent_per_user,
                        max_request_bytes, updated_at, top_p, sampling_both,
                        stream_include_usage, connect_timeout_seconds, retry_backoff_ms,
                        custom_headers, global_daily_token_limit, max_input_tokens,
                        min_interval_seconds, min_interval_scope, readable_upstream_hint
                 FROM ai_settings WHERE id = 1",
                [],
                |row| {
                    Ok(AiSettings {
                        enabled: row.get(0)?,
                        base_url: row.get(1)?,
                        api_key: row.get(2)?,
                        model: row.get(3)?,
                        max_context_tokens: row.get(4)?,
                        max_output_tokens: row.get(5)?,
                        temperature: row.get(6)?,
                        timeout_seconds: row.get(7)?,
                        retry_count: row.get(8)?,
                        daily_request_limit: row.get(9)?,
                        daily_token_limit: row.get(10)?,
                        max_concurrent_per_user: row.get(11)?,
                        max_request_bytes: row.get(12)?,
                        updated_at: row.get(13)?,
                        top_p: row.get(14)?,
                        sampling_both: row.get(15)?,
                        stream_include_usage: row.get(16)?,
                        connect_timeout_seconds: row.get(17)?,
                        retry_backoff_ms: row.get(18)?,
                        custom_headers: row.get(19)?,
                        global_daily_token_limit: row.get(20)?,
                        max_input_tokens: row.get(21)?,
                        min_interval_seconds: row.get(22)?,
                        min_interval_scope: row.get(23)?,
                        readable_upstream_hint: row.get(24)?,
                    })
                },
            )
            .optional()
    }

    pub fn save_ai_settings(&self, settings: &AiSettings) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO ai_settings (
               id, enabled, base_url, api_key, model, max_context_tokens,
               max_output_tokens, temperature, timeout_seconds, retry_count,
               daily_request_limit, daily_token_limit, max_concurrent_per_user,
               max_request_bytes, updated_at, top_p, sampling_both,
               stream_include_usage, connect_timeout_seconds, retry_backoff_ms,
               custom_headers, global_daily_token_limit, max_input_tokens,
               min_interval_seconds, min_interval_scope, readable_upstream_hint
             ) VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                       ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)
             ON CONFLICT(id) DO UPDATE SET
               enabled = excluded.enabled,
               base_url = excluded.base_url,
               api_key = excluded.api_key,
               model = excluded.model,
               max_context_tokens = excluded.max_context_tokens,
               max_output_tokens = excluded.max_output_tokens,
               temperature = excluded.temperature,
               timeout_seconds = excluded.timeout_seconds,
               retry_count = excluded.retry_count,
               daily_request_limit = excluded.daily_request_limit,
               daily_token_limit = excluded.daily_token_limit,
               max_concurrent_per_user = excluded.max_concurrent_per_user,
               max_request_bytes = excluded.max_request_bytes,
               updated_at = excluded.updated_at,
               top_p = excluded.top_p,
               sampling_both = excluded.sampling_both,
               stream_include_usage = excluded.stream_include_usage,
               connect_timeout_seconds = excluded.connect_timeout_seconds,
               retry_backoff_ms = excluded.retry_backoff_ms,
               custom_headers = excluded.custom_headers,
               global_daily_token_limit = excluded.global_daily_token_limit,
               max_input_tokens = excluded.max_input_tokens,
               min_interval_seconds = excluded.min_interval_seconds,
               min_interval_scope = excluded.min_interval_scope,
               readable_upstream_hint = excluded.readable_upstream_hint",
            params![
                settings.enabled,
                settings.base_url,
                settings.api_key,
                settings.model,
                settings.max_context_tokens,
                settings.max_output_tokens,
                settings.temperature,
                settings.timeout_seconds,
                settings.retry_count,
                settings.daily_request_limit,
                settings.daily_token_limit,
                settings.max_concurrent_per_user,
                settings.max_request_bytes,
                settings.updated_at,
                settings.top_p,
                settings.sampling_both,
                settings.stream_include_usage,
                settings.connect_timeout_seconds,
                settings.retry_backoff_ms,
                settings.custom_headers,
                settings.global_daily_token_limit,
                settings.max_input_tokens,
                settings.min_interval_seconds,
                settings.min_interval_scope,
                settings.readable_upstream_hint,
            ],
        )?;
        Ok(())
    }

    /// 库里还没有配置时，用环境变量播种一次。只播种地址、Key 与模型，
    /// 总开关保持关闭；之后数据库是运行期的唯一事实来源。
    pub fn seed_ai_settings(
        &self,
        base_url: &str,
        api_key: &str,
        model: &str,
    ) -> rusqlite::Result<bool> {
        if self.ai_settings()?.is_some() {
            return Ok(false);
        }
        let mut settings = AiSettings::defaults();
        settings.base_url = base_url.trim().trim_end_matches('/').to_string();
        settings.api_key = api_key.trim().to_string();
        settings.model = model.trim().to_string();
        settings.updated_at = util::now();
        self.save_ai_settings(&settings)?;
        Ok(true)
    }

    /// 没有单独设置过的用户默认开启；行不存在就是开启。
    pub fn ai_user_enabled(&self, user_id: i64) -> rusqlite::Result<bool> {
        Ok(self
            .conn()
            .query_row(
                "SELECT enabled FROM ai_user_settings WHERE user_id = ?1",
                [user_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(true))
    }

    pub fn set_ai_user_enabled(&self, user_id: i64, enabled: bool) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO ai_user_settings (user_id, enabled, updated_at)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET
               enabled = excluded.enabled, updated_at = excluded.updated_at",
            params![user_id, enabled, util::now()],
        )?;
        Ok(())
    }

    pub fn ai_usage(&self, user_id: i64, day: &str) -> rusqlite::Result<AiUsage> {
        self.conn().query_row(
            "SELECT
               COALESCE((SELECT requests FROM ai_usage_daily WHERE user_id = ?1 AND day = ?2), 0),
               COALESCE((SELECT input_tokens + output_tokens FROM ai_usage_daily
                         WHERE user_id = ?1 AND day = ?2), 0)",
            params![user_id, day],
            |row| {
                Ok(AiUsage {
                    requests_today: row.get(0)?,
                    tokens_today: row.get(1)?,
                })
            },
        )
    }

    /// 今天全站消耗的 token 合计。
    pub fn ai_global_tokens(&self, day: &str) -> rusqlite::Result<i64> {
        self.conn().query_row(
            "SELECT COALESCE(SUM(input_tokens + output_tokens), 0)
             FROM ai_usage_daily WHERE day = ?1",
            [day],
            |row| row.get(0),
        )
    }

    /// 接受一次请求：先看今日限额与最小间隔，再同时给当天和累计的
    /// 请求数 +1。上游失败也照算，避免失败重试变成免费额度。
    #[allow(clippy::too_many_arguments)] // 限额、全局限额与节流键必须一次判定。
    pub fn ai_claim_request(
        &self,
        user_id: i64,
        day: &str,
        request_limit: i64,
        token_limit: i64,
        global_token_limit: i64,
        throttle_key: &str,
        min_interval_seconds: i64,
    ) -> Result<AiUsage, AiQuotaError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let (requests, tokens): (i64, i64) = tx
            .query_row(
                "SELECT requests, input_tokens + output_tokens FROM ai_usage_daily
                 WHERE user_id = ?1 AND day = ?2",
                params![user_id, day],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((0, 0));
        if request_limit > 0 && requests >= request_limit {
            return Err(AiQuotaError::Requests {
                used: requests,
                limit: request_limit,
            });
        }
        if token_limit > 0 && tokens >= token_limit {
            return Err(AiQuotaError::Tokens {
                used: tokens,
                limit: token_limit,
            });
        }
        if global_token_limit > 0 {
            let global: i64 = tx.query_row(
                "SELECT COALESCE(SUM(input_tokens + output_tokens), 0)
                 FROM ai_usage_daily WHERE day = ?1",
                [day],
                |row| row.get(0),
            )?;
            if global >= global_token_limit {
                return Err(AiQuotaError::GlobalTokens {
                    used: global,
                    limit: global_token_limit,
                });
            }
        }
        if min_interval_seconds > 0 && !throttle_key.is_empty() {
            let last: Option<String> = tx
                .query_row(
                    "SELECT last_request_at FROM ai_throttle WHERE key = ?1",
                    [throttle_key],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(last) = last
                && let Ok(last) = chrono::DateTime::parse_from_rfc3339(&last)
            {
                let elapsed = (chrono::Utc::now() - last.with_timezone(&chrono::Utc))
                    .num_seconds()
                    .max(0);
                if elapsed < min_interval_seconds {
                    return Err(AiQuotaError::Interval {
                        retry_after: min_interval_seconds - elapsed,
                    });
                }
            }
        }
        let now = util::now();
        tx.execute(
            "INSERT INTO ai_usage_daily (user_id, day, requests) VALUES (?1, ?2, 1)
             ON CONFLICT(user_id, day) DO UPDATE SET requests = requests + 1",
            params![user_id, day],
        )?;
        tx.execute(
            "INSERT INTO ai_usage (user_id, requests, last_used_at) VALUES (?1, 1, ?2)
             ON CONFLICT(user_id) DO UPDATE SET
               requests = requests + 1, last_used_at = excluded.last_used_at",
            params![user_id, now],
        )?;
        if !throttle_key.is_empty() {
            tx.execute(
                "INSERT INTO ai_throttle (key, last_request_at) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET last_request_at = excluded.last_request_at",
                params![throttle_key, now],
            )?;
        }
        tx.commit()?;
        drop(conn);
        Ok(self.ai_usage(user_id, day)?)
    }

    /// 请求结束后补记 token；真实值优先，没有真实值就是调用方的估算。
    pub fn ai_record_tokens(
        &self,
        user_id: i64,
        day: &str,
        input_tokens: i64,
        output_tokens: i64,
    ) -> rusqlite::Result<()> {
        let input_tokens = input_tokens.max(0);
        let output_tokens = output_tokens.max(0);
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO ai_usage_daily (user_id, day, input_tokens, output_tokens)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(user_id, day) DO UPDATE SET
               input_tokens = input_tokens + excluded.input_tokens,
               output_tokens = output_tokens + excluded.output_tokens",
            params![user_id, day, input_tokens, output_tokens],
        )?;
        tx.execute(
            "INSERT INTO ai_usage (user_id, input_tokens, output_tokens)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(user_id) DO UPDATE SET
               input_tokens = input_tokens + excluded.input_tokens,
               output_tokens = output_tokens + excluded.output_tokens",
            params![user_id, input_tokens, output_tokens],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 后台用量列表：按最近使用时间排序，只含计数，不含任何内容。
    pub fn ai_usage_rows(
        &self,
        search: &str,
        limit: i64,
        offset: i64,
        day: &str,
    ) -> rusqlite::Result<(i64, Vec<AiUsageRow>)> {
        let pattern = format!("%{}%", search.trim());
        let conn = self.conn();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM users u
             WHERE ?1 = '%%' OR u.username LIKE ?1 OR u.email LIKE ?1",
            [&pattern],
            |row| row.get(0),
        )?;
        let mut statement = conn.prepare(
            "SELECT u.id, u.username, u.disabled, COALESCE(s.enabled, 1),
                    COALESCE(d.requests, 0), COALESCE(d.input_tokens + d.output_tokens, 0),
                    COALESCE(t.requests, 0), COALESCE(t.input_tokens, 0),
                    COALESCE(t.output_tokens, 0), t.last_used_at
             FROM users u
             LEFT JOIN ai_user_settings s ON s.user_id = u.id
             LEFT JOIN ai_usage_daily d ON d.user_id = u.id AND d.day = ?2
             LEFT JOIN ai_usage t ON t.user_id = u.id
             WHERE ?1 = '%%' OR u.username LIKE ?1 OR u.email LIKE ?1
             ORDER BY COALESCE(t.last_used_at, '') DESC, u.id
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = statement.query_map(params![pattern, day, limit, offset], |row| {
            Ok(AiUsageRow {
                id: row.get(0)?,
                username: row.get(1)?,
                account_disabled: row.get(2)?,
                ai_enabled: row.get(3)?,
                requests_today: row.get(4)?,
                tokens_today: row.get(5)?,
                requests_total: row.get(6)?,
                input_tokens_total: row.get(7)?,
                output_tokens_total: row.get(8)?,
                last_used_at: row.get(9)?,
            })
        })?;
        let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((total, rows))
    }
}

/// 按 `user_version` 一步步往上加：线上的库停在哪个版本都能就地升到最新。
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version >= SCHEMA_VERSION {
        return Ok(());
    }
    if version < 1 {
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS users (
               id            INTEGER PRIMARY KEY,
               username      TEXT NOT NULL UNIQUE,
               email         TEXT UNIQUE,
               password_hash TEXT NOT NULL,
               quota_bytes   INTEGER NOT NULL,
               sync_salt     TEXT NOT NULL,
               created_at    TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS sessions (
               token_hash   TEXT PRIMARY KEY,
               user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               created_at   TEXT NOT NULL,
               last_used_at TEXT NOT NULL,
               expires_at   TEXT NOT NULL,
               device       TEXT
             );
             CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);
             CREATE TABLE IF NOT EXISTS snapshots (
               id         INTEGER PRIMARY KEY,
               user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               version    INTEGER NOT NULL,
               size       INTEGER NOT NULL,
               sha256     TEXT NOT NULL,
               created_at TEXT NOT NULL,
               device     TEXT,
               UNIQUE (user_id, version)
             );
             CREATE TABLE IF NOT EXISTS manifests (
               channel      TEXT NOT NULL,
               version      TEXT NOT NULL,
               envelope     TEXT NOT NULL,
               published_at TEXT NOT NULL,
               PRIMARY KEY (channel, version)
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    if version < 2 {
        // 匿名统计的事件，以及每天换一个的设备摘要盐。
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS analytics_events (
               id          INTEGER PRIMARY KEY,
               name        TEXT NOT NULL,
               session_id  TEXT NOT NULL,
               device      TEXT NOT NULL,
               occurred_at TEXT NOT NULL,
               received_at TEXT NOT NULL,
               app_version TEXT NOT NULL,
               os_name     TEXT NOT NULL,
               os_version  TEXT NOT NULL,
               sdk_version TEXT NOT NULL,
               is_debug    INTEGER NOT NULL,
               props       TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS analytics_events_name
               ON analytics_events(name, occurred_at);
             CREATE INDEX IF NOT EXISTS analytics_events_day
               ON analytics_events(occurred_at);
             CREATE TABLE IF NOT EXISTS analytics_salts (
               day  TEXT PRIMARY KEY,
               salt TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS manifests (
               channel      TEXT NOT NULL,
               version      TEXT NOT NULL,
               envelope     TEXT NOT NULL,
               published_at TEXT NOT NULL,
               PRIMARY KEY (channel, version)
             );
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    if version < 3 {
        // 后台管理员、后台会话，以及可在后台维护的更新记录。
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS admin_users (
               id            INTEGER PRIMARY KEY,
               username      TEXT NOT NULL UNIQUE,
               password_hash TEXT NOT NULL,
               created_at    TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS admin_sessions (
               token_hash TEXT PRIMARY KEY,
               admin_id   INTEGER NOT NULL REFERENCES admin_users(id) ON DELETE CASCADE,
               created_at TEXT NOT NULL,
               expires_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS admin_sessions_admin
               ON admin_sessions(admin_id);
             CREATE TABLE IF NOT EXISTS changelog (
               id           INTEGER PRIMARY KEY,
               version      TEXT NOT NULL UNIQUE,
               channel      TEXT NOT NULL,
               level        TEXT NOT NULL DEFAULT 'suggested',
               title        TEXT NOT NULL DEFAULT '',
               body         TEXT NOT NULL DEFAULT '',
               published_at TEXT NOT NULL,
               updated_at   TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS changelog_published
               ON changelog(published_at);
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
        // 老库里的发布记录回填成更新记录，页面内容与升级前一致。
        backfill_changelog(conn)?;
    }
    if version < 4 {
        // 后台角色：owner 全权、publisher 管发布与更新记录、viewer 只读。
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE admin_users ADD COLUMN role TEXT NOT NULL DEFAULT 'owner';
             PRAGMA user_version = 4;
             COMMIT;",
        )?;
    }
    if version < 5 {
        // 更新分级按客户端字段名统一成 severity，旧的 suggested 归到 recommended。
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE changelog RENAME COLUMN level TO severity;
             UPDATE changelog SET severity =
               CASE WHEN severity = 'required' THEN 'required' ELSE 'recommended' END;
             PRAGMA user_version = 5;
             COMMIT;",
        )?;
    }
    if version < 6 {
        // 用户停用、后台可撤下的清单、按 settings/hosts 分开的同步文档，
        // 安装包与发布记录，以及用户/管理员各自独立的登录失败锁定。
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE users ADD COLUMN disabled INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE manifests ADD COLUMN withdrawn INTEGER NOT NULL DEFAULT 0;
             CREATE TABLE IF NOT EXISTS sync_documents (
               id           INTEGER PRIMARY KEY,
               user_id      INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               kind         TEXT NOT NULL,
               version      INTEGER NOT NULL,
               size         INTEGER NOT NULL,
               sha256       TEXT NOT NULL,
               base_version INTEGER,
               created_at   TEXT NOT NULL,
               device       TEXT,
               UNIQUE (user_id, kind, version)
             );
             CREATE INDEX IF NOT EXISTS sync_documents_user
               ON sync_documents(user_id, kind, version);
             CREATE TABLE IF NOT EXISTS release_packages (
               version     TEXT NOT NULL,
               file        TEXT NOT NULL,
               platform    TEXT NOT NULL DEFAULT '',
               arch        TEXT NOT NULL DEFAULT '',
               size        INTEGER NOT NULL DEFAULT 0,
               sha256      TEXT NOT NULL DEFAULT '',
               downloads   INTEGER NOT NULL DEFAULT 0,
               uploaded_at TEXT NOT NULL,
               withdrawn   INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (version, file)
             );
             CREATE TABLE IF NOT EXISTS release_versions (
               version          TEXT NOT NULL,
               channel          TEXT NOT NULL,
               severity         TEXT NOT NULL DEFAULT 'recommended',
               notes            TEXT NOT NULL DEFAULT '',
               notes_url        TEXT NOT NULL DEFAULT '',
               minimum_version  TEXT,
               rollout          REAL NOT NULL DEFAULT 1.0,
               published_at     TEXT NOT NULL,
               withdrawn        INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (version, channel)
             );
             CREATE INDEX IF NOT EXISTS release_versions_published
               ON release_versions(published_at);
             CREATE TABLE IF NOT EXISTS login_lockouts (
               scope        TEXT NOT NULL,
               name         TEXT NOT NULL,
               failures     INTEGER NOT NULL DEFAULT 0,
               locked_until TEXT,
               updated_at   TEXT NOT NULL,
               PRIMARY KEY (scope, name)
             );
             PRAGMA user_version = 6;
             COMMIT;",
        )?;
        // 已经发过的版本补一条发布记录，后台一上来就能看到历史。
        backfill_release_versions(conn)?;
    }
    if version < 7 {
        // 内置 AI：一套上游配置、单用户开关、累计与按天的用量计数。
        // 只存计数与时间，不存任何对话正文。
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS ai_settings (
               id                     INTEGER PRIMARY KEY CHECK (id = 1),
               enabled                INTEGER NOT NULL DEFAULT 0,
               base_url               TEXT NOT NULL DEFAULT '',
               api_key                TEXT NOT NULL DEFAULT '',
               model                  TEXT NOT NULL DEFAULT '',
               max_context_tokens     INTEGER NOT NULL DEFAULT 32000,
               max_output_tokens      INTEGER NOT NULL DEFAULT 4096,
               temperature            REAL,
               timeout_seconds        INTEGER NOT NULL DEFAULT 60,
               retry_count            INTEGER NOT NULL DEFAULT 1,
               daily_request_limit    INTEGER NOT NULL DEFAULT 200,
               daily_token_limit      INTEGER NOT NULL DEFAULT 0,
               max_concurrent_per_user INTEGER NOT NULL DEFAULT 2,
               max_request_bytes      INTEGER NOT NULL DEFAULT 262144,
               updated_at             TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ai_user_settings (
               user_id    INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
               enabled    INTEGER NOT NULL DEFAULT 1,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS ai_usage (
               user_id       INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
               requests      INTEGER NOT NULL DEFAULT 0,
               input_tokens  INTEGER NOT NULL DEFAULT 0,
               output_tokens INTEGER NOT NULL DEFAULT 0,
               last_used_at  TEXT
             );
             CREATE TABLE IF NOT EXISTS ai_usage_daily (
               user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
               day           TEXT NOT NULL,
               requests      INTEGER NOT NULL DEFAULT 0,
               input_tokens  INTEGER NOT NULL DEFAULT 0,
               output_tokens INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY (user_id, day)
             );
             CREATE INDEX IF NOT EXISTS ai_usage_daily_day
               ON ai_usage_daily(day);
             PRAGMA user_version = 7;
             COMMIT;",
        )?;
    }
    if version < 8 {
        // 待验证的新邮箱：还没有邮件通道，先只记录，验证通过前不改
        // `users.email`。一个用户同时只保留一条待验证记录。
        conn.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS user_email_changes (
               user_id    INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
               email      TEXT NOT NULL,
               created_at TEXT NOT NULL
             );
             PRAGMA user_version = 8;
             COMMIT;",
        )?;
    }
    if version < 9 {
        // 注册手机号：可空，老用户没有也正常。没有短信通道，先留出
        // `phone_verified` 状态位（默认 0），不伪造已验证。
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE users ADD COLUMN phone TEXT;
             ALTER TABLE users ADD COLUMN phone_verified INTEGER NOT NULL DEFAULT 0;
             CREATE UNIQUE INDEX IF NOT EXISTS users_phone ON users(phone);
             PRAGMA user_version = 9;
             COMMIT;",
        )?;
    }
    if version < 10 {
        // 邮件：SMTP 配置、两套模板、验证码与发送记录。配置只有一行，
        // 口令只进不出；验证码只存摘要。默认模板随迁移写入，管理员可改。
        conn.execute_batch(
            r#"BEGIN;
             ALTER TABLE users ADD COLUMN email_verified INTEGER NOT NULL DEFAULT 0;
             CREATE TABLE IF NOT EXISTS email_settings (
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
             CREATE TABLE IF NOT EXISTS email_templates (
               purpose    TEXT PRIMARY KEY,
               subject    TEXT NOT NULL,
               body       TEXT NOT NULL,
               updated_at TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS email_codes (
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
             CREATE INDEX IF NOT EXISTS email_codes_lookup
               ON email_codes(purpose, email, created_at);
             CREATE TABLE IF NOT EXISTS email_sends (
               id         INTEGER PRIMARY KEY,
               email      TEXT NOT NULL,
               purpose    TEXT NOT NULL,
               result     TEXT NOT NULL,
               detail     TEXT NOT NULL DEFAULT '',
               created_at TEXT NOT NULL
             );
             INSERT OR IGNORE INTO email_templates (purpose, subject, body, updated_at) VALUES
               ('register_verification', '【Agree ShellAs】邮箱验证码',
                '你好 {username}：

你的 ShellAs 邮箱验证码是 {code}，{minutes} 分钟内有效。
如非本人操作，请忽略这封邮件。

Agree ShellAs',
                strftime('%Y-%m-%dT%H:%M:%SZ','now')),
               ('password_reset', '【Agree ShellAs】重置口令验证码',
                '你好 {username}：

你的 ShellAs 口令重置验证码是 {code}，{minutes} 分钟内有效。
如果这不是你发起的，请忽略这封邮件，并考虑修改口令。

Agree ShellAs',
                strftime('%Y-%m-%dT%H:%M:%SZ','now'));
             PRAGMA user_version = 10;
             COMMIT;"#,
        )?;
    }
    if version < 11 {
        // 内置 AI 的上游参数与防护补齐：采样参数、连接超时、退避、
        // 自定义请求头、输入上限、全局限额与按用户/邮箱的最小间隔。
        // 上下文默认值从 32k 提到 128k，只改仍是老默认值的库。
        conn.execute_batch(
            "BEGIN;
             ALTER TABLE ai_settings ADD COLUMN top_p REAL;
             ALTER TABLE ai_settings ADD COLUMN sampling_both INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE ai_settings ADD COLUMN stream_include_usage INTEGER NOT NULL DEFAULT 1;
             ALTER TABLE ai_settings ADD COLUMN connect_timeout_seconds INTEGER NOT NULL DEFAULT 10;
             ALTER TABLE ai_settings ADD COLUMN retry_backoff_ms INTEGER NOT NULL DEFAULT 300;
             ALTER TABLE ai_settings ADD COLUMN custom_headers TEXT NOT NULL DEFAULT '{}';
             ALTER TABLE ai_settings ADD COLUMN global_daily_token_limit INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE ai_settings ADD COLUMN max_input_tokens INTEGER NOT NULL DEFAULT 122880;
             ALTER TABLE ai_settings ADD COLUMN min_interval_seconds INTEGER NOT NULL DEFAULT 1;
             ALTER TABLE ai_settings ADD COLUMN min_interval_scope TEXT NOT NULL DEFAULT 'user';
             ALTER TABLE ai_settings ADD COLUMN readable_upstream_hint INTEGER NOT NULL DEFAULT 1;
             UPDATE ai_settings SET max_context_tokens = 131072
               WHERE max_context_tokens = 32000;
             CREATE TABLE IF NOT EXISTS ai_throttle (
               key             TEXT PRIMARY KEY,
               last_request_at TEXT NOT NULL
             );
             PRAGMA user_version = 11;
             COMMIT;",
        )?;
    }
    Ok(())
}

/// 把已有清单里的版本号、更新说明搬进 `changelog`，已有的行不覆盖。
fn backfill_changelog(conn: &Connection) -> rusqlite::Result<()> {
    let mut statement = conn.prepare(
        "SELECT channel, version, envelope, published_at FROM manifests
         ORDER BY published_at DESC, rowid DESC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut manifests = Vec::new();
    for row in rows {
        manifests.push(row?);
    }
    drop(statement);
    for (channel, version, envelope, published_at) in manifests {
        let Some(manifest) = parse_envelope(&envelope) else {
            continue;
        };
        // 老清单只有 level（suggested），新清单是 severity；统一归到两个值。
        let severity = manifest
            .get("severity")
            .and_then(Value::as_str)
            .or_else(|| manifest.get("level").and_then(Value::as_str))
            .map(|value| {
                if value == "required" {
                    "required"
                } else {
                    "recommended"
                }
            })
            .unwrap_or("recommended")
            .to_string();
        let notes = manifest
            .get("notes")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        conn.execute(
            "INSERT INTO changelog (version, channel, level, title, body, published_at, updated_at)
             VALUES (?1, ?2, ?3, '', ?4, ?5, ?5)
             ON CONFLICT(version) DO NOTHING",
            params![version, channel, severity, notes, published_at],
        )?;
    }
    Ok(())
}

/// 把已经发过的清单补成发布记录，后台的升级记录一上来就有历史。
fn backfill_release_versions(conn: &Connection) -> rusqlite::Result<()> {
    let rows: Vec<(String, String, String, String, bool)> = {
        let mut statement = conn.prepare(
            "SELECT channel, version, envelope, published_at, withdrawn FROM manifests
             ORDER BY published_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (channel, version, envelope, published_at, withdrawn) in rows {
        let Some(manifest) = parse_envelope(&envelope) else {
            continue;
        };
        let severity = manifest
            .get("severity")
            .or_else(|| manifest.get("level"))
            .and_then(Value::as_str)
            .map(|severity| {
                if severity == crate::release::SEVERITY_REQUIRED {
                    crate::release::SEVERITY_REQUIRED
                } else {
                    crate::release::SEVERITY_RECOMMENDED
                }
            })
            .unwrap_or(crate::release::SEVERITY_RECOMMENDED);
        let notes = manifest.get("notes").and_then(Value::as_str).unwrap_or("");
        let notes_url = manifest
            .get("notes_url")
            .and_then(Value::as_str)
            .unwrap_or("");
        let minimum_version = manifest
            .get("minimum_version")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty());
        let rollout = manifest
            .get("rollout")
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        conn.execute(
            "INSERT INTO release_versions
               (version, channel, severity, notes, notes_url, minimum_version,
                rollout, published_at, withdrawn)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(version, channel) DO NOTHING",
            params![
                version,
                channel,
                severity,
                notes,
                notes_url,
                minimum_version,
                rollout,
                published_at,
                withdrawn
            ],
        )?;
    }
    Ok(())
}

/// 信封里的清单原文。迁移时用，解析不了就跳过这一条。
fn parse_envelope(envelope: &str) -> Option<Value> {
    let envelope: Value = serde_json::from_str(envelope).ok()?;
    serde_json::from_str(envelope.get("manifest")?.as_str()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::in_memory()
    }

    fn user(db: &Db) -> User {
        db.create_user("alice", None, None, "$argon2id$hash", "salt", 1024, false)
            .unwrap()
    }

    #[test]
    fn phone_numbers_are_optional_and_unique() {
        let db = db();
        let alice = db
            .create_user(
                "alice",
                None,
                Some("13800138000"),
                "$argon2id$hash",
                "salt",
                1024,
                false,
            )
            .unwrap();
        assert_eq!(
            db.user_by_id(alice.id).unwrap().unwrap().phone.as_deref(),
            Some("13800138000")
        );
        assert!(db.phone_taken("13800138000").unwrap());
        let bob = db
            .create_user("bob", None, None, "$argon2id$hash", "salt", 1024, false)
            .unwrap();
        assert_eq!(bob.phone, None);
        assert!(
            db.create_user(
                "carol",
                None,
                Some("13800138000"),
                "$argon2id$hash",
                "salt",
                1024,
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn a_user_is_found_by_name_without_case() {
        let db = db();
        user(&db);
        assert!(db.user_by_name("ALICE").unwrap().is_some());
        assert!(db.user_by_name("bob").unwrap().is_none());
    }

    #[test]
    fn a_pending_email_keeps_the_primary_one() {
        let db = db();
        let alice = user(&db);
        let _bob = db
            .create_user(
                "bob",
                Some("bob@example.com"),
                None,
                "$argon2id$hash",
                "salt",
                1024,
                false,
            )
            .unwrap();
        assert_eq!(db.pending_email(alice.id).unwrap(), None);
        assert!(
            db.email_taken_by_other(alice.id, "BOB@example.com")
                .unwrap()
        );
        assert!(
            !db.email_taken_by_other(alice.id, "carol@example.com")
                .unwrap()
        );
        db.set_pending_email(alice.id, "first@example.com").unwrap();
        db.set_pending_email(alice.id, "second@example.com")
            .unwrap();
        assert_eq!(
            db.pending_email(alice.id).unwrap().as_deref(),
            Some("second@example.com")
        );
        assert_eq!(db.user_by_id(alice.id).unwrap().unwrap().email, None);
    }

    #[test]
    fn versions_increase_and_old_ones_are_evicted() {
        let db = db();
        let user = user(&db);
        for i in 0..4 {
            let (snapshot, evicted) = db
                .add_snapshot(user.id, 100, &format!("h{i}"), None, 1024, 2)
                .unwrap();
            assert_eq!(snapshot.version, i + 1);
            if i < 2 {
                assert!(evicted.is_empty());
            }
        }
        let kept = db.snapshots(user.id).unwrap();
        assert_eq!(kept.iter().map(|s| s.version).collect::<Vec<_>>(), [4, 3]);
        assert_eq!(db.used_bytes(user.id).unwrap(), 200);
    }

    #[test]
    fn a_snapshot_that_does_not_fit_is_refused() {
        let db = db();
        let user = user(&db);
        let error = db.add_snapshot(user.id, 900, "a", None, 1024, 5).unwrap();
        assert_eq!(error.0.version, 1);
        assert!(matches!(
            db.add_snapshot(user.id, 900, "b", None, 1024, 5),
            Err(SnapshotError::Quota {
                used: 900,
                quota: 1024
            })
        ));
    }

    #[test]
    fn deleting_a_snapshot_frees_its_bytes() {
        let db = db();
        let user = user(&db);
        db.add_snapshot(user.id, 10, "a", None, 1024, 5).unwrap();
        assert!(db.delete_snapshot(user.id, 1).unwrap().is_some());
        assert!(db.delete_snapshot(user.id, 1).unwrap().is_none());
        assert_eq!(db.used_bytes(user.id).unwrap(), 0);
    }

    #[test]
    fn the_newest_manifest_of_a_channel_wins() {
        let db = db();
        assert!(db.current_manifest("stable").unwrap().is_none());
        db.put_manifest("stable", "0.1.0", "old", "2026-01-01T00:00:00Z")
            .unwrap();
        db.put_manifest("stable", "0.2.0", "new", "2026-02-01T00:00:00Z")
            .unwrap();
        assert_eq!(
            db.current_manifest("stable").unwrap().as_deref(),
            Some("new")
        );
        // 撤下最新版后自动退回上一版，而不是让通道空着。
        db.set_manifest_withdrawn("stable", "0.2.0", true).unwrap();
        assert_eq!(
            db.current_manifest("stable").unwrap().as_deref(),
            Some("old")
        );
        assert_eq!(db.current_manifest("beta").unwrap(), None);
    }

    #[test]
    fn documents_need_the_latest_base_and_keep_versions_apart() {
        let db = db();
        let user = user(&db);
        let (first, evicted) = db
            .add_document(user.id, "settings", 10, "a", None, None, 1024, 2)
            .unwrap();
        assert_eq!(first.version, 1);
        assert!(evicted.is_empty());
        let (second, _) = db
            .add_document(user.id, "settings", 10, "b", Some(1), None, 1024, 2)
            .unwrap();
        assert_eq!(second.version, 2);
        // 客户端还拿着第 1 版来传，服务端已经到第 2 版，必须让它先合并。
        assert!(matches!(
            db.add_document(user.id, "settings", 10, "c", Some(1), None, 1024, 2),
            Err(DocumentError::Conflict { latest: Some(latest) }) if latest.version == 2
        ));
        // settings 与 hosts 各是一条独立的版本链。
        let (hosts, _) = db
            .add_document(user.id, "hosts", 10, "h", None, None, 1024, 2)
            .unwrap();
        assert_eq!(hosts.version, 1);
        assert_eq!(db.used_bytes(user.id).unwrap(), 30);
        let (third, evicted) = db
            .add_document(user.id, "settings", 10, "c", Some(2), None, 1024, 2)
            .unwrap();
        assert_eq!(third.version, 3);
        assert_eq!(evicted, vec![1]);
    }

    #[test]
    fn login_lockouts_are_independent_per_scope() {
        let db = db();
        assert!(
            db.record_login_failure("user", "alice", 2, 15)
                .unwrap()
                .is_empty()
        );
        let until = db.record_login_failure("user", "alice", 2, 15).unwrap();
        assert!(!until.is_empty());
        assert!(db.login_locked_at("user", "alice").unwrap().is_some());
        // 用户入口被锁不会牵连后台管理员。
        assert!(db.login_locked_at("admin", "alice").unwrap().is_none());
        db.clear_login_failures("user", "alice").unwrap();
        assert!(db.login_locked_at("user", "alice").unwrap().is_none());
    }

    #[test]
    fn packages_keep_metadata_and_download_counts() {
        let db = db();
        db.upsert_package("0.1.0", "a.dmg", "macos", "aarch64", 100, "hash")
            .unwrap();
        assert!(db.increment_download("0.1.0", "a.dmg").unwrap());
        let package = db.package("0.1.0", "a.dmg").unwrap().unwrap();
        assert_eq!(package.downloads, 1);
        assert_eq!(package.sha256, "hash");
        db.put_release_version(
            "0.1.0",
            "stable",
            "recommended",
            "notes",
            "https://example.com/changelog#v0.1.0",
            None,
            1.0,
            "2026-11-16T00:00:00Z",
        )
        .unwrap();
        let records = db.release_versions(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].package_count, 1);
        assert_eq!(records[0].downloads, 1);
        db.set_packages_withdrawn("0.1.0", true).unwrap();
        assert!(db.package("0.1.0", "a.dmg").unwrap().unwrap().withdrawn);
    }

    #[test]
    fn an_expired_session_is_refused() {
        let db = db();
        let user = user(&db);
        db.create_session(user.id, "hash", "2020-01-01T00:00:00Z", None)
            .unwrap();
        assert!(db.session_user("hash").unwrap().is_none());
        db.create_session(user.id, "live", "2999-01-01T00:00:00Z", None)
            .unwrap();
        assert_eq!(db.session_user("live").unwrap().unwrap().username, "alice");
    }

    #[test]
    fn the_analytics_salt_is_one_a_day_and_changes_between() {
        let db = db();
        let today = db.analytics_salt("2026-11-16").unwrap();
        assert_eq!(db.analytics_salt("2026-11-16").unwrap(), today);
        assert_ne!(db.analytics_salt("2026-11-17").unwrap(), today);
    }

    #[test]
    fn analytics_events_are_stored_with_their_device_hash() {
        let db = db();
        db.insert_analytics_events(&[AnalyticsEvent {
            name: "app_started".into(),
            session_id: "176000000012345678".into(),
            device: "a0b1".into(),
            occurred_at: "2026-11-16T08:00:00Z".into(),
            received_at: "2026-11-16T09:00:00Z".into(),
            app_version: "0.1.19".into(),
            os_name: "Windows".into(),
            os_version: "10.0.22631".into(),
            sdk_version: "shellas@0.1.19".into(),
            is_debug: false,
            props: r#"{"arch":"x86_64"}"#.into(),
        }])
        .unwrap();
        let conn = db.conn();
        let (name, device, props): (String, String, String) = conn
            .query_row(
                "SELECT name, device, props FROM analytics_events",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "app_started");
        assert_eq!(device, "a0b1");
        assert_eq!(props, r#"{"arch":"x86_64"}"#);
    }

    #[test]
    fn a_version_one_database_is_upgraded_in_place() {
        // 线上停在 v1 的库只补统计表，原有数据原样留着。
        let dir = std::env::temp_dir().join(format!("shellas-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shellas.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (id INTEGER PRIMARY KEY, username TEXT NOT NULL UNIQUE);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let db = Db::open(&path).unwrap();
        let version: i64 = db
            .conn()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        db.analytics_salt("2026-11-16").unwrap();
        db.insert_analytics_events(&[]).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ai_settings_are_seeded_once_and_keep_admin_changes() {
        let db = db();
        assert!(db.ai_settings().unwrap().is_none());
        assert!(
            db.seed_ai_settings("http://example.test/v1/", "key", "model-a")
                .unwrap()
        );
        let settings = db.ai_settings().unwrap().unwrap();
        assert_eq!(settings.base_url, "http://example.test/v1");
        assert_eq!(settings.model, "model-a");
        assert_eq!(settings.daily_request_limit, 200);
        assert_eq!(settings.max_concurrent_per_user, 2);
        assert!(!settings.enabled);

        // 第二次启动不再覆盖后台改过的值。
        assert!(
            !db.seed_ai_settings("http://other.test/v1", "key2", "model-b")
                .unwrap()
        );
        let settings = db.ai_settings().unwrap().unwrap();
        assert_eq!(settings.base_url, "http://example.test/v1");
        assert_eq!(settings.model, "model-a");
    }

    #[test]
    fn ai_claims_count_down_to_the_daily_limit() {
        let db = db();
        let user = user(&db);
        let day = "2026-11-17";
        let usage = db
            .ai_claim_request(user.id, day, 2, 0, 0, "user:1", 0)
            .unwrap();
        assert_eq!(usage.requests_today, 1);
        db.ai_record_tokens(user.id, day, 10, 5).unwrap();
        let usage = db
            .ai_claim_request(user.id, day, 2, 50, 0, "user:1", 0)
            .unwrap();
        assert_eq!(usage.requests_today, 2);
        assert_eq!(usage.tokens_today, 15);
        assert!(matches!(
            db.ai_claim_request(user.id, day, 2, 50, 0, "user:1", 0),
            Err(AiQuotaError::Requests { used: 2, limit: 2 })
        ));
        assert!(matches!(
            db.ai_claim_request(user.id, day, 0, 10, 0, "user:1", 0),
            Err(AiQuotaError::Tokens {
                used: 15,
                limit: 10
            })
        ));
        assert!(matches!(
            db.ai_claim_request(user.id, day, 0, 0, 10, "user:1", 0),
            Err(AiQuotaError::GlobalTokens {
                used: 15,
                limit: 10
            })
        ));
    }

    #[test]
    fn ai_claims_honour_the_minimum_interval_per_key() {
        let db = db();
        let user = user(&db);
        let day = "2026-11-17";
        db.ai_claim_request(user.id, day, 0, 0, 0, "email:alice@example.com", 60)
            .unwrap();
        assert!(matches!(
            db.ai_claim_request(user.id, day, 0, 0, 0, "email:alice@example.com", 60),
            Err(AiQuotaError::Interval {
                retry_after: 1..=60
            })
        ));
        // 另一个键不受影响。
        db.ai_claim_request(user.id, day, 0, 0, 0, "email:bob@example.com", 60)
            .unwrap();
    }

    #[test]
    fn ai_user_switch_defaults_on_and_is_visible_in_the_list() {
        let db = db();
        let user = user(&db);
        assert!(db.ai_user_enabled(user.id).unwrap());
        db.set_ai_user_enabled(user.id, false).unwrap();
        assert!(!db.ai_user_enabled(user.id).unwrap());
        let (total, rows) = db.ai_usage_rows("", 10, 0, "2026-11-17").unwrap();
        assert_eq!(total, 1);
        assert_eq!(rows[0].username, "alice");
        assert!(!rows[0].ai_enabled);
    }

    #[test]
    fn email_settings_are_seeded_once_and_keep_admin_changes() {
        let db = db();
        assert!(db.email_settings().unwrap().is_none());
        let mut settings = EmailSettings::defaults();
        settings.enabled = true;
        settings.host = "smtp.example.com".into();
        settings.from_address = "no-reply@example.com".into();
        settings.updated_at = util::now();
        assert!(db.seed_email_settings(&settings).unwrap());
        assert!(!db.seed_email_settings(&settings).unwrap());

        let mut changed = settings.clone();
        changed.host = "smtp2.example.com".into();
        changed.require_verification = true;
        db.save_email_settings(&changed).unwrap();
        let stored = db.email_settings().unwrap().unwrap();
        assert_eq!(stored.host, "smtp2.example.com");
        assert!(stored.require_verification);
    }

    #[test]
    fn migration_writes_both_default_templates() {
        let db = db();
        for purpose in ["register_verification", "password_reset"] {
            let template = db.email_template(purpose).unwrap().unwrap();
            assert!(template.subject.contains("ShellAs"));
            assert!(template.body.contains("{code}"));
            assert!(template.body.contains("{minutes}"));
        }
    }

    #[test]
    fn a_code_is_single_use() {
        let db = db();
        let email = "alice@example.com";
        db.issue_email_code(
            "register_verification",
            email,
            "hash",
            &util::later_minutes(10),
            5,
        )
        .unwrap();
        assert_eq!(
            db.check_email_code("register_verification", email, "hash")
                .unwrap(),
            EmailCodeCheck::Ok
        );
        assert_eq!(
            db.check_email_code("register_verification", email, "hash")
                .unwrap(),
            EmailCodeCheck::Invalid
        );
    }

    #[test]
    fn five_wrong_codes_invalidate_even_the_right_one() {
        let db = db();
        let email = "alice@example.com";
        db.issue_email_code(
            "password_reset",
            email,
            "right",
            &util::later_minutes(10),
            5,
        )
        .unwrap();
        for attempt in 1..5 {
            assert_eq!(
                db.check_email_code("password_reset", email, "wrong")
                    .unwrap(),
                EmailCodeCheck::Invalid,
                "第 {attempt} 次"
            );
        }
        assert_eq!(
            db.check_email_code("password_reset", email, "wrong")
                .unwrap(),
            EmailCodeCheck::Locked
        );
        assert_eq!(
            db.check_email_code("password_reset", email, "right")
                .unwrap(),
            EmailCodeCheck::Locked
        );
    }

    #[test]
    fn resending_invalidates_the_previous_code() {
        let db = db();
        let email = "alice@example.com";
        db.issue_email_code(
            "register_verification",
            email,
            "old",
            &util::later_minutes(10),
            5,
        )
        .unwrap();
        db.issue_email_code(
            "register_verification",
            email,
            "new",
            &util::later_minutes(10),
            5,
        )
        .unwrap();
        assert_eq!(
            db.check_email_code("register_verification", email, "old")
                .unwrap(),
            EmailCodeCheck::Invalid
        );
        assert_eq!(
            db.check_email_code("register_verification", email, "new")
                .unwrap(),
            EmailCodeCheck::Ok
        );
    }

    #[test]
    fn an_expired_code_is_refused() {
        let db = db();
        let email = "alice@example.com";
        db.issue_email_code("password_reset", email, "hash", &util::seconds_ago(1), 5)
            .unwrap();
        assert_eq!(
            db.check_email_code("password_reset", email, "hash")
                .unwrap(),
            EmailCodeCheck::Expired
        );
    }

    #[test]
    fn send_log_keeps_metadata_only_and_counts_email_codes() {
        let db = db();
        db.insert_email_send("alice@example.com", "test", "ok", "")
            .unwrap();
        db.insert_email_send("alice@example.com", "password_reset", "rejected", "550")
            .unwrap();
        let (total, rows) = db.email_sends(10, 0).unwrap();
        assert_eq!(total, 2);
        assert_eq!(rows[0].result, "rejected");
        assert_eq!(rows[0].detail, "550");

        db.issue_email_code(
            "password_reset",
            "alice@example.com",
            "hash",
            &util::later_minutes(10),
            5,
        )
        .unwrap();
        assert!(
            db.last_email_code_at("alice@example.com")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            db.email_code_count_since("alice@example.com", &util::seconds_ago(60))
                .unwrap(),
            1
        );
        assert_eq!(
            db.email_code_count_since("alice@example.com", &util::later_minutes(1))
                .unwrap(),
            0
        );
    }
}
