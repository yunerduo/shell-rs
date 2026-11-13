//! What an import adds: the rows the dialog lists, with their defaults,
//! checked as a whole before anything is written, then written in one go.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use gpui_kit::{Context, SharedString};
use zeroize::Zeroizing;

use crate::i18n::t;

use super::super::{
    AuthKind, CredentialDraft, CredentialId, CredentialKind, DEFAULT_USER, ErrorMessage as _,
    GroupPathError, HostDraft, HostDraftError, HostStore, read_small,
};
use super::{ImportCandidate, ImportProtocol, ImportScan, passwords};

/// How an imported host logs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowAuth {
    NoPassword,
    /// A password, asked for at each connection.
    Password,
    /// A saved credential.
    Credential(CredentialId),
    /// A key credential made by the import for the row's identity file.
    NewKey,
}

/// One host the import dialog lists: what the source gave and what the
/// user made of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportRow {
    pub candidate: ImportCandidate,
    pub selected: bool,
    pub name: String,
    /// A group path such as `生产/华东`; empty for no group.
    pub group: String,
    pub user: String,
    pub auth: RowAuth,
    /// Whether a saved SSH host is already at this address and port. Such a
    /// row is shown but never imported.
    pub existing: bool,
    /// Whether a master password opened this row's stored password: the
    /// table then says it will be saved, not asked for. Only for display;
    /// the plan opens it again when the import is made.
    pub password_ready: bool,
}

impl ImportRow {
    pub fn will_import(&self) -> bool {
        self.selected && !self.existing
    }
}

/// The rows a scan gives, with their defaults: hosts already saved are
/// left out of the selection, and a host with a key file of its own logs
/// in with a key credential, else without a password.
pub fn import_rows(scan: &ImportScan, store: &HostStore) -> Vec<ImportRow> {
    scan.candidates
        .iter()
        .map(|candidate| {
            let existing = is_saved(store, candidate);
            ImportRow {
                candidate: candidate.clone(),
                selected: !existing,
                name: candidate.name.clone(),
                group: candidate.group.clone(),
                user: candidate.user.clone(),
                auth: default_auth(candidate, store),
                existing,
                password_ready: false,
            }
        })
        .collect()
}

/// Mark the rows whose stored password the master password opens. The
/// master password stays in the dialog: only this flag is kept.
pub fn mark_passwords(rows: &mut [ImportRow], master: &str) {
    for row in rows {
        row.password_ready = row.candidate.needs_password
            && row
                .candidate
                .password_cipher
                .as_deref()
                .is_some_and(|stored| passwords::decrypt(master, stored).is_some());
    }
}

/// Bring rows up to date with a store that changed while the dialog was
/// open: a host saved meanwhile makes its row existing, and a credential
/// deleted meanwhile leaves its rows without a password.
pub fn refresh_rows(rows: &mut [ImportRow], store: &HostStore) {
    for row in rows {
        let existing = is_saved(store, &row.candidate);
        if existing && !row.existing {
            row.selected = false;
        }
        row.existing = existing;
        if let RowAuth::Credential(id) = row.auth
            && store.credential(id).is_none()
        {
            row.auth = RowAuth::NoPassword;
        }
    }
}

/// Whether a saved host of the same protocol is at this address (any case)
/// and port.
fn is_saved(store: &HostStore, candidate: &ImportCandidate) -> bool {
    store.hosts().iter().any(|host| {
        host.port == candidate.port
            && host.address.eq_ignore_ascii_case(&candidate.address)
            && match candidate.protocol {
                ImportProtocol::Ssh => host.is_ssh(),
                ImportProtocol::Telnet => host.protocol.is_telnet(),
            }
    })
}

fn default_auth(candidate: &ImportCandidate, store: &HostStore) -> RowAuth {
    match &candidate.identity {
        Some(path) => match saved_key(store, path, &candidate.user) {
            Some(id) => RowAuth::Credential(id),
            None => RowAuth::NewKey,
        },
        None if candidate.needs_password => RowAuth::Password,
        None => RowAuth::NoPassword,
    }
}

/// A saved key credential for this file and user: one naming the file,
/// or one keeping a copy of it, as importing it makes.
fn saved_key(store: &HostStore, path: &Path, user: &str) -> Option<CredentialId> {
    let same_key = |key: &str| {
        Path::new(key) == path
            || (store.keeps_key_file(key)
                && read_small(Path::new(key)).is_some_and(|kept| Some(kept) == read_small(path)))
    };
    store
        .credentials()
        .iter()
        .find(|credential| {
            credential.kind == CredentialKind::Key
                && credential.user.as_ref() == user
                && credential.key_path.as_deref().is_some_and(same_key)
        })
        .map(|credential| credential.id)
}

