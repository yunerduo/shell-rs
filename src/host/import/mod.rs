//! 导入主机: the hosts OpenSSH already knows about, read from
//! `~/.ssh/config` or `~/.ssh/known_hosts`, previewed in a dialog and added
//! to the store. Existing hosts are never changed: an import only adds.

mod dialog;
mod known_hosts;
mod passwords;
mod plan;
mod ssh_config;
mod xshell;

use std::fs::File;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};

use gpui_kit::SharedString;

pub use dialog::{ImportHandler, ImportOutcome, open_import_dialog};
use plan::{ImportRow, RowAuth, apply_import, import_rows, plan_import};

use crate::i18n::t;

use super::{DEFAULT_KEY_FILES, DEFAULT_USER, SSH_PORT};

/// Where an import reads its hosts from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportSource {
    /// `~/.ssh/config`: the hosts it names, with their settings.
    SshConfig,
    /// `~/.ssh/known_hosts`: the hosts OpenSSH has connected to.
    KnownHosts,
    /// Xshell's `.xts` export or its `Sessions` folder of `.xsh` files.
    Xshell,
}

impl ImportSource {
    /// The file or folder the source reads. Only the SSH sources live in
    /// the SSH directory; Xshell has its own place.
    pub fn default_path(self, ssh_dir: &Path) -> PathBuf {
        match self {
            ImportSource::SshConfig => ssh_dir.join("config"),
            ImportSource::KnownHosts => ssh_dir.join("known_hosts"),
            ImportSource::Xshell => xshell::default_sessions_dir(),
        }
    }
}

/// The protocol an imported host speaks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ImportProtocol {
    #[default]
    Ssh,
    Telnet,
}

/// A setting Xshell stores that this app has no place for. The import
/// keeps the host and says so instead of dropping it silently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiscardedField {
    Passphrase,
    LoginScript,
    RemoteCommand,
    PortForwarding,
    Proxy,
    Settings,
}

impl DiscardedField {
    pub fn message(self) -> SharedString {
        match self {
            DiscardedField::Passphrase => t!("host.import.discarded.passphrase"),
            DiscardedField::LoginScript => t!("host.import.discarded.login_script"),
            DiscardedField::RemoteCommand => t!("host.import.discarded.remote_command"),
            DiscardedField::PortForwarding => t!("host.import.discarded.port_forwarding"),
            DiscardedField::Proxy => t!("host.import.discarded.proxy"),
            DiscardedField::Settings => t!("host.import.discarded.settings"),
        }
    }
}

/// A host a source names, as the source gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportCandidate {
    pub name: String,
    pub address: String,
    pub port: u16,
    pub user: String,
    /// A private key the host logs in with that a host without one would
    /// not try anyway: the file is there, is no public key and is not one
    /// of the default keys.
    pub identity: Option<PathBuf>,
    /// The folder path the source groups this host under, `/` separated.
    pub group: String,
    pub protocol: ImportProtocol,
    /// The source saved a password (or expects one) that cannot be read:
    /// the row asks for it on first connection.
    pub needs_password: bool,
    /// The stored password as the source keeps it: Xshell's Base64 value,
    /// opened with the master password when one is given.
    pub password_cipher: Option<String>,
    /// The key Xshell named but whose file was not found.
    pub missing_key: Option<String>,
    /// The host's description, kept as its note.
    pub notes: String,
    pub discarded: Vec<DiscardedField>,
}

impl ImportCandidate {
    /// The fields shared by every source, so a new source only fills in
    /// what it knows.
    pub fn ssh(name: String, address: String, port: u16, user: String) -> Self {
        Self {
            name,
            address,
            port,
            user,
            identity: None,
            group: String::new(),
            protocol: ImportProtocol::Ssh,
            needs_password: false,
            password_cipher: None,
            missing_key: None,
            notes: String::new(),
            discarded: Vec::new(),
        }
    }
}

