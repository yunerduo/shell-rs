//! Which terminals the assistant may speak to, and what the model calls
//! them. The alias is the only handle the model sees: terminal ids are the
//! host's business.

use gpui_kit::gpui::{Entity, WeakEntity};

use crate::terminal::TerminalView;

/// One terminal the sidebar can offer. `alias` is filled in when it is
/// bound; before that it is empty.
#[derive(Clone, Debug)]
pub struct AssistantTerminal {
    pub id: String,
    /// The tab's title, for people; not the alias.
    pub title: String,
    pub alias: String,
    /// The view a tool call runs on. A local terminal has none: a command
    /// cannot be run beside a shell of this computer, so the panel answers
    /// `unsupported` instead.
    pub view: Option<WeakEntity<TerminalView>>,
    /// The connection carries only the shell: a command cannot go beside it.
    pub shell_only: bool,
}

/// The identity of a terminal is its id and the two names; the view is a
/// handle to the same terminal, not a part of what it is.
impl PartialEq for AssistantTerminal {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
            && self.title == other.title
            && self.alias == other.alias
            && self.shell_only == other.shell_only
    }
}

impl Eq for AssistantTerminal {}

impl AssistantTerminal {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            alias: String::new(),
            view: None,
            shell_only: false,
        }
    }

    /// Offer the terminal with the view its commands run on.
    pub fn with_view(mut self, view: &Entity<TerminalView>, shell_only: bool) -> Self {
        self.view = Some(view.downgrade());
        self.shell_only = shell_only;
        self
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
/// default; the user may add others. Bindings are kept for the session in
/// P0; saving them across restarts is P4's.
#[derive(Debug, Default)]
pub struct Bindings {
    entries: Vec<AssistantTerminal>,
    /// Terminals the user took off by hand; the front one is not put back
    /// until the user picks it again.
    unbound: Vec<String>,
    /// Numbers the fallback names.
    sequence: usize,
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

    /// Bind a terminal, giving it an alias no other binding holds.
    pub fn bind(&mut self, terminal: &AssistantTerminal) {
        if self.is_bound(&terminal.id) {
            return;
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
    }

    /// Take a terminal off. Remembered, so the front terminal does not come
    /// straight back.
    pub fn unbind(&mut self, id: &str) {
        self.entries.retain(|entry| entry.id != id);
        if !self.unbound.iter().any(|unbound| unbound == id) {
            self.unbound.push(id.to_string());
        }
    }

    pub fn toggle(&mut self, terminal: &AssistantTerminal) {
        if self.is_bound(&terminal.id) {
            self.unbind(&terminal.id);
        } else {
            self.bind(terminal);
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
            }
        }
        self.unbound
            .retain(|id| open.iter().any(|terminal| &terminal.id == id));
    }

    /// Put the front terminal in the conversation when nothing is bound.
    pub fn follow_front(&mut self, front: Option<&AssistantTerminal>) {
        let Some(front) = front else {
            return;
        };
        if !self.entries.is_empty()
            || self.is_bound(&front.id)
            || self.unbound.iter().any(|id| id == &front.id)
        {
            return;
        }
        self.bind(front);
    }
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
}
