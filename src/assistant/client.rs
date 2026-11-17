//! The model call, off the UI thread. A worker reads the key from the
//! keychain, sends the request, and forwards text deltas over a channel;
//! the panel polls that channel and never touches a socket.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use zeroize::Zeroizing;

use crate::secrets::KeychainSecretStore;

use super::chat::{ChatMessage, ToolCall};
use super::protocol::{
    self, ChatError, ParsedEvent, SseChunk, SseDecoder, ToolCallDelta, ToolTerminal,
};
use super::settings::{ProviderProtocol, ProxyConfig, ProxyRoute, RequestHeader, account};

/// A proxy the answer goes through. The password stays in the keychain: the
/// worker reads it by account name and never sends it back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxySetting {
    /// The proxy's URL, scheme included.
    pub url: String,
    /// Empty when the proxy needs no account.
    pub username: String,
    /// The keychain account holding the password.
    pub password_account: String,
}

impl From<ProxyRoute> for ProxySetting {
    fn from(route: ProxyRoute) -> Self {
        Self {
            url: route.config.url().unwrap_or_default(),
            username: route.config.username.clone(),
            password_account: route.account,
        }
    }
}

/// Everything one answer needs. The key is not here: only the name of the
/// keychain entry holding it.
#[derive(Clone, Debug)]
pub struct StreamRequest {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    /// The address is the whole URL: no protocol path is appended.
    pub full_url: bool,
    pub model: String,
    pub keychain_id: String,
    pub messages: Vec<ChatMessage>,
    pub max_tokens: u32,
    pub timeout: Duration,
    /// The terminals the tool prompt may name.
    pub terminals: Vec<ToolTerminal>,
    /// This answer belongs to a scheduled task: the tools are the read-only
    /// ones and a proposal that cannot run.
    pub scheduled: bool,
    /// Headers the provider's settings add; a name the protocol already
    /// sets keeps the protocol's value.
    pub headers: Vec<RequestHeader>,
    /// The proxy in force, if any. None leaves the system's own proxy in
    /// charge, as before.
    pub proxy: Option<ProxySetting>,
    /// From the model profile; `None` leaves the service's default.
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    /// The body field carrying the thinking strength, and its value; both
    /// empty leaves the service's default.
    pub thinking_param: String,
    pub thinking_effort: String,
    /// Sequences that end the answer.
    pub stop: Vec<String>,
    /// A JSON object merged into the request body.
    pub extra: String,
    /// Appended to the system prompt for this model.
    pub instruction: String,
    /// The built-in trial: the token is the account's, read from the
    /// keychain here, and the errors are the service's proxy codes.
    pub trial: bool,
}

/// What the worker reports. `Delta` is a fragment of the answer as it
/// arrives; `ToolCalls` is what the turn asked to run, sent once when the
/// turn ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamEvent {
    Delta(String),
    /// A fragment of the model's reasoning, shown in the conversation but
    /// not sent back with the history.
    Thinking(String),
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
    let key = if request.trial {
        super::trial::token()?
    } else {
        KeychainSecretStore::read_account(&account(&request.keychain_id))
            .map_err(|error| ChatError::Keychain(short(&error.to_string())))?
            .ok_or(ChatError::KeyMissing)?
    };
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
    let mut spec = if request.scheduled {
        protocol::scheduled_request_spec(
            request.protocol,
            &request.base_url,
            &request.model,
            key,
            &request.messages,
            request.max_tokens,
            &request.terminals,
        )
    } else {
        protocol::request_spec(
            request.protocol,
            &request.base_url,
            &request.model,
            key,
            &request.messages,
            request.max_tokens,
            &request.terminals,
        )
    };
    protocol::append_headers(&mut spec.headers, &request.headers);
    // A profile whose address is the whole URL skips the protocol's path.
    if request.full_url {
        spec.url = request.base_url.trim().trim_end_matches('/').to_string();
    }
    let thinking = if request.thinking_param.is_empty() || request.thinking_effort.is_empty() {
        None
    } else {
        Some((
            request.thinking_param.as_str(),
            request.thinking_effort.as_str(),
        ))
    };
    protocol::apply_tuning(
        &mut spec.body,
        request.protocol,
        &protocol::Tuning {
            temperature: request.temperature,
            top_p: request.top_p,
            top_k: request.top_k,
            thinking,
            stop: &request.stop,
            extra: &request.extra,
            instruction: &request.instruction,
        },
    );

    let client = build_client(request)?;

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
        return Err(if request.trial {
            super::trial::status_error(status.as_u16(), &body)
        } else {
            protocol::status_error(status.as_u16(), &body)
        });
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

