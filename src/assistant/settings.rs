//! 设置里的 `assistant` 段: which model service the assistant talks to and
//! how. The API key is never here: only the keychain entry's name is, and
//! the key itself lives in the system keychain.

use serde::{Deserialize, Serialize};

use crate::i18n::t;

/// The wire protocol a provider speaks. Both make the same conversation,
/// with different requests and stream events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderProtocol {
    #[default]
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "anthropic")]
    Anthropic,
}

impl ProviderProtocol {
    pub const ALL: [ProviderProtocol; 2] = [ProviderProtocol::OpenAi, ProviderProtocol::Anthropic];

    /// The name in the settings file and of the keychain entry's default.
    pub fn key(self) -> &'static str {
        match self {
            ProviderProtocol::OpenAi => "openai",
            ProviderProtocol::Anthropic => "anthropic",
        }
    }

    pub fn label(self) -> gpui_kit::SharedString {
        match self {
            ProviderProtocol::OpenAi => t!("assistant.settings.protocol.openai"),
            ProviderProtocol::Anthropic => t!("assistant.settings.protocol.anthropic"),
        }
    }

    fn default_base_url(self) -> &'static str {
        match self {
            ProviderProtocol::OpenAi => DEFAULT_OPENAI_BASE_URL,
            ProviderProtocol::Anthropic => DEFAULT_ANTHROPIC_BASE_URL,
        }
    }

    /// No provider invents a model: an empty field stays empty until the
    /// user names one or the trial answers.
    fn default_model(self) -> &'static str {
        ""
    }

    fn default_keychain_id(self) -> &'static str {
        match self {
            ProviderProtocol::OpenAi => "assistant-openai",
            ProviderProtocol::Anthropic => "assistant-anthropic",
        }
    }
}

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";

/// The keychain account that holds one provider's API key. The keychain's
/// service name is the app's; the account is the assistant's alone.
pub fn account(keychain_id: &str) -> String {
    format!("assistant:{keychain_id}")
}

/// The keychain account that holds one proxy's password. The scope is
/// `default` for the global proxy, or the protocol's key for its own.
pub fn proxy_account(scope: &str) -> String {
    format!("assistant:proxy:{scope}")
}

/// The proxy protocol the assistant talks to it with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyKind {
    #[default]
    #[serde(rename = "http")]
    Http,
    #[serde(rename = "https")]
    Https,
    #[serde(rename = "socks5")]
    Socks5,
}

impl ProxyKind {
    pub const ALL: [ProxyKind; 3] = [ProxyKind::Http, ProxyKind::Https, ProxyKind::Socks5];

    /// The scheme in the settings file and in the proxy's URL.
    pub fn key(self) -> &'static str {
        match self {
            ProxyKind::Http => "http",
            ProxyKind::Https => "https",
            ProxyKind::Socks5 => "socks5",
        }
    }

    fn scheme(self) -> &'static str {
        self.key()
    }

    fn default_port(self) -> u16 {
        match self {
            ProxyKind::Http => 8080,
            ProxyKind::Https => 443,
            ProxyKind::Socks5 => 1080,
        }
    }

    pub fn label(self) -> gpui_kit::SharedString {
        match self {
            ProxyKind::Http => t!("assistant.settings.proxy.kind.http"),
            ProxyKind::Https => t!("assistant.settings.proxy.kind.https"),
            ProxyKind::Socks5 => t!("assistant.settings.proxy.kind.socks5"),
        }
    }
}

/// One proxy's address. The password is not here: only whether one has been
/// saved in the keychain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub username: String,
    /// Whether a password sits in the keychain. Never the password itself.
    pub password_saved: bool,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            kind: ProxyKind::Http,
            host: String::new(),
            port: ProxyKind::Http.default_port(),
            username: String::new(),
            password_saved: false,
        }
    }
}

impl ProxyConfig {
    /// The address as reqwest takes it, or `None` when no host is set.
    pub fn url(&self) -> Option<String> {
        let host = self.host.trim();
        if host.is_empty() {
            return None;
        }
        // IPv6 literals need brackets in a URL; the settings keep them
        // either way, so a hand-edited file still works.
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_string()
        };
        Some(format!("{}://{host}:{}", self.kind.scheme(), self.port))
    }

    /// Accept what people paste: a full URL, a `host:port` pair or a bare
    /// host. The port from the text wins; a blank one falls back to the
    /// protocol's usual port.
    pub(crate) fn normalize(&mut self) {
        let mut text = self.host.trim();
        if let Some((_, rest)) = text.split_once("://") {
            text = rest;
        }
        let text = text
            .split(['/', '?', '#'])
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        // Someone may paste `user:password@host`; the fields carry those.
        let text = match text.rsplit_once('@') {
            Some((_, host)) => host.to_string(),
            None => text,
        };
        if let Some(rest) = text.strip_prefix('[') {
            if let Some((host, tail)) = rest.split_once(']') {
                if let Some(port) = tail
                    .trim()
                    .strip_prefix(':')
                    .and_then(|port| port.parse::<u16>().ok())
                    .filter(|port| *port > 0)
                {
                    self.port = port;
                }
                self.host = format!("[{}]", host.trim());
            } else {
                self.host = text;
            }
        } else if let Some((host, port)) = text.rsplit_once(':') {
            // `proxy.test:8080` is a host and a port; a bare IPv6 address
            // (`::1`) is all host and stays as it is.
            if !host.is_empty() && !host.contains(':') {
                if let Some(port) = port.parse::<u16>().ok().filter(|port| *port > 0) {
                    self.port = port;
                }
                self.host = host.to_string();
            } else {
                self.host = text;
            }
        } else {
            self.host = text;
        }
        if self.port == 0 {
            self.port = self.kind.default_port();
        }
        self.username = self.username.trim().chars().take(128).collect();
    }
}

