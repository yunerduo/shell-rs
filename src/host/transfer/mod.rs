//! Exporting and importing the app's own connection information.
//!
//! The file format and its sealing live in [`shellrs_hosts::portable`]; this
//! is the app's share: gathering the store's hosts, groups and keychain
//! secrets into an archive, and merging an archive back into the store with
//! a count of what was added, what was skipped and what still needs a
//! password or a key.

mod export_dialog;
mod import_dialog;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::{Context, SharedString};
use zeroize::Zeroizing;

use crate::host::{
    AuthKind, Credential, CredentialDraft, CredentialId, CredentialKind, GroupDraft, GroupId,
    HostDraft, HostId, HostOs, HostStore,
};
use crate::secrets::{SecretRef, SharedSecretStore};
use shellrs_hosts::portable::{Archive, ArchiveError, ArchivedGroup, ArchivedHost, encode};
use shellrs_hosts::read_small;

pub use export_dialog::open_export_hosts_dialog;
pub use import_dialog::open_import_hosts_dialog;

use crate::i18n::t;

/// What one export will fill from the keychain or the key files, once the
/// work is off the UI thread.
#[derive(Clone, Debug)]
pub enum PendingSecret {
    /// A host's saved login password.
    Password { host: usize, secret: SecretRef },
    /// A key file's passphrase, for every host logging in with it.
    Passphrase { host: usize, key_path: String },
    /// The text of a key ShellAs keeps, so the other machine has the key.
    KeyText { host: usize, path: String },
}

/// Everything one export gathered on the UI thread.
pub struct ExportPlan {
    pub archive: Archive,
    pub pending: Vec<PendingSecret>,
    /// Hosts left out because they are not SSH: Telnet and serial hosts
    /// carry a device and a login script, which this file does not describe.
    pub skipped: usize,
}

impl ExportPlan {
    pub fn hosts(&self) -> usize {
        self.archive.hosts.len()
    }

    pub fn groups(&self) -> usize {
        self.archive.groups.len()
    }
}

/// What an export carried, for the notification that follows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExportDone {
    pub hosts: usize,
    pub groups: usize,
    pub skipped: usize,
}

/// An export the dialog has decided on: what to write, sealed or not.
pub struct ExportRequest {
    pub plan: ExportPlan,
    pub password: Option<Zeroizing<String>>,
    pub path: PathBuf,
}

/// Why an export did not finish.
#[derive(Debug)]
pub enum ExportError {
    /// The keychain could not be read.
    Keychain(String),
    /// The file could not be written (the path, or the disk).
    Write(std::io::Error),
    /// The archive could not be encoded; see [`ArchiveError`].
    Encode(ArchiveError),
}

impl ExportError {
    pub fn message(&self) -> SharedString {
        match self {
            ExportError::Keychain(error) => {
                t!("host.portable.error.keychain", error = error.clone())
            }
            ExportError::Write(error) => t!("host.portable.error.write", error = error.to_string()),
            ExportError::Encode(error) => archive_error(*error),
        }
    }
}

/// What a file says when it cannot be read.
pub fn archive_error(error: ArchiveError) -> SharedString {
    match error {
        ArchiveError::NotArchive => t!("host.portable.error.not_archive"),
        ArchiveError::TooNew => t!("host.portable.error.too_new"),
        ArchiveError::Damaged => t!("host.portable.error.damaged"),
        ArchiveError::NeedsPassword => t!("host.portable.error.needs_password"),
        ArchiveError::WrongPassword => t!("host.portable.error.wrong_password"),
        ArchiveError::SecretWithoutPassword => t!("host.portable.error.secret_without_password"),
    }
}

