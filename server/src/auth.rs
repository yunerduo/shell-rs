//! 账号口令与令牌：口令只存 Argon2id 的 PHC 串，令牌只存摘要。
//!
//! 「同步密钥」不在这里：它由客户端用登录口令加服务端发的盐，
//! 在客户端本地做 Argon2id 派生，服务器永远不知道口令明文。

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash};

use crate::util;

/// 注册与改口令时的口令规则：8–128 位，且至少包含两类字符
/// （小写字母、大写字母、数字、其他）。
pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 128;
/// 口令不合规时统一的提示，注册、改口令和后台共用。
pub const PASSWORD_RULE: &str = "口令要 8–128 位，且至少包含字母、数字、符号中的两类";

pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    Ok(Argon2::default()
        .hash_password(password.as_bytes())?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

/// 会话令牌：发给客户端的那串；库里存 [`token_hash`]。
pub fn token_hash(token: &str) -> String {
    util::sha256_hex(token.as_bytes())
}

pub fn valid_username(username: &str) -> bool {
    let len = username.chars().count();
    (3..=32).contains(&len)
        && username
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

pub fn valid_password(password: &str) -> bool {
    if !(PASSWORD_MIN..=PASSWORD_MAX).contains(&password.chars().count()) {
        return false;
    }
    let mut lower = false;
    let mut upper = false;
    let mut digit = false;
    let mut other = false;
    for ch in password.chars() {
        if ch.is_ascii_lowercase() {
            lower = true;
        } else if ch.is_ascii_uppercase() {
            upper = true;
        } else if ch.is_ascii_digit() {
            digit = true;
        } else {
            other = true;
        }
    }
    lower as usize + upper as usize + digit as usize + other as usize >= 2
}

/// 邮箱可选：填了就得像邮箱，不填也能注册（v1 没有邮件验证）。
pub fn valid_email(email: &str) -> bool {
    let email = email.trim();
    if email.is_empty() || email.len() > 254 || email.chars().any(char::is_whitespace) {
        return false;
    }
    match email.split_once('@') {
        Some((user, domain)) => {
            !user.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_round_trips_through_argon2() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong horse", &hash));
    }

    #[test]
    fn usernames_are_short_and_plain() {
        assert!(valid_username("alice"));
        assert!(valid_username("dev_box-2"));
        assert!(!valid_username("ab"));
        assert!(!valid_username("has space"));
        assert!(!valid_username("中文名"));
        assert!(!valid_username(&"a".repeat(33)));
    }

    #[test]
    fn passwords_need_length_and_two_character_classes() {
        assert!(!valid_password("short"));
        assert!(!valid_password("12345678"));
        assert!(!valid_password("abcdefgh"));
        assert!(valid_password("abcdefg1"));
        assert!(valid_password("abcDEF12"));
        assert!(valid_password("abcdef!@"));
        assert!(!valid_password(&"a".repeat(PASSWORD_MAX + 1)));
        assert!(valid_password(&format!(
            "a1{}",
            "x".repeat(PASSWORD_MAX - 2)
        )));
    }

    #[test]
    fn emails_must_look_like_emails() {
        assert!(valid_email("a@b.co"));
        assert!(!valid_email("@b.co"));
        assert!(!valid_email("a@b"));
        assert!(!valid_email("a b@c.co"));
    }

    #[test]
    fn the_same_token_has_the_same_digest() {
        assert_eq!(token_hash("abc"), token_hash("abc"));
        assert_ne!(token_hash("abc"), token_hash("abd"));
    }
}