/// Whether a provider follows the global proxy, has none, or has its own.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyChoice {
    #[default]
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "off")]
    Off,
    #[serde(rename = "custom")]
    Custom,
}

impl ProxyChoice {
    pub const ALL: [ProxyChoice; 3] = [ProxyChoice::Default, ProxyChoice::Off, ProxyChoice::Custom];

    pub fn label(self) -> gpui_kit::SharedString {
        match self {
            ProxyChoice::Default => t!("assistant.settings.proxy.choice.default"),
            ProxyChoice::Off => t!("assistant.settings.proxy.choice.off"),
            ProxyChoice::Custom => t!("assistant.settings.proxy.choice.custom"),
        }
    }
}

/// The proxy a request actually goes through, and the keychain account its
/// password is read from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyRoute {
    pub config: ProxyConfig,
    pub account: String,
}

/// One extra HTTP header. Most services need none; a gateway may want a
/// tenant or an organization header. A header the protocol sets itself
/// (the key's, the content type, ...) is never replaced by one of these.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestHeader {
    pub name: String,
    pub value: String,
}

/// One provider's connection. No field holds the key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistantProvider {
    pub base_url: String,
    pub model: String,
    /// The keychain entry's name, so the key can be replaced without the
    /// settings file ever knowing it.
    pub keychain_id: String,
    /// The most tokens an answer may take.
    pub max_tokens: u32,
    /// Headers added after the protocol's own.
    pub headers: Vec<RequestHeader>,
    /// Whether this provider follows the global proxy or has its own.
    pub proxy_mode: ProxyChoice,
    /// Used when `proxy_mode` is `custom`.
    pub proxy: ProxyConfig,
}

impl Default for AssistantProvider {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            model: String::new(),
            keychain_id: "assistant-openai".to_string(),
            max_tokens: 2048,
            headers: Vec::new(),
            proxy_mode: ProxyChoice::Default,
            proxy: ProxyConfig::default(),
        }
    }
}

impl AssistantProvider {
    fn for_protocol(protocol: ProviderProtocol) -> Self {
        Self {
            base_url: protocol.default_base_url().to_string(),
            model: protocol.default_model().to_string(),
            keychain_id: protocol.default_keychain_id().to_string(),
            max_tokens: 2048,
            headers: Vec::new(),
            proxy_mode: ProxyChoice::Default,
            proxy: ProxyConfig::default(),
        }
    }

    fn normalize(&mut self, protocol: ProviderProtocol) {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        if self.base_url.is_empty() {
            self.base_url = protocol.default_base_url().to_string();
        }
        self.model = self.model.trim().to_string();
        self.keychain_id = self
            .keychain_id
            .trim()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .take(64)
            .collect();
        if self.keychain_id.is_empty() {
            self.keychain_id = protocol.default_keychain_id().to_string();
        }
        self.max_tokens = self.max_tokens.clamp(1, 8192);
        self.headers = clean_headers(&self.headers);
        self.proxy.normalize();
    }
}

/// The most headers one provider may add, and the longest value.
pub const HEADER_LIMIT: usize = 8;
const HEADER_VALUE_LIMIT: usize = 1024;

/// Headers the protocol writes itself. A user entry with one of these names
/// is dropped rather than allowed to break the call or replace the key.
const RESERVED_HEADERS: [&str; 7] = [
    "authorization",
    "x-api-key",
    "anthropic-version",
    "content-type",
    "content-length",
    "host",
    "accept",
];

/// One header from what the user typed, or `None` when it could not be
/// sent as it stands.
pub fn clean_header(name: &str, value: &str) -> Option<RequestHeader> {
    clean_headers(&[RequestHeader {
        name: name.to_string(),
        value: value.to_string(),
    }])
    .into_iter()
    .next()
}

/// Keep the headers that are well formed: a token name, a value without a
/// line break, no reserved name, no repeat, up to the limit.
fn clean_headers(headers: &[RequestHeader]) -> Vec<RequestHeader> {
    let mut kept: Vec<RequestHeader> = Vec::new();
    for header in headers {
        let name = header.name.trim().to_string();
        if name.is_empty() || !name.bytes().all(is_token_char) {
            continue;
        }
        let lower = name.to_ascii_lowercase();
        if RESERVED_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        if header.value.contains(['\r', '\n']) {
            continue;
        }
        let value = header.value.trim().to_string();
        if value.is_empty() || value.chars().count() > HEADER_VALUE_LIMIT {
            continue;
        }
        if kept
            .iter()
            .any(|kept| kept.name.eq_ignore_ascii_case(&name))
        {
            continue;
        }
        kept.push(RequestHeader { name, value });
        if kept.len() == HEADER_LIMIT {
            break;
        }
    }
    kept
}

