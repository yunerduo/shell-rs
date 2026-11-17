//! 各处共用的小工具：时间、摘要、随机数、HTML 转义。

use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use sha2::{Digest, Sha256};

/// 现在，RFC 3339 的 UTC 秒级时间，例如 `2026-11-16T08:00:00Z`。
///
/// 库里所有时间都是这个格式：定长、带 `Z`，字符串比较即时间先后。
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// 一个 RFC 3339 时间往后推 `days` 天。
pub fn later(days: i64) -> String {
    format_at(Utc::now() + chrono::Duration::days(days))
}

/// 从现在往后推 `minutes` 分钟。
pub fn later_minutes(minutes: i64) -> String {
    format_at(Utc::now() + chrono::Duration::minutes(minutes))
}

/// 从现在往前推 `days` 天，用来清理过期的登录失败记录。
pub fn days_ago(days: i64) -> String {
    format_at(Utc::now() - chrono::Duration::days(days))
}

fn format_at(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// 一个 RFC 3339 时间是否已经过去。
pub fn is_past(at: &str) -> bool {
    DateTime::parse_from_rfc3339(at).is_ok_and(|at| at < Utc::now())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// 令牌与盐都从系统随机源来：32 字节的令牌，16 字节的盐。
pub fn random_bytes<const N: usize>() -> [u8; N] {
    rand::random()
}

/// 给客户端的会话令牌：base64url，无填充。
pub fn random_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<32>())
}

/// 同步密钥的盐。服务器公开它，但永远不用它算密钥。
pub fn random_salt() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<16>())
}

/// 用户内容写进 HTML 前一律转义。
pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

/// 恒定时间比较，给管理员令牌和令牌摘要用。
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// `12.3 MB`，给人看的体积。
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes / MB)
    } else if bytes >= KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{bytes:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_sorts_as_text() {
        let a = now();
        let b = later(30);
        assert!(a < b, "{a} {b}");
        assert!(!is_past(&b));
        assert!(is_past("2020-01-01T00:00:00Z"));
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(
            escape_html(r#"<a href="x">'&'"#),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;"
        );
    }

    #[test]
    fn tokens_differ_and_are_url_safe() {
        let a = random_token();
        let b = random_token();
        assert_ne!(a, b);
        assert!(!a.contains('+') && !a.contains('/') && !a.contains('='));
        assert_eq!(a.len(), 43);
    }

    #[test]
    fn constant_time_comparison_is_still_correct() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
    }

    #[test]
    fn sizes_read_as_people_write_them() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    }
}
