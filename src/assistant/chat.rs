//! One conversation's messages, as the panel keeps them and as the
//! provider turns them into a request.

/// Who said a thing in the conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatMessage {
    pub role: Role,
    /// Plain text. The panel renders assistant text as Markdown, but the
    /// wire format is text either way.
    pub text: String,
}

impl ChatMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            text: text.into(),
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            text: text.into(),
        }
    }
}

/// The conversation exported as Markdown, for the copy button: the user's
/// words are a quoted heading, the assistant's are already Markdown.
pub fn to_markdown(messages: &[ChatMessage], user_heading: &str) -> String {
    let mut out = String::new();
    for message in messages {
        if message.text.trim().is_empty() {
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
}