/// The characters an HTTP header name may hold.
fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// One model the conversation may pick. The provider it names says where the
/// request goes and which key signs it; the profile says what to ask for and
/// how.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelProfile {
    /// `protocol:model`, what `active_model` stores.
    pub id: String,
    /// What the model menu calls it; the model's name when blank.
    pub label: String,
    pub protocol: ProviderProtocol,
    /// The model name the service knows.
    pub model: String,
    /// The endpoint this model talks to; blank means its protocol's
    /// provider address.
    pub base_url: String,
    /// The address is already the whole URL: no protocol path is appended.
    pub full_url: bool,
    /// The keychain entry holding this model's key; blank means the
    /// protocol's provider entry.
    pub keychain_id: String,
    /// Appended to the system prompt for this model only.
    pub instruction: String,
    /// The model takes pictures. Off keeps pasted or dropped pictures out
    /// of the composer, so a text-only model is never handed one. The test
    /// button in the form settles what the service actually does.
    pub supports_images: bool,
    /// The most tokens an answer may take; 0 means the provider's.
    pub max_tokens: u32,
    /// How many tokens the model's context window holds; 0 means the
    /// default. Only the usage display reads it.
    pub context_window: u32,
    /// The most tool-call turns one answer may take; 0 means the default.
    pub tool_rounds: u32,
    /// `None` leaves the service's default.
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    /// The body field carrying the model's thinking strength, dotted for a
    /// nested field (`reasoning.effort`). Blank sends nothing.
    pub thinking_param: String,
    /// The value written at `thinking_param`; blank sends nothing.
    pub thinking_effort: String,
    /// Sequences that end an answer.
    pub stop: Vec<String>,
    /// A JSON object merged into the request body, after the fields above.
    pub extra: String,
}

/// The most models someone may configure, and the most stop sequences one
/// model may name.
pub const MODEL_LIMIT: usize = 20;

/// How many tokens a model's context window holds when the profile does not
/// say. Most current models are at least this large.
pub const DEFAULT_CONTEXT_WINDOW: u32 = 128_000;

/// The largest window a profile may declare.
pub const CONTEXT_WINDOW_MAX: u32 = 2_000_000;

/// The most tokens one answer may ask for.
pub const OUTPUT_TOKENS_MAX: u32 = 131_072;

/// The tool-call turns one answer gets when the profile does not say.
pub const DEFAULT_TOOL_ROUNDS: u32 = 8;

/// The most tool-call turns a profile may ask for.
pub const TOOL_ROUNDS_MAX: u32 = 500;
const STOP_LIMIT: usize = 8;
const STOP_LENGTH_LIMIT: usize = 64;
const INSTRUCTION_LIMIT: usize = 2000;
const EXTRA_LIMIT: usize = 2000;
const THINKING_PARAM_LIMIT: usize = 80;
const THINKING_EFFORT_LIMIT: usize = 40;

impl Default for ModelProfile {
    fn default() -> Self {
        Self {
            id: String::new(),
            label: String::new(),
            protocol: ProviderProtocol::OpenAi,
            model: String::new(),
            base_url: String::new(),
            full_url: false,
            keychain_id: String::new(),
            instruction: String::new(),
            // Most models someone configures by hand are vision models, and
            // a service that refuses pictures says so when one arrives.
            supports_images: true,
            max_tokens: 0,
            context_window: 0,
            tool_rounds: 0,
            temperature: None,
            top_p: None,
            top_k: None,
            thinking_param: String::new(),
            thinking_effort: String::new(),
            stop: Vec::new(),
            extra: String::new(),
        }
    }
}

impl ModelProfile {
    /// The id a profile of `protocol` and `model` carries.
    pub fn id_of(protocol: ProviderProtocol, model: &str) -> String {
        format!("{}:{}", protocol.key(), model.trim())
    }

    /// What the menu shows.
    pub fn display_name(&self) -> &str {
        if self.label.trim().is_empty() {
            &self.model
        } else {
            &self.label
        }
    }

    /// Trim what can be trimmed, drop what cannot be sent, and put the
    /// numbers back in range.
    pub fn normalize(&mut self) {
        self.model = self.model.trim().chars().take(128).collect();
        self.label = self.label.trim().chars().take(64).collect();
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        self.base_url = self.base_url.chars().take(512).collect();
        self.keychain_id = self.keychain_id.trim().chars().take(120).collect();
        self.instruction = self
            .instruction
            .trim()
            .chars()
            .take(INSTRUCTION_LIMIT)
            .collect();
        self.extra = clean_extra(&self.extra);
        self.max_tokens = self.max_tokens.clamp(0, OUTPUT_TOKENS_MAX);
        self.context_window = self.context_window.clamp(0, CONTEXT_WINDOW_MAX);
        self.tool_rounds = self.tool_rounds.min(TOOL_ROUNDS_MAX);
        self.temperature = self.temperature.filter(|value| (0.0..=2.0).contains(value));
        self.top_p = self.top_p.filter(|value| (0.0..=1.0).contains(value));
        self.top_k = self.top_k.filter(|value| (1..=1000).contains(value));
        self.thinking_param = clean_thinking_param(&self.thinking_param);
        self.thinking_effort = self
            .thinking_effort
            .trim()
            .chars()
            .take(THINKING_EFFORT_LIMIT)
            .collect();
        self.stop = clean_stop(&self.stop);
        self.id = Self::id_of(self.protocol, &self.model);
    }
}

/// A body field name for the thinking strength: dot-separated parts of ASCII
/// letters, digits and underscores. Anything else is dropped, since it could
/// rewrite a field the protocol sets itself.
fn clean_thinking_param(text: &str) -> String {
    let text = text.trim();
    let valid = !text.is_empty()
        && text.chars().count() <= THINKING_PARAM_LIMIT
        && text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|letter| letter.is_ascii_alphanumeric() || letter == '_')
        });
    if valid {
        text.to_string()
    } else {
        String::new()
    }
}