/// Gather the hosts and groups an export would carry. Secrets stay out
/// unless `include_secrets`, and are only read later, on the background
/// executor.
pub fn plan_export(store: &HostStore, include_secrets: bool) -> ExportPlan {
    let groups = store
        .groups()
        .iter()
        .map(|group| ArchivedGroup {
            id: group.id.0.to_string(),
            name: group.name.to_string(),
            parent: group.parent.map(|parent| parent.0.to_string()),
        })
        .collect();

    let mut archive = Archive {
        groups,
        hosts: Vec::new(),
    };
    let mut pending = Vec::new();
    let mut skipped = 0;
    for host in store.hosts() {
        if !host.is_ssh() {
            skipped += 1;
            continue;
        }
        let index = archive.hosts.len();
        let credential = host.credential.and_then(|id| store.credential(id));
        let (auth, key_path): (&str, Option<String>) = match credential {
            Some(credential) => match credential.kind {
                CredentialKind::Key => (
                    "key",
                    credential.key_path.as_ref().map(|path| path.to_string()),
                ),
                CredentialKind::Password => ("password", None),
                CredentialKind::Agent => ("agent", None),
            },
            None => (
                match host.auth {
                    AuthKind::Password => "password",
                    AuthKind::NoPassword => "none",
                },
                None,
            ),
        };
        if include_secrets {
            match (credential, auth) {
                (Some(credential), "password") => pending.push(PendingSecret::Password {
                    host: index,
                    secret: credential.password_secret(),
                }),
                (None, "password") => pending.push(PendingSecret::Password {
                    host: index,
                    secret: host.password_secret(),
                }),
                _ => {}
            }
            if let Some(path) = key_path.as_deref() {
                pending.push(PendingSecret::Passphrase {
                    host: index,
                    key_path: path.to_owned(),
                });
                // A key the user keeps elsewhere stays where it is: only the
                // path travels, and the other machine may not have the file.
                if credential.is_some_and(|credential| credential.keeps_key_in(store.key_dir())) {
                    pending.push(PendingSecret::KeyText {
                        host: index,
                        path: path.to_owned(),
                    });
                }
            }
        }
        archive.hosts.push(ArchivedHost {
            name: host.name.to_string(),
            address: host.address.to_string(),
            port: host.port,
            user: host.user.to_string(),
            auth: auth.to_owned(),
            key_path,
            private_key: None,
            passphrase: None,
            password: None,
            os: host.os.map(|os| os.as_str().to_owned()),
            notes: host.notes.to_string(),
            group: host.group.map(|group| group.0.to_string()),
        });
    }
    ExportPlan {
        archive,
        pending,
        skipped,
    }
}

/// Fill the secrets a plan asked for and write the file. Runs on the
/// background executor: it reads the keychain.
pub fn finish_export(
    request: ExportRequest,
    secrets: &SharedSecretStore,
) -> Result<ExportDone, ExportError> {
    let ExportRequest {
        mut plan,
        password,
        path,
    } = request;
    let mut passphrases: HashMap<String, Option<Zeroizing<String>>> = HashMap::new();
    let mut keys: HashMap<String, Option<String>> = HashMap::new();
    for pending in std::mem::take(&mut plan.pending) {
        match pending {
            PendingSecret::Password { host, secret } => {
                let value = secrets
                    .get(&secret)
                    .map_err(|error| ExportError::Keychain(error.to_string()))?;
                plan.archive.hosts[host].password = value.map(|value| value.to_string());
            }
            PendingSecret::Passphrase { host, key_path } => {
                // One key file has one passphrase, however many hosts use it.
                let value = match passphrases.get(&key_path) {
                    Some(cached) => cached.clone(),
                    None => {
                        let read = secrets
                            .get(&SecretRef::passphrase(&key_path))
                            .map_err(|error| ExportError::Keychain(error.to_string()))?;
                        passphrases.insert(key_path.clone(), read.clone());
                        read
                    }
                };
                plan.archive.hosts[host].passphrase = value.map(|value| value.to_string());
            }
            PendingSecret::KeyText { host, path } => {
                let text = match keys.get(&path) {
                    Some(cached) => cached.clone(),
                    None => {
                        let read = read_small(Path::new(&path)).map(|text| text.to_string());
                        keys.insert(path.clone(), read.clone());
                        read
                    }
                };
                plan.archive.hosts[host].private_key = text;
            }
        }
    }
    let text = encode(&plan.archive, password.as_deref().map(String::as_str))
        .map_err(ExportError::Encode)?;
    std::fs::write(&path, text).map_err(ExportError::Write)?;
    Ok(ExportDone {
        hosts: plan.archive.hosts.len(),
        groups: plan.archive.groups.len(),
        skipped: plan.skipped,
    })
}

