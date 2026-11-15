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

    fn default_model(self) -> &'static str {
        match self {
            ProviderProtocol::OpenAi => DEFAULT_OPENAI_MODEL,
            ProviderProtocol::Anthropic => DEFAULT_ANTHROPIC_MODEL,
        }
    }

    fn default_keychain_id(self) -> &'static str {
        match self {
            ProviderProtocol::OpenAi => "assistant-openai",
            ProviderProtocol::Anthropic => "assistant-anthropic",
        }
    }
}

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini";
pub const DEFAULT_ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_ANTHROPIC_MODEL: &str = "claude-sonnet-4-5";

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
            model: DEFAULT_OPENAI_MODEL.to_string(),
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
        if self.model.is_empty() {
            self.model = protocol.default_model().to_string();
        }
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

/// `settings.json`'s `assistant` section. One provider of each protocol is
/// configured; `protocol` picks the one that talks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AssistantSettings {
    pub protocol: ProviderProtocol,
    pub openai: AssistantProvider,
    pub anthropic: AssistantProvider,
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

    /// The account the active provider's key lives under.
    pub fn account(&self) -> String {
        account(&self.active().keychain_id)
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
        assert_eq!(settings.openai.model, DEFAULT_OPENAI_MODEL);
        assert_eq!(settings.openai.keychain_id, "mykey");
        assert_eq!(settings.openai.max_tokens, 1);
        assert_eq!(settings.anthropic.base_url, DEFAULT_ANTHROPIC_BASE_URL);
        assert_eq!(settings.anthropic.model, DEFAULT_ANTHROPIC_MODEL);
        assert_eq!(settings.anthropic.keychain_id, "assistant-anthropic");
        assert_eq!(settings.anthropic.max_tokens, 8192);
        assert_eq!(settings.timeout_seconds, 10);
        assert_eq!(settings.user_blacklist, vec!["systemctl stop *"]);
        assert_eq!(settings.user_whitelist, vec!["df -h"]);
        assert_eq!(settings.classifier_model, "small-model");
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
}
