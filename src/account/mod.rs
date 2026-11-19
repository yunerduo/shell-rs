//! The optional account: sign-in and registration, the profile, and the
//! account's cloud sync.
//!
//! The account is a feature, not a gate. Signed out — which is the default,
//! and the only state on a machine with no network — hosts, terminals,
//! SFTP, the AI assistant with a locally configured key, and every setting
//! keep working. Only the cloud features are absent, and their interfaces
//! say so instead of failing.

mod avatar_crop;
pub mod cloud_sync;
mod describe;
mod gravatar;
mod login_dialog;
mod model;
mod popover;
mod remote_avatar;
mod security_dialog;
mod service;
mod settings_page;
mod store;

pub use avatar_crop::open_avatar_crop_dialog;
pub use describe::{
    account_error, email_problem, login_password_problem, new_password_problem, upload_error,
    username_problem,
};
pub use login_dialog::{LoginMode, open_login_dialog};
pub use model::{
    AccountNotice, AccountPhase, AccountProfile, AccountSnapshot, EmailProblem, PasswordProblem,
    UsernameProblem, check_email, check_login_password, check_new_password, check_username,
};
pub use popover::account_popover;
pub use security_dialog::{open_change_email_dialog, open_change_password_dialog};
/// The service's root, for the built-in AI trial's two endpoints.
pub(crate) use service::API_BASE;
pub use service::{
    AccountError, AccountService, AuthReply, DocumentReply, HttpAccountService, MeReply,
    ProfileReply, Quota, SharedAccountService, SyncKind, UploadError,
};
pub use settings_page::{account_groups, open_nickname_dialog, open_sync_password_dialog};
pub use store::{AccountEvent, AccountStore, CloudConflict, ProfileChange};

/// A byte count as 「1.2 MB」, for the quota notice.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_are_readable() {
        assert_eq!(super::format_size(12), "12 B");
        assert_eq!(super::format_size(1024), "1.0 KB");
        assert_eq!(super::format_size(3 * 1024 * 1024), "3.0 MB");
    }
}