/// A session the source holds but this import cannot turn into a host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedSession {
    pub name: String,
    pub reason: SkipReason,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkipReason {
    UnsupportedProtocol(String),
    MissingAddress,
    Unreadable(String),
}

impl SkipReason {
    pub fn message(&self) -> SharedString {
        match self {
            SkipReason::UnsupportedProtocol(protocol) => {
                t!(
                    "host.import.skip.unsupported_protocol",
                    protocol = protocol.clone()
                )
            }
            SkipReason::MissingAddress => t!("host.import.skip.missing_address"),
            SkipReason::Unreadable(error) => {
                t!("host.import.skip.unreadable", error = error.clone())
            }
        }
    }
}

/// What reading a source found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportScan {
    pub candidates: Vec<ImportCandidate>,
    /// known_hosts entries whose names are hashed, so cannot be read back.
    pub hashed: usize,
    /// Sessions that were there but could not become hosts.
    pub skipped: Vec<SkippedSession>,
}

/// The outcome of reading a source's file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImportLoad {
    Loaded(ImportScan),
    Missing,
    Failed(String),
}

/// The most of a file an import reads: far more than any real
/// configuration, little enough that a wrong pick cannot stall the app.
const MAX_FILE_SIZE: u64 = 16 * 1024 * 1024;

/// Read `path` as `source`. Blocking: it reads files and checks that the
/// key files named exist, so it runs on the background executor.
/// `ssh_dir` is where relative includes resolve and what `~` is under.
pub fn read_source(source: ImportSource, path: &Path, ssh_dir: &Path) -> ImportLoad {
    if source == ImportSource::Xshell {
        return xshell::scan(path);
    }
    let text = match read_capped(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return ImportLoad::Missing,
        Err(error) => return ImportLoad::Failed(error.to_string()),
    };
    let home = home_of(ssh_dir);
    let mut scan = if source == ImportSource::SshConfig {
        let config = ssh_config::SshConfig::parse_with_includes(&text, ssh_dir, &home);
        let local_user = local_user_name();
        let candidates = config
            .hosts(&home, &local_user)
            .into_iter()
            .map(|mut candidate| {
                candidate.identity = candidate
                    .identity
                    .filter(|path| usable_identity(path, ssh_dir));
                candidate
            })
            .collect();
        ImportScan {
            candidates,
            hashed: 0,
            skipped: Vec::new(),
        }
    } else {
        known_hosts::parse(&text)
    };
    scan.candidates.retain(|candidate| !is_git_host(candidate));
    ImportLoad::Loaded(scan)
}

/// Git hosting services' SSH addresses: they push and pull, no shell.
const GIT_HOSTS: &[&str] = &[
    "github.com",
    "ssh.github.com",
    "gitlab.com",
    "altssh.gitlab.com",
    "bitbucket.org",
    "altssh.bitbucket.org",
    "ssh.dev.azure.com",
    "vs-ssh.visualstudio.com",
    "codeberg.org",
    "git.sr.ht",
    "gitee.com",
    "gitcode.com",
    "jihulab.com",
    "e.coding.net",
    "codeup.aliyun.com",
];

/// Whether a host is for Git, not for logging in: it logs in as `git`, or
/// is a Git hosting service.
fn is_git_host(candidate: &ImportCandidate) -> bool {
    candidate.user == "git"
        || GIT_HOSTS
            .iter()
            .any(|host| candidate.address.eq_ignore_ascii_case(host))
}

/// A file's text, read up to [`MAX_FILE_SIZE`] and decoded leniently.
fn read_capped(path: &Path) -> io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_FILE_SIZE)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The home directory an SSH directory sits in, for `~` and `%d`.
fn home_of(ssh_dir: &Path) -> PathBuf {
    ssh_dir
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| ssh_dir.to_path_buf())
}

