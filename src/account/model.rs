//! The account as it is kept on this computer, and what the forms accept.
//!
//! The account is optional. Everything here works with no server, no
//! network and no login: the profile is a small JSON file beside the
//! database, and the login token lives in the system keychain only.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::app::data_dir;

/// The account's public state, as `account.json` keeps it. It never holds
/// the login token or the sync password.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AccountProfile {
    pub username: String,
    pub email: String,
    pub nickname: String,
    /// Relative to the data directory, e.g. `account/avatars/<uuid>.png`.
    pub avatar: Option<String>,
    /// base64url, from the server: the salt the sync key is derived with.
    pub sync_salt: String,
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub cloud_sync: bool,
    /// Whether the cloud sync password is kept in the keychain. Off by
    /// default: it stays in memory for the session only.
    pub remember_sync_password: bool,
    pub settings_version: Option<i64>,
    pub hosts_version: Option<i64>,
    pub last_sync_at: Option<String>,
}

impl AccountProfile {
    /// What the AI assistant calls the user: the nickname, else the
    /// username.
    pub fn display_name(&self) -> &str {
        let nickname = self.nickname.trim();
        if nickname.is_empty() {
            &self.username
        } else {
            nickname
        }
    }

    /// The avatar file, when one was chosen and is still there.
    pub fn avatar_path(&self) -> Option<PathBuf> {
        let avatar = self.avatar.as_deref()?;
        let path = data_dir().join(avatar);
        path.is_file().then_some(path)
    }

    /// The initial on the placeholder avatar.
    pub fn initial(&self) -> String {
        self.display_name()
            .chars()
            .next()
            .map(|ch| ch.to_uppercase().to_string())
            .unwrap_or_default()
    }

    /// A missing file is a fresh start, not an error: a first launch has no
    /// account.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::from_json(&text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.to_string()),
        }
    }

    /// Every field is read on its own, so one field of the wrong type does
    /// not throw the rest away.
    pub fn from_json(text: &str) -> Self {
        let Ok(Value::Object(object)) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        let string = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let number = |key: &str| object.get(key).and_then(Value::as_u64).unwrap_or(0);
        let version = |key: &str| object.get(key).and_then(Value::as_i64);
        let optional_string = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        Self {
            username: string("username"),
            email: string("email"),
            nickname: string("nickname"),
            avatar: optional_string("avatar"),
            sync_salt: string("sync_salt"),
            quota_bytes: number("quota_bytes"),
            used_bytes: number("used_bytes"),
            cloud_sync: object
                .get("cloud_sync")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            remember_sync_password: object
                .get("remember_sync_password")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            settings_version: version("settings_version"),
            hosts_version: version("hosts_version"),
            last_sync_at: optional_string("last_sync_at"),
        }
    }

    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        object.insert("username".into(), self.username.clone().into());
        object.insert("email".into(), self.email.clone().into());
        object.insert("nickname".into(), self.nickname.clone().into());
        if let Some(avatar) = &self.avatar {
            object.insert("avatar".into(), avatar.clone().into());
        }
        object.insert("sync_salt".into(), self.sync_salt.clone().into());
        object.insert("quota_bytes".into(), self.quota_bytes.into());
        object.insert("used_bytes".into(), self.used_bytes.into());
        object.insert("cloud_sync".into(), self.cloud_sync.into());
        object.insert(
            "remember_sync_password".into(),
            self.remember_sync_password.into(),
        );
        if let Some(version) = self.settings_version {
            object.insert("settings_version".into(), version.into());
        }
        if let Some(version) = self.hosts_version {
            object.insert("hosts_version".into(), version.into());
        }
        if let Some(at) = &self.last_sync_at {
            object.insert("last_sync_at".into(), at.clone().into());
        }
        Value::Object(object).to_string()
    }

    /// Write the file whole, through a temporary one beside it, so a crash
    /// never leaves half a profile.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, self.to_json()).map_err(|error| error.to_string())?;
        std::fs::rename(&temporary, path).map_err(|error| error.to_string())
    }
}

/// Where the account stands in a login or sign-out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AccountPhase {
    /// No account, or a token that no longer works. Every local feature is
    /// available; only the cloud features are not.
    #[default]
    SignedOut,
    /// A request is in flight.
    SigningIn,
    /// A token is held and the profile is known.
    SignedIn,
}

/// Why the account went back to signed out, for the popover to explain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccountNotice {
    /// The server no longer accepts the token.
    Expired,
    /// The server could not be reached. This is not an error: an offline or
    /// intranet machine is a supported way to use ShellAs.
    Unreachable,
}

/// What the sidebar button, the popover and the account page need, as a
/// plain snapshot: render callbacks never read the store itself.
#[derive(Clone, Debug, Default)]
pub struct AccountSnapshot {
    pub phase: AccountPhase,
    pub signed_in: bool,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub nickname: String,
    pub avatar: Option<PathBuf>,
    pub initial: String,
    pub cloud_sync: bool,
    pub remember_sync_password: bool,
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub settings_version: Option<i64>,
    pub hosts_version: Option<i64>,
    pub last_sync_at: Option<String>,
    pub notice: Option<AccountNotice>,
}

/// Why a username cannot be used. The server accepts 3–32 ASCII letters,
/// digits, `_` and `-`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsernameProblem {
    Empty,
    TooShort,
    TooLong,
    NotPlain,
}

