//! SQLite：用户、会话、同步快照的元数据和更新清单。
//!
//! 快照本体和安装包都在磁盘上，库里只有元数据。连接只有一个，包在
//! `Mutex` 里：这台服务的写入都是毫秒级的小事务，串行反而简单可靠；
//! 文件读写不占着这把锁。

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

use crate::util;

/// 正在升级的库版本，见 [`migrate`]。
const SCHEMA_VERSION: i64 = 2;

#[derive(Clone, Debug)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    pub password_hash: String,
    pub quota_bytes: i64,
    pub sync_salt: String,
    pub created_at: String,
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

#[derive(Debug, Error)]
pub enum SnapshotError {
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

    pub fn create_user(
        &self,
        username: &str,
        email: Option<&str>,
        password_hash: &str,
        sync_salt: &str,
        quota_bytes: i64,
    ) -> rusqlite::Result<User> {
        let created_at = util::now();
        self.conn().execute(
            "INSERT INTO users (username, email, password_hash, quota_bytes, sync_salt, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                username,
                email,
                password_hash,
                quota_bytes,
                sync_salt,
                created_at
            ],
        )?;
        Ok(User {
            id: self.conn().last_insert_rowid(),
            username: username.to_string(),
            email: email.map(str::to_string),
            password_hash: password_hash.to_string(),
            quota_bytes,
            sync_salt: sync_salt.to_string(),
            created_at,
        })
    }

    pub fn user_by_name(&self, username: &str) -> rusqlite::Result<Option<User>> {
        self.conn()
            .query_row(
                "SELECT id, username, email, password_hash, quota_bytes, sync_salt, created_at
                 FROM users WHERE username = ?1 COLLATE NOCASE",
                [username],
                user_row,
            )
            .optional()
    }

    /// 令牌摘要对应的用户；顺手记一次使用时间，过期会话当场删掉。
    pub fn session_user(&self, token_hash: &str) -> rusqlite::Result<Option<User>> {
        let conn = self.conn();
        let found = conn
            .query_row(
                "SELECT u.id, u.username, u.email, u.password_hash, u.quota_bytes,
                        u.sync_salt, u.created_at, s.expires_at
                 FROM sessions s JOIN users u ON u.id = s.user_id
                 WHERE s.token_hash = ?1",
                [token_hash],
                |row| {
                    let expires_at: String = row.get(7)?;
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

    pub fn used_bytes(&self, user_id: i64) -> rusqlite::Result<i64> {
        self.conn().query_row(
            "SELECT COALESCE(SUM(size), 0) FROM snapshots WHERE user_id = ?1",
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
            "SELECT COALESCE(SUM(size), 0) FROM snapshots WHERE user_id = ?1",
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

    /// 存一个通道的签名清单（信封：清单原文 + minisign 签名）。
    pub fn put_manifest(
        &self,
        channel: &str,
        version: &str,
        envelope: &str,
        published_at: &str,
    ) -> rusqlite::Result<()> {
        self.conn().execute(
            "INSERT INTO manifests (channel, version, envelope, published_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(channel, version) DO UPDATE SET
               envelope = excluded.envelope,
               published_at = excluded.published_at",
            params![channel, version, envelope, published_at],
        )?;
        Ok(())
    }

    /// 一个通道当前的清单信封，最后发布的那个。
    pub fn current_manifest(&self, channel: &str) -> rusqlite::Result<Option<String>> {
        self.conn()
            .query_row(
                "SELECT envelope FROM manifests WHERE channel = ?1
                 ORDER BY published_at DESC, rowid DESC LIMIT 1",
                [channel],
                |row| row.get(0),
            )
            .optional()
    }

    /// 更新日志用：发布过的清单，新的在前。
    pub fn manifest_history(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT channel, envelope FROM manifests
             ORDER BY published_at DESC, rowid DESC LIMIT 200",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect()
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
    })
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
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        Db::in_memory()
    }

    fn user(db: &Db) -> User {
        db.create_user("alice", None, "$argon2id$hash", "salt", 1024)
            .unwrap()
    }

    #[test]
    fn a_user_is_found_by_name_without_case() {
        let db = db();
        user(&db);
        assert!(db.user_by_name("ALICE").unwrap().is_some());
        assert!(db.user_by_name("bob").unwrap().is_none());
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
        assert_eq!(db.current_manifest("beta").unwrap(), None);
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
            sdk_version: "shellrs@0.1.19".into(),
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
}
