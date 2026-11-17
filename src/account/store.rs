//! The optional account's state, kept in memory while the app runs and in
//! `account.json` between runs. The login token goes to the system keychain
//! and nowhere else.
//!
//! Nothing here is required to use ShellAs. With no account, no token and no
//! network the store simply reports signed out, and every local feature keeps
//! working. The store never blocks the UI thread on the keychain or the
//! network: both run on the background executor.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use gpui_kit::{AppContext as _, Context, EventEmitter};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::cloud_sync;
use super::model::{AccountNotice, AccountPhase, AccountProfile, AccountSnapshot};
use super::service::{
    AccountError, DocumentReply, MeReply, SharedAccountService, SyncKind, UploadError,
};
use crate::app::{avatars_dir, cloud_dir};
use crate::i18n::t;
use crate::secrets::{SecretRef, SharedSecretStore};

/// A cloud sync document the server refused because it has a newer one.
/// The plain document is kept so 以本机覆盖云端 can be retried without
/// rebuilding it.
#[derive(Clone, Debug)]
pub struct CloudConflict {
    pub kind: SyncKind,
    pub latest: DocumentReply,
    plaintext: Vec<u8>,
}

/// What happened outside the store's own state, for the workspace to turn
/// into notifications and dialogs.
#[derive(Clone, Debug)]
pub enum AccountEvent {
    /// `account.json` could not be written. The in-memory state stands.
    PersistFailed(String),
    /// The account was created, but the optional phone number was not
    /// stored: the server does not take that field yet. Worth saying, not
    /// worth failing over.
    PhoneNotApplied,
    /// An upload is waiting for the cloud sync password, which is only in
    /// memory after a restart.
    NeedsSyncPassword,
    CloudUploaded {
        kind: SyncKind,
        version: i64,
    },
    CloudFailed(UploadError),
    CloudConflict(CloudConflict),
    /// The cloud settings document, opened and ready to apply.
    SettingsDownloaded(Vec<u8>),
    /// The cloud hosts document, written here for review.
    HostsDownloaded(PathBuf),
}

/// One document waiting to be uploaded.
struct PendingUpload {
    kind: SyncKind,
    plaintext: Vec<u8>,
    /// `Some(version)` after the user chose 以本机覆盖云端; `None` uses the
    /// version this computer last uploaded.
    base: Option<i64>,
}

/// The account. Optional: every method degrades to signed out when there is
/// no token, no service or no network.
pub struct AccountStore {
    path: Option<PathBuf>,
    profile: Option<AccountProfile>,
    phase: AccountPhase,
    token: Option<Zeroizing<String>>,
    /// The password the token was obtained with, reused as the cloud sync
    /// password for this session. Never written anywhere unless the user
    /// asked to remember it.
    sync_password: Option<Zeroizing<String>>,
    login_error: Option<AccountError>,
    notice: Option<AccountNotice>,
    services: Option<SharedAccountService>,
    secrets: SharedSecretStore,
    pending: VecDeque<PendingUpload>,
    uploading: bool,
    conflict: Option<CloudConflict>,
    /// Whether a cloud download is in flight, so it is not started twice.
    downloading: bool,
}

impl EventEmitter<AccountEvent> for AccountStore {}

impl AccountStore {
    pub fn new(path: Option<PathBuf>, secrets: SharedSecretStore, cx: &mut Context<Self>) -> Self {
        let profile = match path.as_deref().map(AccountProfile::load) {
            Some(Ok(profile)) => Some(profile),
            Some(Err(error)) => {
                cx.emit(AccountEvent::PersistFailed(error));
                None
            }
            None => None,
        };
        Self {
            path,
            profile,
            phase: AccountPhase::SignedOut,
            token: None,
            sync_password: None,
            login_error: None,
            notice: None,
            services: None,
            secrets,
            pending: VecDeque::new(),
            uploading: false,
            conflict: None,
            downloading: false,
        }
    }