/// `stop` without blank entries, line breaks, repeats or entries too long to
/// send, up to [`STOP_LIMIT`].
fn clean_stop(entries: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.trim();
        if entry.is_empty()
            || entry.contains(['\r', '\n'])
            || entry.chars().count() > STOP_LENGTH_LIMIT
        {
            continue;
        }
        if kept.iter().any(|kept| kept == entry) {
            continue;
        }
        kept.push(entry.to_string());
        if kept.len() == STOP_LIMIT {
            break;
        }
    }
    kept
}

/// The extra body fields when they parse as a JSON object, trimmed; empty
/// otherwise. Anything else would be sent as a broken body.
fn clean_extra(extra: &str) -> String {
    let extra = extra.trim();
    if extra.chars().count() > EXTRA_LIMIT {
        return String::new();
    }
    match serde_json::from_str::<serde_json::Value>(extra) {
        Ok(value) if value.is_object() => extra.to_string(),
        _ => String::new(),
    }
}

fn dedupe_roles(roles: &mut Vec<super::roles::Role>) {
    let mut seen: Vec<String> = Vec::new();
    roles.retain(|role| {
        if seen.contains(&role.id) {
            return false;
        }
        seen.push(role.id.clone());
        true
    });
}

fn dedupe_skills(skills: &mut Vec<super::roles::Skill>) {
    let mut seen: Vec<String> = Vec::new();
    skills.retain(|skill| {
        if seen.contains(&skill.id) {
            return false;
        }
        seen.push(skill.id.clone());
        true
    });
}

/// What the next request uses: the picked model when there is one, the
/// provider's own model otherwise.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSelection {
    pub protocol: ProviderProtocol,
    pub base_url: String,
    /// The address is the whole URL; no protocol path is appended.
    pub full_url: bool,
    pub model: String,
    pub keychain_id: String,
    pub max_tokens: u32,
    pub context_window: u32,
    pub tool_rounds: u32,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub thinking_param: String,
    pub thinking_effort: String,
    pub stop: Vec<String>,
    pub extra: String,
    pub instruction: String,
    /// Whether the model takes pictures; the composer checks it before it
    /// keeps one.
    pub supports_images: bool,
    pub headers: Vec<RequestHeader>,
    /// The built-in trial: the request uses the account token instead of a
    /// keychain API key, and the endpoint is the service's proxy.
    pub trial: bool,
}

/// `settings.json`'s `assistant` section. One provider of each protocol is
/// configured; `protocol` picks the one that talks unless a model profile
/// does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistantSettings {
    pub protocol: ProviderProtocol,
    pub openai: AssistantProvider,
    pub anthropic: AssistantProvider,
    /// The models the person configured; the menu lists them.
    pub models: Vec<ModelProfile>,
    /// The id of the model picked; empty means the provider's own.
    pub active_model: String,
    /// Whether the built-in trial is the saved choice. Missing means the
    /// user never chose: a fresh setup starts on the trial, while an older
    /// file that already named a model keeps it.
    #[serde(default)]
    pub trial_selected: Option<bool>,
    /// Roles people wrote. Built-in roles come from `roles`.
    pub roles: Vec<super::roles::Role>,
    /// Skills people added, each on or off.
    pub skills: Vec<super::roles::Skill>,
    /// Domain packs imported, with their roles and skills.
    pub packs: Vec<super::roles::DomainPack>,
    /// How long one answer may take, streaming included.
    pub timeout_seconds: u64,
    /// Commands refused whatever the level. Entries may use `*` in an
    /// argument position.
    pub user_blacklist: Vec<String>,
    /// Read-only commands allowed without a card. Only plain, single
    /// commands: no metacharacter, no wildcard.
    pub user_whitelist: Vec<String>,
    /// Ask the model about a command the rules could not place. Off by
    /// default; it can only make a judgment harsher.
    pub classifier_enabled: bool,
    /// The model the classifier asks; empty means the active provider's.
    pub classifier_model: String,
    /// Whether the global proxy below is used by providers that follow it.
    pub proxy_enabled: bool,
    /// The proxy a provider uses when it follows the default.
    pub proxy: ProxyConfig,
}

/// The most entries each list keeps. Enough for a person, not a policy
/// file: the panel shows them all.
const LIST_LIMIT: usize = 50;

impl Default for AssistantSettings {
    fn default() -> Self {
        Self {
            protocol: ProviderProtocol::OpenAi,
            openai: AssistantProvider::for_protocol(ProviderProtocol::OpenAi),
            anthropic: AssistantProvider::for_protocol(ProviderProtocol::Anthropic),
            models: Vec::new(),
            active_model: String::new(),
            trial_selected: None,
            roles: Vec::new(),
            skills: Vec::new(),
            packs: Vec::new(),
            timeout_seconds: 120,
            user_blacklist: Vec::new(),
            user_whitelist: Vec::new(),
            classifier_enabled: false,
            classifier_model: String::new(),
            proxy_enabled: false,
            proxy: ProxyConfig::default(),
        }
    }
}

impl AssistantSettings {
    pub fn provider(&self, protocol: ProviderProtocol) -> &AssistantProvider {
        match protocol {
            ProviderProtocol::OpenAi => &self.openai,
            ProviderProtocol::Anthropic => &self.anthropic,
        }
    }

    pub fn provider_mut(&mut self, protocol: ProviderProtocol) -> &mut AssistantProvider {
        match protocol {
            ProviderProtocol::OpenAi => &mut self.openai,
            ProviderProtocol::Anthropic => &mut self.anthropic,
        }
    }

    pub fn active(&self) -> &AssistantProvider {
        self.provider(self.protocol)
    }

