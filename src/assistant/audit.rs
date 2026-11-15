//! The assistant's audit trail: one JSON line per command it considered, in
//! `<data dir>/assistant/audit.jsonl`. Only masked commands are written, and
//! a failing write is a notice, never a reason to stop a command the user
//! already approved.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The policy generation this build judges by. A record from an older
/// version can be told apart when the lists or ladders change.
pub const POLICY_VERSION: &str = "p1";

/// The most of one command kept in a record.
const COMMAND_LIMIT: usize = 4096;

/// What one decision left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry<'a> {
    /// The bound terminal the command was meant for.
    pub alias: &'a str,
    /// The command with sensitive paths and secrets already replaced.
    pub command: &'a str,
    /// `R0` to `R3`.
    pub risk: &'a str,
    /// The rule that decided, as a stable name.
    pub rule: &'a str,
    /// How it was carried out: `exec`, `denied`, `unsupported`.
    pub channel: &'a str,
    pub exit_code: Option<i32>,
    /// `restricted` or `full`.
    pub level: &'a str,
    /// `auto`, `user` or `policy`.
    pub approver: &'a str,
    pub override_reason: Option<&'a str>,
}

impl AuditEntry<'_> {
    fn as_json(&self) -> serde_json::Value {
        // Redact again here: a caller that forgot is not enough reason to
        // write a key or a path to disk.
        let command = truncate(&crate::assistant::policy::mask_sensitive_paths(
            self.command,
        ));
        serde_json::json!({
            "at": unix_now(),
            "alias": self.alias,
            "command": command,
            "hash": command_hash(&command),
            "risk": self.risk,
            "rule": self.rule,
            "channel": self.channel,
            "exit_code": self.exit_code,
            "level": self.level,
            "approver": self.approver,
            "override_reason": self.override_reason,
            "policy_version": POLICY_VERSION,
        })
    }
}

/// Where the trail lives inside the data directory.
pub fn path_in(data_dir: &Path) -> PathBuf {
    data_dir.join("assistant").join("audit.jsonl")
}

/// Append one record to the trail of this installation. Best effort: the
/// caller shows a notice if it returns an error.
pub fn append(entry: &AuditEntry<'_>) -> std::io::Result<()> {
    append_at(&crate::app::data_dir(), entry)
}

/// Append one record under `data_dir`, creating the folder when needed. The
/// tests point this at a scratch directory.
pub fn append_at(data_dir: &Path, entry: &AuditEntry<'_>) -> std::io::Result<()> {
    let path = path_in(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(&entry.as_json()).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(line.as_bytes())
}

/// A stable digest of a command, so the panel can check that what it runs
/// is what the card showed. FNV-1a 64, printed as hex: no dependency, and no
/// need for a cryptographic property here.
pub fn command_hash(command: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in command.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn truncate(command: &str) -> String {
    if command.chars().count() <= COMMAND_LIMIT {
        return command.to_string();
    }
    command.chars().take(COMMAND_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shellrs-audit-{}-{name}-{}",
            std::process::id(),
            unix_now()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn entry() -> AuditEntry<'static> {
        AuditEntry {
            alias: "web",
            command: "rm <redacted>",
            risk: "R2",
            rule: "recursive_delete",
            channel: "exec",
            exit_code: Some(0),
            level: "restricted",
            approver: "user",
            override_reason: None,
        }
    }

    #[test]
    fn one_line_per_decision_with_every_field() {
        let dir = scratch_dir("append");
        append_at(&dir, &entry()).expect("first record");
        append_at(&dir, &entry()).expect("second record");
        let text = fs::read_to_string(path_in(&dir)).expect("trail");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let record: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
        assert_eq!(record["alias"], "web");
        assert_eq!(record["command"], "rm <redacted>");
        assert_eq!(record["risk"], "R2");
        assert_eq!(record["rule"], "recursive_delete");
        assert_eq!(record["channel"], "exec");
        assert_eq!(record["exit_code"], 0);
        assert_eq!(record["level"], "restricted");
        assert_eq!(record["approver"], "user");
        assert_eq!(record["policy_version"], POLICY_VERSION);
        assert!(record["at"].as_u64().expect("timestamp") > 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_password_in_a_command_never_reaches_the_trail() {
        let dir = scratch_dir("secret");
        let mut secret = entry();
        secret.command = "export KEY=sk-abcdefghijklmnop";
        append_at(&dir, &secret).expect("record");
        let text = fs::read_to_string(path_in(&dir)).expect("trail");
        assert!(!text.contains("sk-abcdefghijklmnop"));
        assert!(text.contains("<redacted>"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_hash_is_stable_and_different_for_different_commands() {
        assert_eq!(command_hash("df -h"), command_hash("df -h"));
        assert_ne!(command_hash("df -h"), command_hash("df -H"));
        assert_eq!(command_hash("").len(), 16);
    }

    #[test]
    fn a_long_command_is_cut_to_the_limit() {
        let long = "a".repeat(COMMAND_LIMIT + 10);
        assert_eq!(truncate(&long).chars().count(), COMMAND_LIMIT);
    }
}