    pub fn set_services(&mut self, services: SharedAccountService, cx: &mut Context<Self>) {
        self.services = Some(services);
        self.resume(cx);
    }

    /// The server client, for the flows the login dialog drives itself:
    /// asking for a code and resetting the password do not change the
    /// account's state until they succeed, so their results belong to the
    /// form, not here.
    pub fn service(&self) -> Option<SharedAccountService> {
        self.services.clone()
    }

    /// Where `account.json` lives. Production attaches it once the workspace
    /// is built; UI tests never do, so nothing reads or writes the real
    /// account file.
    pub fn attach(&mut self, path: Option<PathBuf>, cx: &mut Context<Self>) {
        self.path = path;
        match self.path.as_deref().map(AccountProfile::load) {
            Some(Ok(profile)) => self.profile = Some(profile),
            Some(Err(error)) => {
                self.profile = None;
                cx.emit(AccountEvent::PersistFailed(error));
            }
            None => self.profile = None,
        }
        cx.notify();
    }

    pub fn phase(&self) -> AccountPhase {
        self.phase
    }

    pub fn is_signed_in(&self) -> bool {
        self.phase == AccountPhase::SignedIn && self.token.is_some()
    }

    pub fn login_error(&self) -> Option<&AccountError> {
        self.login_error.as_ref()
    }

    /// Forget the last request's error, e.g. when the form switches between
    /// signing in and registering.
    pub fn clear_login_error(&mut self, cx: &mut Context<Self>) {
        if self.login_error.take().is_some() {
            cx.notify();
        }
    }

    pub fn notice(&self) -> Option<AccountNotice> {
        self.notice
    }

    pub fn conflict(&self) -> Option<&CloudConflict> {
        self.conflict.as_ref()
    }

