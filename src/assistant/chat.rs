//! One conversation's messages, as the panel keeps them and as the
//! provider turns them into a request.

use serde::Deserialize;

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

/// The arguments of `run_command`, as the tool's schema declares them.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RunCommandArgs {
    /// The alias the model was given, not a terminal id.
    pub terminal: String,
    pub command: String,
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

/// The conversation exported as Markdown, for the copy button: the user's
/// words are a quoted heading, the assistant's are already Markdown. Tool
/// results are left out: they are the model's working notes.
pub fn to_markdown(messages: &[ChatMessage], user_heading: &str) -> String {
    let mut out = String::new();
    for message in messages {
        if message.text.trim().is_empty() || message.role == Role::Tool {
            continue;
        }
        if !out.is_empty() {
            out.push_str("\n\n---\n\n");
        }
        match message.role {
            Role::User => {
                out.push_str("**");
                out.push_str(user_heading);
                out.push_str("**\n\n");
                out.push_str(&message.text);
            }
            Role::Assistant | Role::System => out.push_str(&message.text),
            Role::Tool => {}
        }
    }
    out
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
        let markdown = to_markdown(&messages, "You");
        assert_eq!(
            markdown,
            "**You**\n\nlist the files\n\n---\n\n```\nls\n```\n\n---\n\n**You**\n\nthanks"
        );
    }

    #[test]
    fn empty_turns_are_skipped() {
        let messages = vec![ChatMessage::user(""), ChatMessage::assistant("  ")];
        assert_eq!(to_markdown(&messages, "You"), "");
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
            to_markdown(&messages, "You"),
            "**You**\n\ncheck the disk\n\n---\n\nThe disk is fine."
        );
    }

    #[test]
    fn the_run_command_arguments_read_from_json() {
        let args: RunCommandArgs =
            serde_json::from_str(r#"{"terminal":"web-2","command":"ps aux"}"#).expect("parse");
        assert_eq!(args.terminal, "web-2");
        assert_eq!(args.command, "ps aux");
        assert!(serde_json::from_str::<RunCommandArgs>(r#"{"command":"ls"}"#).is_err());
    }
}
