//! 服务配置：全部来自环境变量，容器里不需要配置文件。

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct Config {
    /// 监听地址。默认只绑本机，由 1Panel 的 OpenResty 反代进来。
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    /// 对外地址，写进更新清单里的安装包链接。
    pub public_url: String,
    /// 管理接口的令牌。不设就关闭管理接口。
    pub admin_token: Option<String>,
    /// 首次启动时用来创建后台管理员的账号与口令；库里已有管理员时不覆盖。
    pub admin_user: Option<String>,
    pub admin_password: Option<String>,
    /// 后台登录有效期。
    pub admin_session_days: i64,
    /// 清单签名私钥。设了以后才能在后台直接发布版本；不设只影响后台发布，
    /// 命令行 `publish` 仍可在本地签名上传。
    pub minisign_key: Option<PathBuf>,
    pub minisign_password: Option<String>,
    /// 后台前端构建产物目录（`server/admin-ui/dist`）。
    pub admin_ui_dir: PathBuf,
    /// 同一账号连续失败多少次后临时锁定；用户与管理员各记各的。
    pub login_max_failures: i64,
    pub login_lock_minutes: i64,
    /// 新用户的同步配额。
    pub default_quota_bytes: i64,
    /// 每个用户保留的快照个数，更旧的自动删掉。
    pub max_snapshots: i64,
    pub session_days: i64,
    /// 单次同步上传的硬上限。
    pub max_snapshot_bytes: u64,
    /// 单个安装包的上限。
    pub max_package_bytes: u64,
    /// 内置 AI 上游的首次播种值。只在库里还没有配置时读一次；之后
    /// 以后台保存的配置为准，改完立即生效，不必重启。
    pub ai_base_url: Option<String>,
    pub ai_api_key: Option<String>,
    pub ai_model: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let bind = env_or("SHELLAS_BIND", "127.0.0.1:8080")
            .parse()
            .context("SHELLAS_BIND 不是合法的地址，例如 127.0.0.1:8080")?;
        let data_dir = PathBuf::from(env_or("SHELLAS_DATA_DIR", "data"));
        let public_url = env_or("SHELLAS_PUBLIC_URL", "https://shellas.agree.host")
            .trim_end_matches('/')
            .to_string();
        if !public_url.starts_with("https://") && !public_url.starts_with("http://") {
            bail!("SHELLAS_PUBLIC_URL 要以 http:// 或 https:// 开头");
        }
        let admin_token = env::var("SHELLAS_ADMIN_TOKEN")
            .ok()
            .filter(|token| !token.trim().is_empty());
        if let Some(token) = &admin_token
            && token.len() < 24
        {
            bail!("SHELLAS_ADMIN_TOKEN 太短，至少 24 个字符");
        }
        let admin_user = env::var("SHELLAS_ADMIN_USER")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let admin_password = env::var("SHELLAS_ADMIN_PASSWORD")
            .ok()
            .filter(|value| !value.is_empty());
        if admin_user.is_some() != admin_password.is_some() {
            bail!("SHELLAS_ADMIN_USER 和 SHELLAS_ADMIN_PASSWORD 要同时设置");
        }
        if let Some(password) = &admin_password
            && password.chars().count() < 8
        {
            bail!("SHELLAS_ADMIN_PASSWORD 至少 8 个字符");
        }
        let minisign_key = env::var("SHELLAS_MINISIGN_KEY")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let minisign_password = env::var("SHELLAS_MINISIGN_KEY_PASSWORD")
            .ok()
            .filter(|value| !value.is_empty());
        Ok(Self {
            bind,
            data_dir,
            public_url,
            admin_token,
            admin_user,
            admin_password,
            admin_session_days: number(env_or("SHELLAS_ADMIN_SESSION_DAYS", "7"))?.max(1),
            minisign_key,
            minisign_password,
            admin_ui_dir: PathBuf::from(env_or("SHELLAS_ADMIN_UI_DIR", "admin-ui/dist")),
            login_max_failures: number(env_or("SHELLAS_LOGIN_MAX_FAILURES", "5"))?.max(1),
            login_lock_minutes: number(env_or("SHELLAS_LOGIN_LOCK_MINUTES", "15"))?.max(1),
            default_quota_bytes: mb(env_or("SHELLAS_DEFAULT_QUOTA_MB", "50"))?,
            max_snapshots: number(env_or("SHELLAS_MAX_SNAPSHOTS", "20"))?,
            session_days: number(env_or("SHELLAS_SESSION_DAYS", "30"))?,
            max_snapshot_bytes: mb_u64(env_or("SHELLAS_MAX_SNAPSHOT_MB", "100"))?,
            max_package_bytes: mb_u64(env_or("SHELLAS_MAX_PACKAGE_MB", "400"))?,
            ai_base_url: optional("SHELLAS_AI_BASE_URL"),
            ai_api_key: optional("SHELLAS_AI_API_KEY"),
            ai_model: optional("SHELLAS_AI_MODEL"),
        })
    }

    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("shellas.db")
    }

    pub fn sync_dir(&self) -> PathBuf {
        self.data_dir.join("sync")
    }

    pub fn releases_dir(&self) -> PathBuf {
        self.data_dir.join("releases")
    }

    /// 清单的落盘副本，方便直接看和备份；对外服务读数据库。
    pub fn manifests_dir(&self) -> PathBuf {
        self.data_dir.join("update").join("v1")
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

fn optional(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn number(text: String) -> Result<i64> {
    text.trim()
        .parse()
        .with_context(|| format!("这不是一个整数：{text}"))
}

fn mb(text: String) -> Result<i64> {
    Ok(number(text)? * 1024 * 1024)
}

fn mb_u64(text: String) -> Result<u64> {
    let megabytes = number(text.clone())?;
    let megabytes = u64::try_from(megabytes).with_context(|| format!("不能是负数：{text}"))?;
    Ok(megabytes * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn megabytes_become_bytes() {
        assert_eq!(mb("50".into()).unwrap(), 50 * 1024 * 1024);
        assert!(mb("many".into()).is_err());
    }
}