    pub fn active_mut(&mut self) -> &mut AssistantProvider {
        self.provider_mut(self.protocol)
    }

    /// The configured model with `id`, if there is one.
    pub fn model_profile(&self, id: &str) -> Option<&ModelProfile> {
        self.models.iter().find(|profile| profile.id == id)
    }

    /// The model the conversation picked.
    pub fn active_profile(&self) -> Option<&ModelProfile> {
        if self.active_model.is_empty() {
            return None;
        }
        self.model_profile(&self.active_model)
    }

    /// Whether the built-in trial is the saved choice. A file that never
    /// recorded a choice follows its model: nothing picked means the trial,
    /// which is what a fresh sign-in should show.
    pub fn prefers_trial(&self) -> bool {
        self.trial_selected.unwrap_or(self.active_model.is_empty())
    }

    /// Everything the next request needs, from the picked model when there
    /// is one and from the active provider otherwise.
    pub fn selection(&self) -> ModelSelection {
        match self.active_profile() {
            Some(profile) => {
                let provider = self.provider(profile.protocol);
                ModelSelection {
                    protocol: profile.protocol,
                    base_url: if profile.base_url.is_empty() {
                        provider.base_url.clone()
                    } else {
                        profile.base_url.clone()
                    },
                    full_url: profile.full_url,
                    model: profile.model.clone(),
                    keychain_id: if profile.keychain_id.is_empty() {
                        provider.keychain_id.clone()
                    } else {
                        profile.keychain_id.clone()
                    },
                    max_tokens: if profile.max_tokens == 0 {
                        provider.max_tokens
                    } else {
                        profile.max_tokens
                    },
                    context_window: if profile.context_window == 0 {
                        DEFAULT_CONTEXT_WINDOW
                    } else {
                        profile.context_window
                    },
                    tool_rounds: if profile.tool_rounds == 0 {
                        DEFAULT_TOOL_ROUNDS
                    } else {
                        profile.tool_rounds
                    },
                    temperature: profile.temperature,
                    top_p: profile.top_p,
                    top_k: profile.top_k,
                    thinking_param: profile.thinking_param.clone(),
                    thinking_effort: profile.thinking_effort.clone(),
                    stop: profile.stop.clone(),
                    extra: profile.extra.clone(),
                    instruction: profile.instruction.clone(),
                    supports_images: profile.supports_images,
                    headers: provider.headers.clone(),
                    trial: false,
                }
            }
            None => {
                let provider = self.active();
                ModelSelection {
                    protocol: self.protocol,
                    base_url: provider.base_url.clone(),
                    full_url: false,
                    model: provider.model.clone(),
                    keychain_id: provider.keychain_id.clone(),
                    max_tokens: provider.max_tokens,
                    context_window: DEFAULT_CONTEXT_WINDOW,
                    tool_rounds: DEFAULT_TOOL_ROUNDS,
                    temperature: None,
                    top_p: None,
                    top_k: None,
                    thinking_param: String::new(),
                    thinking_effort: String::new(),
                    stop: Vec::new(),
                    extra: String::new(),
                    instruction: String::new(),
                    supports_images: true,
                    headers: provider.headers.clone(),
                    trial: false,
                }
            }
        }
    }

    /// How many tokens the active model's context holds: the profile's
    /// number, or the default when it does not say.
    pub fn context_window(&self) -> u32 {
        self.active_profile()
            .map(|profile| profile.context_window)
            .filter(|window| *window > 0)
            .unwrap_or(DEFAULT_CONTEXT_WINDOW)
    }

    /// The account the active provider's key lives under.
    pub fn account(&self) -> String {
        account(&self.selection().keychain_id)
    }

    /// The proxy one provider's requests go through, if any. A provider with
    /// its own proxy uses it; otherwise the global proxy decides, provided it
    /// was switched on and has an address.
    pub fn proxy_route(&self, protocol: ProviderProtocol) -> Option<ProxyRoute> {
        let provider = self.provider(protocol);
        let (config, scope) = match provider.proxy_mode {
            ProxyChoice::Off => return None,
            ProxyChoice::Custom => (&provider.proxy, protocol.key()),
            ProxyChoice::Default => {
                if !self.proxy_enabled {
                    return None;
                }
                (&self.proxy, "default")
            }
        };
        config.url()?;
        Some(ProxyRoute {
            config: config.clone(),
            account: proxy_account(scope),
        })
    }

