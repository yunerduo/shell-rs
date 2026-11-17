//! The account server's API: sign-in, the profile, and the cloud sync
//! documents. Blocking calls, made from a background thread; the store
//! never touches the network on the UI thread.

use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use zeroize::Zeroizing;

/// Where the account server lives. The website and the update feed are on
/// the same host.
pub const API_BASE: &str = "https://shellas.agree.host";

/// The account's profile as the server returns it.
#[derive(Clone, Debug, Default)]
pub struct MeReply {
    pub username: String,
    pub email: Option<String>,
    pub quota_bytes: u64,
    pub used_bytes: u64,
    pub sync_salt: String,
    pub created_at: String,
}

/// A successful sign-in or registration.
#[derive(Clone, Debug)]
pub struct AuthReply {
    pub token: Zeroizing<String>,
    pub expires_at: String,
    pub user: MeReply,
}

/// A successful registration. The phone is optional and the server may not
/// take it yet; `phone_used` is false when the number was dropped so that
/// the account could still be created.
#[derive(Clone, Debug)]
pub struct RegisterReply {
    pub auth: AuthReply,
    pub phone_used: bool,
}

/// A verification code the server says it has sent, with the two intervals
/// the client shows: how long the code lasts and how long until another may
/// be requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodeSent {
    pub expires_in: u64,
    pub resend_after: u64,
}

/// One cloud sync document, as the server stores it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentReply {
    pub version: i64,
    pub size: u64,
    pub sha256: String,
    pub created_at: String,
}

/// Which cloud sync document a request is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncKind {
    Settings,
    Hosts,
}

impl SyncKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Hosts => "hosts",
        }
    }

    /// The plain document is `settings.json` or `hosts.json` in the cloud.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Settings => "settings.json",
            Self::Hosts => "hosts.json",
        }
    }
}

/// What one request failed with. `Network` means the server could not be
/// reached at all: an offline or intranet machine, which is not an error
/// condition for ShellAs and only affects the account features.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountError {
    Network(String),
    Unauthorized,
    Credentials(String),
    Forbidden(String),
    Locked(String),
    TooMany(String),
    TooLarge(String),
    BadRequest(String),
    Conflict(String),
    Server(String),
    Decode(String),
    /// The registration wants a code for this e-mail address before it can
    /// go on. The dialog answers by showing the code field.
    EmailVerificationRequired,
    /// The server no longer requires the code: the registration can be sent
    /// again as it is.
    EmailVerificationDisabled,
    /// The code is wrong.
    EmailCodeInvalid,
    /// The code is older than ten minutes; a new one is needed.
    EmailCodeExpired,
    /// The code was entered wrong too many times and is void.
    EmailCodeLocked,
    /// Mail cannot be sent at all: SMTP is unset, or the template is missing
    /// or broken. Not the user's problem to fix.
    EmailUnavailable,
    /// The mail server could not be reached, refused the login or the
    /// message. Worth trying again.
    EmailSendFailed,
}

impl AccountError {
    /// Whether the server was not reached. The caller treats this as "not
    /// signed in for now" rather than as a failure.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, Self::Network(_))
    }
}

/// How much of the quota an upload used, when it did not fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quota {
    pub used_bytes: u64,
    pub quota_bytes: u64,
}

/// Why an upload did not happen. A conflict carries the server's latest
/// version so the user can choose to overwrite it or to download it.
#[derive(Clone, Debug)]
pub enum UploadError {
    Conflict { latest: DocumentReply },
    Quota(Quota),
    Other(AccountError),
}

impl From<AccountError> for UploadError {
    fn from(error: AccountError) -> Self {
        Self::Other(error)
    }
}

