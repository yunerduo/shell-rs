//! Which terminals the assistant may speak to, and what the model calls
//! them. The alias is the only handle the model sees: terminal ids are the
//! host's business. The bound aliases are saved with the terminal's name so
//! a later session can take them back from the one terminal that still
//! carries that name; when it cannot, the alias waits to be bound again
//! rather than silently moving to another terminal.

use std::fs;
use std::path::{Path, PathBuf};

use gpui_kit::gpui::{Entity, WeakEntity};
use serde::{Deserialize, Serialize};

use crate::terminal::TerminalView;

/// One terminal the sidebar can offer. `alias` is filled in when it is
/// bound; before that it is empty.
#[derive(Clone, Debug)]
pub struct AssistantTerminal {
    pub id: String,
    /// What identifies the terminal after a restart: the host's permanent
    /// public id for a remote terminal. Empty when there is nothing more
    /// stable than the title.
    pub stable: String,
    /// The tab's title, for people; not the alias.
    pub title: String,
    pub alias: String,
    /// The view a tool call runs on. A terminal with no view has no channel
    /// to run on at all.
    pub view: Option<WeakEntity<TerminalView>>,
    /// The connection carries only the shell: a command cannot go beside it.
    pub shell_only: bool,
    /// A shell of this computer: a command cannot run beside it, so the
    /// terminal is the only path.
    pub local: bool,
}

/// The identity of a terminal is its id and the three flags; the view is a
/// handle to the same terminal, not a part of what it is.
impl PartialEq for AssistantTerminal {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.stable == other.stable
            && self.title == other.title
            && self.alias == other.alias
            && self.shell_only == other.shell_only
            && self.local == other.local
    }
}

impl Eq for AssistantTerminal {}

impl AssistantTerminal {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            stable: String::new(),
            title: title.into(),
            alias: String::new(),
            view: None,
            shell_only: false,
            local: false,
        }
    }

    /// Give it the identity it keeps across restarts.
    pub fn with_stable(mut self, stable: impl Into<String>) -> Self {
        self.stable = stable.into();
        self
    }

    /// Offer the terminal with the view its commands run on.
    pub fn with_view(mut self, view: &Entity<TerminalView>, shell_only: bool) -> Self {
        self.view = Some(view.downgrade());
        self.shell_only = shell_only;
        self
    }

    /// Mark it as a terminal of this computer, where only injection works.
    pub fn local(mut self) -> Self {
        self.local = true;
        self
    }

    /// Whether a terminal is there to type into. The probe decides whether
    /// its shell takes a line.
    pub fn can_inject(&self) -> bool {
        self.view.is_some()
    }

    /// Whether a command can run beside the shell, without a terminal.
    pub fn can_exec(&self) -> bool {
        self.view.is_some() && !self.shell_only && !self.local
    }

    /// Why a command cannot run here, if it cannot.
    pub fn unsupported(&self) -> Option<&'static str> {
        if self.shell_only || self.view.is_none() {
            return Some("shell_only");
        }
        None
    }
}

/// The longest alias, so a chain of collisions cannot grow without bound.
const ALIAS_LIMIT: usize = 24;

/// How many terminals one conversation may bind. A card per terminal, and
/// four is as many as a person can follow at once.
pub const MAX_BINDINGS: usize = 4;

/// One binding as the file keeps it: what an alias needs to find its
/// terminal again, without the runtime id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedBinding {
    pub alias: String,
    pub title: String,
    #[serde(default)]
    pub local: bool,
    /// The host's public id, empty for a binding saved before this was
    /// kept or for a terminal with no stable name.
    #[serde(default)]
    pub stable: String,
}

/// The file bindings are kept in, under the assistant's directory.
pub fn path_in(dir: &Path) -> PathBuf {
    dir.join("assistant").join("bindings.json")
}