    /// Every number in range, every blank field back to its default, so a
    /// hand-edited file cannot hand a request an empty address. The lists
    /// lose entries the policy would not accept, and duplicates.
    pub fn normalize(&mut self) {
        self.openai.normalize(ProviderProtocol::OpenAi);
        self.anthropic.normalize(ProviderProtocol::Anthropic);
        for profile in &mut self.models {
            profile.normalize();
        }
        // A model the service would refuse, and the same model twice, are
        // not worth keeping.
        let mut kept: Vec<ModelProfile> = Vec::new();
        for profile in self.models.drain(..) {
            if profile.model.is_empty() {
                continue;
            }
            if kept.iter().any(|other| other.id == profile.id) {
                continue;
            }
            kept.push(profile);
            if kept.len() == MODEL_LIMIT {
                break;
            }
        }
        self.models = kept;
        self.active_model = self.active_model.trim().to_string();
        if self.model_profile(&self.active_model).is_none() {
            self.active_model.clear();
        }
        for role in &mut self.roles {
            role.normalize();
        }
        self.roles.retain(|role| !role.name.is_empty());
        dedupe_roles(&mut self.roles);
        self.roles.truncate(super::roles::ROLES_LIMIT);
        for skill in &mut self.skills {
            skill.normalize();
        }
        self.skills.retain(|skill| !skill.name.is_empty());
        dedupe_skills(&mut self.skills);
        self.skills.truncate(super::roles::SKILLS_LIMIT);
        for pack in &mut self.packs {
            pack.normalize();
        }
        self.packs.retain(|pack| !pack.name.is_empty());
        {
            let mut seen: Vec<String> = Vec::new();
            self.packs.retain(|pack| {
                if seen.contains(&pack.id) {
                    return false;
                }
                seen.push(pack.id.clone());
                true
            });
        }
        self.packs.truncate(super::roles::PACKS_LIMIT);
        self.timeout_seconds = self.timeout_seconds.clamp(10, 600);
        self.user_blacklist = clean_list(
            &self.user_blacklist,
            crate::assistant::policy::clean_blacklist_entry,
        );
        self.user_whitelist = clean_list(
            &self.user_whitelist,
            crate::assistant::policy::clean_whitelist_entry,
        );
        self.classifier_model = self.classifier_model.trim().chars().take(128).collect();
        self.proxy.normalize();
    }
}

