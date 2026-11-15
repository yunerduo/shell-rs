//! The model call, off the UI thread. A worker reads the key from the
//! keychain, sends the request, and forwards text deltas over a channel;
//! the panel polls that channel and never touches a socket.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::Duration;

use crate::secrets::KeychainSecretStore;

use super::chat::{ChatMessage, ToolCall};
use super::protocol::{
    self, ChatError, ParsedEvent, SseChunk, SseDecoder, ToolCallDelta, ToolTerminal,
};
use super::settings::{ProviderProtocol, account};

/// Everything one answer needs. The key is not here: only the name of the
/// keychain entry holding it.
#[derive(Clone, Debug)]
pub struct StreamRequest {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    pub model: String,
    pub keychain_id: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub timeout: Duration,
    /// The terminals the tool prompt may name.
    pub terminals: Vec<ToolTerminal>,
}

/// What the worker reports. `Delta` is a fragment of the answer as it
/// arrives; `ToolCalls` is what the turn asked to run, sent once when the
/// turn ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    Delta(String),
    ToolCalls(Vec<ToolCall>),
    Done,
    Failed(ChatError),
}

/// The panel's handle on a running answer. Dropping it is a cancellation:
/// the worker's next send fails and it exits.
pub struct StreamHandle {
    pub rx: Receiver<StreamEvent>,
    cancel: Arc<AtomicBool>,
}

impl StreamHandle {
    /// Ask the worker to stop, and wait for nothing: the answer may already
    /// be on its way out.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Start an answer on a worker thread. The worker owns the key until the
/// request is away, and never sends it back.
pub fn start(request: StreamRequest) -> StreamHandle {
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let worker_cancel = Arc::clone(&cancel);
    std::thread::spawn(move || {
        let outcome = run(&request, &tx, &worker_cancel);
        match outcome {
            Ok(()) => {
                let _ = tx.send(StreamEvent::Done);
            }
            // A cancelled answer is dropped without a word; the panel has
            // already stopped listening.
            Err(ChatError::Cancelled) => {}
            Err(error) => {
                let _ = tx.send(StreamEvent::Failed(error));
            }
        }
    });
    StreamHandle { rx, cancel }
}

/// Read one event, or say the channel is empty for now.
pub fn next_event(rx: &Receiver<StreamEvent>) -> Option<StreamEvent> {
    match rx.try_recv() {
        Ok(event) => Some(event),
        Err(TryRecvError::Empty) => None,
        Err(TryRecvError::Disconnected) => Some(StreamEvent::Done),
    }
}

fn run(
    request: &StreamRequest,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &AtomicBool,
) -> Result<(), ChatError> {
    let key = KeychainSecretStore::read_account(&account(&request.keychain_id))
        .map_err(|error| ChatError::Keychain(short(&error.to_string())))?
        .ok_or(ChatError::KeyMissing)?;
    run_with_key(request, &key, tx, cancel)
}

/// The call itself, with the key already in hand. Kept apart from the
/// keychain so a test can drive a real endpoint with a key from the
/// environment, without the key ever reaching a file.
pub(super) fn run_with_key(
    request: &StreamRequest,
    key: &str,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &AtomicBool,
) -> Result<(), ChatError> {
    let spec = protocol::request_spec(
        request.protocol,
        &request.base_url,
        &request.model,
        key,
        &request.messages,
        request.max_tokens,
        &request.terminals,
    );

    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(request.timeout)
        .build()
        .map_err(ChatError::from)?;

    let mut call = client
        .post(&spec.url)
        .header("Content-Type", "application/json");
    for (name, value) in &spec.headers {
        call = call.header(name, value);
    }
    let body = serde_json::to_vec(&spec.body)
        .map_err(|error| ChatError::Network(short(&error.to_string())))?;
    let response = call.body(body).send().map_err(ChatError::from)?;
    let status = response.status();
    if !status.is_success() {
        let mut body = String::new();
        // Enough to carry the provider's message, not a page of HTML.
        let _ = response.take(8192).read_to_string(&mut body);
        return Err(protocol::status_error(status.as_u16(), &body));
    }

    let mut response = response;
    let mut decoder = SseDecoder::default();
    let mut calls = ToolAccumulator::default();
    let outcome = stream_into(request, &mut response, tx, cancel, &mut decoder, &mut calls);
    // The turn may end either way; a half-merged call without an id is
    // dropped rather than run.
    if outcome.is_ok() && !calls.is_empty() {
        let _ = tx.send(StreamEvent::ToolCalls(calls.finish()));
    }
    outcome
}

/// Read the streamed body and forward text as it comes, keeping the tool
/// call fragments whole until the turn ends.
fn stream_into(
    request: &StreamRequest,
    response: &mut reqwest::blocking::Response,
    tx: &mpsc::Sender<StreamEvent>,
    cancel: &AtomicBool,
    decoder: &mut SseDecoder,
    calls: &mut ToolAccumulator,
) -> Result<(), ChatError> {
    let mut chunk = [0u8; 8192];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ChatError::Cancelled);
        }
        let read = response.read(&mut chunk).map_err(ChatError::from)?;
        if read == 0 {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&chunk[..read]);
        for event in decoder.push(&text) {
            match event {
                SseChunk::Done => return Ok(()),
                SseChunk::Data(data) => match protocol::parse_event(request.protocol, &data)? {
                    ParsedEvent::Text(text) if !text.is_empty() => {
                        if tx.send(StreamEvent::Delta(text)).is_err() {
                            return Err(ChatError::Cancelled);
                        }
                    }
                    ParsedEvent::ToolCalls(deltas) => {
                        for delta in deltas {
                            calls.merge(delta);
                        }
                    }
                    ParsedEvent::Done => return Ok(()),
                    ParsedEvent::Text(_) | ParsedEvent::Ignore => {}
                },
            }
        }
    }
}

