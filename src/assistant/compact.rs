//! Compacting a conversation that is close to the model's context window.
//! The oldest turns are replaced by a summary the current model writes; when
//! that call fails, the same turns are simply dropped. Everything here is a
//! pure function so the strategy can be tested without a model.

use std::time::Duration;

use super::chat::{ChatMessage, Role};
use super::client::{ProxySetting, StreamRequest};
use super::settings::ModelSelection;

/// Compress before the next turn once the estimate reaches this much of the
/// window.
pub const TRIGGER_PERCENT: usize = 80;

/// Keep this many recent user turns verbatim.
pub const KEEP_ROUNDS: usize = 4;

/// A compaction aims to bring the conversation down to this much.
pub const TARGET_PERCENT: usize = 60;

/// The most tokens the summarizing answer may take.
const SUMMARY_TOKENS: u32 = 2_048;

/// The longest a summarizing answer may take before it is abandoned; a slow
/// summary is worse than a trim.
pub const SUMMARY_TIMEOUT: Duration = Duration::from_secs(120);

/// How much of one old message the summary prompt carries.
const TEXT_LIMIT: usize = 8_000;
const RESULT_LIMIT: usize = 2_000;

/// What the model is asked to do with the old turns.
const SUMMARY_GUIDE: &str = "Compact the earlier part of a conversation between a user and an \
assistant. The assistant may have used terminal tools. Write a short factual summary that a \
reader can use to continue the conversation. Keep: what the user asked; what was done; commands \
and their outcomes; files, paths or hosts that were touched; the current conclusions; and what is \
still unfinished. Do not invent anything. Omit greetings and repetition. Write in the language of \
the conversation. Output only the summary, with no preamble and no code fence.";

/// The summary written when the model could not produce one. It tells the
/// next turn that the missing part exists but is not available.
pub const FALLBACK_SUMMARY: &str = "Earlier messages were removed to fit the model's context \
window; they are not available. Continue from the messages that remain.";

/// Where a compaction cuts the conversation and what the model is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The first message that stays; everything before it is replaced by the
    /// summary. The cut is always at a user turn, so an assistant's calls
    /// and their results are never separated.
    pub split: usize,
    /// How many stored messages the summary replaces. Only user and
    /// assistant messages are journaled, so this is the number the session
    /// can replay.
    pub count: usize,
    /// The one user message the summarizing request carries.
    pub prompt: String,
}

/// A rough token estimate for the next request: the messages, the standing
/// prompt and the tool descriptions. The display calls it an estimate; the
/// window comes from the model profile or the trial's status.
pub fn estimate_tokens(messages: &[ChatMessage], standing_chars: usize) -> usize {
    let mut chars = standing_chars;
    let mut image_tokens = 0;
    for message in messages {
        chars += message.text.chars().count() + message.reasoning.chars().count();
        for call in &message.tool_calls {
            chars += call.name.chars().count() + call.arguments.chars().count() + 8;
        }
        for attachment in &message.attachments {
            match attachment.text_content() {
                Some((text, _)) => chars += text.chars().count(),
                // Roughly what one picture costs once it is scaled.
                None => image_tokens += 1_100,
            }
        }
    }
    // The platform prompt and the tool schemas travel every time.
    chars += 12_000;
    chars / 4 + messages.len() * 4 + image_tokens
}

/// How many user turns the conversation has, counting only turns with
/// something in them.
pub fn user_turns(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter(|message| {
            message.role == Role::User
                && (!message.text.trim().is_empty() || !message.attachments.is_empty())
        })
        .count()
}

/// Whether there is anything old enough to compact: at least one user turn
/// beyond the ones compaction keeps verbatim.
pub fn can_plan(messages: &[ChatMessage]) -> bool {
    user_turns(messages) > KEEP_ROUNDS
}