/// Why a password cannot be used, on registration or on sign-in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PasswordProblem {
    Empty,
    TooShort,
    TooLong,
}

/// Why an e-mail address cannot be used. Empty is fine: it is optional.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailProblem {
    TooLong,
    NotAddress,
}

pub const USERNAME_MIN: usize = 3;
pub const USERNAME_MAX: usize = 32;
pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 128;
pub const EMAIL_MAX: usize = 254;

pub fn check_username(username: &str) -> Result<(), UsernameProblem> {
    let username = username.trim();
    let length = username.chars().count();
    if username.is_empty() {
        return Err(UsernameProblem::Empty);
    }
    if length < USERNAME_MIN {
        return Err(UsernameProblem::TooShort);
    }
    if length > USERNAME_MAX {
        return Err(UsernameProblem::TooLong);
    }
    if !username
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return Err(UsernameProblem::NotPlain);
    }
    Ok(())
}

/// Sign-in accepts any non-empty password; only registration imposes the
/// server's length rule, so an older account is never locked out here.
pub fn check_login_password(password: &str) -> Result<(), PasswordProblem> {
    if password.is_empty() {
        return Err(PasswordProblem::Empty);
    }
    Ok(())
}

pub fn check_new_password(password: &str) -> Result<(), PasswordProblem> {
    let length = password.chars().count();
    if password.is_empty() {
        return Err(PasswordProblem::Empty);
    }
    if length < PASSWORD_MIN {
        return Err(PasswordProblem::TooShort);
    }
    if length > PASSWORD_MAX {
        return Err(PasswordProblem::TooLong);
    }
    Ok(())
}

/// The e-mail is optional; a filled one has to look like an address.
pub fn check_email(email: &str) -> Result<(), EmailProblem> {
    let email = email.trim();
    if email.is_empty() {
        return Ok(());
    }
    if email.len() > EMAIL_MAX || email.chars().any(char::is_whitespace) {
        return Err(EmailProblem::NotAddress);
    }
    match email.split_once('@') {
        Some((user, domain)) => {
            if user.is_empty()
                || !domain.contains('.')
                || domain.starts_with('.')
                || domain.ends_with('.')
            {
                return Err(EmailProblem::NotAddress);
            }
            Ok(())
        }
        None => Err(EmailProblem::NotAddress),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AccountProfile, check_email, check_login_password, check_new_password, check_username,
    };

    #[test]
    fn a_profile_survives_a_round_trip() {
        let profile = AccountProfile {
            username: "alice".into(),
            email: "a@b.co".into(),
            nickname: "Alice".into(),
            avatar: Some("account/avatars/one.png".into()),
            sync_salt: "c2FsdA".into(),
            quota_bytes: 1024,
            used_bytes: 12,
            cloud_sync: true,
            remember_sync_password: true,
            settings_version: Some(3),
            hosts_version: None,
            last_sync_at: Some("2026-01-02T03:04:05Z".into()),
        };
        assert_eq!(AccountProfile::from_json(&profile.to_json()), profile);
    }

    #[test]
    fn missing_and_broken_fields_fall_back_one_by_one() {
        let profile = AccountProfile::from_json(
            r#"{"username":"alice","quota_bytes":"lots","cloud_sync":true,"settings_version":7}"#,
        );
        assert_eq!(profile.username, "alice");
        assert_eq!(profile.quota_bytes, 0);
        assert!(profile.cloud_sync);
        assert_eq!(profile.settings_version, Some(7));
        assert_eq!(
            AccountProfile::from_json("not json"),
            AccountProfile::default()
        );
    }

    #[test]
    fn a_missing_file_is_an_empty_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.json");
        assert_eq!(
            AccountProfile::load(&path).unwrap(),
            AccountProfile::default()
        );
    }

    #[test]
    fn usernames_match_the_server() {
        assert!(check_username("alice").is_ok());
        assert!(check_username("dev_box-2").is_ok());
        assert!(check_username("ab").is_err());
        assert!(check_username("has space").is_err());
        assert!(check_username(&"a".repeat(33)).is_err());
    }

    #[test]
    fn registration_passwords_are_longer_than_sign_in_ones() {
        assert!(check_login_password("x").is_ok());
        assert!(check_new_password("short").is_err());
        assert!(check_new_password("12345678").is_ok());
        assert!(check_new_password(&"a".repeat(129)).is_err());
    }

    #[test]
    fn an_email_is_optional_but_checked_when_filled() {
        assert!(check_email("").is_ok());
        assert!(check_email("a@b.co").is_ok());
        assert!(check_email("no-at-sign").is_err());
        assert!(check_email("a@localhost").is_err());
        assert!(check_email("a b@c.d").is_err());
    }

    #[test]
    fn the_nickname_is_what_the_assistant_says() {
        let signed_out = AccountProfile::default();
        assert_eq!(signed_out.display_name(), "");
        assert_eq!(signed_out.initial(), "");

        let named = AccountProfile {
            username: "ada".into(),
            nickname: "Ada Lovelace".into(),
            ..AccountProfile::default()
        };
        assert_eq!(named.display_name(), "Ada Lovelace");
        assert_eq!(named.initial(), "A");

        // A blank nickname falls back to the username.
        let unnamed = AccountProfile {
            username: "grace".into(),
            nickname: "   ".into(),
            ..AccountProfile::default()
        };
        assert_eq!(unnamed.display_name(), "grace");
    }
}
