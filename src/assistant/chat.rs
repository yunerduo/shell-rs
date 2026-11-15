//! One conversation's messages, as the panel keeps them and as the
//! provider turns them into a request.

use serde::Deserialize;

use crate::i18n::t;

/// Who said a thing in the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
    /// What a tool answered the assistant. The wire carries it back as a
    /// result, not as something a person said.
    Tool,
}

/// One call the model asked for. The arguments stay the provider's JSON
/// text until the panel reads them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Whether the call waits for the command to end or leaves it running.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    #[default]
    Wait,
    Background,
}

/// The arguments of `run_command`, as the tool's schema declares them.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RunCommandArgs {
    /// The alias the model was given, not a terminal id.
    pub terminal: String,
    pub command: String,
    #[serde(default)]
    pub mode: RunMode,
}

/// How `wait_for_output` compares a line with its pattern.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchMode {
    /// The line contains the pattern anywhere.
    #[default]
    Contains,
    /// The whole line, trimmed, is the pattern.
    Line,
}

/// How long a wait runs when the model does not say, and the longest it may
/// ask for: a wait holds a tool call open, so it is bounded.
pub const WAIT_DEFAULT_SECONDS: u32 = 30;
pub const WAIT_MAX_SECONDS: u32 = 120;

/// The arguments of `wait_for_output`, as the tool's schema declares them.
/// It waits for text on one terminal and runs nothing.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForOutputArgs {
    pub terminal: String,
    pub pattern: String,
    #[serde(default)]
    pub r#match: MatchMode,
    #[serde(default = "default_wait_seconds")]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub ignore_case: bool,
}

fn default_wait_seconds() -> u32 {
    WAIT_DEFAULT_SECONDS
}

impl WaitForOutputArgs {
    /// Whether the wait is worth starting: a pattern short enough to scan
    /// for. The timeout is clamped rather than refused.
    pub fn usable(&self) -> bool {
        let pattern = self.pattern.trim();
        !pattern.is_empty() && pattern.chars().count() <= 200
    }

    /// The timeout, kept between one second and the maximum.
    pub fn timeout_seconds(&self) -> u32 {
        self.timeout_seconds.clamp(1, WAIT_MAX_SECONDS)
    }
}

/// One choice the model offers the user.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AskOption {
    pub id: String,
    pub label: String,
}

/// The arguments of `ask_user`, as the tool's schema declares them. Asking
/// runs nothing; it only gives the conversation one thing to wait for.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct AskUserArgs {
    pub question: String,
    #[serde(default)]
    pub options: Vec<AskOption>,
    #[serde(default = "default_allow_free_text")]
    pub allow_free_text: bool,
}

fn default_allow_free_text() -> bool {
    true
}