/// The tool calls of one turn, put back together from the chunks that
/// carried them.
#[derive(Default)]
struct ToolAccumulator {
    ids: Vec<Option<String>>,
    names: Vec<Option<String>>,
    arguments: Vec<String>,
}

impl ToolAccumulator {
    fn merge(&mut self, delta: ToolCallDelta) {
        while self.ids.len() <= delta.index {
            self.ids.push(None);
            self.names.push(None);
            self.arguments.push(String::new());
        }
        if let Some(id) = delta.id {
            self.ids[delta.index] = Some(id);
        }
        if let Some(name) = delta.name {
            self.names[delta.index] = Some(name);
        }
        self.arguments[delta.index].push_str(&delta.arguments);
    }

    fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    fn finish(self) -> Vec<ToolCall> {
        self.ids
            .into_iter()
            .zip(self.names)
            .zip(self.arguments)
            .filter_map(|((id, name), arguments)| {
                Some(ToolCall {
                    id: id?,
                    name: name?,
                    arguments,
                })
            })
            .collect()
    }
}

/// A keychain error's words, cut so a long OS message cannot fill the
/// panel. The key itself is never part of these errors.
fn short(reason: &str) -> String {
    reason.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_handle_looks_like_the_end() {
        let (tx, rx) = mpsc::channel();
        drop(tx);
        assert_eq!(next_event(&rx), Some(StreamEvent::Done));
    }

    #[test]
    fn an_empty_channel_is_not_the_end() {
        let (_tx, rx) = mpsc::channel();
        assert_eq!(next_event(&rx), None);
    }

    #[test]
    fn the_account_names_the_keychain_entry() {
        assert_eq!(account("assistant-openai"), "assistant:assistant-openai");
    }

    #[test]
    fn tool_call_chunks_merge_by_index_and_keep_the_order() {
        let mut calls = ToolAccumulator::default();
        calls.merge(ToolCallDelta {
            index: 1,
            id: Some("call-b".into()),
            name: Some("run_command".into()),
            arguments: String::new(),
        });
        calls.merge(ToolCallDelta {
            index: 0,
            id: Some("call-a".into()),
            name: Some("run_command".into()),
            arguments: r#"{"terminal":"#.into(),
        });
        calls.merge(ToolCallDelta {
            index: 0,
            id: None,
            name: None,
            arguments: r#""web","command":"df -h"}"#.into(),
        });
        let calls = calls.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call-a");
        assert_eq!(
            calls[0].arguments,
            r#"{"terminal":"web","command":"df -h"}"#
        );
        assert_eq!(calls[1].id, "call-b");
        assert_eq!(calls[1].arguments, "");
    }

    #[test]
    fn a_fragment_without_a_name_is_dropped() {
        let mut calls = ToolAccumulator::default();
        calls.merge(ToolCallDelta {
            index: 0,
            id: Some("call-a".into()),
            name: None,
            arguments: "{}".into(),
        });
        assert!(calls.finish().is_empty());
    }

    /// The one check that talks to a real service. It is ignored by default
    /// and takes the key from the environment, so no file holds one:
    ///
    /// ```text
    /// SHELLRS_TEST_MODEL_KEY=... SHELLRS_TEST_MODEL_BASE=... \
    /// SHELLRS_TEST_MODEL=... cargo test --lib a_real_stream_answers -- --ignored
    /// ```
    #[test]
    #[ignore = "needs a model endpoint and SHELLRS_TEST_MODEL_KEY"]
    fn a_real_stream_answers() {
        let Ok(key) = std::env::var("SHELLRS_TEST_MODEL_KEY") else {
            return;
        };
        let base_url = std::env::var("SHELLRS_TEST_MODEL_BASE")
            .unwrap_or_else(|_| "http://127.0.0.1:18045/v1".to_string());
        let model = std::env::var("SHELLRS_TEST_MODEL")
            .unwrap_or_else(|_| "gemini-3.5-flash-lite".to_string());
        let request = StreamRequest {
            protocol: ProviderProtocol::OpenAi,
            base_url,
            model,
            keychain_id: "unused-in-this-test".into(),
            messages: vec![ChatMessage::user("Reply with the single word: pong")],
            max_tokens: 64,
            timeout: Duration::from_secs(60),
            terminals: Vec::new(),
        };
        let (tx, rx) = mpsc::channel();
        let cancel = AtomicBool::new(false);
        run_with_key(&request, &key, &tx, &cancel).expect("the call should stream");
        drop(tx);
        let mut answer = String::new();
        for event in rx.try_iter() {
            if let StreamEvent::Delta(text) = event {
                answer.push_str(&text);
            }
        }
        assert!(
            !answer.trim().is_empty(),
            "the model should answer with text"
        );
        println!("the model answered with {} characters", answer.len());
    }
}
