//! What `wait_for_output` looks for, without a window: the lines to ignore,
//! the line that matched, and the answer the model reads. The panel hands
//! it the terminal's screen; this module decides.
//!
//! A wait starts at the moment the call is accepted, so lines already on
//! screen are the past and never match. A command the assistant typed into
//! the terminal is echoed by the shell, and that echo is not output: a line
//! that is the injected command (behind whatever prompt) is skipped, and the
//! answer says a real match may still have been the echo.

use std::collections::HashSet;
use std::time::Duration;

use serde_json::json;

use super::chat::MatchMode;

/// How many lines a timeout hands back; the panel shows the same tail.
pub const RECENT_LINES: usize = 20;

/// What a scan found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    /// The line as it stood on screen, trimmed.
    pub line: String,
    /// An echo of the assistant's own command was skipped, so this match
    /// could in principle be one too.
    pub echo_possible: bool,
}

/// The scan for one wait.
#[derive(Clone, Debug)]
pub struct Watcher {
    pattern: String,
    mode: MatchMode,
    ignore_case: bool,
    /// The screen as it was when the wait began, trimmed line by line.
    baseline: HashSet<String>,
    /// Commands the assistant typed on this terminal, normalized.
    echoes: Vec<String>,
    /// An echo was skipped earlier, so a later hit carries the hint.
    echo_seen: bool,
}

impl Watcher {
    /// Watch for `pattern` on a screen that looked like `screen` when the
    /// call arrived.
    pub fn new(pattern: &str, mode: MatchMode, ignore_case: bool, screen: &str) -> Self {
        let pattern = pattern.trim().to_string();
        let baseline = screen.lines().map(normalize).collect();
        Self {
            pattern,
            mode,
            ignore_case,
            baseline,
            echoes: Vec::new(),
            echo_seen: false,
        }
    }

    /// Tell the watcher a command the assistant typed into this terminal:
    /// its echo on screen is not output.
    pub fn watched_command(&mut self, command: &str) {
        let command = normalize(command);
        if !command.is_empty() && !self.echoes.contains(&command) {
            self.echoes.push(command);
        }
    }

    /// Look at the screen now. `None` means keep waiting: nothing new
    /// matched, or only the echo of the assistant's own command did.
    pub fn observe(&mut self, screen: &str) -> Option<Hit> {
        let mut echo_here = false;
        for line in screen.lines() {
            if !self.matches(line) {
                continue;
            }
            let key = normalize(line);
            if self.baseline.contains(&key) {
                continue;
            }
            if self.is_echo(&key) {
                echo_here = true;
                continue;
            }
            let echo_possible = echo_here || self.echo_seen;
            self.echo_seen = echo_possible;
            return Some(Hit {
                line: line.trim().to_string(),
                echo_possible,
            });
        }
        self.echo_seen |= echo_here;
        None
    }

    /// Whether an echo was skipped, so a timeout can say the text may have
    /// been there all along as the assistant's own command line.
    pub fn echo_seen(&self) -> bool {
        self.echo_seen
    }

    fn matches(&self, line: &str) -> bool {
        let line = line.trim();
        if self.ignore_case {
            let line = line.to_lowercase();
            let pattern = self.pattern.to_lowercase();
            match self.mode {
                MatchMode::Contains => line.contains(&pattern),
                MatchMode::Line => line == pattern,
            }
        } else {
            match self.mode {
                MatchMode::Contains => line.contains(&self.pattern),
                MatchMode::Line => line == self.pattern,
            }
        }
    }

    fn is_echo(&self, line: &str) -> bool {
        self.echoes
            .iter()
            .any(|echo| line == echo || line.ends_with(echo.as_str()))
    }
}