/// The HTTP client one answer uses, with the proxy in force. The proxy's
/// password is read here, on the worker thread, and stays in the client.
fn build_client(request: &StreamRequest) -> Result<reqwest::blocking::Client, ChatError> {
    http_client(request.timeout, request.proxy.as_ref())
}

fn http_client(
    timeout: Duration,
    proxy: Option<&ProxySetting>,
) -> Result<reqwest::blocking::Client, ChatError> {
    let mut builder = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(timeout);
    if let Some(proxy) = proxy {
        let password = if proxy.username.is_empty() {
            None
        } else {
            KeychainSecretStore::read_account(&proxy.password_account)
                .map_err(|error| ChatError::Keychain(short(&error.to_string())))?
        };
        let mut configured = reqwest::Proxy::all(&proxy.url)?;
        if !proxy.username.is_empty() {
            configured = configured.basic_auth(
                &proxy.username,
                password.as_deref().map(String::as_str).unwrap_or(""),
            );
        }
        builder = builder.proxy(configured);
    }
    builder.build().map_err(ChatError::from)
}

/// One model test, for the settings. The key is not here: a typed key comes
/// with the call, otherwise the keychain entry named by `keychain_id`.
#[derive(Clone, Debug)]
pub struct ProbeRequest {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    /// The address is the whole URL: no protocol path is appended.
    pub full_url: bool,
    pub model: String,
    pub keychain_id: String,
    pub headers: Vec<RequestHeader>,
    pub proxy: Option<ProxySetting>,
    pub timeout: Duration,
    /// Send the picture probe too, after the plain one answered.
    pub images: bool,
}

/// What the service said to the test.
#[derive(Clone, Debug)]
pub struct ProbeOutcome {
    /// How long the plain answer took.
    pub elapsed: Duration,
    /// The picture probe, when one ran: `Ok` when the service took the
    /// picture, `Err` with the service's own reason when it refused.
    pub image: Option<Result<(), PictureRefusal>>,
}

/// A service that would not take the probe picture. The status is kept
/// apart from the mapped error because a rejected body and a broken service
/// both arrive as a refusal, and only the first says anything about vision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PictureRefusal {
    pub status: u16,
    pub error: ChatError,
}

/// Test one model: ask it a one-word question and, when `images` is set, ask
/// again with a small picture. Both requests are the same shape a real turn
/// uses, so a model that fails here fails in the conversation too. This
/// blocks; the settings call it on the background executor.
pub fn probe(request: &ProbeRequest, typed_key: Option<&str>) -> Result<ProbeOutcome, ChatError> {
    let typed = typed_key
        .filter(|key| !key.trim().is_empty())
        .map(|key| Zeroizing::new(key.to_string()));
    let key = match typed {
        Some(key) => key,
        None => KeychainSecretStore::read_account(&account(&request.keychain_id))
            .map_err(|error| ChatError::Keychain(short(&error.to_string())))?
            .ok_or(ChatError::KeyMissing)?,
    };
    let client = http_client(request.timeout, request.proxy.as_ref())?;
    let started = Instant::now();
    probe_once(&client, request, &key, None).map_err(|(_, error)| error)?;
    let elapsed = started.elapsed();
    let image = if request.images {
        super::attachment::probe_picture().map(|(mime, data)| {
            probe_once(&client, request, &key, Some((mime, data.as_str())))
                .map_err(|(status, error)| PictureRefusal { status, error })
        })
    } else {
        None
    };
    Ok(ProbeOutcome { elapsed, image })
}

/// One request of the test. A failure carries the status as well, because
/// only a status the service refused outright says anything about the body.
fn probe_once(
    client: &reqwest::blocking::Client,
    request: &ProbeRequest,
    key: &str,
    image: Option<(&str, &str)>,
) -> Result<(), (u16, ChatError)> {
    let mut spec = protocol::probe_spec(
        request.protocol,
        &request.base_url,
        &request.model,
        key,
        image,
    );
    protocol::append_headers(&mut spec.headers, &request.headers);
    if request.full_url {
        spec.url = request.base_url.trim().trim_end_matches('/').to_string();
    }
    let mut call = client
        .post(&spec.url)
        .header("Content-Type", "application/json");
    for (name, value) in &spec.headers {
        call = call.header(name, value);
    }
    let body = serde_json::to_vec(&spec.body)
        .map_err(|error| (0, ChatError::Network(short(&error.to_string()))))?;
    let response = call
        .body(body)
        .send()
        .map_err(|error| (0, ChatError::from(error)))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let mut body = String::new();
    let _ = response.take(8192).read_to_string(&mut body);
    Err((
        status.as_u16(),
        protocol::status_error(status.as_u16(), &body),
    ))
}