/// A stable name for a terminal, from its title: lower case, Latin letters,
/// digits and dashes; a name with nothing usable falls back to a numbered
/// one. Collisions take `-2`, `-3`, and so on.
pub fn alias_for(title: &str, taken: &[String], sequence: usize) -> String {
    let mut base = String::new();
    let mut last_dash = false;
    for character in title.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            base.push(character);
            last_dash = false;
        } else if !last_dash && !base.is_empty() {
            base.push('-');
            last_dash = true;
        }
        if base.len() >= ALIAS_LIMIT {
            break;
        }
    }
    let base = base.trim_matches('-').to_string();
    let base = if base.is_empty() {
        format!("terminal-{sequence}")
    } else {
        base
    };
    let exists = |candidate: &str| {
        taken
            .iter()
            .any(|taken| taken.eq_ignore_ascii_case(candidate))
    };
    if !exists(&base) {
        return base;
    }
    for suffix in 2.. {
        let suffix = format!("-{suffix}");
        let stem: String = base
            .chars()
            .take(ALIAS_LIMIT.saturating_sub(suffix.len()))
            .collect();
        let candidate = format!("{stem}{suffix}");
        if !exists(&candidate) {
            return candidate;
        }
    }
    unreachable!("an alias with a free suffix always exists")
}

/// The terminals a conversation is bound to. One is the front terminal by
/// default; the user may add others, up to [`MAX_BINDINGS`]. The names are
/// saved across restarts; an alias whose terminal cannot be identified
/// again is kept as pending until the user binds it or dismisses it.
#[derive(Debug, Default)]
pub struct Bindings {
    entries: Vec<AssistantTerminal>,
    /// Terminals the user took off by hand; the front one is not put back
    /// until the user picks it again.
    unbound: Vec<String>,
    /// Numbers the fallback names.
    sequence: usize,
    /// Saved aliases still looking for their terminal.
    pending: Vec<SavedBinding>,
}

impl Bindings {
    pub fn entries(&self) -> &[AssistantTerminal] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_bound(&self, id: &str) -> bool {
        self.entries.iter().any(|entry| entry.id == id)
    }

    /// Aliases that could not be matched to a terminal this session. They
    /// are not usable and are shown as waiting to be bound again.
    pub fn pending(&self) -> &[SavedBinding] {
        &self.pending
    }

    /// Whether another terminal may be bound.
    pub fn has_room(&self) -> bool {
        self.entries.len() < MAX_BINDINGS
    }

    /// Bind a terminal, giving it an alias no other binding holds. Returns
    /// whether it is bound afterwards: the list may be full.
    pub fn bind(&mut self, terminal: &AssistantTerminal) -> bool {
        if self.is_bound(&terminal.id) {
            return true;
        }
        if !self.has_room() {
            return false;
        }
        self.unbound.retain(|id| id != &terminal.id);
        self.sequence += 1;
        let taken: Vec<String> = self
            .entries
            .iter()
            .map(|entry| entry.alias.clone())
            .collect();
        let mut entry = terminal.clone();
        entry.alias = alias_for(&terminal.title, &taken, self.sequence);
        self.entries.push(entry);
        // Binding by hand answers a saved alias of the same terminal.
        self.pending
            .retain(|wanted| !same_terminal_name(wanted, terminal));
        true
    }

    /// Bind with a name the file kept, if that name is free.
    fn bind_as(&mut self, terminal: &AssistantTerminal, alias: &str) -> bool {
        if self.is_bound(&terminal.id) || !self.has_room() {
            return false;
        }
        if self
            .entries
            .iter()
            .any(|entry| entry.alias.eq_ignore_ascii_case(alias))
        {
            return false;
        }
        let mut entry = terminal.clone();
        entry.alias = alias.to_ascii_lowercase();
        self.unbound.retain(|id| id != &terminal.id);
        self.entries.push(entry);
        self.pending
            .retain(|wanted| !wanted.alias.eq_ignore_ascii_case(alias));
        true
    }

    /// Take a terminal off. Remembered, so the front terminal does not come
    /// straight back.
    pub fn unbind(&mut self, id: &str) {
        self.entries.retain(|entry| entry.id != id);
        if !self.unbound.iter().any(|unbound| unbound == id) {
            self.unbound.push(id.to_string());
        }
    }

    /// Bind or take off, as the sidebar's checkbox says. Returns whether the
    /// terminal is bound afterwards.
    pub fn toggle(&mut self, terminal: &AssistantTerminal) -> bool {
        if self.is_bound(&terminal.id) {
            self.unbind(&terminal.id);
            false
        } else {
            self.bind(terminal)
        }
    }

    /// Take the terminals that are still open, dropping bindings whose
    /// terminal has closed. Aliases of the rest stay as they were.
    pub fn sync(&mut self, open: &[AssistantTerminal]) {
        self.entries
            .retain(|entry| open.iter().any(|terminal| terminal.id == entry.id));
        for entry in &mut self.entries {
            if let Some(terminal) = open.iter().find(|terminal| terminal.id == entry.id) {
                entry.title = terminal.title.clone();
                entry.stable = terminal.stable.clone();
            }
        }
        self.unbound
            .retain(|id| open.iter().any(|terminal| &terminal.id == id));
    }

