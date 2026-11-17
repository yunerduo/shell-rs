//! The file that carries saved hosts and groups to another machine: one JSON
//! document, optionally sealed with a password the user chooses.
//!
//! In the clear it holds names, addresses and key *paths* only. Passwords,
//! key passphrases and the text of keys ShellAs keeps are secrets, so they
//! are written only into a sealed file; [`encode`] refuses to write them in
//! the clear.
//!
//! ```text
//! {
//!   "format": 1,
//!   "kind": "agree-shellas-hosts",
//!   "encrypted": false,
//!   "groups": [ { "id": "1", "name": "生产", "parent": null }, … ],
//!   "hosts":  [ { "name": "web-01", "address": "10.0.0.5", "port": 22,
//!                 "user": "root", "auth": "password", "group": "1", … } ]
//! }
//! ```
//!
//! Sealed file: the same header, then the Argon2id parameters, a salt, a
//! nonce and the body encrypted with XChaCha20-Poly1305 (the parameters sync's
//! vault uses). The header is authenticated with the body, so a sealed file
//! relabelled as another format fails to open instead of yielding garbage.

use std::fmt;

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead as _, KeyInit as _, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

/// What this writer writes and the newest this reader takes.
pub const FORMAT: u32 = 1;
/// Tells this file from any other JSON.
pub const KIND: &str = "agree-shellas-hosts";

const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

const MEMORY_KIB: u32 = 64 * 1024;
const PASSES: u32 = 3;
const PARALLELISM: u32 = 1;
/// The most a sealed file may ask of this machine, so one that was tampered
/// with cannot make it run out of memory or spin.
const MEMORY_KIB_MAX: u32 = 256 * 1024;
const PASSES_MAX: u32 = 10;
const PARALLELISM_MAX: u32 = 4;

/// One group, by an id the file makes up; `parent` names another of the
/// file's ids, so names need no escaping and the tree is kept exactly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedGroup {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// One saved host. `group` is one of the file's group ids.
///
/// `auth` is `"password"`, `"none"`, `"key"` or `"agent"`, the same spelling
/// [`crate::CredentialKind`] uses where it applies.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivedHost {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub user: String,
    pub auth: String,
    /// The key file as configured, for `auth == "key"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_path: Option<String>,
    /// The text of a key ShellAs keeps itself, so the other machine has it.
    /// A secret: sealed files only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<String>,
    /// The key file's passphrase. A secret: sealed files only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passphrase: Option<String>,
    /// The login password, saved in the keychain. A secret: sealed files only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// The detected system, as the database spells it (`ubuntu`, `debian`…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl fmt::Debug for ArchivedHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArchivedHost")
            .field("name", &self.name)
            .field("address", &self.address)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("auth", &self.auth)
            .field("key_path", &self.key_path)
            .field(
                "private_key",
                &self.private_key.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "passphrase",
                &self.passphrase.as_ref().map(|_| "<redacted>"),
            )
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("os", &self.os)
            .field("notes", &self.notes)
            .field("group", &self.group)
            .finish()
    }
}

/// What a file holds: the groups in file order, and the hosts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Archive {
    pub groups: Vec<ArchivedGroup>,
    pub hosts: Vec<ArchivedHost>,
}

impl Archive {
    /// Whether it carries anything that must not be written in the clear.
    pub fn has_secrets(&self) -> bool {
        self.hosts.iter().any(|host| {
            host.password.is_some() || host.passphrase.is_some() || host.private_key.is_some()
        })
    }
}

/// Why a file cannot be written or read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveError {
    /// Not JSON, or some other file's JSON.
    NotArchive,
    /// Written by a newer ShellAs.
    TooNew,
    /// Ours, but cut short or with a part missing.
    Damaged,
    /// Sealed: it needs the password it was written with.
    NeedsPassword,
    /// The password does not open it, or the file was changed after sealing.
    WrongPassword,
    /// [`encode`] was asked for secrets without a password to seal them.
    SecretWithoutPassword,
}