/// The user a row logs in as: an empty one is [`DEFAULT_USER`].
fn login_user(user: &str) -> String {
    match user.trim() {
        "" => DEFAULT_USER.to_string(),
        user => user.to_string(),
    }
}

/// Everything an import writes, checked.
#[derive(Debug)]
pub struct ImportPlan {
    entries: Vec<Entry>,
    credentials: Vec<PlannedCredential>,
}

/// A credential to write, and the password opened for it when there is
/// one. The password is never printed: [`Debug`] shows only that it exists.
struct PlannedCredential {
    draft: CredentialDraft,
    secret: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for PlannedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlannedCredential")
            .field("draft", &self.draft)
            .field("secret", &self.secret.as_ref().map(|_| "<hidden>"))
            .finish()
    }
}

#[derive(Debug)]
struct Entry {
    draft: HostDraft,
    group: String,
    login: Login,
}

#[derive(Debug)]
enum Login {
    Own,
    Saved(CredentialId),
    /// The credential at this index of the plan's new ones.
    New(usize),
}

/// Why an import cannot go ahead, naming the row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportError {
    Group { name: String, error: GroupPathError },
    Host { name: String, error: HostDraftError },
}

impl ImportError {
    pub fn message(&self) -> SharedString {
        match self {
            ImportError::Group {
                name,
                error: GroupPathError::EmptyLevel { path },
            } => t!(
                "host.import.error.group_empty_level",
                name = name,
                path = path
            ),
            ImportError::Group {
                name,
                error: GroupPathError::Ambiguous { count, path },
            } => t!(
                "host.import.error.group_ambiguous",
                name = name,
                count = count,
                path = path
            ),
            ImportError::Host { name, error } => {
                t!(
                    "host.import.error.host",
                    name = name,
                    error = error.message()
                )
            }
        }
    }
}

/// Check the rows to import and work out what to write: nothing is
/// written when any of them is wrong. A key file shared by rows logging in
/// as the same user gets one credential, an existing one when there is.
/// `master` is the Xshell master password, when one was typed: a saved
/// password it opens becomes a password credential of the row.
pub fn plan_import(
    rows: &[ImportRow],
    store: &HostStore,
    master: Option<&str>,
) -> Result<ImportPlan, ImportError> {
    let mut entries = Vec::new();
    let mut credentials = Vec::new();
    let mut new_keys: HashMap<(PathBuf, String), usize> = HashMap::new();
    for row in rows.iter().filter(|row| row.will_import()) {
        let candidate = &row.candidate;
        if is_saved(store, candidate) {
            continue;
        }
        let shown_name = match row.name.trim() {
            "" => candidate.address.clone(),
            name => name.to_string(),
        };
        store
            .resolve_group_path(&row.group)
            .map_err(|error| ImportError::Group {
                name: shown_name.clone(),
                error,
            })?;
        let user = match candidate.protocol {
            ImportProtocol::Ssh => login_user(&row.user),
            // Telnet 的用户名可以为空：没有自动登录时由远端提示输入。
            ImportProtocol::Telnet => row.user.trim().to_string(),
        };
        let (auth, login) = match candidate.protocol {
            ImportProtocol::Telnet => match row.auth {
                RowAuth::Password => (AuthKind::Password, Login::Own),
                _ => (AuthKind::NoPassword, Login::Own),
            },
            ImportProtocol::Ssh => match row.auth {
                RowAuth::NoPassword => (AuthKind::NoPassword, Login::Own),
                RowAuth::Password => match opened_password(candidate, master) {
                    Some(secret) => {
                        credentials.push(PlannedCredential {
                            draft: CredentialDraft::new(
                                shown_name.clone(),
                                CredentialKind::Password,
                                user.clone(),
                            ),
                            secret: Some(secret),
                        });
                        (AuthKind::default(), Login::New(credentials.len() - 1))
                    }
                    // 没有主密码、主密码不对，或这条本来就没存密码：首次连接时再问。
                    None => (AuthKind::Password, Login::Own),
                },
                RowAuth::Credential(id) if store.credential(id).is_some() => {
                    (AuthKind::default(), Login::Saved(id))
                }
                RowAuth::Credential(_) => (AuthKind::NoPassword, Login::Own),
                RowAuth::NewKey => match &candidate.identity {
                    Some(path) => match saved_key(store, path, &user) {
                        Some(id) => (AuthKind::default(), Login::Saved(id)),
                        None => {
                            let index = *new_keys
                                .entry((path.clone(), user.clone()))
                                .or_insert_with(|| {
                                    credentials.push(PlannedCredential {
                                        draft: key_credential(path, &user),
                                        secret: None,
                                    });
                                    credentials.len() - 1
                                });
                            (AuthKind::default(), Login::New(index))
                        }
                    },
                    None => (AuthKind::NoPassword, Login::Own),
                },
            },
        };
        let mut draft = match candidate.protocol {
            ImportProtocol::Ssh => HostDraft::new(
                row.name.trim().to_string(),
                candidate.address.clone(),
                candidate.port,
                user,
                auth,
                None,
            ),
            ImportProtocol::Telnet => {
                let mut draft = HostDraft::telnet(
                    row.name.trim().to_string(),
                    candidate.address.clone(),
                    candidate.port,
                    None,
                );
                draft.user = user.into();
                draft.auth = auth;
                draft
            }
        };
        draft.notes = candidate.notes.clone().into();
        let draft = draft.validated().map_err(|error| ImportError::Host {
            name: shown_name,
            error,
        })?;
        entries.push(Entry {
            draft,
            group: row.group.clone(),
            login,
        });
    }
    Ok(ImportPlan {
        entries,
        credentials,
    })
}