/// One line with its edges and inner runs of spaces made regular, so a
/// padded cell row and the bare text compare equal.
fn normalize(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The tail of the screen, as a timeout hands it back: the last
/// [`RECENT_LINES`] non-blank lines.
pub fn recent(screen: &str) -> Vec<String> {
    let mut lines: Vec<String> = screen
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect();
    let keep = lines.len().saturating_sub(RECENT_LINES);
    lines.split_off(keep)
}

/// What the model reads when the text appeared.
pub fn hit_body(hit: &Hit, elapsed: Duration) -> String {
    let mut body = json!({
        "status": "ok",
        "matched": hit.line,
        "elapsed": elapsed.as_secs(),
    });
    if hit.echo_possible {
        body["echo_possible"] = json!(true);
    }
    body.to_string()
}

/// What the model reads when the wait ran out. The last lines come with it,
/// so the model can tell a failed start from a slow one; nothing is retried.
pub fn timeout_body(waited: Duration, recent: &[String], echo_possible: bool) -> String {
    let mut body = json!({
        "status": "timeout",
        "waited": waited.as_secs(),
        "recent": recent,
    });
    if echo_possible {
        body["echo_possible"] = json!(true);
    }
    body.to_string()
}

/// What the model reads when the wait ended before its text appeared.
pub fn cancelled_body(reason: &str) -> String {
    json!({ "status": "cancelled", "reason": reason }).to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn screen(lines: &[&str]) -> String {
        lines.join("\n")
    }

    #[test]
    fn only_text_that_appears_after_the_wait_counts() {
        let before = screen(&["$ tail -f app.log", "Serving HTTP on 0.0.0.0 port 8000"]);
        let mut watcher = Watcher::new("Serving HTTP on", MatchMode::Contains, false, &before);
        assert_eq!(watcher.observe(&before), None, "old output is the past");

        let after = screen(&["$ tail -f app.log", "Serving HTTP on 0.0.0.0 port 8000"]);
        let mut watcher = Watcher::new("listening on", MatchMode::Contains, false, &screen(&["$"]));
        assert!(watcher.observe(&after).is_none());
        assert_eq!(
            watcher
                .observe(&screen(&[
                    "$ tail -f app.log",
                    "now listening on port 9000"
                ]))
                .expect("hit")
                .line,
            "now listening on port 9000"
        );
    }

    #[test]
    fn a_line_match_wants_the_whole_line() {
        let mut watcher = Watcher::new("ready", MatchMode::Line, false, "");
        assert!(watcher.observe(&screen(&["not ready yet"])).is_none());
        assert_eq!(
            watcher
                .observe(&screen(&["   ready   "]))
                .expect("hit")
                .line,
            "ready"
        );
    }

    #[test]
    fn a_case_insensitive_wait_finds_another_spelling() {
        let mut watcher = Watcher::new("serving http", MatchMode::Contains, true, "");
        assert!(watcher.observe(&screen(&["Serving HTTP on"])).is_some());
        let mut exact = Watcher::new("serving http", MatchMode::Contains, false, "");
        assert!(exact.observe(&screen(&["Serving HTTP on"])).is_none());
    }

    #[test]
    fn the_echo_of_the_assistants_own_command_is_not_output() {
        let command = "python3 -m http.server 8000";
        let mut watcher = Watcher::new("8000", MatchMode::Contains, false, "");
        watcher.watched_command(command);
        let echo = screen(&[
            "me@web:~$ python3 -m http.server 8000",
            "Serving HTTP on 0.0.0.0 port 8000",
        ]);
        let hit = watcher.observe(&echo).expect("the real line matches");
        assert_eq!(hit.line, "Serving HTTP on 0.0.0.0 port 8000");
        assert!(hit.echo_possible, "an echo was skipped");

        let only_echo = screen(&["me@web:~$ python3 -m http.server 8000"]);
        let mut watcher = Watcher::new("8000", MatchMode::Contains, false, "");
        watcher.watched_command(command);
        assert!(watcher.observe(&only_echo).is_none());
        assert!(
            watcher
                .observe(&screen(&["serving on 8000"]))
                .expect("later hit")
                .echo_possible
        );
    }

    #[test]
    fn a_timeout_carries_the_tail_of_the_screen() {
        let lines: Vec<String> = (0..30).map(|index| format!("line {index}")).collect();
        let recent = recent(&screen(
            &lines.iter().map(String::as_str).collect::<Vec<_>>(),
        ));
        assert_eq!(recent.len(), RECENT_LINES);
        assert_eq!(recent.last().expect("last"), "line 29");
        let body: Value =
            serde_json::from_str(&timeout_body(Duration::from_secs(30), &recent, true)).unwrap();
        assert_eq!(body["status"], "timeout");
        assert_eq!(body["waited"], 30);
        assert_eq!(body["echo_possible"], true);
        assert_eq!(
            body["recent"].as_array().expect("lines").len(),
            RECENT_LINES
        );
    }

    #[test]
    fn a_hit_and_a_cancellation_read_as_their_own_status() {
        let hit = Hit {
            line: "ready".into(),
            echo_possible: false,
        };
        let body: Value = serde_json::from_str(&hit_body(&hit, Duration::from_secs(2))).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["matched"], "ready");
        assert_eq!(body["elapsed"], 2);
        assert!(body.get("echo_possible").is_none());

        let body: Value = serde_json::from_str(&cancelled_body("user_takeover")).unwrap();
        assert_eq!(body["status"], "cancelled");
        assert_eq!(body["reason"], "user_takeover");
    }
}