/// Write `archive` as the text of the file. `password` seals the whole body;
/// without one the file is readable, and secrets are refused rather than
/// leaked.
pub fn encode(archive: &Archive, password: Option<&str>) -> Result<String, ArchiveError> {
    let body = serde_json::to_vec(&Body {
        groups: archive.groups.clone(),
        hosts: archive.hosts.clone(),
    })
    .map_err(|_| ArchiveError::Damaged)?;
    match password {
        None => {
            if archive.has_secrets() {
                return Err(ArchiveError::SecretWithoutPassword);
            }
            let envelope = Envelope {
                format: FORMAT,
                kind: KIND.to_owned(),
                encrypted: false,
                kdf: None,
                nonce: None,
                payload: None,
                groups: Some(archive.groups.clone()),
                hosts: Some(archive.hosts.clone()),
            };
            serde_json::to_string_pretty(&envelope).map_err(|_| ArchiveError::Damaged)
        }
        Some(password) => {
            let salt: [u8; SALT_LEN] = rand::random();
            let kdf = Kdf {
                algorithm: "argon2id".to_owned(),
                memory_kib: MEMORY_KIB,
                passes: PASSES,
                parallelism: PARALLELISM,
                salt: STANDARD.encode(salt),
            };
            let key = derive(password, &salt, MEMORY_KIB, PASSES, PARALLELISM)?;
            let nonce: [u8; NONCE_LEN] = rand::random();
            let sealed = XChaCha20Poly1305::new_from_slice(&*key)
                .map_err(|_| ArchiveError::Damaged)?
                .encrypt(
                    &XNonce::from(nonce),
                    Payload {
                        msg: &body,
                        aad: aad(),
                    },
                )
                .map_err(|_| ArchiveError::Damaged)?;
            let envelope = Envelope {
                format: FORMAT,
                kind: KIND.to_owned(),
                encrypted: true,
                kdf: Some(kdf),
                nonce: Some(STANDARD.encode(nonce)),
                payload: Some(STANDARD.encode(sealed)),
                groups: None,
                hosts: None,
            };
            serde_json::to_string_pretty(&envelope).map_err(|_| ArchiveError::Damaged)
        }
    }
}

/// Read the archive in `text`, unsealing it with `password` when it is
/// sealed. Every failure leaves nothing half read: the body is authenticated
/// as a whole.
pub fn decode(text: &str, password: Option<&str>) -> Result<Archive, ArchiveError> {
    let envelope: Envelope = serde_json::from_str(text).map_err(|_| ArchiveError::NotArchive)?;
    if envelope.kind != KIND {
        return Err(ArchiveError::NotArchive);
    }
    if envelope.format > FORMAT {
        return Err(ArchiveError::TooNew);
    }
    if envelope.format == 0 {
        return Err(ArchiveError::NotArchive);
    }
    if !envelope.encrypted {
        return Ok(Archive {
            groups: envelope.groups.ok_or(ArchiveError::Damaged)?,
            hosts: envelope.hosts.ok_or(ArchiveError::Damaged)?,
        });
    }
    let kdf = envelope.kdf.ok_or(ArchiveError::Damaged)?;
    if kdf.algorithm != "argon2id" {
        return Err(ArchiveError::Damaged);
    }
    let salt: [u8; SALT_LEN] = STANDARD
        .decode(&kdf.salt)
        .ok()
        .and_then(|salt| salt.try_into().ok())
        .ok_or(ArchiveError::Damaged)?;
    let nonce: [u8; NONCE_LEN] = envelope
        .nonce
        .as_deref()
        .and_then(|nonce| STANDARD.decode(nonce).ok())
        .and_then(|nonce| nonce.try_into().ok())
        .ok_or(ArchiveError::Damaged)?;
    let payload = envelope
        .payload
        .as_deref()
        .and_then(|payload| STANDARD.decode(payload).ok())
        .ok_or(ArchiveError::Damaged)?;
    let password = password.ok_or(ArchiveError::NeedsPassword)?;
    let key = derive(password, &salt, kdf.memory_kib, kdf.passes, kdf.parallelism)?;
    let body = XChaCha20Poly1305::new_from_slice(&*key)
        .map_err(|_| ArchiveError::Damaged)?
        .decrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &payload,
                aad: aad(),
            },
        )
        .map_err(|_| ArchiveError::WrongPassword)?;
    let body: Body = serde_json::from_slice(&body).map_err(|_| ArchiveError::WrongPassword)?;
    Ok(Archive {
        groups: body.groups,
        hosts: body.hosts,
    })
}

fn derive(
    password: &str,
    salt: &[u8],
    memory_kib: u32,
    passes: u32,
    parallelism: u32,
) -> Result<Zeroizing<[u8; KEY_LEN]>, ArchiveError> {
    let within = memory_kib <= MEMORY_KIB_MAX
        && (1..=PASSES_MAX).contains(&passes)
        && (1..=PARALLELISM_MAX).contains(&parallelism);
    if !within {
        return Err(ArchiveError::Damaged);
    }
    let params = Params::new(memory_kib, passes, parallelism, Some(KEY_LEN))
        .map_err(|_| ArchiveError::Damaged)?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = Zeroizing::new([0u8; KEY_LEN]);
    argon
        .hash_password_into(password.as_bytes(), salt, &mut *key)
        .map_err(|_| ArchiveError::Damaged)?;
    Ok(key)
}