    /// Put the front terminal in the conversation when nothing is bound.
    pub fn follow_front(&mut self, front: Option<&AssistantTerminal>) -> bool {
        let Some(front) = front else {
            return false;
        };
        if !self.entries.is_empty()
            || self.is_bound(&front.id)
            || self.unbound.iter().any(|id| id == &front.id)
        {
            return false;
        }
        self.bind(front)
    }

    /// Keep the saved aliases for this session: each is bound again only
    /// when exactly one open terminal carries its name.
    pub fn restore(&mut self, saved: Vec<SavedBinding>) {
        self.pending = saved;
    }

    /// Look for the terminals the pending aliases name: first by the host's
    /// permanent id, then, for older files, by title. Returns whether a
    /// binding changed; the rest stay pending.
    pub fn match_pending(&mut self, open: &[AssistantTerminal]) -> bool {
        let mut changed = false;
        let mut still = Vec::new();
        for wanted in std::mem::take(&mut self.pending) {
            let pool: Vec<&AssistantTerminal> = open
                .iter()
                .filter(|terminal| terminal.local == wanted.local && !self.is_bound(&terminal.id))
                .collect();
            // The host's permanent id is the identity; the title is only
            // used for bindings saved before it was kept.
            let by_id: Vec<&AssistantTerminal> = pool
                .iter()
                .copied()
                .filter(|terminal| !wanted.stable.is_empty() && terminal.stable == wanted.stable)
                .collect();
            let matched = if by_id.len() == 1 {
                Some(by_id[0])
            } else if by_id.is_empty() {
                let by_name: Vec<&AssistantTerminal> = pool
                    .iter()
                    .copied()
                    .filter(|terminal| terminal.title.eq_ignore_ascii_case(&wanted.title))
                    .collect();
                (by_name.len() == 1).then(|| by_name[0])
            } else {
                None
            };
            match matched {
                Some(terminal) if self.bind_as(terminal, &wanted.alias) => changed = true,
                _ => still.push(wanted),
            }
        }
        self.pending = still;
        changed
    }

    /// Bind a pending alias to the terminal the user picked. The alias comes
    /// back only if it is still free; the pending entry is dropped either
    /// way because the user answered it.
    pub fn rebind_pending(&mut self, index: usize, terminal: &AssistantTerminal) -> bool {
        let Some(wanted) = self.pending.get(index) else {
            return false;
        };
        let alias = wanted.alias.clone();
        let bound = self.bind_as(terminal, &alias);
        if !bound {
            self.pending.remove(index);
        }
        bound
    }

    /// Forget a pending alias the user does not want back.
    pub fn dismiss_pending(&mut self, index: usize) -> bool {
        if index >= self.pending.len() {
            return false;
        }
        self.pending.remove(index);
        true
    }

    /// Everything the next session should know: the bound aliases and the
    /// ones still waiting.
    pub fn saved(&self) -> Vec<SavedBinding> {
        let mut saved: Vec<SavedBinding> = self
            .entries
            .iter()
            .map(|entry| SavedBinding {
                alias: entry.alias.clone(),
                title: entry.title.clone(),
                local: entry.local,
                stable: entry.stable.clone(),
            })
            .collect();
        saved.extend(self.pending.iter().cloned());
        saved
    }