/// What a compaction would do, or `None` when the conversation is too short
/// or the cut would leave nothing to summarize.
pub fn plan(messages: &[ChatMessage], existing_summary: Option<&str>) -> Option<Plan> {
    let users: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::User
                && (!message.text.trim().is_empty() || !message.attachments.is_empty())
        })
        .map(|(index, _)| index)
        .collect();
    if users.len() <= KEEP_ROUNDS {
        return None;
    }
    let split = users[users.len() - KEEP_ROUNDS];
    if split == 0 {
        return None;
    }
    let count = durable_count(&messages[..split]);
    if count == 0 {
        return None;
    }
    Some(Plan {
        split,
        count,
        prompt: transcript(&messages[..split], existing_summary),
    })
}

/// Drop the oldest turns until the estimate is at or below
/// [`TARGET_PERCENT`] of the window. At least the last user turn stays.
/// Returns how many stored messages were dropped.
pub fn trim_to_target(
    messages: &mut Vec<ChatMessage>,
    standing_chars: usize,
    window: usize,
) -> usize {
    if window == 0 {
        return 0;
    }
    let target = window * TARGET_PERCENT / 100;
    let mut removed = 0;
    while estimate_tokens(messages, standing_chars) > target {
        let Some(first) = messages
            .iter()
            .position(|message| message.role == Role::User)
        else {
            break;
        };
        let Some(next) = messages[first + 1..]
            .iter()
            .position(|message| message.role == Role::User)
            .map(|offset| first + 1 + offset)
        else {
            break;
        };
        let dropped: Vec<ChatMessage> = messages.drain(..next).collect();
        removed += durable_count(&dropped);
    }
    removed
}

/// The request that writes the summary. It is text only, uses the same
/// model as the conversation (the trial included), and carries no tools.
pub fn request(
    selection: &ModelSelection,
    prompt: String,
    timeout: Duration,
    proxy: Option<ProxySetting>,
) -> StreamRequest {
    StreamRequest {
        protocol: selection.protocol,
        base_url: selection.base_url.clone(),
        full_url: selection.full_url,
        model: selection.model.clone(),
        keychain_id: selection.keychain_id.clone(),
        messages: vec![ChatMessage::user(prompt)],
        max_tokens: selection.max_tokens.clamp(256, SUMMARY_TOKENS),
        timeout,
        terminals: Vec::new(),
        scheduled: false,
        plain: true,
        headers: selection.headers.clone(),
        proxy,
        temperature: None,
        top_p: None,
        top_k: None,
        thinking_param: String::new(),
        thinking_effort: String::new(),
        stop: Vec::new(),
        extra: String::new(),
        instruction: String::new(),
        trial: selection.trial,
    }
}

/// How many of these messages a session journal stores: a user message with
/// words or an attachment, or an assistant message with words.
fn durable_count(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .filter(|message| match message.role {
            Role::User => !message.text.trim().is_empty() || !message.attachments.is_empty(),
            Role::Assistant => !message.text.trim().is_empty(),
            Role::System | Role::Tool => false,
        })
        .count()
}

/// The old part of the conversation as text, bounded per message so the
/// summarizing request itself cannot overflow the window.
fn transcript(messages: &[ChatMessage], existing_summary: Option<&str>) -> String {
    let mut out = String::from(SUMMARY_GUIDE);
    out.push('\n');
    if let Some(summary) = existing_summary
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        out.push_str("\n[Summary of the even earlier conversation]\n");
        out.push_str(summary);
        out.push('\n');
    }
    out.push_str("\n[Conversation to summarize]\n");
    for message in messages {
        if message.role == Role::Assistant && !message.tool_calls.is_empty() {
            for call in &message.tool_calls {
                out.push_str("assistant called ");
                out.push_str(&call.name);
                out.push('(');
                out.push_str(&cut(&call.arguments, RESULT_LIMIT));
                out.push_str(")\n");
            }
        }
        let limit = if message.role == Role::Tool {
            RESULT_LIMIT
        } else {
            TEXT_LIMIT
        };
        let text = cut(&message.text, limit);
        if !text.trim().is_empty() {
            out.push_str(role_name(message.role));
            out.push_str(": ");
            out.push_str(&text);
            out.push('\n');
        }
        for attachment in &message.attachments {
            out.push_str("[attachment: ");
            out.push_str(&attachment.name);
            out.push_str("]\n");
        }
    }
    out
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