/// What the header is authenticated with, so it cannot be relabelled.
fn aad() -> &'static [u8] {
    b"agree-shellas-hosts:1"
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    format: u32,
    kind: String,
    #[serde(default)]
    encrypted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kdf: Option<Kdf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    groups: Option<Vec<ArchivedGroup>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hosts: Option<Vec<ArchivedHost>>,
}

#[derive(Serialize, Deserialize)]
struct Kdf {
    algorithm: String,
    memory_kib: u32,
    passes: u32,
    parallelism: u32,
    salt: String,
}

#[derive(Serialize, Deserialize)]
struct Body {
    groups: Vec<ArchivedGroup>,
    hosts: Vec<ArchivedHost>,
}

#[cfg(test)]
mod tests {
    use super::{Archive, ArchiveError, ArchivedGroup, ArchivedHost, KIND, decode, encode};

    fn host(name: &str) -> ArchivedHost {
        ArchivedHost {
            name: name.to_owned(),
            address: "10.0.0.5".to_owned(),
            port: 22,
            user: "root".to_owned(),
            auth: "password".to_owned(),
            key_path: None,
            private_key: None,
            passphrase: None,
            password: None,
            os: Some("ubuntu".to_owned()),
            notes: String::new(),
            group: Some("g1".to_owned()),
        }
    }

    fn archive() -> Archive {
        Archive {
            groups: vec![
                ArchivedGroup {
                    id: "g1".to_owned(),
                    name: "生产".to_owned(),
                    parent: None,
                },
                ArchivedGroup {
                    id: "g2".to_owned(),
                    name: "华东".to_owned(),
                    parent: Some("g1".to_owned()),
                },
            ],
            hosts: vec![host("web-01")],
        }
    }

    #[test]
    fn a_plain_file_round_trips_and_says_what_it_is() {
        let text = encode(&archive(), None).unwrap();
        assert!(text.contains(KIND));
        assert!(!text.contains("\"encrypted\": true"));
        assert_eq!(decode(&text, None).unwrap(), archive());
    }

    #[test]
    fn secrets_are_refused_without_a_password_to_seal_them() {
        let mut archive = archive();
        archive.hosts[0].password = Some("hunter2".to_owned());
        assert_eq!(
            encode(&archive, None),
            Err(ArchiveError::SecretWithoutPassword)
        );
        let text = encode(&archive, Some("opensesame")).unwrap();
        assert!(!text.contains("hunter2"));
        assert_eq!(decode(&text, Some("opensesame")).unwrap(), archive);
    }

    #[test]
    fn a_sealed_file_needs_the_password_it_was_written_with() {
        let mut archive = archive();
        archive.hosts[0].passphrase = Some("key-pass".to_owned());
        let text = encode(&archive, Some("correct horse")).unwrap();
        assert_eq!(decode(&text, None), Err(ArchiveError::NeedsPassword));
        assert_eq!(
            decode(&text, Some("wrong horse")),
            Err(ArchiveError::WrongPassword)
        );
        assert!(!text.contains("key-pass"));
    }

    #[test]
    fn a_changed_body_does_not_half_open() {
        let mut archive = archive();
        archive.hosts[0].password = Some("hunter2".to_owned());
        let text = encode(&archive, Some("opensesame")).unwrap();
        // Flip one base64 character of the payload.
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let mut payload = value["payload"].as_str().unwrap().to_owned();
        let last = payload.pop().unwrap();
        payload.push(if last == 'A' { 'B' } else { 'A' });
        let mut tampered = value.clone();
        tampered["payload"] = serde_json::Value::String(payload);
        let tampered = serde_json::to_string(&tampered).unwrap();
        assert_eq!(
            decode(&tampered, Some("opensesame")),
            Err(ArchiveError::WrongPassword)
        );
    }

    #[test]
    fn another_file_and_a_newer_one_are_told_apart() {
        assert_eq!(decode("{}", None), Err(ArchiveError::NotArchive));
        assert_eq!(
            decode(r#"{"format":99,"kind":"agree-shellas-hosts"}"#, None),
            Err(ArchiveError::TooNew)
        );
        assert_eq!(
            decode(r#"{"format":1,"kind":"something-else"}"#, None),
            Err(ArchiveError::NotArchive)
        );
    }
}