/// Why a proxy check failed. The reasons are kept apart so the settings can
/// name the one that happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyTestError {
    /// The address does not make a proxy URL.
    Address,
    /// The proxy or the service behind it could not be resolved.
    Dns,
    /// Nothing accepted the connection.
    Refused,
    /// The proxy rejected the account or password.
    Auth,
    Timeout,
    Other(String),
}

impl ProxyTestError {
    pub fn message(&self) -> gpui_kit::SharedString {
        use crate::i18n::t;
        match self {
            ProxyTestError::Address => t!("assistant.settings.proxy.test.address"),
            ProxyTestError::Dns => t!("assistant.settings.proxy.test.dns"),
            ProxyTestError::Refused => t!("assistant.settings.proxy.test.refused"),
            ProxyTestError::Auth => t!("assistant.settings.proxy.test.auth"),
            ProxyTestError::Timeout => t!("assistant.settings.proxy.test.timeout"),
            ProxyTestError::Other(reason) => {
                t!(
                    "assistant.settings.proxy.test.failed",
                    reason = reason.as_str()
                )
            }
        }
    }
}

/// Send one real request through the proxy. Any HTTP answer means the tunnel
/// worked; 407 means the proxy refused the credentials. `target` is the
/// service address the assistant would call.
pub fn test_proxy(
    config: &ProxyConfig,
    password: Option<&str>,
    target: &str,
) -> Result<u16, ProxyTestError> {
    let url = config.url().ok_or(ProxyTestError::Address)?;
    let mut proxy = reqwest::Proxy::all(&url)
        .map_err(|error| ProxyTestError::Other(short(&error.to_string())))?;
    if !config.username.is_empty() {
        proxy = proxy.basic_auth(&config.username, password.unwrap_or(""));
    }
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .proxy(proxy)
        .build()
        .map_err(|error| ProxyTestError::Other(short(&error.to_string())))?;
    let response = client
        .get(target)
        .header("User-Agent", "shellas-assistant-proxy-check")
        .send()
        .map_err(classify_proxy_error)?;
    let status = response.status().as_u16();
    if status == 407 {
        return Err(ProxyTestError::Auth);
    }
    Ok(status)
}

