//! 解开 Xshell 会话里保存的密码。
//!
//! 会话里的 `Password` 是 `Base64(RC4(明文) ‖ SHA-256(明文))`，RC4 的密钥
//! 是主密码 UTF-8 字节的 SHA-256。没有主密码、主密码不对，或这条会话是用
//! 别的主密码保存的，解不开：调用方退回“首次连接时询问”。

use base64::Engine as _;
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

/// 存储值末尾的明文校验值长度。
const TAG_LEN: usize = 32;

/// 用主密码解开一条存储值。解出的明文只在内存里，交给调用方写入钥匙串。
pub(super) fn decrypt(master: &str, stored: &str) -> Option<Zeroizing<String>> {
    if master.is_empty() {
        return None;
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(stored.trim().as_bytes())
        .ok()?;
    if data.len() <= TAG_LEN {
        return None;
    }
    let (cipher, tag) = data.split_at(data.len() - TAG_LEN);
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&Sha256::digest(master.as_bytes()));
    let plain = rc4(&key[..], cipher);
    if Sha256::digest(plain.as_slice()).as_slice() != tag {
        return None;
    }
    let text = std::str::from_utf8(plain.as_slice()).ok()?;
    if text.is_empty() {
        return None;
    }
    Some(Zeroizing::new(text.to_string()))
}

/// RC4 的加解密是同一条流。明文和密钥都在离开作用域时清零。
fn rc4(key: &[u8], data: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut state = [0u8; 256];
    for (index, byte) in state.iter_mut().enumerate() {
        *byte = index as u8;
    }
    let mut j = 0usize;
    for i in 0..256 {
        j = (j + state[i] as usize + key[i % key.len()] as usize) & 0xff;
        state.swap(i, j);
    }
    let mut out = Zeroizing::new(Vec::with_capacity(data.len()));
    let (mut i, mut j) = (0usize, 0usize);
    for byte in data {
        i = (i + 1) & 0xff;
        j = (j + state[i] as usize) & 0xff;
        state.swap(i, j);
        let mixed = state[(state[i] as usize + state[j] as usize) & 0xff];
        out.push(byte ^ mixed);
    }
    out
}

/// 只用于测试：用和来源相同的构造封一条存储值，测试不必碰真实主密码。
#[cfg(test)]
pub(super) fn seal(master: &str, plain: &str) -> String {
    let mut key = Zeroizing::new([0u8; 32]);
    key.copy_from_slice(&Sha256::digest(master.as_bytes()));
    let mut stored = rc4(&key[..], plain.as_bytes()).to_vec();
    stored.extend_from_slice(&Sha256::digest(plain.as_bytes()));
    base64::engine::general_purpose::STANDARD.encode(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_master_password_opens_what_the_same_one_sealed() {
        let stored = seal("test master", "Test1234");
        let plain = decrypt("test master", &stored).expect("opens");
        assert_eq!(plain.as_str(), "Test1234");
    }

    #[test]
    fn a_wrong_or_missing_master_password_opens_nothing() {
        let stored = seal("test master", "Test1234");
        assert!(decrypt("other master", &stored).is_none());
        assert!(decrypt("", &stored).is_none());
    }

    #[test]
    fn a_damaged_or_foreign_value_opens_nothing() {
        let stored = seal("test master", "Test1234");
        let mut bytes = base64::engine::general_purpose::STANDARD
            .decode(stored)
            .unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        let damaged = base64::engine::general_purpose::STANDARD.encode(bytes);
        assert!(decrypt("test master", &damaged).is_none());
        assert!(decrypt("test master", "not base64!").is_none());
        assert!(decrypt("test master", "AAAA").is_none());
    }

    #[test]
    fn an_empty_password_is_not_a_password() {
        let stored = seal("test master", "");
        assert!(decrypt("test master", &stored).is_none());
    }
}