    /// What the AI assistant calls the user, when signed in. `None` keeps
    /// its neutral 「你」.
    pub fn assistant_addressing(&self) -> Option<String> {
        if !self.is_signed_in() {
            return None;
        }
        self.profile
            .as_ref()
            .map(AccountProfile::display_name)
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string)
    }

    pub fn snapshot(&self) -> AccountSnapshot {
        let signed_in = self.is_signed_in();
        match (self.profile.as_ref(), signed_in) {
            (Some(profile), true) => AccountSnapshot {
                phase: self.phase,
                signed_in,
                username: profile.username.clone(),
                display_name: profile.display_name().to_string(),
                email: profile.email.clone(),
                nickname: profile.nickname.clone(),
                avatar: profile.avatar_path(),
                initial: profile.initial(),
                cloud_sync: profile.cloud_sync,
                remember_sync_password: profile.remember_sync_password,
                quota_bytes: profile.quota_bytes,
                used_bytes: profile.used_bytes,
                settings_version: profile.settings_version,
                hosts_version: profile.hosts_version,
                last_sync_at: profile.last_sync_at.clone(),
                notice: self.notice,
            },
            _ => AccountSnapshot {
                phase: self.phase,
                notice: self.notice,
                ..AccountSnapshot::default()
            },
        }
    }

    /// Read the token from the keychain and ask the server who it belongs
    /// to. With no token, the keychain unavailable or the server off the
    /// network, this leaves the store signed out and says nothing else.
    pub fn resume(&mut self, cx: &mut Context<Self>) {
        if self.token.is_some() || self.phase != AccountPhase::SignedOut {
            return;
        }
        let Some(service) = self.services.clone() else {
            return;
        };
        let secrets = self.secrets.clone();
        cx.spawn(async move |this, cx| {
            let read = cx
                .background_spawn(async move { secrets.get(&SecretRef::AccountToken) })
                .await;
            let Ok(Some(token)) = read else {
                return;
            };
            let request = token.clone();
            let result = cx
                .background_spawn(async move { service.me(request.as_str()) })
                .await;
            this.update(cx, |store, cx| store.apply_me(result, token, cx))
                .ok();
        })
        .detach();
    }

    /// Sign in with the account server. The password is kept in memory as
    /// the cloud sync password; the token goes to the keychain.
    pub fn login(&mut self, username: String, password: Zeroizing<String>, cx: &mut Context<Self>) {
        if self.phase == AccountPhase::SigningIn {
            return;
        }
        let Some(service) = self.services.clone() else {
            self.login_error = Some(AccountError::Network(String::new()));
            cx.notify();
            return;
        };
        self.phase = AccountPhase::SigningIn;
        self.login_error = None;
        self.notice = None;
        cx.notify();
        let request_username = username.trim().to_string();
        let request_password = password.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    service.login(&request_username, request_password.as_str())
                })
                .await;
            this.update(cx, |store, cx| {
                store.apply_auth(result, true, Some(password), cx)
            })
            .ok();
        })
        .detach();
    }

    pub fn register(
        &mut self,
        username: String,
        password: Zeroizing<String>,
        email: String,
        phone: String,
        verification_code: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.phase == AccountPhase::SigningIn {
            return;
        }
        let Some(service) = self.services.clone() else {
            self.login_error = Some(AccountError::Network(String::new()));
            cx.notify();
            return;
        };
        self.phase = AccountPhase::SigningIn;
        self.login_error = None;
        self.notice = None;
        cx.notify();
        let request_username = username.trim().to_string();
        let request_password = password.clone();
        let request_email = email.trim().to_string();
        let request_phone = phone.trim().to_string();
        let request_code = verification_code
            .map(|code| code.trim().to_string())
            .filter(|code| !code.is_empty());
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    service.register(
                        &request_username,
                        request_password.as_str(),
                        (!request_email.is_empty()).then_some(request_email.as_str()),
                        (!request_phone.is_empty()).then_some(request_phone.as_str()),
                        request_code.as_deref(),
                    )
                })
                .await;
            let phone_used = result
                .as_ref()
                .map(|reply| reply.phone_used)
                .unwrap_or(true);
            this.update(cx, |store, cx| {
                store.apply_auth(
                    result.map(|reply| reply.auth),
                    phone_used,
                    Some(password),
                    cx,
                )
            })
            .ok();
        })
        .detach();
    }

    /// Sign out. Every local thing stays: hosts, terminals, SFTP and
    /// settings are not the account's. Only the cloud features stop.
    pub fn sign_out(&mut self, cx: &mut Context<Self>) {
        let token = self.token.take();
        self.phase = AccountPhase::SignedOut;
        self.profile = None;
        self.login_error = None;
        self.notice = None;
        self.sync_password = None;
        self.pending.clear();
        self.conflict = None;
        self.uploading = false;
        self.persist(cx);
        self.forget_token(cx);
        if let (Some(service), Some(token)) = (self.services.clone(), token) {
            cx.background_spawn(async move {
                let _ = service.logout(token.as_str());
            })
            .detach();
        }
        cx.notify();
    }

    /// A password reset on the server ends every session of the account, so
    /// whatever token this computer held opens nothing now. The sign-in
    /// dialog runs the reset, and this is the one bit of store state it has
    /// to change.
    pub fn forget_after_reset(&mut self, cx: &mut Context<Self>) {
        self.token = None;
        self.phase = AccountPhase::SignedOut;
        self.login_error = None;
        self.sync_password = None;
        self.pending.clear();
        self.uploading = false;
        self.conflict = None;
        self.forget_token(cx);
        cx.notify();
    }

    /// Forget the rejected token and go back to signed out. The local
    /// profile stays on disk so an offline launch can retry; the UI treats
    /// the account as signed out either way.
    fn forget_session(
        &mut self,
        cx: &mut Context<Self>,
        notice: AccountNotice,
        remove_token: bool,
    ) {
        self.token = None;
        self.phase = AccountPhase::SignedOut;
        self.notice = Some(notice);
        self.pending.clear();
        self.uploading = false;
        self.conflict = None;
        if remove_token {
            self.forget_token(cx);
        }
        cx.notify();
    }

    fn apply_me(
        &mut self,
        result: Result<MeReply, AccountError>,
        token: Zeroizing<String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(me) => {
                self.token = Some(token);
                self.phase = AccountPhase::SignedIn;
                self.notice = None;
                self.login_error = None;
                self.apply_reply(&me);
                self.persist(cx);
            }
            Err(error) if error.is_unreachable() => {
                // Offline or intranet: not an error, just no cloud today.
                self.token = None;
                self.phase = AccountPhase::SignedOut;
                self.notice = Some(AccountNotice::Unreachable);
            }
            Err(_) => self.forget_session(cx, AccountNotice::Expired, true),
        }
        cx.notify();
    }

    fn apply_auth(
        &mut self,
        result: Result<super::service::AuthReply, AccountError>,
        phone_used: bool,
        password: Option<Zeroizing<String>>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(reply) => {
                self.token = Some(reply.token.clone());
                self.phase = AccountPhase::SignedIn;
                self.login_error = None;
                self.notice = None;
                self.apply_reply(&reply.user);
                self.remember_password(cx, password);
                self.store_token(cx, reply.token);
                self.persist(cx);
                if !phone_used {
                    cx.emit(AccountEvent::PhoneNotApplied);
                }
                self.start_next(cx);
            }
            Err(error) => {
                self.phase = AccountPhase::SignedOut;
                self.login_error = Some(error);
            }
        }
        cx.notify();
    }

    fn apply_reply(&mut self, me: &MeReply) {
        let profile = self.profile.get_or_insert_with(AccountProfile::default);
        profile.username = me.username.clone();
        if let Some(email) = &me.email {
            profile.email = email.clone();
        }
        profile.quota_bytes = me.quota_bytes;
        profile.used_bytes = me.used_bytes;
        if !me.sync_salt.is_empty() {
            profile.sync_salt = me.sync_salt.clone();
        }
    }

    fn remember_password(&mut self, cx: &mut Context<Self>, password: Option<Zeroizing<String>>) {
        self.sync_password = password;
        let remember = self
            .profile
            .as_ref()
            .is_some_and(|profile| profile.remember_sync_password);
        if remember {
            self.write_sync_password(cx);
        }
    }

    pub fn set_sync_password(&mut self, password: Zeroizing<String>, cx: &mut Context<Self>) {
        self.sync_password = Some(password);
        if self
            .profile
            .as_ref()
            .is_some_and(|profile| profile.remember_sync_password)
        {
            self.write_sync_password(cx);
        }
        self.start_next(cx);
        cx.notify();
    }

    pub fn has_sync_password(&self) -> bool {
        self.sync_password.is_some()
    }

    /// Whether the sync password is remembered across restarts.
    pub fn set_remember_sync_password(&mut self, remember: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        profile.remember_sync_password = remember;
        if remember {
            self.write_sync_password(cx);
        } else {
            let secrets = self.secrets.clone();
            cx.background_spawn(async move {
                let _ = secrets.delete(&SecretRef::AccountSyncPassword);
            })
            .detach();
        }
        self.persist(cx);
        cx.notify();
    }

    pub fn set_nickname(&mut self, nickname: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        profile.nickname = nickname.trim().chars().take(40).collect();
        self.persist(cx);
        cx.notify();
    }

    /// Copy a chosen picture into `account/avatars/` and point the profile
    /// at it. A new file name each time keeps the image cache from showing
    /// the old picture.
    pub fn set_avatar(&mut self, source: &Path, cx: &mut Context<Self>) -> Result<(), String> {
        if self.profile.is_none() {
            return Err(t!("account.error.expired").to_string());
        }
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .filter(|extension| {
                matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif")
            })
            .unwrap_or_else(|| "png".to_string());
        let directory = avatars_dir();
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let name = format!("{}.{extension}", Uuid::new_v4());
        let target = directory.join(&name);
        std::fs::copy(source, &target).map_err(|error| error.to_string())?;
        if let Some(profile) = self.profile.as_mut() {
            profile.avatar = Some(format!("account/avatars/{name}"));
        }
        self.persist(cx);
        cx.notify();
        Ok(())
    }

    pub fn set_cloud_sync(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if enabled && !self.is_signed_in() {
            return;
        }
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        profile.cloud_sync = enabled;
        self.persist(cx);
        cx.notify();
    }

    /// Queue a settings or hosts document for the cloud. Nothing happens
    /// when signed out: the local document is the only copy, as before.
    pub fn upload(&mut self, kind: SyncKind, plaintext: Vec<u8>, cx: &mut Context<Self>) {
        if !self.is_signed_in() {
            return;
        }
        self.pending.push_back(PendingUpload {
            kind,
            plaintext,
            base: None,
        });
        self.start_next(cx);
    }

    /// Queue both documents of a manual sync, settings first.
    pub fn upload_both(&mut self, settings: Vec<u8>, hosts: Vec<u8>, cx: &mut Context<Self>) {
        if !self.is_signed_in() {
            return;
        }
        self.pending.push_back(PendingUpload {
            kind: SyncKind::Settings,
            plaintext: settings,
            base: None,
        });
        self.pending.push_back(PendingUpload {
            kind: SyncKind::Hosts,
            plaintext: hosts,
            base: None,
        });
        self.start_next(cx);
    }

    fn start_next(&mut self, cx: &mut Context<Self>) {
        if self.uploading || self.conflict.is_some() {
            return;
        }
        let Some(service) = self.services.clone() else {
            return;
        };
        let Some(token) = self.token.clone() else {
            return;
        };
        let Some(password) = self.sync_password.clone() else {
            if !self.pending.is_empty() {
                cx.emit(AccountEvent::NeedsSyncPassword);
            }
            return;
        };
        let Some(profile) = self.profile.as_ref() else {
            return;
        };
        let salt = profile.sync_salt.clone();
        if salt.is_empty() {
            cx.emit(AccountEvent::CloudFailed(UploadError::Other(
                AccountError::Server(String::new()),
            )));
            return;
        }
        let Some(pending) = self.pending.pop_front() else {
            return;
        };
        let base = pending.base.or(match pending.kind {
            SyncKind::Settings => profile.settings_version,
            SyncKind::Hosts => profile.hosts_version,
        });
        let kind = pending.kind;
        let plaintext = pending.plaintext.clone();
        self.uploading = true;
        cx.spawn(async move |this, cx| {
            let uploading = plaintext.clone();
            let result = cx
                .background_spawn(async move {
                    let key = cloud_sync::derive_key(password.as_str(), &salt)
                        .map_err(|error| AccountError::Server(error.to_string()))?;
                    let sealed = cloud_sync::seal(&key, &uploading)
                        .map_err(|error| AccountError::Server(error.to_string()))?;
                    let digest = cloud_sync::sha256_hex(&sealed);
                    service.upload_document(token.as_str(), kind, sealed, &digest, base)
                })
                .await;
            this.update(cx, |store, cx| {
                store.uploading = false;
                store.apply_upload(kind, plaintext, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn apply_upload(
        &mut self,
        kind: SyncKind,
        plaintext: Vec<u8>,
        result: Result<DocumentReply, UploadError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(reply) => {
                if let Some(profile) = self.profile.as_mut() {
                    match kind {
                        SyncKind::Settings => profile.settings_version = Some(reply.version),
                        SyncKind::Hosts => profile.hosts_version = Some(reply.version),
                    }
                    profile.last_sync_at = Some(if reply.created_at.is_empty() {
                        chrono::Utc::now().to_rfc3339()
                    } else {
                        reply.created_at.clone()
                    });
                }
                self.persist(cx);
                cx.emit(AccountEvent::CloudUploaded {
                    kind,
                    version: reply.version,
                });
                self.start_next(cx);
            }
            Err(UploadError::Conflict { latest }) => {
                self.conflict = Some(CloudConflict {
                    kind,
                    latest,
                    plaintext,
                });
                cx.emit(AccountEvent::CloudConflict(
                    self.conflict.clone().expect("just set"),
                ));
            }
            Err(error) => {
                cx.emit(AccountEvent::CloudFailed(error));
            }
        }
        cx.notify();
    }

    /// 以本机覆盖云端: send the kept document again, based on the version
    /// the server reported.
    pub fn overwrite_conflict(&mut self, cx: &mut Context<Self>) {
        let Some(conflict) = self.conflict.take() else {
            return;
        };
        self.pending.push_front(PendingUpload {
            kind: conflict.kind,
            plaintext: conflict.plaintext,
            base: Some(conflict.latest.version),
        });
        self.start_next(cx);
        cx.notify();
    }

    /// 下载云端版本: open the server's document. Settings are handed to the
    /// workspace to apply; hosts are written beside the database for
    /// review, never applied behind the user's back.
    pub fn download_conflict(&mut self, cx: &mut Context<Self>) {
        if self.downloading {
            return;
        }
        let Some(conflict) = self.conflict.clone() else {
            return;
        };
        let Some(service) = self.services.clone() else {
            return;
        };
        let Some(token) = self.token.clone() else {
            return;
        };
        let Some(password) = self.sync_password.clone() else {
            cx.emit(AccountEvent::NeedsSyncPassword);
            return;
        };
        let Some(profile) = self.profile.as_ref() else {
            return;
        };
        let salt = profile.sync_salt.clone();
        let version = conflict.latest.version;
        let kind = conflict.kind;
        self.downloading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let sealed = service.download_document(token.as_str(), kind, version)?;
                    let key = cloud_sync::derive_key(password.as_str(), &salt)
                        .map_err(|error| AccountError::Server(error.to_string()))?;
                    cloud_sync::open(&key, &sealed)
                        .map_err(|error| AccountError::Server(error.to_string()))
                })
                .await;
            this.update(cx, |store, cx| {
                store.downloading = false;
                store.apply_download(kind, version, result, cx);
            })
            .ok();
        })
        .detach();
    }

    fn apply_download(
        &mut self,
        kind: SyncKind,
        version: i64,
        result: Result<Vec<u8>, AccountError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(plaintext) => {
                self.conflict = None;
                match kind {
                    SyncKind::Settings => cx.emit(AccountEvent::SettingsDownloaded(plaintext)),
                    SyncKind::Hosts => {
                        let directory = cloud_dir();
                        let path = directory.join(format!("hosts-{version}.json"));
                        let written = std::fs::create_dir_all(&directory)
                            .and_then(|()| std::fs::write(&path, plaintext));
                        match written {
                            Ok(()) => cx.emit(AccountEvent::HostsDownloaded(path)),
                            Err(error) => cx.emit(AccountEvent::PersistFailed(error.to_string())),
                        }
                    }
                }
                self.start_next(cx);
            }
            Err(error) => cx.emit(AccountEvent::CloudFailed(UploadError::Other(error))),
        }
        cx.notify();
    }

    fn persist(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let result = match self.profile.as_ref() {
            Some(profile) => profile.save(path),
            None => match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.to_string()),
            },
        };
        if let Err(error) = result {
            cx.emit(AccountEvent::PersistFailed(error));
        }
    }

    fn store_token(&self, cx: &mut Context<Self>, token: Zeroizing<String>) {
        let secrets = self.secrets.clone();
        cx.background_spawn(async move {
            let _ = secrets.set(&SecretRef::AccountToken, token.as_str());
        })
        .detach();
    }

    fn forget_token(&self, cx: &mut Context<Self>) {
        let secrets = self.secrets.clone();
        cx.background_spawn(async move {
            let _ = secrets.delete(&SecretRef::AccountToken);
        })
        .detach();
    }

    fn write_sync_password(&self, cx: &mut Context<Self>) {
        let Some(password) = self.sync_password.clone() else {
            return;
        };
        let secrets = self.secrets.clone();
        cx.background_spawn(async move {
            let _ = secrets.set(&SecretRef::AccountSyncPassword, password.as_str());
        })
        .detach();
    }
}