/// What an import would do, before it does it: shown in the dialog so the
/// result is no surprise.
#[derive(Clone, Debug, Default)]
pub struct ImportPreview {
    pub hosts: usize,
    pub groups: usize,
    /// Groups in the file that merge into a saved group of the same name
    /// under the same parent.
    pub groups_reused: usize,
    /// Hosts this file holds that the store already has, by name or by
    /// address and port. An import never changes a saved host.
    pub conflicts: Vec<SharedString>,
    /// Hosts that will ask for a password on their first connection.
    pub needs_password: Vec<SharedString>,
    /// Hosts whose key is neither in the file nor on this machine.
    pub needs_key: Vec<SharedString>,
}

impl ImportPreview {
    /// Hosts the import would add.
    pub fn importable(&self) -> usize {
        self.hosts.saturating_sub(self.conflicts.len())
    }
}

/// What an import did.
#[derive(Clone, Debug, Default)]
pub struct ImportSummary {
    pub hosts_added: usize,
    pub hosts_skipped: usize,
    pub groups_added: usize,
    /// Groups the file names that were already there, by name and parent.
    pub groups_reused: usize,
    pub secrets_saved: usize,
    pub needs_password: Vec<SharedString>,
    pub needs_key: Vec<SharedString>,
}

/// Look at an archive against what is saved now, without changing anything.
pub fn preview_import(store: &HostStore, archive: &Archive) -> ImportPreview {
    let mut preview = ImportPreview {
        hosts: archive.hosts.len(),
        groups: archive.groups.len(),
        ..ImportPreview::default()
    };
    preview.groups_reused = resolved_groups(store, archive)
        .values()
        .filter(|saved| saved.is_some())
        .count();
    for host in &archive.hosts {
        let name: SharedString = host.name.as_str().into();
        if already_saved(store, host) {
            preview.conflicts.push(name);
            continue;
        }
        if host.auth == "password" && host.password.is_none() {
            preview.needs_password.push(name.clone());
        }
        if host.auth == "key" && !key_available(host) {
            preview.needs_key.push(name);
        }
    }
    preview
}