/// The account server's API, as the store uses it.
pub trait AccountService: Send + Sync + 'static {
    fn login(&self, username: &str, password: &str) -> Result<AuthReply, AccountError>;
    fn register(
        &self,
        username: &str,
        password: &str,
        email: Option<&str>,
        phone: Option<&str>,
        verification_code: Option<&str>,
    ) -> Result<RegisterReply, AccountError>;
    /// Ask for the code a registration needs. Only works when the server has
    /// the registration verification switched on.
    fn request_email_code(
        &self,
        email: &str,
        username: Option<&str>,
    ) -> Result<CodeSent, AccountError>;
    /// Ask for a password reset code. The server answers the same way whether
    /// or not the address has an account, on purpose.
    fn request_password_code(&self, email: &str) -> Result<CodeSent, AccountError>;
    fn reset_password(&self, email: &str, code: &str, password: &str) -> Result<(), AccountError>;
    fn logout(&self, token: &str) -> Result<(), AccountError>;
    fn me(&self, token: &str) -> Result<MeReply, AccountError>;
    fn upload_document(
        &self,
        token: &str,
        kind: SyncKind,
        body: Vec<u8>,
        sha256: &str,
        base_version: Option<i64>,
    ) -> Result<DocumentReply, UploadError>;
    fn download_document(
        &self,
        token: &str,
        kind: SyncKind,
        version: i64,
    ) -> Result<Vec<u8>, AccountError>;
}

pub type SharedAccountService = Arc<dyn AccountService>;

/// The real server, over HTTPS through the system proxy.
pub struct HttpAccountService {
    base: String,
    client: reqwest::blocking::Client,
}

impl HttpAccountService {
    pub fn system() -> Self {
        Self::new(API_BASE)
    }

    pub fn new(base: impl Into<String>) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new());
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            client,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// Send a request and read its body, whatever the status. Only transport
    /// failures become [`AccountError::Network`].
    fn call(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<(reqwest::StatusCode, String), AccountError> {
        let response = request.send().map_err(network_error)?;
        let status = response.status();
        let body = response.text().map_err(network_error)?;
        Ok((status, body))
    }

    fn authorized(
        &self,
        method: reqwest::Method,
        path: &str,
        token: &str,
    ) -> reqwest::blocking::RequestBuilder {
        self.client
            .request(method, self.url(path))
            .bearer_auth(token)
    }
}

fn network_error(error: reqwest::Error) -> AccountError {
    AccountError::Network(error.to_string())
}

#[derive(Deserialize)]
struct MeBody {
    #[serde(default)]
    username: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    quota_bytes: i64,
    #[serde(default)]
    used_bytes: i64,
    #[serde(default)]
    sync_salt: String,
    #[serde(default)]
    created_at: String,
}

impl From<MeBody> for MeReply {
    fn from(body: MeBody) -> Self {
        Self {
            username: body.username,
            email: body.email,
            quota_bytes: body.quota_bytes.max(0) as u64,
            used_bytes: body.used_bytes.max(0) as u64,
            sync_salt: body.sync_salt,
            created_at: body.created_at,
        }
    }
}

#[derive(Deserialize)]
struct AuthBody {
    #[serde(default)]
    token: String,
    #[serde(default)]
    expires_at: String,
    user: MeBody,
}