/// Name the failure from the error and everything under it: hyper and the
/// system say different words for the same thing.
fn classify_proxy_error(error: reqwest::Error) -> ProxyTestError {
    if error.is_timeout() {
        return ProxyTestError::Timeout;
    }
    let mut text = String::new();
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&error);
    while let Some(cause) = source {
        text.push_str(&cause.to_string());
        text.push(' ');
        source = cause.source();
    }
    let lower = text.to_ascii_lowercase();
    if lower.contains("dns")
        || lower.contains("failed to lookup")
        || lower.contains("name or service not known")
        || lower.contains("nodename nor servname")
        || lower.contains("name resolution")
        || lower.contains("no such host")
        || lower.contains("11001")
    {
        ProxyTestError::Dns
    } else if lower.contains("connection refused")
        || lower.contains("actively refused")
        || lower.contains("10061")
    {
        ProxyTestError::Refused
    } else if lower.contains("proxy authentication") {
        ProxyTestError::Auth
    } else {
        ProxyTestError::Other(short(text.trim()))
    }
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
                    ParsedEvent::Thinking(text) => {
                        if tx.send(StreamEvent::Thinking(text)).is_err() {
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

    /// A port with nothing behind it is its own reason, not a generic failure.
    #[test]
    fn a_closed_port_is_named_as_refused() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("the bound address").port()
        };
        let config = ProxyConfig {
            host: "127.0.0.1".into(),
            port,
            ..ProxyConfig::default()
        };
        assert_eq!(
            test_proxy(&config, None, "http://example.com/"),
            Err(ProxyTestError::Refused)
        );
    }

    /// A proxy that answers 407 is reachable, but it rejected us; that is not
    /// the same as being unreachable.
    #[test]
    fn a_proxy_that_asks_for_credentials_is_an_auth_failure() {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("the bound address").port();
        let server = std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\n\
                  Content-Length: 0\r\n\
                  \r\n",
            );
            let _ = stream.flush();
        });
        let config = ProxyConfig {
            host: "127.0.0.1".into(),
            port,
            ..ProxyConfig::default()
        };
        assert_eq!(
            test_proxy(&config, None, "http://example.com/"),
            Err(ProxyTestError::Auth)
        );
        server.join().expect("the fake proxy should finish");
    }

    /// The reserved top-level domain never resolves, so this is a DNS answer
    /// without leaving the machine.
    #[test]
    fn an_unknown_proxy_host_is_a_dns_failure() {
        let config = ProxyConfig {
            host: "no-such-proxy.invalid".into(),
            port: 8080,
            ..ProxyConfig::default()
        };
        assert_eq!(
            test_proxy(&config, None, "http://example.com/"),
            Err(ProxyTestError::Dns)
        );
    }

    /// The body of one HTTP request, read past its headers.
    fn read_probe_request(stream: &mut std::net::TcpStream) -> String {
        use std::io::{BufRead as _, BufReader, Read as _};

        let mut length = 0;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                length = value.trim().parse().unwrap_or(0);
            }
            line.clear();
        }
        let mut body = vec![0u8; length];
        let _ = reader.read_exact(&mut body);
        String::from_utf8_lossy(&body).to_string()
    }

    /// A service that answers the plain question and refuses the picture is
    /// exactly the model the settings must be able to describe.
    #[test]
    fn a_probe_reports_a_model_that_refuses_a_picture() {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("the bound address").port();
        let server = std::thread::spawn(move || {
            let answers = [
                ("200 OK", "{}"),
                (
                    "400 Bad Request",
                    r#"{"error":{"message":"this model takes text only"}}"#,
                ),
            ];
            for (index, (status, body)) in answers.iter().enumerate() {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let raw = read_probe_request(&mut stream);
                assert_eq!(raw.contains("image_url"), index == 1, "{raw}");
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.flush();
            }
        });
        let request = ProbeRequest {
            protocol: ProviderProtocol::OpenAi,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            full_url: false,
            model: "text-only".into(),
            keychain_id: String::new(),
            headers: Vec::new(),
            proxy: None,
            timeout: Duration::from_secs(10),
            images: true,
        };
        let outcome = probe(&request, Some("a-key")).expect("the plain question is answered");
        server.join().expect("the fake service should finish");
        let refusal = outcome
            .image
            .expect("the picture probe ran")
            .expect_err("the service refused the picture");
        assert_eq!(refusal.status, 400);
        assert!(refusal.error.message().contains("text only"));
    }

    /// A service that takes both answers is a vision model; the test says so
    /// instead of guessing from its name.
    #[test]
    fn a_probe_reports_a_model_that_takes_a_picture() {
        use std::io::Write as _;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("the bound address").port();
        let server = std::thread::spawn(move || {
            let mut bodies = Vec::new();
            for _ in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return bodies;
                };
                bodies.push(read_probe_request(&mut stream));
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                      Content-Length: 2\r\nConnection: close\r\n\r\n{}",
                );
                let _ = stream.flush();
            }
            bodies
        });
        let request = ProbeRequest {
            protocol: ProviderProtocol::OpenAi,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            full_url: false,
            model: "vision".into(),
            keychain_id: String::new(),
            headers: Vec::new(),
            proxy: None,
            timeout: Duration::from_secs(10),
            images: true,
        };
        let outcome = probe(&request, Some("a-key")).expect("both questions are answered");
        let bodies = server.join().expect("the fake service should finish");
        assert_eq!(outcome.image, Some(Ok(())));
        assert!(bodies[0].contains("pong"));
        assert!(bodies[1].contains("data:image/png;base64,"));
    }

    /// The one check that talks to a real service. It is ignored by default
    /// and takes the key from the environment, so no file holds one:
    ///
    /// ```text
    /// SHELLAS_TEST_MODEL_KEY=... SHELLAS_TEST_MODEL_BASE=... \
    /// SHELLAS_TEST_MODEL=... cargo test --lib a_real_stream_answers -- --ignored
    /// ```
    #[test]
    #[ignore = "needs a model endpoint and SHELLAS_TEST_MODEL_KEY"]
    fn a_real_stream_answers() {
        let Ok(key) = std::env::var("SHELLAS_TEST_MODEL_KEY") else {
            return;
        };
        let base_url = std::env::var("SHELLAS_TEST_MODEL_BASE")
            .unwrap_or_else(|_| "http://127.0.0.1:18045/v1".to_string());
        let model = std::env::var("SHELLAS_TEST_MODEL")
            .unwrap_or_else(|_| "gemini-3.5-flash-lite".to_string());
        let request = StreamRequest {
            protocol: ProviderProtocol::OpenAi,
            base_url,
            full_url: false,
            model,
            keychain_id: "unused-in-this-test".into(),
            messages: vec![ChatMessage::user("Reply with the single word: pong")],
            max_tokens: 64,
            timeout: Duration::from_secs(60),
            terminals: Vec::new(),
            scheduled: false,
            headers: Vec::new(),
            proxy: None,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking_param: String::new(),
            thinking_effort: String::new(),
            stop: Vec::new(),
            extra: String::new(),
            instruction: String::new(),
            trial: false,
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