/// Merge the archive into the store: groups with the same name under the
/// same parent are reused, hosts already saved are skipped, and passwords
/// and passphrases are handed to the keychain.
///
/// A saved host is never changed: a file host with the same name, or the
/// same address and port, is skipped and counted.
pub fn apply_import(
    store: &mut HostStore,
    archive: &Archive,
    cx: &mut Context<HostStore>,
) -> ImportSummary {
    let mut summary = ImportSummary::default();
    let group_ids = insert_groups(store, archive, &mut summary, cx);
    let mut passphrases: std::collections::HashSet<String> = std::collections::HashSet::new();

    for host in &archive.hosts {
        if already_saved(store, host) {
            summary.hosts_skipped += 1;
            continue;
        }
        let name: SharedString = host.name.as_str().into();
        let group = host
            .group
            .as_deref()
            .and_then(|id| group_ids.get(id).copied());

        // A key in the file is written into ShellAs's own keys folder, so
        // the other machine has it. A key path only travels as a path: it
        // works here only if the file is here too.
        let key_path = match host.auth.as_str() {
            "key" => {
                let written = host
                    .private_key
                    .as_deref()
                    .and_then(|text| store.save_private_key(None, text).ok())
                    .map(|path| path.to_string());
                match written.or_else(|| {
                    host.key_path
                        .clone()
                        .filter(|path| Path::new(path).is_file())
                }) {
                    Some(path) => Some(path),
                    None => {
                        summary.needs_key.push(name.clone());
                        None
                    }
                }
            }
            _ => None,
        };

        let credential = match (host.auth.as_str(), key_path.as_deref()) {
            ("key", Some(path)) => Some(existing_or_new_key(store, host, path, cx)),
            ("agent", _) => Some(existing_or_new_agent(store, host, cx)),
            _ => None,
        };
        let auth = match credential {
            // A host using a credential keeps the default here; the
            // credential says how it logs in.
            Some(_) => AuthKind::NoPassword,
            None if host.auth == "password" => AuthKind::Password,
            None => AuthKind::NoPassword,
        };
        let mut draft = HostDraft::new(
            host.name.as_str(),
            host.address.as_str(),
            host.port,
            host.user.as_str(),
            auth,
            group,
        );
        draft.credential = credential;
        draft.notes = host.notes.as_str().into();
        let id: HostId = store.insert(draft, cx);
        if let Some(os) = host.os.as_deref().and_then(HostOs::from_stored) {
            store.set_host_os(id, Some(os), cx);
        }

        if credential.is_none() && host.auth == "password" {
            match host.password.as_deref() {
                Some(password) => {
                    let secret =
                        SecretRef::password(host.user.as_str(), host.address.as_str(), host.port);
                    store.save_secret(secret, Some(password.to_owned()), cx);
                    summary.secrets_saved += 1;
                }
                None => summary.needs_password.push(name.clone()),
            }
        }
        if let (Some(path), Some(passphrase)) = (key_path.as_deref(), host.passphrase.as_deref())
            && passphrases.insert(path.to_owned())
        {
            store.save_secret(SecretRef::passphrase(path), Some(passphrase.to_owned()), cx);
            summary.secrets_saved += 1;
        }
        summary.hosts_added += 1;
    }
    summary
}

/// Make sure every group the file names is there, reusing a saved group of
/// the same name under the same parent. Parents are made first, so a
/// nested tree comes out as it went in, and the file ids are mapped to the
/// store's ids.
fn insert_groups(
    store: &mut HostStore,
    archive: &Archive,
    summary: &mut ImportSummary,
    cx: &mut Context<HostStore>,
) -> HashMap<String, GroupId> {
    let mut ids: HashMap<String, GroupId> = HashMap::new();
    let mut waiting: Vec<&ArchivedGroup> = archive.groups.iter().collect();
    loop {
        if waiting.is_empty() {
            break;
        }
        let before = waiting.len();
        let mut deferred = Vec::new();
        for group in waiting {
            let parent = match group.parent.as_deref() {
                Some(parent) => match ids.get(parent) {
                    Some(id) => Some(*id),
                    // The parent is in the file but not made yet: later.
                    None if archive.groups.iter().any(|other| other.id == parent) => {
                        deferred.push(group);
                        continue;
                    }
                    // The parent is not in the file: keep the group, rooted.
                    None => None,
                },
                None => None,
            };
            ids.insert(
                group.id.clone(),
                group_in(store, parent, &group.name, summary, cx),
            );
        }
        if deferred.len() == before {
            // A cycle in the file (only a hand-written one): root the rest.
            for group in deferred {
                ids.insert(
                    group.id.clone(),
                    group_in(store, None, &group.name, summary, cx),
                );
            }
            break;
        }
        waiting = deferred;
    }
    ids
}

fn group_in(
    store: &mut HostStore,
    parent: Option<GroupId>,
    name: &str,
    summary: &mut ImportSummary,
    cx: &mut Context<HostStore>,
) -> GroupId {
    match existing_group(store, parent, name) {
        Some(id) => {
            summary.groups_reused += 1;
            id
        }
        None => {
            summary.groups_added += 1;
            store.insert_group(GroupDraft::new(name, parent), cx)
        }
    }
}

