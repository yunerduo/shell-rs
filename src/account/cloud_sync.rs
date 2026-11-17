//! The cloud sync documents: what is uploaded and how it is sealed.
//!
//! The server never sees the plain document. The sync password derives a
//! key with Argon2id over the account's `sync_salt`; the document is sealed
//! with AES-256-GCM, bound to the account format by its associated data.
//! Nothing here runs when the account is signed out or the machine is
//! offline: cloud sync is an optional feature, never a prerequisite.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use argon2::{Algorithm, Argon2, Params, Version as ArgonVersion};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

use crate::host::HostStore;
use crate::settings::AppSettings;

/// Bound into every sealed document, so a ciphertext of another format or
/// version cannot be passed off as one of these.
const AAD: &[u8] = b"shellas-sync-v1";

/// AES-GCM's nonce length.
const NONCE_LEN: usize = 12;

/// Argon2id's cost, matching the server's documentation: 64 MiB, three
/// passes.
const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;
const PARALLELISM: u32 = 1;

/// The format marker of the hosts document. It is for review and backup,
/// not the sync core's device-file format.
const HOSTS_FORMAT: &str = "shellas-cloud-hosts";
const HOSTS_VERSION: i64 = 1;

/// What sealing or opening a document failed with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CryptoError {
    BadSalt,
    Key(argon2::Error),
    Cipher(String),
    TooShort,
}

impl std::fmt::Display for CryptoError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadSalt => formatter.write_str("bad sync salt"),
            Self::Key(error) => write!(formatter, "key derivation failed: {error}"),
            Self::Cipher(error) => write!(formatter, "cipher failed: {error}"),
            Self::TooShort => formatter.write_str("sealed document is too short"),
        }
    }
}

/// Derive the AES key from the sync password and the account's salt.
pub fn derive_key(password: &str, sync_salt: &str) -> Result<Zeroizing<[u8; 32]>, CryptoError> {
    let salt = URL_SAFE_NO_PAD
        .decode(sync_salt.trim())
        .map_err(|_| CryptoError::BadSalt)?;
    if salt.is_empty() {
        return Err(CryptoError::BadSalt);
    }
    let params =
        Params::new(MEMORY_KIB, PASSES, PARALLELISM, Some(32)).map_err(CryptoError::Key)?;
    let argon = Argon2::new(Algorithm::Argon2id, ArgonVersion::V0x13, params);
    let mut key = Zeroizing::new([0u8; 32]);
    argon
        .hash_password_into(password.as_bytes(), &salt, &mut *key)
        .map_err(CryptoError::Key)?;
    Ok(key)
}

/// `nonce || ciphertext`, as the document is uploaded.
pub fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|error| CryptoError::Cipher(error.to_string()))?;
    let nonce_bytes: [u8; NONCE_LEN] = rand::random();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let sealed = cipher
        .encrypt(
            nonce,
            Payload {
                msg: plaintext,
                aad: AAD,
            },
        )
        .map_err(|error| CryptoError::Cipher(error.to_string()))?;
    let mut out = Vec::with_capacity(NONCE_LEN + sealed.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// Open what [`seal`] wrote. A wrong password fails here rather than
/// producing garbage.
pub fn open(key: &[u8; 32], sealed: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if sealed.len() <= NONCE_LEN {
        return Err(CryptoError::TooShort);
    }
    let cipher =
        Aes256Gcm::new_from_slice(key).map_err(|error| CryptoError::Cipher(error.to_string()))?;
    let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: ciphertext,
                aad: AAD,
            },
        )
        .map_err(|error| CryptoError::Cipher(error.to_string()))
}

/// The `X-Snapshot-Sha256` header the upload needs.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The settings document: the `settings.json` shape, as it is on this
/// computer.
pub fn settings_document(settings: &AppSettings) -> Result<Vec<u8>, String> {
    serde_json::to_vec(settings).map_err(|error| error.to_string())
}

/// A settings document read back from the cloud.
pub fn read_settings_document(bytes: &[u8]) -> Result<AppSettings, String> {
    serde_json::from_slice(bytes).map_err(|error| error.to_string())
}

/// The hosts document: the hosts and groups as data, without credentials,
/// keys or any other secret. A cloud copy is for review and backup here;
/// the sync core's device files remain the way hosts travel between
/// computers.
pub fn hosts_document(store: &HostStore) -> Vec<u8> {
    let data = store;
    let hosts: Vec<Value> = data
        .hosts()
        .iter()
        .map(|host| {
            json!({
                "id": host.id.0,
                "public_id": host.public_id.as_str(),
                "name": host.name.to_string(),
                "protocol": host.protocol.as_str(),
                "address": host.address.to_string(),
                "port": host.port,
                "user": host.user.to_string(),
                "auth": host.auth.as_str(),
                "credential": host.credential.map(|id| id.0),
                "group": host.group.map(|id| id.0),
                "notes": host.notes.to_string(),
                "sort_order": host.sort_order,
                "os": host.os.map(|os| os.as_str()),
                "directory_follow": host.directory_follow,
            })
        })
        .collect();
    let groups: Vec<Value> = data
        .groups()
        .iter()
        .map(|group| {
            json!({
                "id": group.id.0,
                "name": group.name.to_string(),
                "parent": group.parent.map(|id| id.0),
                "sort_order": group.sort_order,
            })
        })
        .collect();
    let document = json!({
        "format": HOSTS_FORMAT,
        "version": HOSTS_VERSION,
        "exported_at": chrono::Utc::now().to_rfc3339(),
        "hosts": hosts,
        "groups": groups,
    });
    serde_json::to_vec_pretty(&document).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{open, seal};

    fn salt() -> String {
        base64::Engine::encode(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD,
            b"a-salt-for-tests",
        )
    }

    #[test]
    fn a_document_round_trips_through_the_cipher() {
        let key = super::derive_key("correct horse", &salt()).unwrap();
        let sealed = seal(&key, br#"{"theme":"dark"}"#).unwrap();
        assert_eq!(open(&key, &sealed).unwrap(), br#"{"theme":"dark"}"#);
    }

    #[test]
    fn the_wrong_password_does_not_open_it() {
        let salt = salt();
        let key = super::derive_key("correct horse", &salt).unwrap();
        let other = super::derive_key("wrong horse", &salt).unwrap();
        let sealed = seal(&key, b"secret").unwrap();
        assert!(open(&other, &sealed).is_err());
    }

    #[test]
    fn a_tampered_document_does_not_open_it() {
        let key = super::derive_key("correct horse", &salt()).unwrap();
        let mut sealed = seal(&key, b"secret").unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        assert!(open(&key, &sealed).is_err());
    }

    #[test]
    fn a_bad_salt_is_refused() {
        assert!(super::derive_key("x", "not base64!").is_err());
    }
}