/// The password a saved value opens, when a master password opens it. Only
/// rows that log in with a password ask: a key or a chosen credential does
/// not take one from here.
fn opened_password(candidate: &ImportCandidate, master: Option<&str>) -> Option<Zeroizing<String>> {
    if !candidate.needs_password {
        return None;
    }
    let stored = candidate.password_cipher.as_deref()?;
    passwords::decrypt(master?, stored)
}

/// A key credential for a file, named after it.
fn key_credential(path: &Path, user: &str) -> CredentialDraft {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());
    CredentialDraft::new(name, CredentialKind::Key, user.to_string())
        .with_key_path(path.to_string_lossy().into_owned())
}

/// What an import wrote.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    pub hosts: usize,
    pub credentials: usize,
    /// The passwords opened with the master password and handed to the
    /// keychain.
    pub passwords: usize,
    /// The rows that will ask for a password on their first connection.
    pub passwords_asked: usize,
    /// The writes that did not reach the database, already reported as
    /// `PersistFailed`.
    pub failures: Vec<SharedString>,
}

/// Write a plan: the credentials, then each host in row order, making its
/// groups as they are first needed, all in one transaction.
pub fn apply_import(
    plan: ImportPlan,
    store: &mut HostStore,
    cx: &mut Context<HostStore>,
) -> Imported {
    let ImportPlan {
        entries,
        credentials,
    } = plan;
    let passwords_asked = entries
        .iter()
        .filter(|entry| matches!(entry.login, Login::Own) && entry.draft.auth == AuthKind::Password)
        .count();
    let mut not_copied = Vec::new();
    // Each key file is copied among ShellRS's own keys, which is what
    // syncs; one that cannot be is used where it is. The opened passwords
    // are kept apart, so they reach the keychain and nothing prints them.
    let (credentials, secrets): (Vec<CredentialDraft>, Vec<Option<Zeroizing<String>>>) =
        credentials
            .into_iter()
            .map(|planned| {
                let Some(path) = planned.draft.key_path.clone() else {
                    return (planned.draft, planned.secret);
                };
                let draft = match store.keep_key_file(None, &path) {
                    Ok(Some(kept)) => planned.draft.with_key_path(kept),
                    Ok(None) => planned.draft,
                    Err(error) => {
                        not_copied.push(t!("host.import.key_not_copied", error = error.message()));
                        planned.draft
                    }
                };
                (draft, planned.secret)
            })
            .collect();
    let (made, mut failures) = store.capture_failures(cx, |store, cx| {
        store.in_one_transaction(cx, |store, cx| {
            let made: Vec<CredentialId> = credentials
                .into_iter()
                .map(|draft| store.insert_credential(draft, cx))
                .collect();
            for entry in &entries {
                let mut draft = entry.draft.clone();
                draft.group = match store.resolve_group_path(&entry.group) {
                    Ok(plan) => store.place_group(plan, cx),
                    Err(_) => None,
                };
                let credential = match entry.login {
                    Login::Own => None,
                    Login::Saved(id) => Some(id),
                    Login::New(index) => Some(made[index]),
                };
                if let Some(credential) = credential {
                    // As the preview shows it: as the credential's user.
                    draft.user = SharedString::default();
                    draft = draft.with_credential(credential);
                }
                store.insert(draft, cx);
            }
            made
        })
    });
    let mut passwords = 0;
    for (id, secret) in made.iter().zip(secrets) {
        let Some(secret) = secret else { continue };
        let Some(credential) = store.credential(*id) else {
            continue;
        };
        store.save_secret(
            credential.password_secret(),
            Some(secret.as_str().to_string()),
            cx,
        );
        passwords += 1;
    }
    failures.extend(not_copied);
    Imported {
        hosts: entries.len(),
        credentials: made.len(),
        passwords,
        passwords_asked,
        failures,
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::{GroupDraft, GroupPlan, HostDatabase};
    use super::*;

    fn candidate(name: &str, address: &str, identity: Option<&str>) -> ImportCandidate {
        let mut candidate = ImportCandidate::ssh(name.into(), address.into(), 22, "deploy".into());
        candidate.identity = identity.map(PathBuf::from);
        candidate
    }

    fn scan(candidates: Vec<ImportCandidate>) -> ImportScan {
        ImportScan {
            candidates,
            ..ImportScan::default()
        }
    }

    #[test]
    fn rows_default_to_importing_what_is_new_the_way_it_logs_in() {
        let mut store = HostStore::empty();
        store.insert_unnotified(HostDraft::new(
            "old",
            "WEB.example.com",
            22,
            "someone",
            AuthKind::Password,
            None,
        ));
        let saved = store.insert_credential_unnotified(
            CredentialDraft::new("ops", CredentialKind::Key, "deploy").with_key_path("/k/ops"),
        );
        let rows = import_rows(
            &scan(vec![
                candidate("web", "web.example.com", None),
                candidate("app", "10.0.0.2", Some("/k/ops")),
                candidate("db", "10.0.0.3", Some("/k/db")),
                candidate("cache", "10.0.0.4", None),
            ]),
            &store,
        );
        let summary: Vec<(bool, bool, RowAuth)> = rows
            .iter()
            .map(|row| (row.existing, row.selected, row.auth))
            .collect();
        assert_eq!(
            summary,
            [
                (true, false, RowAuth::NoPassword),
                (false, true, RowAuth::Credential(saved)),
                (false, true, RowAuth::NewKey),
                (false, true, RowAuth::NoPassword),
            ]
        );
    }

    #[test]
    fn a_store_that_changed_meanwhile_updates_the_rows() {
        let mut store = HostStore::empty();
        let saved = store.insert_credential_unnotified(
            CredentialDraft::new("ops", CredentialKind::Key, "deploy").with_key_path("/k/ops"),
        );
        let mut rows = import_rows(
            &scan(vec![
                candidate("web", "10.0.0.1", None),
                candidate("app", "10.0.0.2", Some("/k/ops")),
            ]),
            &store,
        );
        store.insert_unnotified(HostDraft::new(
            "web",
            "10.0.0.1",
            22,
            "x",
            AuthKind::Password,
            None,
        ));
        store.remove_credential_unnotified(saved);
        refresh_rows(&mut rows, &store);
        assert!(rows[0].existing && !rows[0].selected);
        assert_eq!(rows[1].auth, RowAuth::NoPassword);
    }

    #[test]
    fn a_key_file_gets_one_credential_per_user_and_reuses_a_saved_one() {
        let mut store = HostStore::empty();
        let saved = store.insert_credential_unnotified(
            CredentialDraft::new("ops", CredentialKind::Key, "admin").with_key_path("/k/shared"),
        );
        let mut rows = import_rows(
            &scan(vec![
                candidate("a", "10.0.0.1", Some("/k/shared")),
                candidate("b", "10.0.0.2", Some("/k/shared")),
                candidate("c", "10.0.0.3", Some("/k/shared")),
                candidate("d", "10.0.0.4", Some("/k/shared")),
            ]),
            &store,
        );
        // Edited to the saved credential's user: that one is used.
        rows[2].user = " admin ".into();
        // Emptied: logs in as root, with a key of its own.
        rows[3].user = String::new();
        let plan = plan_import(&rows, &store, None).unwrap();
        assert_eq!(plan.credentials.len(), 2);
        assert_eq!(plan.credentials[0].draft.user.as_ref(), "deploy");
        assert_eq!(plan.credentials[0].draft.name.as_ref(), "shared");
        assert_eq!(plan.credentials[1].draft.user.as_ref(), "root");
        let logins: Vec<String> = plan
            .entries
            .iter()
            .map(|entry| format!("{:?}", entry.login))
            .collect();
        assert_eq!(
            logins,
            [
                "New(0)",
                "New(0)",
                format!("Saved({saved:?})").as_str(),
                "New(1)"
            ]
        );
    }

    #[test]
    fn a_master_password_turns_a_stored_password_into_a_credential() {
        let store = HostStore::empty();
        let mut sealed = candidate("web", "10.0.0.1", None);
        sealed.needs_password = true;
        sealed.password_cipher = Some(passwords::seal("test master", "Test1234"));
        let mut unreadable = candidate("db", "10.0.0.2", None);
        unreadable.needs_password = true;
        let mut rows = import_rows(&scan(vec![sealed, unreadable]), &store);

        mark_passwords(&mut rows, "test master");
        assert!(rows[0].password_ready);
        assert!(!rows[1].password_ready);

        let plan = plan_import(&rows, &store, Some("test master")).unwrap();
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.credentials.len(), 1);
        assert_eq!(
            plan.credentials[0]
                .secret
                .as_ref()
                .map(|secret| secret.as_str()),
            Some("Test1234")
        );
        assert!(matches!(plan.entries[0].login, Login::New(0)));
        // 没有存密码的那条仍然在首次连接时问。
        assert!(matches!(plan.entries[1].login, Login::Own));

        // 主密码不对：两条都退回询问，不写占位符。
        let plan = plan_import(&rows, &store, Some("wrong master")).unwrap();
        assert!(plan.credentials.is_empty());
        assert!(
            plan.entries
                .iter()
                .all(|entry| matches!(entry.login, Login::Own))
        );
    }

    #[test]
    fn a_wrong_group_path_names_its_row_and_nothing_is_planned() {
        let mut store = HostStore::empty();
        store.insert_group_unnotified(GroupDraft::new("生产", None));
        store.insert_group_unnotified(GroupDraft::new("生产", None));
        let mut rows = import_rows(
            &scan(vec![
                candidate("web", "10.0.0.1", None),
                candidate("", "10.0.0.2", None),
            ]),
            &store,
        );
        rows[0].group = "a//b".into();
        let error = plan_import(&rows, &store, None).unwrap_err();
        assert_eq!(
            error,
            ImportError::Group {
                name: "web".into(),
                error: GroupPathError::EmptyLevel {
                    path: "a//b".into()
                }
            }
        );
        rows[0].group.clear();
        rows[1].group = "生产".into();
        let error = plan_import(&rows, &store, None).unwrap_err();
        assert!(matches!(
            &error,
            ImportError::Group { name, .. } if name == "10.0.0.2"
        ));
        assert!(error.message().contains("10.0.0.2"));
    }

    #[test]
    fn only_selected_new_rows_are_planned() {
        let store = HostStore::empty();
        let mut rows = import_rows(
            &scan(vec![
                candidate("web", "10.0.0.1", None),
                candidate("db", "10.0.0.2", None),
            ]),
            &store,
        );
        rows[1].selected = false;
        assert_eq!(plan_import(&rows, &store, None).unwrap().entries.len(), 1);
        rows[0].existing = true;
        assert_eq!(plan_import(&rows, &store, None).unwrap().entries.len(), 0);
    }

    #[gpui_kit::test]
    fn an_opened_password_reaches_the_keychain_write(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;

        let store = cx.new(|_| HostStore::load(HostDatabase::in_memory().unwrap()).unwrap());
        let imported = store.update(cx, |store, cx| {
            let mut sealed = candidate("web", "10.0.0.1", None);
            sealed.needs_password = true;
            sealed.password_cipher = Some(passwords::seal("test master", "Test1234"));
            let mut unreadable = candidate("db", "10.0.0.2", None);
            unreadable.needs_password = true;
            let rows = import_rows(&scan(vec![sealed, unreadable]), store);
            let plan = plan_import(&rows, store, Some("test master")).unwrap();
            apply_import(plan, store, cx)
        });
        assert_eq!(imported.hosts, 2);
        assert_eq!(imported.credentials, 1);
        assert_eq!(imported.passwords, 1);
        assert_eq!(imported.passwords_asked, 1);
        assert!(imported.failures.is_empty());
        store.read_with(cx, |store, _| {
            assert_eq!(store.credentials().len(), 1);
            let logins: Vec<bool> = store
                .hosts()
                .iter()
                .map(|host| host.credential.is_some())
                .collect();
            assert_eq!(logins, [true, false]);
        });
    }

    #[gpui_kit::test]
    fn an_import_makes_each_new_group_once_and_its_credentials(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;

        let store = cx.new(|_| HostStore::load(HostDatabase::in_memory().unwrap()).unwrap());
        let imported = store.update(cx, |store, cx| {
            let production = store.insert_group(GroupDraft::new("生产", None), cx);
            let mut rows = import_rows(
                &scan(vec![
                    candidate("web", "10.0.0.1", Some("/k/deploy")),
                    candidate("", "10.0.0.2", Some("/k/deploy")),
                    candidate("db", "10.0.0.3", None),
                ]),
                store,
            );
            rows[0].group = "生产/华东".into();
            rows[1].group = " 生产 / 华东 ".into();
            rows[2].group = "开发".into();
            let plan = plan_import(&rows, store, None).unwrap();
            let imported = apply_import(plan, store, cx);
            assert_eq!(
                store
                    .resolve_group_path("生产/华东")
                    .map(|plan| match plan {
                        GroupPlan::Existing(Some(group)) => store.group(group).map(|g| g.parent),
                        _ => None,
                    }),
                Ok(Some(Some(production)))
            );
            imported
        });
        assert_eq!(
            imported,
            Imported {
                hosts: 3,
                credentials: 1,
                passwords: 0,
                passwords_asked: 0,
                failures: Vec::new()
            }
        );
        store.read_with(cx, |store, _| {
            assert_eq!(store.groups().len(), 3);
            let hosts: Vec<(String, String, String)> = store
                .hosts()
                .iter()
                .map(|host| {
                    (
                        host.name.to_string(),
                        host.group
                            .map(|group| store.group_path(group))
                            .unwrap_or_default(),
                        format!("{:?}{:?}", host.auth, host.credential.is_some()),
                    )
                })
                .collect();
            assert_eq!(
                hosts,
                [
                    ("web".into(), "生产 / 华东".into(), "Passwordtrue".into()),
                    (
                        "10.0.0.2".into(),
                        "生产 / 华东".into(),
                        "Passwordtrue".into()
                    ),
                    ("db".into(), "开发".into(), "NoPasswordfalse".into()),
                ]
            );
            let credential = &store.credentials()[0];
            assert_eq!(credential.key_path.as_deref(), Some("/k/deploy"));
            assert_eq!(credential.user.as_ref(), "deploy");
        });
    }

    /// Imported keys are copied among ShellRS's own, which is what syncs,
    /// and importing again knows the copy for the file.
    #[gpui_kit::test]
    fn an_imported_key_is_copied_and_known_again(cx: &mut gpui_kit::TestAppContext) {
        use gpui_kit::AppContext as _;

        let data = tempfile::tempdir().unwrap();
        let key = data.path().join("id_deploy");
        std::fs::write(&key, "the key").unwrap();
        let identity = key.display().to_string();
        let store = cx.new(|_| HostStore::empty().with_key_dir(data.path().join("keys")));
        store.update(cx, |store, cx| {
            let rows = import_rows(
                &scan(vec![candidate("web", "10.0.0.1", Some(&identity))]),
                store,
            );
            let imported = apply_import(plan_import(&rows, store, None).unwrap(), store, cx);
            assert_eq!(imported.credentials, 1);
            let kept = store.credentials()[0].key_path.clone().unwrap();
            assert!(store.keeps_key_file(&kept), "{kept}");
            assert_eq!(std::fs::read_to_string(kept.as_ref()).unwrap(), "the key");

            let rows = import_rows(
                &scan(vec![candidate("api", "10.0.0.2", Some(&identity))]),
                store,
            );
            let imported = apply_import(plan_import(&rows, store, None).unwrap(), store, cx);
            assert_eq!(imported.credentials, 0, "the copy is the same key");
            assert_eq!(store.credentials().len(), 1);
        });
    }
}