/// The saved group a file group merges into: `Some` with the saved id, or
/// `None` when the import would make it.
fn resolved_groups(store: &HostStore, archive: &Archive) -> HashMap<String, Option<GroupId>> {
    let mut resolved: HashMap<String, Option<GroupId>> = HashMap::new();
    let mut waiting: Vec<&ArchivedGroup> = archive.groups.iter().collect();
    loop {
        if waiting.is_empty() {
            break;
        }
        let before = waiting.len();
        let mut deferred = Vec::new();
        for group in waiting {
            let (parent, parent_is_saved) = match group.parent.as_deref() {
                Some(parent) => match resolved.get(parent) {
                    Some(Some(id)) => (Some(*id), true),
                    // A group under one the import makes cannot be an
                    // existing group: its parent has no saved id yet.
                    Some(None) => (None, false),
                    None if archive.groups.iter().any(|other| other.id == parent) => {
                        deferred.push(group);
                        continue;
                    }
                    // The parent is not in the file: keep the group, rooted.
                    None => (None, true),
                },
                None => (None, true),
            };
            let saved = parent_is_saved
                .then(|| existing_group(store, parent, &group.name))
                .flatten();
            resolved.insert(group.id.clone(), saved);
        }
        if deferred.len() == before {
            for group in deferred {
                let saved = existing_group(store, None, &group.name);
                resolved.insert(group.id.clone(), saved);
            }
            break;
        }
        waiting = deferred;
    }
    resolved
}

/// Whether the store already saves a host this one would duplicate: the
/// same name, or the same address and port. An import never changes a saved
/// host.
fn already_saved(store: &HostStore, host: &ArchivedHost) -> bool {
    let name = host.name.trim();
    store.hosts().iter().any(|saved| {
        saved.is_ssh()
            && ((saved.address.as_ref() == host.address && saved.port == host.port)
                || saved.name.trim() == name)
    })
}

fn key_available(host: &ArchivedHost) -> bool {
    host.private_key.is_some()
        || host
            .key_path
            .as_deref()
            .is_some_and(|path| Path::new(path).is_file())
}

fn existing_group(store: &HostStore, parent: Option<GroupId>, name: &str) -> Option<GroupId> {
    store
        .groups()
        .iter()
        .find(|group| group.parent == parent && group.name.as_ref() == name)
        .map(|group| group.id)
}

fn existing_or_new_key(
    store: &mut HostStore,
    host: &ArchivedHost,
    key_path: &str,
    cx: &mut Context<HostStore>,
) -> CredentialId {
    if let Some(credential) = store
        .credentials()
        .iter()
        .find(|credential| {
            credential.kind == CredentialKind::Key
                && credential.user.as_ref() == host.user
                && credential.key_path.as_deref() == Some(key_path)
        })
        .map(|credential| credential.id)
    {
        return credential;
    }
    store.insert_credential(
        CredentialDraft::new(
            credential_name(host),
            CredentialKind::Key,
            host.user.as_str(),
        )
        .with_key_path(key_path),
        cx,
    )
}

fn existing_or_new_agent(
    store: &mut HostStore,
    host: &ArchivedHost,
    cx: &mut Context<HostStore>,
) -> CredentialId {
    if let Some(credential) = store
        .credentials()
        .iter()
        .find(|credential: &&Credential| {
            credential.kind == CredentialKind::Agent && credential.user.as_ref() == host.user
        })
        .map(|credential| credential.id)
    {
        return credential;
    }
    store.insert_credential(
        CredentialDraft::new(
            credential_name(host),
            CredentialKind::Agent,
            host.user.as_str(),
        ),
        cx,
    )
}

fn credential_name(host: &ArchivedHost) -> String {
    format!("{} ({})", host.name, host.user)
}

