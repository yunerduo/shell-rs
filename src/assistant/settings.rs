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
}

impl Default for AssistantProvider {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_OPENAI_BASE_URL.to_string(),
            model: DEFAULT_OPENAI_MODEL.to_string(),
            keychain_id: "assistant-openai".to_string(),
            max_tokens: 2048,
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
    }
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
}

impl Default for AssistantSettings {
    fn default() -> Self {
        Self {
            protocol: ProviderProtocol::OpenAi,
            openai: AssistantProvider::for_protocol(ProviderProtocol::OpenAi),
            anthropic: AssistantProvider::for_protocol(ProviderProtocol::Anthropic),
            timeout_seconds: 120,
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

    /// Every number in range, every blank field back to its default, so a
    /// hand-edited file cannot hand a request an empty address.
    pub fn normalize(&mut self) {
        self.openai.normalize(ProviderProtocol::OpenAi);
        self.anthropic.normalize(ProviderProtocol::Anthropic);
        self.timeout_seconds = self.timeout_seconds.clamp(10, 600);
    }
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
            },
            anthropic: AssistantProvider {
                base_url: "".into(),
                model: "".into(),
                keychain_id: "".into(),
                max_tokens: 99_999,
            },
            timeout_seconds: 1,
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
