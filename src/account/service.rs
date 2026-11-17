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
    ) -> Result<AuthReply, AccountError>;
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
        let reply: AuthBody = decode(&body)?;
        Ok(AuthReply {
            token: Zeroizing::new(reply.token),
            expires_at: reply.expires_at,
            user: reply.user.into(),
        })
    }

    fn register(
        &self,
        username: &str,
        password: &str,
        email: Option<&str>,
    ) -> Result<AuthReply, AccountError> {
        let mut payload = serde_json::json!({ "username": username, "password": password });
        if let Some(email) = email.filter(|email| !email.trim().is_empty()) {
            payload["email"] = email.trim().into();
        }
        let request = self
            .client
            .post(self.url("/api/v1/auth/register"))
            .json(&payload);
        let (status, body) = self.call(request)?;
        if !status.is_success() {
            return Err(error_from(status, &body));
        }
        let reply: AuthBody = decode(&body)?;
        Ok(AuthReply {
            token: Zeroizing::new(reply.token),
            expires_at: reply.expires_at,
            user: reply.user.into(),
        })
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