/// The path a file dialog should suggest, and where exports go when the
/// picker is dismissed: the user's home.
pub fn default_export_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{ExportRequest, apply_import, finish_export, plan_export, preview_import};
    use crate::host::{HostOs, HostStore};
    use crate::secrets::{InMemorySecretStore, SecretRef, SharedSecretStore};
    use gpui_kit::{AppContext as _, TestAppContext};
    use shellrs_hosts::portable::{Archive, ArchivedGroup, ArchivedHost, decode, encode};
    use zeroize::Zeroizing;

    fn host(name: &str, address: &str, auth: &str) -> ArchivedHost {
        ArchivedHost {
            name: name.to_owned(),
            address: address.to_owned(),
            port: 22,
            user: "root".to_owned(),
            auth: auth.to_owned(),
            key_path: None,
            private_key: None,
            passphrase: None,
            password: None,
            os: Some("ubuntu".to_owned()),
            notes: String::new(),
            group: None,
        }
    }

    #[test]
    fn a_plan_keeps_groups_and_hosts_and_leaves_secrets_out() {
        let store = HostStore::seed();
        let plan = plan_export(&store, false);
        assert_eq!(plan.hosts(), 6);
        assert_eq!(plan.groups(), 3);
        assert!(plan.pending.is_empty());
        assert!(!plan.archive.has_secrets());
        // Hosts keep their group, named by the file's own group id.
        assert!(plan.archive.hosts.iter().any(|host| host.group.is_some()));
        // Only SSH hosts travel; the seed holds nothing else.
        assert_eq!(plan.skipped, 0);
    }

    #[test]
    fn preview_names_the_conflicts_and_the_hosts_that_will_ask_for_a_password() {
        let store = HostStore::seed();
        let archive = Archive {
            groups: vec![ArchivedGroup {
                id: "g1".to_owned(),
                name: "生产".to_owned(),
                parent: None,
            }],
            hosts: vec![
                host("web-01", "10.0.0.5", "password"),
                host("新机器", "10.9.9.9", "password"),
            ],
        };
        let preview = preview_import(&store, &archive);
        assert_eq!(preview.hosts, 2);
        assert_eq!(preview.importable(), 1);
        assert_eq!(preview.conflicts.len(), 1);
        assert_eq!(preview.conflicts[0].as_ref(), "web-01");
        assert_eq!(preview.needs_password.len(), 1);
        assert_eq!(preview.needs_password[0].as_ref(), "新机器");
        // 生产 already exists at the root, so the file's group merges.
        assert_eq!(preview.groups_reused, 1);
    }

    #[test]
    fn a_file_with_a_saved_password_round_trips_through_its_password() {
        let mut archive = Archive {
            groups: Vec::new(),
            hosts: vec![host("app", "10.0.0.7", "password")],
        };
        archive.hosts[0].password = Some("hunter2".to_owned());
        let text = encode(&archive, Some("file password")).unwrap();
        assert!(!text.contains("hunter2"));
        assert_eq!(decode(&text, Some("file password")).unwrap(), archive);
    }

    #[gpui_kit::test]
    fn importing_into_an_empty_store_recreates_the_tree(cx: &mut TestAppContext) {
        let source = HostStore::seed();
        let archive = plan_export(&source, false).archive;
        // The seed's groups are all at the root; nest one to prove the
        // parent mapping survives.
        let mut archive = archive;
        let nested = archive.groups.iter().position(|group| {
            archive
                .hosts
                .iter()
                .any(|host| host.group.as_deref() == Some(group.id.as_str()))
        });
        let nested = nested.map(|index| archive.groups[index].id.clone());
        if let Some(nested) = nested {
            archive.groups.push(ArchivedGroup {
                id: "extra".to_owned(),
                name: "华东".to_owned(),
                parent: Some(nested),
            });
            archive.hosts[0].group = Some("extra".to_owned());
        }

        let store = cx.new(|_| HostStore::empty());
        archive.hosts[0].os = Some("ubuntu".to_owned());
        let summary = store.update(cx, |store, cx| apply_import(store, &archive, cx));
        assert_eq!(summary.hosts_added, archive.hosts.len());
        assert_eq!(summary.hosts_skipped, 0);
        assert_eq!(summary.groups_added, archive.groups.len());
        assert!(summary.needs_password.len() == archive.hosts.len());

        let (hosts, groups) = cx.update(|cx| {
            let store = store.read(cx);
            (store.hosts().len(), store.groups().len())
        });
        assert_eq!(hosts, 6);
        assert_eq!(groups, 4);
        let (child_id, child_parent, host_group, host_os) = cx.update(|cx| {
            let store = store.read(cx);
            let child = store
                .groups()
                .iter()
                .find(|group| group.name.as_ref() == "华东")
                .unwrap();
            let host = store
                .hosts()
                .iter()
                .find(|host| host.name.as_ref() == archive.hosts[0].name)
                .unwrap();
            (child.id, child.parent, host.group, host.os)
        });
        assert!(child_parent.is_some());
        assert_eq!(host_group, Some(child_id));
        assert_eq!(host_os, Some(HostOs::Ubuntu));
    }

    #[gpui_kit::test]
    fn an_export_with_secrets_comes_back_into_an_empty_store(cx: &mut TestAppContext) {
        let source_secrets: SharedSecretStore = Arc::new(InMemorySecretStore::default());
        source_secrets
            .set(&SecretRef::password("root", "10.0.1.12", 22), "hunter2")
            .unwrap();
        let source = HostStore::seed().with_secrets(source_secrets.clone());
        let plan = plan_export(&source, true);
        assert!(!plan.pending.is_empty());

        let path =
            std::env::temp_dir().join(format!("shellas-transfer-test-{}.json", std::process::id()));
        let done = finish_export(
            ExportRequest {
                plan,
                password: Some(Zeroizing::new("file password".to_owned())),
                path: path.clone(),
            },
            &source_secrets,
        )
        .unwrap();
        assert_eq!(done.hosts, 6);
        assert_eq!(done.groups, 3);

        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        // The file is sealed: the password is nowhere in it in the clear.
        assert!(!text.contains("hunter2"));
        assert!(!text.contains("file password"));

        let archive = decode(&text, Some("file password")).unwrap();
        let carried = archive
            .hosts
            .iter()
            .filter(|host| host.password.as_deref() == Some("hunter2"))
            .count();
        assert_eq!(carried, 1);
        assert!(decode(&text, Some("wrong")).is_err());

        let target_secrets: SharedSecretStore = Arc::new(InMemorySecretStore::default());
        let store = cx.new(|_| HostStore::empty().with_secrets(target_secrets.clone()));
        let summary = store.update(cx, |store, cx| apply_import(store, &archive, cx));
        cx.run_until_parked();
        assert_eq!(summary.hosts_added, 6);
        assert_eq!(summary.secrets_saved, 1);
        assert!(summary.needs_password.len() == 5);
        assert_eq!(
            target_secrets
                .get(&SecretRef::password("root", "10.0.1.12", 22))
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("hunter2")
        );
    }

    #[gpui_kit::test]
    fn importing_again_skips_every_host_and_reuses_every_group(cx: &mut TestAppContext) {
        let archive = plan_export(&HostStore::seed(), false).archive;
        let store = cx.new(|_| HostStore::seed());
        let summary = store.update(cx, |store, cx| apply_import(store, &archive, cx));
        assert_eq!(summary.hosts_added, 0);
        assert_eq!(summary.hosts_skipped, 6);
        assert_eq!(summary.groups_added, 0);
        assert_eq!(summary.groups_reused, 3);
        assert_eq!(cx.update(|cx| store.read(cx).hosts().len()), 6);
    }
}
