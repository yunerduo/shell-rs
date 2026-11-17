//! The words for the account's problems: form checks, the server's answers
//! and the reasons an account went back to signed out. The core and the
//! HTTP layer give types; this is the only place that names them.

use gpui_kit::SharedString;

use super::model::{AccountNotice, EmailProblem, PasswordProblem, UsernameProblem};
use super::service::{AccountError, UploadError};
use crate::i18n::t;

/// Why the account went back to signed out. Both are gentle: an offline or
/// intranet machine is a supported way to work.
pub fn notice_text(notice: AccountNotice) -> SharedString {
    match notice {
        AccountNotice::Expired => t!("account.notice.expired"),
        AccountNotice::Unreachable => t!("account.notice.unreachable"),
    }
}

/// What a failed request says. The server's own message is used when it
/// sent one: it is more precise than anything invented here.
pub fn account_error(error: &AccountError) -> SharedString {
    match error {
        AccountError::Network(_) => t!("account.error.network"),
        AccountError::Unauthorized => t!("account.error.expired"),
        AccountError::Credentials(message) => {
            if message.is_empty() {
                t!("account.error.credentials")
            } else {
                message.clone().into()
            }
        }
        AccountError::Forbidden(message) => {
            if message.is_empty() {
                t!("account.error.disabled")
            } else {
                message.clone().into()
            }
        }
        AccountError::Locked(message) => {
            if message.is_empty() {
                t!("account.error.too_many")
            } else {
                message.clone().into()
            }
        }
        AccountError::TooMany(message) => {
            if message.is_empty() {
                t!("account.error.too_many")
            } else {
                message.clone().into()
            }
        }
        AccountError::TooLarge(message) => {
            if message.is_empty() {
                t!("account.error.too_large")
            } else {
                message.clone().into()
            }
        }
        AccountError::BadRequest(message) => {
            if message.is_empty() {
                t!("account.error.server")
            } else {
                message.clone().into()
            }
        }
        AccountError::Conflict(message) => {
            if message.is_empty() {
                t!("account.error.conflict")
            } else {
                message.clone().into()
            }
        }
        AccountError::Server(message) => {
            if message.is_empty() {
                t!("account.error.server")
            } else {
                message.clone().into()
            }
        }
        AccountError::Decode(_) => t!("account.error.server"),
    }
}

/// What an upload that did not happen says. Conflicts and the quota have
/// their own wording, since the user has a choice or a number to read.
pub fn upload_error(error: &UploadError) -> SharedString {
    match error {
        UploadError::Conflict { .. } => t!("account.error.cloud_conflict"),
        UploadError::Quota(quota) => t!(
            "account.error.quota",
            used = super::format_size(quota.used_bytes),
            quota = super::format_size(quota.quota_bytes)
        ),
        UploadError::Other(error) => account_error(error),
    }
}

pub fn username_problem(problem: UsernameProblem) -> SharedString {
    match problem {
        UsernameProblem::Empty => t!("account.form.username.empty"),
        UsernameProblem::TooShort => t!("account.form.username.short"),
        UsernameProblem::TooLong => t!("account.form.username.long"),
        UsernameProblem::NotPlain => t!("account.form.username.plain"),
    }
}

pub fn login_password_problem(_problem: PasswordProblem) -> SharedString {
    t!("account.form.password.empty")
}

pub fn new_password_problem(problem: PasswordProblem) -> SharedString {
    match problem {
        PasswordProblem::Empty => t!("account.form.password.empty"),
        PasswordProblem::TooShort => t!("account.form.password.short"),
        PasswordProblem::TooLong => t!("account.form.password.long"),
    }
}

pub fn email_problem(problem: EmailProblem) -> SharedString {
    match problem {
        EmailProblem::TooLong | EmailProblem::NotAddress => t!("account.form.email.invalid"),
    }
}