/// The `{ "error": ..., "message": ..., "latest": ..., ... }` body of a
/// failed request.
fn error_body(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

fn message_of(body: &Value) -> String {
    body.get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn code_of(body: &Value) -> String {
    body.get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn document_of(value: &Value) -> Option<DocumentReply> {
    let version = value.get("version").and_then(Value::as_i64)?;
    Some(DocumentReply {
        version,
        size: value.get("size").and_then(Value::as_u64).unwrap_or(0),
        sha256: value
            .get("sha256")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

fn error_from(status: reqwest::StatusCode, body: &str) -> AccountError {
    let value = error_body(body);
    let message = message_of(&value);
    let code = code_of(&value);
    // The e-mail flows have their own words for each failure; the form says
    // which of them happened instead of one flat "failed".
    match code.as_str() {
        // With the switch on, a registration sent without an address is
        // stopped before the code: same answer for the client, which then
        // asks for both.
        "email_required" | "email_verification_required" => {
            return AccountError::EmailVerificationRequired;
        }
        "email_verification_disabled" => return AccountError::EmailVerificationDisabled,
        "email_code_invalid" => return AccountError::EmailCodeInvalid,
        "email_code_expired" => return AccountError::EmailCodeExpired,
        "email_code_locked" => return AccountError::EmailCodeLocked,
        "email_not_configured" | "email_template_missing" | "email_template_invalid" => {
            return AccountError::EmailUnavailable;
        }
        "email_unreachable" | "email_auth_failed" | "email_rejected" => {
            return AccountError::EmailSendFailed;
        }
        _ => {}
    }
    match status.as_u16() {
        401 if code == "credentials" => AccountError::Credentials(message),
        401 => AccountError::Unauthorized,
        403 => AccountError::Forbidden(message),
        409 => AccountError::Conflict(message),
        429 if code == "locked" => AccountError::Locked(message),
        429 => AccountError::TooMany(message),
        413 => AccountError::TooLarge(message),
        400 => AccountError::BadRequest(message),
        _ => AccountError::Server(if message.is_empty() {
            format!("HTTP {status}")
        } else {
            message
        }),
    }
}

fn decode<T: serde::de::DeserializeOwned>(body: &str) -> Result<T, AccountError> {
    serde_json::from_str(body).map_err(|error| AccountError::Decode(error.to_string()))
}

fn auth_from(body: &str) -> Result<AuthReply, AccountError> {
    let reply: AuthBody = decode(body)?;
    Ok(AuthReply {
        token: Zeroizing::new(reply.token),
        expires_at: reply.expires_at,
        user: reply.user.into(),
    })
}

#[derive(Deserialize)]
struct CodeBody {
    #[serde(default)]
    expires_in: i64,
    #[serde(default)]
    resend_after: i64,
}

/// The two intervals both code endpoints return. Missing values fall back to
/// the server's documented defaults rather than to zero, which would let the
/// button be pressed again at once.
fn code_sent(body: &str) -> Result<CodeSent, AccountError> {
    let reply: CodeBody = decode(body)?;
    Ok(CodeSent {
        expires_in: if reply.expires_in > 0 {
            reply.expires_in as u64
        } else {
            600
        },
        resend_after: if reply.resend_after > 0 {
            reply.resend_after as u64
        } else {
            60
        },
    })
}

/// Whether the server refused the optional `phone` field itself rather than
/// the account. Only an explicit complaint about the phone counts, and only
/// on a request the server could otherwise read.
fn phone_refused(status: reqwest::StatusCode, body: &str) -> bool {
    if !matches!(status.as_u16(), 400 | 422) {
        return false;
    }
    let value = error_body(body);
    format!("{} {}", code_of(&value), message_of(&value))
        .to_lowercase()
        .contains("phone")
}

impl AccountService for HttpAccountService {
    fn login(&self, username: &str, password: &str) -> Result<AuthReply, AccountError> {
        let request = self
            .client
            .post(self.url("/api/v1/auth/login"))
            .json(&serde_json::json!({ "username": username, "password": password }));
        let (status, body) = self.call(request)?;
        if !status.is_success() {
            return Err(error_from(status, &body));
        }
        auth_from(&body)
    }

    fn register(
        &self,
        username: &str,
        password: &str,
        email: Option<&str>,
        phone: Option<&str>,
        verification_code: Option<&str>,
    ) -> Result<RegisterReply, AccountError> {
        let email = email.map(str::trim).filter(|email| !email.is_empty());
        let phone = phone.map(str::trim).filter(|phone| !phone.is_empty());
        let verification_code = verification_code
            .map(str::trim)
            .filter(|code| !code.is_empty());
        let send = |with_phone: bool| {
            let mut payload = serde_json::json!({ "username": username, "password": password });
            if let Some(email) = email {
                payload["email"] = email.into();
            }
            if let Some(phone) = phone.filter(|_| with_phone) {
                payload["phone"] = phone.into();
            }
            if let Some(code) = verification_code {
                payload["verification_code"] = code.into();
            }
            self.call(
                self.client
                    .post(self.url("/api/v1/auth/register"))
                    .json(&payload),
            )
        };
        let (status, body) = send(phone.is_some())?;
        if status.is_success() {
            return Ok(RegisterReply {
                auth: auth_from(&body)?,
                phone_used: phone.is_some(),
            });
        }
        // The phone is optional: a server that does not know the field yet
        // must not keep the account from being created. Register once more
        // without it and say the number did not take effect.
        if phone.is_some() && phone_refused(status, &body) {
            let (status, body) = send(false)?;
            if status.is_success() {
                return Ok(RegisterReply {
                    auth: auth_from(&body)?,
                    phone_used: false,
                });
            }
            return Err(error_from(status, &body));
        }
        Err(error_from(status, &body))
    }

    fn request_email_code(
        &self,
        email: &str,
        username: Option<&str>,
    ) -> Result<CodeSent, AccountError> {
        let mut payload = serde_json::json!({ "email": email.trim() });
        if let Some(username) = username.map(str::trim).filter(|name| !name.is_empty()) {
            payload["username"] = username.into();
        }
        let (status, body) = self.call(
            self.client
                .post(self.url("/api/v1/auth/email/code"))
                .json(&payload),
        )?;
        if !status.is_success() {
            return Err(error_from(status, &body));
        }
        code_sent(&body)
    }

    fn request_password_code(&self, email: &str) -> Result<CodeSent, AccountError> {
        let payload = serde_json::json!({ "email": email.trim() });
        let (status, body) = self.call(
            self.client
                .post(self.url("/api/v1/auth/password/forgot"))
                .json(&payload),
        )?;
        if !status.is_success() {
            return Err(error_from(status, &body));
        }
        code_sent(&body)
    }

    fn reset_password(&self, email: &str, code: &str, password: &str) -> Result<(), AccountError> {
        let payload = serde_json::json!({
            "email": email.trim(),
            "code": code.trim(),
            "password": password,
        });
        let (status, body) = self.call(
            self.client
                .post(self.url("/api/v1/auth/password/reset"))
                .json(&payload),
        )?;
        if !status.is_success() {
            return Err(error_from(status, &body));
        }
        Ok(())
    }

    fn logout(&self, token: &str) -> Result<(), AccountError> {
        let request = self.authorized(reqwest::Method::POST, "/api/v1/auth/logout", token);
        let (status, body) = self.call(request)?;
        // A token the server already forgot is as good as logged out.
        if status.is_success() || status == reqwest::StatusCode::UNAUTHORIZED {
            return Ok(());
        }
        Err(error_from(status, &body))
    }

    fn me(&self, token: &str) -> Result<MeReply, AccountError> {
        let request = self.authorized(reqwest::Method::GET, "/api/v1/me", token);
        let (status, body) = self.call(request)?;
        if !status.is_success() {
            return Err(error_from(status, &body));
        }
        let me: MeBody = decode(&body)?;
        Ok(me.into())
    }

    fn upload_document(
        &self,
        token: &str,
        kind: SyncKind,
        body: Vec<u8>,
        sha256: &str,
        base_version: Option<i64>,
    ) -> Result<DocumentReply, UploadError> {
        let path = format!("/api/v1/sync/documents/{}", kind.as_str());
        let mut request = self
            .authorized(reqwest::Method::PUT, &path, token)
            .header("X-Snapshot-Sha256", sha256)
            .body(body);
        if let Some(version) = base_version {
            request = request.header("X-Base-Version", version.to_string());
        }
        let (status, text) = self.call(request)?;
        if status.is_success() {
            let value = error_body(&text);
            return document_of(&value)
                .ok_or_else(|| AccountError::Decode(text.clone()))
                .map_err(UploadError::Other);
        }
        if status == reqwest::StatusCode::CONFLICT {
            let value = error_body(&text);
            if let Some(latest) = value.get("latest").and_then(document_of) {
                return Err(UploadError::Conflict { latest });
            }
        }
        if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE {
            let value = error_body(&text);
            let used = value.get("used_bytes").and_then(Value::as_u64);
            let quota = value.get("quota_bytes").and_then(Value::as_u64);
            if let (Some(used_bytes), Some(quota_bytes)) = (used, quota) {
                return Err(UploadError::Quota(Quota {
                    used_bytes,
                    quota_bytes,
                }));
            }
        }
        Err(UploadError::Other(error_from(status, &text)))
    }

    fn download_document(
        &self,
        token: &str,
        kind: SyncKind,
        version: i64,
    ) -> Result<Vec<u8>, AccountError> {
        let path = format!("/api/v1/sync/documents/{}/{version}", kind.as_str());
        let request = self.authorized(reqwest::Method::GET, &path, token);
        let response = request.send().map_err(network_error)?;
        let status = response.status();
        let bytes = response.bytes().map_err(network_error)?;
        if !status.is_success() {
            return Err(error_from(status, &String::from_utf8_lossy(&bytes)));
        }
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountError, code_sent, error_from, phone_refused};

    #[test]
    fn only_a_complaint_about_the_phone_drops_it() {
        let bad_request = reqwest::StatusCode::BAD_REQUEST;
        // An unknown field, and the server's own check on the number: both
        // mean "not this request", so it is sent again without the phone.
        assert!(phone_refused(
            bad_request,
            r#"{"error":"unknown_field","message":"unknown field: phone"}"#
        ));
        assert!(phone_refused(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            r#"{"error":"invalid_phone","message":"not a mainland mobile number"}"#
        ));
        // A rejection about anything else is the account's own failure.
        assert!(!phone_refused(
            bad_request,
            r#"{"error":"username_taken","message":"that username is taken"}"#
        ));
        // Only a request the server understood can be retried this way.
        assert!(!phone_refused(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            r#"{"error":"phone","message":"phone"}"#
        ));
        // Not a JSON error body at all: nothing names the phone.
        assert!(!phone_refused(bad_request, "Bad Request"));
    }

    #[test]
    fn each_email_failure_keeps_its_own_meaning() {
        let bad_request = reqwest::StatusCode::BAD_REQUEST;
        // Four sentences for four situations, none of them "failed".
        assert_eq!(
            error_from(bad_request, r#"{"error":"email_code_invalid"}"#),
            AccountError::EmailCodeInvalid
        );
        assert_eq!(
            error_from(bad_request, r#"{"error":"email_code_expired"}"#),
            AccountError::EmailCodeExpired
        );
        assert_eq!(
            error_from(bad_request, r#"{"error":"email_code_locked"}"#),
            AccountError::EmailCodeLocked
        );
        assert_eq!(
            error_from(
                bad_request,
                r#"{"error":"email_verification_required","message":"verification code required"}"#
            ),
            AccountError::EmailVerificationRequired
        );
        // The switch being off is not a failure of the registration.
        assert_eq!(
            error_from(bad_request, r#"{"error":"email_verification_disabled"}"#),
            AccountError::EmailVerificationDisabled
        );
        // A mail server that is not set up and one that refuses are two
        // different things to say.
        assert_eq!(
            error_from(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                r#"{"error":"email_not_configured"}"#
            ),
            AccountError::EmailUnavailable
        );
        assert_eq!(
            error_from(
                reqwest::StatusCode::BAD_GATEWAY,
                r#"{"error":"email_rejected"}"#
            ),
            AccountError::EmailSendFailed
        );
        // Requests too close together stay the generic rate limit.
        assert!(matches!(
            error_from(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                r#"{"error":"too_many"}"#
            ),
            AccountError::TooMany(_)
        ));
    }

    #[test]
    fn a_code_reply_falls_back_to_the_documented_intervals() {
        let sent = code_sent(r#"{"expires_in":600,"resend_after":60}"#).unwrap();
        assert_eq!((sent.expires_in, sent.resend_after), (600, 60));
        // A reply that leaves the numbers out must not enable the button at
        // once.
        let sent = code_sent("{}").unwrap();
        assert_eq!((sent.expires_in, sent.resend_after), (600, 60));
    }
}