fn cut(text: &str, limit: usize) -> String {
    let mut cut: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        cut.push('…');
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation long enough that its estimate crosses 80% of 128K.
    fn long_conversation(turns: usize, chars: usize) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        for index in 0..turns {
            messages.push(ChatMessage::user(format!(
                "turn {index}: {}",
                "u".repeat(chars)
            )));
            messages.push(ChatMessage::assistant(format!(
                "answer {index}: {}",
                "a".repeat(chars)
            )));
        }
        messages
    }

    #[test]
    fn a_long_conversation_crosses_the_trigger_and_fits_after_compacting() {
        let window = 131_072;
        let mut messages = long_conversation(30, 8_000);
        let standing = 12_000;
        let before = estimate_tokens(&messages, standing);
        assert!(
            before * 100 >= window * TRIGGER_PERCENT,
            "the sample must trigger: {before} tokens"
        );

        let plan = plan(&messages, None).expect("there are turns to compact");
        assert_eq!(plan.split, 52, "the last four user turns stay");
        assert_eq!(plan.count, 52);
        assert!(plan.prompt.contains("turn 0"));
        assert!(
            !plan.prompt.contains("turn 29"),
            "the kept tail is not summarized"
        );
        messages.drain(..plan.split);

        let after = estimate_tokens(&messages, standing);
        assert!(
            after * 100 <= window * TARGET_PERCENT,
            "the kept tail must fit the target: {after} tokens"
        );
        assert_eq!(user_turns(&messages), KEEP_ROUNDS);
        assert!(messages[0].text.starts_with("turn 26:"));
    }

    #[test]
    fn a_short_conversation_has_nothing_to_compact() {
        let messages = long_conversation(KEEP_ROUNDS, 100);
        assert!(!can_plan(&messages));
        assert_eq!(plan(&messages, None), None);
    }

    #[test]
    fn a_second_compaction_carries_the_first_summary() {
        let messages = long_conversation(12, 1_000);
        let first = plan(&messages, None).expect("the first plan");
        let second = plan(&messages, Some("the earlier work")).expect("the second plan");
        assert_eq!(first.split, second.split);
        assert!(second.prompt.contains("the earlier work"));
        assert_eq!(
            second
                .prompt
                .matches("[Summary of the even earlier conversation]")
                .count(),
            1
        );
    }

    #[test]
    fn a_failed_summary_still_trims_to_the_target() {
        let window = 131_072;
        let standing = 12_000;
        let mut messages = long_conversation(30, 8_000);
        let removed = trim_to_target(&mut messages, standing, window);
        assert!(removed > 0);
        assert!(
            estimate_tokens(&messages, standing) * 100 <= window * TARGET_PERCENT,
            "the trim brings the estimate down"
        );
        assert!(
            messages
                .iter()
                .rev()
                .any(|message| message.role == Role::User)
        );
    }

    #[test]
    fn the_summary_request_is_text_only() {
        let selection = ModelSelection {
            protocol: super::super::settings::ProviderProtocol::OpenAi,
            base_url: "https://example.test/v1".into(),
            full_url: false,
            model: "m".into(),
            keychain_id: "key".into(),
            max_tokens: 8_192,
            context_window: 128_000,
            tool_rounds: 8,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking_param: String::new(),
            thinking_effort: String::new(),
            stop: Vec::new(),
            extra: String::new(),
            instruction: String::new(),
            supports_images: true,
            headers: Vec::new(),
            trial: true,
        };
        let request = request(&selection, "compact this".into(), SUMMARY_TIMEOUT, None);
        assert!(request.plain);
        assert!(request.trial);
        assert_eq!(request.max_tokens, SUMMARY_TOKENS);
        assert_eq!(request.messages.len(), 1);
    }
}