/// Whether a key file is worth a credential of its own: it is there, it is
/// no public key, and it is not one of the default keys a host without a
/// password tries anyway.
fn usable_identity(path: &Path, ssh_dir: &Path) -> bool {
    let public = path.extension().is_some_and(|extension| extension == "pub");
    let default = DEFAULT_KEY_FILES
        .iter()
        .any(|name| path == ssh_dir.join(name));
    !public && !default && path.is_file()
}

/// The name of the user running ShellRS: whom OpenSSH logs in as when
/// nothing names a user.
fn local_user_name() -> String {
    ["USER", "USERNAME"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|name| name.trim().to_string())
        .find(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_USER.to_string())
}

/// Whether `text` matches `pattern`, where `*` stands for any run of
/// characters and `?` for any one, as in OpenSSH's patterns and glob(3).
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0, 0);
    // Where the last `*` was, and where in the text it started matching.
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, t));
                p += 1;
            }
            Some('?') => {
                p += 1;
                t += 1;
            }
            Some(c) if *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

fn has_wildcard(text: &str) -> bool {
    text.contains(['*', '?'])
}

/// The port a source gives, when it is one: OpenSSH refuses 0 too.
fn parse_port(text: &str) -> Option<u16> {
    text.parse::<u16>().ok().filter(|port| *port != 0)
}

/// The port when a source names none.
const DEFAULT_PORT: u16 = SSH_PORT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_match_like_openssh_patterns() {
        assert!(wildcard_match("*", ""));
        assert!(wildcard_match("web-*", "web-01"));
        assert!(wildcard_match("*.example.com", "a.b.example.com"));
        assert!(wildcard_match("db-??", "db-01"));
        assert!(!wildcard_match("db-??", "db-1"));
        assert!(wildcard_match("a*b*c", "aXXbYYc"));
        assert!(!wildcard_match("a*b*c", "aXXbYY"));
        assert!(wildcard_match("config.d", "config.d"));
        assert!(!wildcard_match("config", "configs"));
    }

    #[test]
    fn a_key_file_needs_a_credential_only_when_nothing_else_tries_it() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        for name in ["id_ed25519", "deploy", "deploy.pub"] {
            std::fs::write(ssh.join(name), "key").unwrap();
        }
        assert!(usable_identity(&ssh.join("deploy"), &ssh));
        assert!(!usable_identity(&ssh.join("deploy.pub"), &ssh));
        assert!(!usable_identity(&ssh.join("id_ed25519"), &ssh));
        assert!(!usable_identity(&ssh.join("missing"), &ssh));
        assert!(!usable_identity(&ssh, &ssh));
    }

    #[test]
    fn git_hosts_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        std::fs::create_dir(&ssh).unwrap();
        std::fs::write(
            ssh.join("config"),
            "Host github.com\n  User git\nHost gitea\n  User git\n\
             Host gl\n  HostName GitLab.com\nHost web\n  User deploy\n",
        )
        .unwrap();
        std::fs::write(
            ssh.join("known_hosts"),
            "github.com,140.82.112.3 ssh-ed25519 AAAA\n\
             [ssh.github.com]:443 ssh-ed25519 AAAA\n\
             10.0.0.1 ssh-ed25519 AAAA\n",
        )
        .unwrap();
        let names = |source| match read_source(source, &source.default_path(&ssh), &ssh) {
            ImportLoad::Loaded(scan) => scan
                .candidates
                .into_iter()
                .map(|host| host.name)
                .collect::<Vec<_>>(),
            load => panic!("{load:?}"),
        };
        assert_eq!(names(ImportSource::SshConfig), ["web"]);
        assert_eq!(names(ImportSource::KnownHosts), ["10.0.0.1"]);
    }

    #[test]
    fn a_missing_file_is_told_apart_from_one_that_cannot_be_read() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join(".ssh");
        assert_eq!(
            read_source(ImportSource::SshConfig, &ssh.join("config"), &ssh),
            ImportLoad::Missing
        );
        assert!(matches!(
            read_source(ImportSource::KnownHosts, dir.path(), &ssh),
            ImportLoad::Failed(_)
        ));
    }
}