/// Keep the entries `clean` accepts, in order, without repeats, up to the
/// list's limit.
fn clean_list(entries: &[String], clean: fn(&str) -> Option<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for entry in entries {
        let Some(entry) = clean(entry) else {
            continue;
        };
        if kept.contains(&entry) {
            continue;
        }
        kept.push(entry);
        if kept.len() == LIST_LIMIT {
            break;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_the_section_gets_both_providers_and_no_key() {
        let settings = AssistantSettings::default();
        assert_eq!(settings.protocol, ProviderProtocol::OpenAi);
        assert_eq!(settings.openai.base_url, DEFAULT_OPENAI_BASE_URL);
        assert_eq!(settings.anthropic.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        assert_eq!(settings.timeout_seconds, 120);

        let json = serde_json::to_string(&settings).unwrap();
        assert!(!json.contains("key:"), "{json}");
        assert!(!json.contains("sk-"), "{json}");
        assert!(json.contains("assistant-openai"), "the entry's name only");
    }

    #[test]
    fn blank_or_stray_values_fall_back_and_stay_in_range() {
        let mut settings = AssistantSettings {
            protocol: ProviderProtocol::Anthropic,
            openai: AssistantProvider {
                base_url: "  https://example.test/v1/  ".into(),
                model: "  ".into(),
                keychain_id: " my key ".into(),
                max_tokens: 0,
                headers: Vec::new(),
                ..AssistantProvider::default()
            },
            anthropic: AssistantProvider {
                base_url: "".into(),
                model: "".into(),
                keychain_id: "".into(),
                max_tokens: 99_999,
                headers: Vec::new(),
                ..AssistantProvider::default()
            },
            timeout_seconds: 1,
            user_blacklist: vec![
                "  systemctl   stop * ".into(),
                "".into(),
                "systemctl stop *".into(),
            ],
            user_whitelist: vec![" df -h ".into(), "rm -rf /".into(), "df -h".into()],
            classifier_enabled: true,
            classifier_model: "  small-model  ".into(),
            ..AssistantSettings::default()
        };
        settings.normalize();
        assert_eq!(settings.openai.base_url, "https://example.test/v1");
        assert_eq!(settings.openai.model, "", "a blank model stays blank");
        assert_eq!(settings.openai.keychain_id, "mykey");
        assert_eq!(settings.openai.max_tokens, 1);
        assert_eq!(settings.anthropic.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        assert_eq!(settings.anthropic.model, "");
        assert_eq!(settings.anthropic.keychain_id, "assistant-anthropic");
        assert_eq!(settings.anthropic.max_tokens, 8192);
        assert_eq!(settings.timeout_seconds, 10);
        assert_eq!(settings.user_blacklist, vec!["systemctl stop *"]);
        assert_eq!(settings.user_whitelist, vec!["df -h"]);
        assert_eq!(settings.classifier_model, "small-model");
    }

    #[test]
    fn a_picked_model_brings_its_protocol_and_parameters() {
        let mut settings = AssistantSettings {
            models: vec![ModelProfile {
                label: " 长上下文 ".into(),
                protocol: ProviderProtocol::Anthropic,
                model: " claude-sonnet-4-5 ".into(),
                instruction: " 回答尽量简短 ".into(),
                max_tokens: 0,
                temperature: Some(0.3),
                top_p: Some(1.5),
                stop: vec!["\n\n".into(), "END".into(), "END".into()],
                extra: r#" {"reasoning_effort": "high"} "#.into(),
                ..ModelProfile::default()
            }],
            active_model: "anthropic:claude-sonnet-4-5".into(),
            ..AssistantSettings::default()
        };
        settings.normalize();
        let profile = settings.active_profile().expect("the picked model");
        assert_eq!(profile.id, "anthropic:claude-sonnet-4-5");
        assert_eq!(profile.label, "长上下文");
        assert_eq!(profile.stop, vec!["END"]);
        assert_eq!(profile.top_p, None, "out of range");
        assert_eq!(profile.extra, r#"{"reasoning_effort": "high"}"#);

        let selection = settings.selection();
        assert_eq!(selection.protocol, ProviderProtocol::Anthropic);
        assert_eq!(selection.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        assert_eq!(selection.model, "claude-sonnet-4-5");
        assert_eq!(selection.max_tokens, settings.anthropic.max_tokens);
        assert_eq!(selection.temperature, Some(0.3));
        assert_eq!(selection.instruction, "回答尽量简短");
        assert!(!selection.full_url);
        assert_eq!(selection.context_window, DEFAULT_CONTEXT_WINDOW);
        assert_eq!(selection.tool_rounds, DEFAULT_TOOL_ROUNDS);
        assert_eq!(selection.keychain_id, settings.anthropic.keychain_id);
        assert_eq!(settings.account(), "assistant:assistant-anthropic");
    }

    #[test]
    fn a_model_can_override_its_address_key_and_tool_budget() {
        let mut settings = AssistantSettings {
            models: vec![ModelProfile {
                protocol: ProviderProtocol::OpenAi,
                model: "gateway/model".into(),
                base_url: " https://gateway.test/v1/ ".into(),
                full_url: true,
                keychain_id: " model:openai:gateway/model ".into(),
                context_window: 256_000,
                tool_rounds: 40,
                top_k: Some(0),
                thinking_param: " reasoning.effort ".into(),
                thinking_effort: " high ".into(),
                ..ModelProfile::default()
            }],
            active_model: "openai:gateway/model".into(),
            ..AssistantSettings::default()
        };
        settings.normalize();
        let selection = settings.selection();
        assert_eq!(selection.base_url, "https://gateway.test/v1");
        assert!(selection.full_url);
        assert_eq!(selection.keychain_id, "model:openai:gateway/model");
        assert_eq!(selection.context_window, 256_000);
        assert_eq!(selection.tool_rounds, 40);
        assert_eq!(selection.top_k, None, "zero is not a top_k");
        assert_eq!(selection.thinking_param, "reasoning.effort");
        assert_eq!(selection.thinking_effort, "high");
    }

    #[test]
    fn a_thinking_field_name_that_could_break_the_body_is_dropped() {
        let mut profile = ModelProfile {
            model: "m".into(),
            thinking_param: "a b".into(),
            thinking_effort: "high".into(),
            ..ModelProfile::default()
        };
        profile.normalize();
        assert_eq!(profile.thinking_param, "");
        assert_eq!(profile.thinking_effort, "high");
    }

    #[test]
    fn models_without_a_name_or_with_the_same_name_are_dropped() {
        let mut settings = AssistantSettings {
            models: vec![
                ModelProfile {
                    protocol: ProviderProtocol::OpenAi,
                    model: "  ".into(),
                    ..ModelProfile::default()
                },
                ModelProfile {
                    protocol: ProviderProtocol::OpenAi,
                    model: "gpt-4.1".into(),
                    ..ModelProfile::default()
                },
                ModelProfile {
                    protocol: ProviderProtocol::OpenAi,
                    model: "gpt-4.1".into(),
                    label: "again".into(),
                    ..ModelProfile::default()
                },
                ModelProfile {
                    protocol: ProviderProtocol::Anthropic,
                    model: "claude-sonnet-4-5".into(),
                    ..ModelProfile::default()
                },
            ],
            active_model: "openai:gone".into(),
            ..AssistantSettings::default()
        };
        settings.normalize();
        assert_eq!(settings.models.len(), 2);
        assert_eq!(settings.models[0].model, "gpt-4.1");
        assert_eq!(settings.models[1].model, "claude-sonnet-4-5");
        assert_eq!(settings.active_model, "", "nothing matched, so the default");
        let selection = settings.selection();
        assert_eq!(selection.protocol, ProviderProtocol::OpenAi);
        assert_eq!(selection.model, "", "no provider invents a model");
    }

    #[test]
    fn a_fresh_setup_prefers_the_trial_until_a_model_is_chosen() {
        let mut settings = AssistantSettings::default();
        assert!(
            settings.prefers_trial(),
            "nothing picked starts on the trial"
        );

        settings.active_model = "openai:gpt-4.1".into();
        assert!(
            !settings.prefers_trial(),
            "a file that already picked a model keeps it"
        );

        settings.trial_selected = Some(true);
        assert!(settings.prefers_trial(), "an explicit trial choice wins");

        settings.trial_selected = Some(false);
        settings.active_model.clear();
        assert!(
            !settings.prefers_trial(),
            "an explicit provider choice is remembered"
        );
    }

    #[test]
    fn a_file_without_the_trial_field_leaves_it_unset() {
        let settings: AssistantSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.trial_selected, None);
    }

    #[test]
    fn each_protocol_has_its_own_provider_and_keychain_entry() {
        let mut settings = AssistantSettings {
            protocol: ProviderProtocol::Anthropic,
            ..AssistantSettings::default()
        };
        assert_eq!(settings.active().base_url, DEFAULT_ANTHROPIC_BASE_URL);
        assert_eq!(settings.account(), "assistant:assistant-anthropic");
        settings.provider_mut(ProviderProtocol::OpenAi).keychain_id = "work".into();
        assert_eq!(settings.account(), "assistant:assistant-anthropic");
        settings.protocol = ProviderProtocol::OpenAi;
        assert_eq!(settings.account(), "assistant:work");
    }

    #[test]
    fn headers_keep_their_shape_and_lose_reserved_names() {
        let header = |name: &str, value: &str| RequestHeader {
            name: name.into(),
            value: value.into(),
        };
        let mut settings = AssistantSettings::default();
        settings.openai.headers = vec![
            header(" X-Tenant ", " acme "),
            header("Authorization", "Bearer stolen"),
            header("x-api-key", "stolen"),
            header("content-type", "text/plain"),
            header("Bad Name", "v"),
            header("X-Break", "a\r\nInjected: yes"),
            header("x-empty", "   "),
            header("x-long", &"v".repeat(1025)),
            header("x-tenant", "second"),
            header("X-Trace", "1"),
            header("X-More", "2"),
            header("X-Six", "3"),
            header("X-Seven", "4"),
            header("X-Eight", "5"),
            header("X-Nine", "6"),
        ];
        settings.normalize();
        let kept: Vec<(&str, &str)> = settings
            .openai
            .headers
            .iter()
            .map(|header| (header.name.as_str(), header.value.as_str()))
            .collect();
        assert_eq!(
            kept,
            vec![
                ("X-Tenant", "acme"),
                ("X-Trace", "1"),
                ("X-More", "2"),
                ("X-Six", "3"),
                ("X-Seven", "4"),
                ("X-Eight", "5"),
                ("X-Nine", "6"),
            ],
            "reserved, malformed, empty, long and repeated entries are dropped, up to eight"
        );
    }

    #[test]
    fn a_pasted_proxy_address_keeps_only_its_host_and_port() {
        let mut config = ProxyConfig {
            kind: ProxyKind::Socks5,
            host: "socks5://user:secret@10.0.0.2:1081/path".into(),
            port: 1080,
            username: "  bob  ".into(),
            password_saved: true,
        };
        config.normalize();
        assert_eq!(config.host, "10.0.0.2");
        assert_eq!(config.port, 1081, "the port in the text wins");
        assert_eq!(config.username, "bob");
        assert_eq!(config.url().as_deref(), Some("socks5://10.0.0.2:1081"));

        let mut bare = ProxyConfig {
            host: "proxy.test".into(),
            port: 0,
            ..ProxyConfig::default()
        };
        bare.normalize();
        assert_eq!(bare.host, "proxy.test");
        assert_eq!(bare.port, 8080, "a blank port falls back to the protocol's");
        assert_eq!(bare.url().as_deref(), Some("http://proxy.test:8080"));

        let mut v6 = ProxyConfig {
            host: "[::1]:7890".into(),
            port: 0,
            ..ProxyConfig::default()
        };
        v6.normalize();
        assert_eq!(v6.host, "[::1]");
        assert_eq!(v6.port, 7890);

        let mut named = ProxyConfig {
            host: "proxy.test:8888".into(),
            port: 0,
            ..ProxyConfig::default()
        };
        named.normalize();
        assert_eq!(named.host, "proxy.test");
        assert_eq!(named.port, 8888);

        let mut bare_v6 = ProxyConfig {
            host: "::1".into(),
            port: 1080,
            ..ProxyConfig::default()
        };
        bare_v6.normalize();
        assert_eq!(bare_v6.host, "::1");
        assert_eq!(bare_v6.url().as_deref(), Some("http://[::1]:1080"));

        assert_eq!(ProxyConfig::default().url(), None);
    }

    #[test]
    fn a_provider_follows_the_global_proxy_or_keeps_its_own() {
        let mut settings = AssistantSettings {
            proxy_enabled: true,
            ..AssistantSettings::default()
        };
        settings.proxy.host = "10.0.0.2".into();
        settings.proxy.port = 3128;
        let route = settings.proxy_route(ProviderProtocol::OpenAi).unwrap();
        assert_eq!(route.config.url().as_deref(), Some("http://10.0.0.2:3128"));
        assert_eq!(route.account, "assistant:proxy:default");

        settings.proxy_enabled = false;
        assert!(settings.proxy_route(ProviderProtocol::OpenAi).is_none());

        settings.proxy_enabled = true;
        settings.provider_mut(ProviderProtocol::OpenAi).proxy_mode = ProxyChoice::Off;
        assert!(settings.proxy_route(ProviderProtocol::OpenAi).is_none());

        let provider = settings.provider_mut(ProviderProtocol::Anthropic);
        provider.proxy_mode = ProxyChoice::Custom;
        provider.proxy.kind = ProxyKind::Socks5;
        provider.proxy.host = "127.0.0.1".into();
        provider.proxy.port = 1080;
        let route = settings.proxy_route(ProviderProtocol::Anthropic).unwrap();
        assert_eq!(
            route.config.url().as_deref(),
            Some("socks5://127.0.0.1:1080")
        );
        assert_eq!(route.account, "assistant:proxy:anthropic");
    }

    #[test]
    fn the_settings_file_round_trips_the_section() {
        let mut settings = AssistantSettings {
            protocol: ProviderProtocol::Anthropic,
            ..AssistantSettings::default()
        };
        settings.anthropic.model = "custom-model".into();
        let json = serde_json::to_value(&settings).unwrap();
        let read: AssistantSettings = serde_json::from_value(json).unwrap();
        assert_eq!(read, settings);
        let read: AssistantSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(read, AssistantSettings::default());
    }

    /// A profile saved before the picture switch existed takes pictures as
    /// far as the composer is concerned: the field is absent and the default
    /// keeps pictures allowed rather than quietly dropping them.
    #[test]
    fn a_profile_saved_before_the_picture_switch_still_takes_pictures() {
        let profile: ModelProfile =
            serde_json::from_str(r#"{"id":"openai:old","model":"old"}"#).unwrap();
        assert!(profile.supports_images);
        let settings = AssistantSettings {
            protocol: ProviderProtocol::OpenAi,
            active_model: profile.id.clone(),
            models: vec![profile],
            ..AssistantSettings::default()
        };
        assert!(settings.selection().supports_images);
    }
}
