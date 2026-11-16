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
    /// 新用户的同步配额。
    pub default_quota_bytes: i64,
    /// 每个用户保留的快照个数，更旧的自动删掉。
    pub max_snapshots: i64,
    pub session_days: i64,
    /// 单次同步上传的硬上限。
    pub max_snapshot_bytes: u64,
    /// 单个安装包的上限。
    pub max_package_bytes: u64,
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
        Ok(Self {
            bind,
            data_dir,
            public_url,
            admin_token,
            default_quota_bytes: mb(env_or("SHELLAS_DEFAULT_QUOTA_MB", "50"))?,
            max_snapshots: number(env_or("SHELLAS_MAX_SNAPSHOTS", "20"))?,
            session_days: number(env_or("SHELLAS_SESSION_DAYS", "30"))?,
            max_snapshot_bytes: mb_u64(env_or("SHELLAS_MAX_SNAPSHOT_MB", "100"))?,
            max_package_bytes: mb_u64(env_or("SHELLAS_MAX_PACKAGE_MB", "400"))?,
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