    /// Write the bindings, replacing the file in one step so a half-written
    /// list is never read.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(&self.saved())
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, text)?;
        fs::rename(&temporary, path)
    }

    /// Read a saved list. A missing or unreadable file is no bindings at
    /// all, not an error the panel has to report.
    pub fn load(path: &Path) -> Vec<SavedBinding> {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

/// Whether a saved alias names this terminal: the same name on the same
/// side of the connection.
fn same_terminal_name(wanted: &SavedBinding, terminal: &AssistantTerminal) -> bool {
    wanted.local == terminal.local
        && (!wanted.stable.is_empty() && wanted.stable == terminal.stable
            || wanted.title.eq_ignore_ascii_case(&terminal.title))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal(id: &str, title: &str) -> AssistantTerminal {
        AssistantTerminal::new(id, title)
    }

    #[test]
    fn an_alias_is_lower_case_and_keeps_one_dash_per_gap() {
        assert_eq!(alias_for("My  Host.example", &[], 1), "my-host-example");
        assert_eq!(alias_for("  --Web-01--  ", &[], 1), "web-01");
        assert_eq!(alias_for("生产主机", &[], 3), "terminal-3");
    }

    #[test]
    fn an_alias_is_short_enough_for_a_card() {
        let long = "a".repeat(100);
        assert!(alias_for(&long, &[], 1).len() <= ALIAS_LIMIT);
    }

    #[test]
    fn colliding_names_take_a_suffix_without_growing_past_the_limit() {
        let taken = vec!["web".to_string(), "web-2".to_string()];
        assert_eq!(alias_for("WEB", &taken, 1), "web-3");
        let long = "b".repeat(ALIAS_LIMIT);
        let taken = vec![long.clone()];
        let alias = alias_for(&long, &taken, 1);
        assert_eq!(alias.len(), ALIAS_LIMIT);
        assert!(alias.ends_with("-2"));
    }

    #[test]
    fn the_front_terminal_joins_an_empty_conversation_once() {
        let mut bindings = Bindings::default();
        let web = terminal("remote:1", "web");
        bindings.follow_front(Some(&web));
        assert_eq!(bindings.entries().len(), 1);
        assert_eq!(bindings.entries()[0].alias, "web");

        let other = terminal("remote:2", "db");
        bindings.follow_front(Some(&other));
        assert_eq!(bindings.entries().len(), 1, "the user's pick is kept");
    }

    #[test]
    fn a_terminal_taken_off_by_hand_does_not_come_back_as_the_front() {
        let mut bindings = Bindings::default();
        let web = terminal("remote:1", "web");
        bindings.bind(&web);
        bindings.unbind("remote:1");
        bindings.follow_front(Some(&web));
        assert!(bindings.is_empty());
        bindings.bind(&web);
        assert_eq!(
            bindings.entries()[0].alias,
            "web",
            "a freed name may come back"
        );
    }

    #[test]
    fn a_closed_terminal_leaves_and_the_others_keep_their_aliases() {
        let mut bindings = Bindings::default();
        bindings.bind(&terminal("remote:1", "web"));
        bindings.bind(&terminal("remote:2", "db"));
        bindings.sync(&[terminal("remote:2", "db"), terminal("remote:3", "cache")]);
        assert_eq!(bindings.entries().len(), 1);
        assert_eq!(bindings.entries()[0].alias, "db");
        bindings.bind(&terminal("remote:1", "web"));
        assert_eq!(bindings.entries()[1].alias, "web");
    }

    #[test]
    fn toggling_a_bound_terminal_takes_it_off() {
        let mut bindings = Bindings::default();
        let web = terminal("remote:1", "web");
        bindings.toggle(&web);
        assert!(bindings.is_bound("remote:1"));
        bindings.toggle(&web);
        assert!(!bindings.is_bound("remote:1"));
    }

    #[test]
    fn a_terminal_without_a_view_says_it_cannot_run() {
        assert_eq!(
            terminal("local:1", "local").unsupported(),
            Some("shell_only")
        );
        assert_eq!(
            AssistantTerminal::new("remote:1", "web").unsupported(),
            Some("shell_only"),
            "no view means no channel to run on"
        );
    }

    #[test]
    fn four_bindings_are_the_most_one_conversation_takes() {
        let mut bindings = Bindings::default();
        for index in 0..MAX_BINDINGS {
            assert!(bindings.bind(&terminal(&format!("remote:{index}"), "web")));
        }
        assert!(!bindings.has_room());
        assert!(
            !bindings.bind(&terminal("remote:9", "cache")),
            "the fifth is refused"
        );
        assert_eq!(bindings.entries().len(), MAX_BINDINGS);
    }

    #[test]
    fn a_saved_alias_returns_to_the_one_terminal_of_that_name() {
        let mut bindings = Bindings::default();
        bindings.restore(vec![SavedBinding {
            alias: "web".into(),
            title: "Web".into(),
            local: false,
            stable: String::new(),
        }]);
        assert!(bindings.match_pending(&[terminal("remote:7", "web")]));
        assert_eq!(bindings.entries().len(), 1);
        assert_eq!(bindings.entries()[0].alias, "web");
        assert!(bindings.pending().is_empty());
    }

    #[test]
    fn an_ambiguous_or_missing_name_waits_instead_of_switching_terminals() {
        let saved = vec![SavedBinding {
            alias: "web".into(),
            title: "web".into(),
            local: false,
            stable: String::new(),
        }];
        let mut bindings = Bindings::default();
        bindings.restore(saved.clone());
        let twins = [terminal("remote:1", "web"), terminal("remote:2", "web")];
        assert!(!bindings.match_pending(&twins));
        assert!(bindings.is_empty(), "nothing bound by guess");
        assert_eq!(bindings.pending().len(), 1);
        assert_eq!(bindings.pending()[0].alias, "web");

        let mut bindings = Bindings::default();
        bindings.restore(saved);
        assert!(!bindings.match_pending(&[terminal("remote:9", "db")]));
        assert!(bindings.is_empty());
        assert_eq!(bindings.pending().len(), 1);
    }

    #[test]
    fn a_saved_alias_finds_its_host_again_after_a_rename() {
        let mut bindings = Bindings::default();
        bindings.restore(vec![SavedBinding {
            alias: "web".into(),
            title: "web".into(),
            local: false,
            stable: "host:abc".into(),
        }]);
        let renamed = AssistantTerminal::new("remote:9", "web-01").with_stable("host:abc");
        assert!(bindings.match_pending(&[renamed]));
        assert_eq!(bindings.entries().len(), 1);
        assert_eq!(bindings.entries()[0].alias, "web");
    }

    #[test]
    fn two_tabs_of_one_host_wait_instead_of_picking_one() {
        let mut bindings = Bindings::default();
        bindings.restore(vec![SavedBinding {
            alias: "web".into(),
            title: "web".into(),
            local: false,
            stable: "host:abc".into(),
        }]);
        let twins = [
            AssistantTerminal::new("remote:1", "web").with_stable("host:abc"),
            AssistantTerminal::new("remote:2", "web").with_stable("host:abc"),
        ];
        assert!(!bindings.match_pending(&twins));
        assert!(bindings.is_empty());
        assert_eq!(bindings.pending().len(), 1);
    }

    #[test]
    fn a_pending_alias_takes_the_terminal_the_user_picked() {
        let mut bindings = Bindings::default();
        bindings.restore(vec![
            SavedBinding {
                alias: "web".into(),
                title: "web".into(),
                local: false,
                stable: "host:abc".into(),
            },
            SavedBinding {
                alias: "db".into(),
                title: "db".into(),
                local: false,
                stable: "host:def".into(),
            },
        ]);
        let picked = AssistantTerminal::new("remote:4", "renamed").with_stable("host:abc");
        assert!(bindings.rebind_pending(0, &picked));
        assert_eq!(bindings.entries()[0].alias, "web");
        assert_eq!(bindings.entries()[0].title, "renamed");
        assert_eq!(bindings.pending().len(), 1, "the other one still waits");
        assert_eq!(bindings.pending()[0].alias, "db");
    }

    #[test]
    fn a_local_terminal_and_a_remote_one_of_the_same_name_do_not_mix() {
        let mut bindings = Bindings::default();
        bindings.restore(vec![SavedBinding {
            alias: "shell".into(),
            title: "shell".into(),
            local: true,
            stable: String::new(),
        }]);
        bindings.match_pending(&[terminal("remote:1", "shell")]);
        assert!(bindings.is_empty(), "the remote one is a different host");
        bindings.match_pending(&[terminal("local:1", "shell").local()]);
        assert!(bindings.is_bound("local:1"));
    }

    #[test]
    fn bindings_survive_a_write_and_a_read() {
        let dir = std::env::temp_dir().join(format!(
            "shellas-bindings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let path = path_in(&dir);
        let mut bindings = Bindings::default();
        bindings.bind(&terminal("remote:1", "web"));
        bindings.bind(&terminal("local:1", "shell").local());
        bindings.save(&path).expect("save");
        let saved = Bindings::load(&path);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0].alias, "web");
        assert!(saved[1].local);
        let _ = std::fs::remove_dir_all(&dir);

        assert!(Bindings::load(&path_in(&dir)).is_empty(), "nothing on disk");
    }

    #[test]
    fn a_pending_alias_is_resolved_by_binding_its_terminal_by_hand() {
        let mut bindings = Bindings::default();
        bindings.restore(vec![SavedBinding {
            alias: "web".into(),
            title: "web".into(),
            local: false,
            stable: String::new(),
        }]);
        assert!(bindings.bind(&terminal("remote:5", "web")));
        assert!(bindings.pending().is_empty());
        assert!(!bindings.dismiss_pending(0), "nothing left to dismiss");
    }
}