impl AskUserArgs {
    /// Whether the choices are something a person can answer: a question,
    /// then either two to six short options with distinct ids, or free
    /// text.
    pub fn usable(&self) -> bool {
        let question = self.question.trim();
        if question.is_empty() || question.chars().count() > 600 {
            return false;
        }
        match self.options.len() {
            0 => self.allow_free_text,
            2..=6 => {
                let ids: std::collections::BTreeSet<&str> = self
                    .options
                    .iter()
                    .map(|option| option.id.as_str())
                    .collect();
                ids.len() == self.options.len()
                    && self.options.iter().all(|option| {
                        !option.id.trim().is_empty()
                            && !option.label.trim().is_empty()
                            && option.label.chars().count() <= 200
                    })
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    /// Plain text. The panel renders assistant text as Markdown, but the
    /// wire format is text either way. A tool message's text is its JSON
    /// result.
    pub text: String,
    /// The calls the assistant made this turn.
    pub tool_calls: Vec<ToolCall>,
    /// For a tool message: the call it answers.
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// The assistant's turn that asked for tools, and whatever it said
    /// beside them.
    pub fn assistant_calls(text: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// One tool's answer, addressed back to the call that asked.
    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
    }
}

/// The conversation as one Markdown file: a title, the facts a reader
/// needs, and one section per turn. A heading separates turns because some
/// renderers read `---` under text as a heading underline. Tool results are
/// left out: they are the model's working notes.
pub fn export_markdown(
    messages: &[ChatMessage],
    user_heading: &str,
    title: &str,
    meta: &[(String, String)],
) -> String {
    let assistant_heading = t!("assistant.role.assistant");
    let mut body = String::new();
    for message in messages {
        if message.text.trim().is_empty() || message.role == Role::Tool {
            continue;
        }
        let heading: &str = match message.role {
            Role::User => user_heading,
            Role::Assistant | Role::System => &assistant_heading,
            Role::Tool => continue,
        };
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str("## ");
        body.push_str(heading);
        body.push_str("\n\n");
        body.push_str(&plain(&message.text));
    }
    if body.is_empty() {
        return String::new();
    }
    balance_fences(&mut body);

    let mut out = String::new();
    out.push_str("# ");
    out.push_str(&plain(title.trim()));
    out.push_str("\n\n");
    let mut listed = false;
    for (label, value) in meta {
        let value = plain(value.trim());
        if value.is_empty() {
            continue;
        }
        out.push_str("- ");
        out.push_str(label);
        out.push_str(": ");
        out.push_str(&value);
        out.push('\n');
        listed = true;
    }
    if listed {
        out.push('\n');
    }
    out.push_str(&body);
    out
}

/// Control characters are not text a person meant to write; a tab and a
/// newline are kept, and everything else that is not printable is dropped.
fn plain(text: &str) -> String {
    text.chars()
        .filter(|character| *character == '\n' || *character == '\t' || !character.is_control())
        .collect()
}

/// A code fence left open would swallow the rest of the file, so an odd
/// count of them gets one closing line.
fn balance_fences(text: &mut String) {
    let fences = text
        .lines()
        .filter(|line| line.trim_start().starts_with("```"))
        .count();
    if fences % 2 == 1 {
        text.push_str("\n```");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_keeps_the_assistant_text_as_written() {
        let messages = vec![
            ChatMessage::user("list the files"),
            ChatMessage::assistant("```\nls\n```"),
            ChatMessage::user("thanks"),
        ];
        let markdown = export_markdown(&messages, "You", "A talk", &[]);
        assert_eq!(
            markdown,
            format!(
                "# A talk\n\n## You\n\nlist the files\n\n## {}\n\n```\nls\n```\n\n## You\n\nthanks",
                t!("assistant.role.assistant")
            )
        );
    }

    #[test]
    fn empty_turns_are_skipped() {
        let messages = vec![ChatMessage::user(""), ChatMessage::assistant("  ")];
        assert_eq!(export_markdown(&messages, "You", "A talk", &[]), "");
    }

    #[test]
    fn the_heading_carries_the_facts_a_reader_needs() {
        let messages = vec![ChatMessage::user("check the disk")];
        let meta = vec![
            ("Time".to_string(), "2026-11-15 10:00:00".to_string()),
            ("Model".to_string(), "gpt-4o-mini".to_string()),
            ("Empty".to_string(), "  ".to_string()),
        ];
        let markdown = export_markdown(&messages, "You", "Disk check", &meta);
        assert_eq!(
            markdown,
            "# Disk check\n\n- Time: 2026-11-15 10:00:00\n- Model: gpt-4o-mini\n\n## You\n\ncheck the disk"
        );
    }

    #[test]
    fn an_unclosed_code_fence_is_closed_at_the_end() {
        let unclosed = export_markdown(&[ChatMessage::assistant("```\nls")], "You", "T", &[]);
        assert!(unclosed.ends_with("```\nls\n```"), "{unclosed}");
        let closed = export_markdown(&[ChatMessage::assistant("```\nls\n```")], "You", "T", &[]);
        assert!(closed.ends_with("```\nls\n```"), "{closed}");
    }

    #[test]
    fn control_characters_are_not_written_out() {
        let messages = vec![ChatMessage::user("hi\u{7}ther\u{1b}e")];
        assert!(
            export_markdown(&messages, "You", "T", &[]).contains("hithere"),
            "a bell and an escape do not belong in a report"
        );
    }

    #[test]
    fn a_tool_result_is_not_copied_into_the_markdown() {
        let messages = vec![
            ChatMessage::user("check the disk"),
            ChatMessage::assistant_calls(
                "",
                vec![ToolCall {
                    id: "call-1".into(),
                    name: "run_command".into(),
                    arguments: r#"{"terminal":"web","command":"df -h"}"#.into(),
                }],
            ),
            ChatMessage::tool_result("call-1", r#"{"status":"ok"}"#),
            ChatMessage::assistant("The disk is fine."),
        ];
        assert_eq!(
            export_markdown(&messages, "You", "T", &[]),
            format!(
                "# T\n\n## You\n\ncheck the disk\n\n## {}\n\nThe disk is fine.",
                t!("assistant.role.assistant")
            )
        );
    }

    #[test]
    fn the_run_command_arguments_read_from_json() {
        let args: RunCommandArgs =
            serde_json::from_str(r#"{"terminal":"web-2","command":"ps aux"}"#).expect("parse");
        assert_eq!(args.terminal, "web-2");
        assert_eq!(args.command, "ps aux");
        assert_eq!(args.mode, RunMode::Wait);
        assert!(serde_json::from_str::<RunCommandArgs>(r#"{"command":"ls"}"#).is_err());
    }

    #[test]
    fn a_run_mode_is_wait_or_background() {
        let args: RunCommandArgs =
            serde_json::from_str(r#"{"terminal":"web","command":"npm start","mode":"background"}"#)
                .expect("parse");
        assert_eq!(args.mode, RunMode::Background);
        assert!(
            serde_json::from_str::<RunCommandArgs>(
                r#"{"terminal":"web","command":"ls","mode":"later"}"#
            )
            .is_err()
        );
    }

    fn ask(json: &str) -> AskUserArgs {
        serde_json::from_str(json).expect("parse")
    }

    #[test]
    fn a_wait_reads_its_pattern_and_defaults() {
        let wait: WaitForOutputArgs =
            serde_json::from_str(r#"{"terminal":"server","pattern":"Serving HTTP on"}"#)
                .expect("parse");
        assert_eq!(wait.r#match, MatchMode::Contains);
        assert_eq!(wait.timeout_seconds(), WAIT_DEFAULT_SECONDS);
        assert!(!wait.ignore_case);
        assert!(wait.usable());
        assert!(serde_json::from_str::<WaitForOutputArgs>(r#"{"pattern":"x"}"#).is_err());
    }

    #[test]
    fn a_wait_takes_a_line_match_and_a_bounded_timeout() {
        let wait: WaitForOutputArgs = serde_json::from_str(
            r#"{"terminal":"server","pattern":"ready","match":"line","timeoutSeconds":600,"ignoreCase":true}"#,
        )
        .expect("parse");
        assert_eq!(wait.r#match, MatchMode::Line);
        assert!(wait.ignore_case);
        assert_eq!(wait.timeout_seconds(), WAIT_MAX_SECONDS);
        assert!(
            serde_json::from_str::<WaitForOutputArgs>(
                r#"{"terminal":"server","pattern":"x","match":"regex"}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_wait_needs_a_pattern_short_enough_to_scan_for() {
        assert!(
            !serde_json::from_str::<WaitForOutputArgs>(r#"{"terminal":"s","pattern":"  "}"#)
                .expect("parse")
                .usable()
        );
        let long = "a".repeat(201);
        let wait: WaitForOutputArgs =
            serde_json::from_str(&format!(r#"{{"terminal":"s","pattern":"{long}"}}"#))
                .expect("parse");
        assert!(!wait.usable());
    }

    #[test]
    fn an_ask_offers_two_to_six_choices_or_free_text() {
        assert!(
            ask(r#"{"question":"Which port?","options":[{"id":"a","label":"8080"},{"id":"b","label":"3000"}]}"#)
                .usable()
        );
        assert!(
            ask(r#"{"question":"Which port?"}"#).usable(),
            "free text by default"
        );
        assert!(
            !ask(r#"{"question":"Which port?","allow_free_text":false}"#).usable(),
            "nothing to pick"
        );
        assert!(
            !ask(r#"{"question":"Which port?","options":[{"id":"a","label":"8080"}]}"#).usable()
        );
        assert!(
            !ask(
                r#"{"question":"Which?","options":[{"id":"a","label":"1"},{"id":"a","label":"2"}]}"#
            )
            .usable(),
            "the same id twice"
        );
        assert!(!ask(r#"{"question":"  "}"#).usable());
    }
}
