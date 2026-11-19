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
use super::gravatar;
use super::model::{AccountNotice, AccountPhase, AccountProfile, AccountSnapshot};
use super::remote_avatar;
use super::service::{
    AccountError, DocumentReply, MAX_AVATAR_BYTES, MeReply, ProfileReply, SharedAccountService,
    SyncKind, UploadError,
};
use crate::app::{avatars_dir, cloud_dir, data_dir};
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
    /// 改密码 succeeded: the server ended every session, this computer's
    /// included, so the account is signed out again.
    PasswordChanged,
    /// 改邮箱 succeeded. The session stays valid; other devices see the new
    /// address on their next `me`.
    EmailChanged(String),
    /// A profile change was refused. The local state has already been put
    /// back to what the server still has.
    ProfileFailed {
        change: ProfileChange,
        error: AccountError,
    },
}

/// Which profile change failed, so the notice can name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileChange {
    Nickname,
    Avatar,
    AvatarRemoval,
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
    /// The email avatar service's picture for the current mailbox, when one
    /// was cached. `None` falls back to the initial.
    email_avatar: Option<PathBuf>,
    /// The mailbox whose picture was already asked for in this run, so a miss
    /// is not retried on every redraw.
    gravatar_for: Option<String>,
    /// The account server's picture, downloaded and cached. It stands in for
    /// a local upload on a computer that never chose this picture.
    server_avatar: Option<PathBuf>,
    /// The `avatar_url` already asked for in this run, so a miss is not
    /// retried on every redraw.
    server_avatar_for: Option<String>,
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
        let email_avatar = profile
            .as_ref()
            .filter(|profile| profile.avatar.is_none() && profile.email_avatar)
            .and_then(|profile| gravatar::cached(&profile.email));
        let server_avatar = profile
            .as_ref()
            .filter(|profile| profile.avatar.is_none())
            .and_then(|profile| profile.avatar_url.as_deref())
            .and_then(remote_avatar::cached);
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
            email_avatar,
            gravatar_for: None,
            server_avatar,
            server_avatar_for: None,
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

    /// The held login token, for a dialog that calls the server itself and
    /// only reports back once the request succeeded.
    pub fn token(&self) -> Option<Zeroizing<String>> {
        self.token.clone()
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
        self.email_avatar = self
            .profile
            .as_ref()
            .filter(|profile| profile.avatar.is_none() && profile.email_avatar)
            .and_then(|profile| gravatar::cached(&profile.email));
        self.gravatar_for = None;
        self.server_avatar = self
            .profile
            .as_ref()
            .filter(|profile| profile.avatar.is_none())
            .and_then(|profile| profile.avatar_url.as_deref())
            .and_then(remote_avatar::cached);
        self.server_avatar_for = None;
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
                avatar: profile
                    .avatar_path()
                    .or_else(|| self.server_avatar.clone())
                    .or_else(|| self.email_avatar.clone()),
                custom_avatar: profile.avatar.is_some() || profile.avatar_url.is_some(),
                initial: profile.initial(),
                email_avatar: profile.email_avatar,
                cloud_sync: profile.cloud_sync,
                remember_sync_password: profile.remember_sync_password,
                quota_bytes: profile.quota_bytes,
                used_bytes: profile.used_bytes,
                settings_version: profile.settings_version,
                hosts_version: profile.hosts_version,
                last_sync_at: profile.last_sync_at.clone(),
                sync_stale: profile.sync_stale,
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
        self.email_avatar = None;
        self.gravatar_for = None;
        self.server_avatar = None;
        self.server_avatar_for = None;
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
        self.email_avatar = None;
        self.gravatar_for = None;
        self.server_avatar = None;
        self.server_avatar_for = None;
        self.forget_token(cx);
        cx.notify();
    }

    /// 改密码 succeeded. The server ended every session of the account,
    /// including the one this computer held, so the store goes back to
    /// signed out. The remembered sync password is the old password and
    /// would derive a key the server no longer matches; it goes too.
    pub fn finish_password_change(&mut self, cx: &mut Context<Self>) {
        self.token = None;
        self.phase = AccountPhase::SignedOut;
        self.login_error = None;
        self.notice = None;
        self.sync_password = None;
        self.pending.clear();
        self.uploading = false;
        self.downloading = false;
        self.conflict = None;
        self.email_avatar = None;
        self.gravatar_for = None;
        self.server_avatar = None;
        self.server_avatar_for = None;
        self.forget_token(cx);
        // The cloud copies were sealed with the old password. They cannot be
        // opened with the new one, so record that until this computer
        // uploads again; the sync page says so and never pretends otherwise.
        if let Some(profile) = self.profile.as_mut() {
            profile.sync_stale = true;
            self.persist(cx);
        }
        let secrets = self.secrets.clone();
        cx.background_spawn(async move {
            let _ = secrets.delete(&SecretRef::AccountSyncPassword);
        })
        .detach();
        cx.emit(AccountEvent::PasswordChanged);
        cx.notify();
    }

    /// 改邮箱 succeeded. The mailbox is not the token, so the session stays;
    /// the profile takes the new address and, when the picture comes from
    /// the mailbox, the old one is dropped for the new address's.
    pub fn apply_email(&mut self, email: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        profile.email = email.clone();
        self.email_avatar = None;
        self.gravatar_for = None;
        self.persist(cx);
        self.refresh_email_avatar(cx);
        cx.emit(AccountEvent::EmailChanged(email));
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
        self.email_avatar = None;
        self.gravatar_for = None;
        self.server_avatar = None;
        self.server_avatar_for = None;
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
                self.refresh_server_avatar(cx);
                self.refresh_email_avatar(cx);
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
                self.refresh_server_avatar(cx);
                self.refresh_email_avatar(cx);
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

    /// Take what `me` or a sign-in reply says. The profile fields follow the
    /// same version rule as every other reply, so a stale local copy never
    /// replaces a newer server one.
    fn apply_reply(&mut self, me: &MeReply) {
        let other_user = self.profile.as_ref().is_some_and(|profile| {
            !profile.username.is_empty()
                && !me.username.is_empty()
                && profile.username != me.username
        });
        if other_user {
            // Another account signed in on this computer: the local picture,
            // the server picture and the versions are not this account's.
            if let Some(profile) = self.profile.as_mut() {
                if let Some(old) = profile.avatar.take() {
                    let _ = std::fs::remove_file(data_dir().join(old));
                }
                profile.nickname.clear();
                profile.username.clear();
                profile.avatar_url = None;
                profile.profile_version = 0;
                profile.settings_version = None;
                profile.hosts_version = None;
                profile.last_sync_at = None;
                profile.sync_stale = false;
            }
            self.server_avatar = None;
            self.server_avatar_for = None;
            self.email_avatar = None;
            self.gravatar_for = None;
        }
        self.profile.get_or_insert_with(AccountProfile::default);
        self.apply_profile(
            &ProfileReply {
                username: me.username.clone(),
                nickname: me.nickname.clone(),
                avatar_url: me.avatar_url.clone(),
                profile_version: me.profile_version,
                profile_updated_at: me.profile_updated_at.clone(),
            },
            false,
        );
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

    /// Set or clear the nickname (at most 24 characters, per the account
    /// API). The change shows at once and rolls back if the server refuses
    /// it, so the local state never claims more than the server has.
    pub fn set_nickname(&mut self, nickname: String, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        let next: String = nickname.trim().chars().take(24).collect();
        let username = profile.username.clone();
        let previous = std::mem::replace(&mut profile.nickname, next.clone());
        if previous == next {
            return;
        }
        self.persist(cx);
        cx.notify();
        let (Some(service), Some(token)) = (self.services.clone(), self.token.clone()) else {
            return;
        };
        let sent = (!next.is_empty()).then_some(next);
        cx.spawn(async move |this, cx| {
            let reply = cx
                .background_spawn(async move {
                    service.update_nickname(token.as_str(), sent.as_deref())
                })
                .await;
            this.update(cx, |store, cx| {
                store.nickname_replied(username, previous, reply, cx)
            })
            .ok();
        })
        .detach();
    }

    fn nickname_replied(
        &mut self,
        username: String,
        previous: String,
        reply: Result<ProfileReply, AccountError>,
        cx: &mut Context<Self>,
    ) {
        match reply {
            Ok(profile) => self.profile_applied(&profile, false, cx),
            Err(error) => {
                if self.belongs_to(&username) {
                    if let Some(profile) = self.profile.as_mut() {
                        profile.nickname = previous;
                    }
                    self.persist(cx);
                }
                cx.emit(AccountEvent::ProfileFailed {
                    change: ProfileChange::Nickname,
                    error,
                });
                cx.notify();
            }
        }
    }

    /// Copy a chosen picture into `account/avatars/` and upload it. The
    /// picture is checked against the API's limits here, before any request,
    /// so an oversized or unreadable file never leaves the computer. A new
    /// file name each time keeps the image cache from showing the old one.
    pub fn set_avatar(&mut self, source: &Path, cx: &mut Context<Self>) -> Result<(), String> {
        if self.profile.is_none() {
            return Err(t!("account.error.expired").to_string());
        }
        let picture = std::fs::read(source).map_err(|error| error.to_string())?;
        check_avatar(&picture)?;
        let extension = source
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .filter(|extension| matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp"))
            .unwrap_or_else(|| "png".to_string());
        let directory = avatars_dir();
        std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
        let name = format!("{}.{extension}", Uuid::new_v4());
        let target = directory.join(&name);
        std::fs::write(&target, &picture).map_err(|error| error.to_string())?;
        let relative = format!("account/avatars/{name}");
        let previous = self
            .profile
            .as_ref()
            .and_then(|profile| profile.avatar.clone());
        if let Some(profile) = self.profile.as_mut() {
            profile.avatar = Some(relative.clone());
        }
        self.persist(cx);
        cx.notify();
        let (Some(service), Some(token)) = (self.services.clone(), self.token.clone()) else {
            return Ok(());
        };
        cx.spawn(async move |this, cx| {
            let reply = cx
                .background_spawn(async move { service.upload_avatar(token.as_str(), picture) })
                .await;
            this.update(cx, |store, cx| {
                store.avatar_replied(previous, relative, target, reply, cx)
            })
            .ok();
        })
        .detach();
        Ok(())
    }

    fn avatar_replied(
        &mut self,
        previous: Option<String>,
        relative: String,
        target: PathBuf,
        reply: Result<ProfileReply, AccountError>,
        cx: &mut Context<Self>,
    ) {
        match reply {
            Ok(profile) => {
                if !self.is_signed_in() {
                    return;
                }
                // The local file is this very upload; keep it rather than
                // downloading the same picture back.
                self.profile_applied(&profile, true, cx);
                if let Some(old) = previous.filter(|old| old != &relative) {
                    let _ = std::fs::remove_file(data_dir().join(old));
                }
            }
            Err(error) => {
                if let Some(profile) = self.profile.as_mut()
                    && profile.avatar.as_deref() == Some(relative.as_str())
                {
                    profile.avatar = previous;
                }
                let _ = std::fs::remove_file(&target);
                self.persist(cx);
                cx.emit(AccountEvent::ProfileFailed {
                    change: ProfileChange::Avatar,
                    error,
                });
                cx.notify();
            }
        }
    }

    /// Remove the custom picture. It leaves the local display at once and
    /// goes back if the server refuses, so the button is never a no-op and
    /// the result is always visible.
    pub fn remove_avatar(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_ref() else {
            return;
        };
        if profile.avatar.is_none() && profile.avatar_url.is_none() {
            return;
        }
        let username = profile.username.clone();
        let previous_avatar = profile.avatar.clone();
        let previous_url = profile.avatar_url.clone();
        if let Some(profile) = self.profile.as_mut() {
            profile.avatar = None;
            profile.avatar_url = None;
        }
        self.server_avatar = None;
        self.server_avatar_for = None;
        self.persist(cx);
        cx.notify();
        let (Some(service), Some(token)) = (self.services.clone(), self.token.clone()) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let reply = cx
                .background_spawn(async move { service.delete_avatar(token.as_str()) })
                .await;
            this.update(cx, |store, cx| {
                store.avatar_removed(username, previous_avatar, previous_url, reply, cx)
            })
            .ok();
        })
        .detach();
    }

    fn avatar_removed(
        &mut self,
        username: String,
        previous_avatar: Option<String>,
        previous_url: Option<String>,
        reply: Result<ProfileReply, AccountError>,
        cx: &mut Context<Self>,
    ) {
        match reply {
            Ok(profile) => {
                if !self.is_signed_in() || !self.belongs_to(&username) {
                    return;
                }
                self.profile_applied(&profile, false, cx);
                if let Some(old) = previous_avatar {
                    let _ = std::fs::remove_file(data_dir().join(old));
                }
                self.server_avatar = None;
                self.server_avatar_for = None;
            }
            Err(error) => {
                if self.belongs_to(&username) {
                    if let Some(profile) = self.profile.as_mut() {
                        profile.avatar = previous_avatar;
                        profile.avatar_url = previous_url;
                    }
                    self.persist(cx);
                    self.refresh_server_avatar(cx);
                    self.refresh_email_avatar(cx);
                }
                cx.emit(AccountEvent::ProfileFailed {
                    change: ProfileChange::AvatarRemoval,
                    error,
                });
                cx.notify();
            }
        }
    }

    /// Whether the profile still belongs to the account a request was made
    /// for: another account may have signed in while it was out.
    fn belongs_to(&self, username: &str) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|profile| profile.username.is_empty() || profile.username == username)
    }

    /// Take a server profile snapshot, unless the local one is already at
    /// that version. The version only rises on a real server change, so a
    /// newer server profile always replaces an older local copy, and a local
    /// edit that has not reached the server is not overwritten by the same
    /// version coming back.
    fn apply_profile(&mut self, reply: &ProfileReply, keep_local_avatar: bool) -> bool {
        let Some(profile) = self.profile.as_mut() else {
            return false;
        };
        let other_user = !profile.username.is_empty()
            && !reply.username.is_empty()
            && profile.username != reply.username;
        // A reply for another account — a request that finished after the
        // user switched — is not this profile's to apply.
        if other_user {
            return false;
        }
        if reply.profile_version <= profile.profile_version {
            return false;
        }
        if !reply.username.is_empty() {
            profile.username = reply.username.clone();
        }
        profile.nickname = reply.nickname.clone().unwrap_or_default();
        let avatar_changed = profile.avatar_url != reply.avatar_url;
        profile.avatar_url = reply.avatar_url.clone();
        profile.profile_version = reply.profile_version;
        if avatar_changed
            && !keep_local_avatar
            && let Some(old) = profile.avatar.take()
        {
            let _ = std::fs::remove_file(data_dir().join(old));
        }
        true
    }

    fn profile_applied(
        &mut self,
        reply: &ProfileReply,
        keep_local_avatar: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.apply_profile(reply, keep_local_avatar) {
            return;
        }
        self.persist(cx);
        self.refresh_server_avatar(cx);
        self.refresh_email_avatar(cx);
        cx.notify();
    }

    /// The picture the account server names, from the cache, or (once per
    /// run) downloaded. A local upload stands in front of it.
    fn refresh_server_avatar(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_ref() else {
            self.server_avatar = None;
            self.server_avatar_for = None;
            return;
        };
        if profile.avatar.is_some() {
            self.server_avatar = None;
            return;
        }
        let Some(url) = profile.avatar_url.clone() else {
            self.server_avatar = None;
            self.server_avatar_for = None;
            return;
        };
        self.server_avatar = remote_avatar::cached(&url);
        if self.server_avatar.is_some()
            || !self.is_signed_in()
            || self.server_avatar_for.as_deref() == Some(url.as_str())
        {
            return;
        }
        let Some(service) = self.services.clone() else {
            return;
        };
        self.server_avatar_for = Some(url.clone());
        let wanted = url.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move { remote_avatar::fetch(service, url) })
                .await;
            let Ok(path) = found else {
                return;
            };
            this.update(cx, |store, cx| {
                let still_wanted = store.is_signed_in()
                    && store.profile.as_ref().is_some_and(|profile| {
                        profile.avatar.is_none()
                            && profile.avatar_url.as_deref() == Some(wanted.as_str())
                    });
                if !still_wanted || !path.is_file() {
                    return;
                }
                store.server_avatar = Some(path);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Pick the picture the avatar shows after a sign-in or a profile change:
    /// the email service's cache first, then (asynchronously, once per run)
    /// a request for the mailbox's picture. A missing one leaves the initial.
    fn refresh_email_avatar(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_ref() else {
            self.email_avatar = None;
            return;
        };
        let email = profile.email.trim().to_lowercase();
        if !profile.email_avatar {
            // The switch is off: use the initial, and ask nothing.
            self.email_avatar = None;
            self.gravatar_for = None;
            return;
        }
        if profile.avatar.is_some() || profile.avatar_url.is_some() || email.is_empty() {
            // A picture of the user's own — uploaded here or chosen on
            // another device — stands ahead of the mailbox service, so it
            // is not asked for at all.
            self.email_avatar = None;
            return;
        }
        self.email_avatar = gravatar::cached(&email);
        if self.email_avatar.is_some()
            || !self.is_signed_in()
            || self.gravatar_for.as_deref() == Some(email.as_str())
        {
            return;
        }
        self.gravatar_for = Some(email.clone());
        let wanted = email.clone();
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_spawn(async move { gravatar::fetch(&email) })
                .await;
            let Ok(path) = found else {
                return;
            };
            this.update(cx, |store, cx| {
                // The mailbox or the chosen picture may have changed while
                // the request was out; only a still-wanted picture is used.
                let still_wanted = store.is_signed_in()
                    && store.profile.as_ref().is_some_and(|profile| {
                        profile.avatar.is_none()
                            && profile.avatar_url.is_none()
                            && profile.email.trim().to_lowercase() == wanted
                    });
                if !still_wanted || !path.is_file() {
                    return;
                }
                store.email_avatar = Some(path);
                cx.notify();
            })
            .ok();
        })
        .detach();
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

    /// Turn the mailbox avatar service on or off. Off clears the picture in
    /// hand and makes sure nothing is requested; the choice is local only.
    pub fn set_email_avatar(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let Some(profile) = self.profile.as_mut() else {
            return;
        };
        profile.email_avatar = enabled;
        self.persist(cx);
        self.refresh_email_avatar(cx);
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
                    // Both documents go out together. Once the hosts one
                    // lands and nothing is queued, the cloud is sealed with
                    // the password in hand again.
                    if kind == SyncKind::Hosts && self.pending.is_empty() {
                        profile.sync_stale = false;
                    }
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
                if let Some(profile) = self.profile.as_mut()
                    && profile.sync_stale
                {
                    // It opened with the password in hand, so the cloud copy
                    // is current after all (another device uploaded it with
                    // the new password).
                    profile.sync_stale = false;
                    self.persist(cx);
                }
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

/// The API's picture rule, checked before anything leaves the computer:
/// PNG, JPEG or WebP, at most 2 MiB. The crop dialog always writes a small
/// PNG; this guards the paths that do not go through it.
fn check_avatar(picture: &[u8]) -> Result<(), String> {
    if picture.len() > MAX_AVATAR_BYTES {
        return Err(t!("account.avatar.too_large").to_string());
    }
    match image::guess_format(picture) {
        Ok(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP) => Ok(()),
        _ => Err(t!("account.avatar.unsupported_format").to_string()),
    }
}
