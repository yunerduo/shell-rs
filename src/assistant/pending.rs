//! Commands an unattended trigger did not run: they wait here until the
//! user allows or refuses them. The file is the assistant's own; nothing in
//! it has run.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::audit::POLICY_VERSION;

/// Why a command is waiting.
pub const REASON_WRITE: &str = "write";
pub const REASON_AI_PROPOSAL: &str = "ai_proposal";
pub const REASON_UNRESOLVED: &str = "unresolved";

/// One command waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingItem {
    pub id: String,
    pub schedule_id: String,
    pub name: String,
    pub alias: String,
    pub command: String,
    /// [`REASON_WRITE`], [`REASON_AI_PROPOSAL`] or [`REASON_UNRESOLVED`].
    pub reason: String,
    pub created_at: i64,
    pub risk: String,
    pub rule: String,
    pub policy_version: String,
    /// The terminal the command was meant for, so it runs where it was
    /// judged even after a restart.
    pub terminal_id: String,
    pub title: String,
    #[serde(default)]
    pub local: bool,
}

impl PendingItem {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        schedule_id: impl Into<String>,
        name: impl Into<String>,
        alias: impl Into<String>,
        command: impl Into<String>,
        reason: &str,
        created_at: i64,
        risk: impl Into<String>,
        rule: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            schedule_id: schedule_id.into(),
            name: name.into(),
            alias: alias.into(),
            command: command.into(),
            reason: reason.to_string(),
            created_at,
            risk: risk.into(),
            rule: rule.into(),
            policy_version: POLICY_VERSION.to_string(),
            terminal_id: String::new(),
            title: String::new(),
            local: false,
        }
    }

    /// Say which terminal the command was meant for.
    pub fn on_terminal(
        mut self,
        terminal_id: impl Into<String>,
        title: impl Into<String>,
        local: bool,
    ) -> Self {
        self.terminal_id = terminal_id.into();
        self.title = title.into();
        self.local = local;
        self
    }
}

/// The file the queue is kept in, under the assistant's directory.
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("assistant").join("pending.json")
}

/// Read the queue. A missing or broken file means nothing waits; it is not
/// an error to show.
pub fn load(path: &Path) -> Vec<PendingItem> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the queue in one step, so a half-written file is never read.
pub fn save(path: &Path, items: &[PendingItem]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(items)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, text)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> PendingItem {
        PendingItem::new(
            id,
            "schedule-1",
            "disk",
            "web",
            "touch /tmp/report",
            REASON_WRITE,
            1_700_000_000_000,
            "R1",
            "write",
        )
        .on_terminal("remote:1", "web", false)
    }

    #[test]
    fn the_queue_survives_a_write_and_a_read() {
        let dir = std::env::temp_dir().join(format!(
            "shellas-pending-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let path = path_in(&dir);
        save(&path, &[item("one"), item("two")]).expect("save");
        let loaded = load(&path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].command, "touch /tmp/report");
        assert_eq!(loaded[0].policy_version, POLICY_VERSION);
        assert_eq!(loaded[0].terminal_id, "remote:1");
        assert!(!loaded[0].local);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load(&path).is_empty(), "nothing left on disk");
    }

    #[test]
    fn a_later_entry_keeps_the_side_of_the_connection_it_names() {
        let item = item("one").on_terminal("local:1", "本地终端", true);
        assert!(item.local);
        assert_eq!(item.title, "本地终端");
    }
}
