//! The assistant's settings, in one dialog: the model and protocol, the extra
//! request headers, the command rules, the classifier and the scheduled tasks.
//! The panel keeps only the conversation; everything that configures it lives
//! here, where the controls have room.

use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    accordion::Accordion,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::{DialogClose, DialogFooter},
    form::{Field, Form},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    tab::{Tab, TabBar},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeroize::Zeroizing;

use crate::app::CatalogIcon;
use crate::i18n::t;
use crate::secrets::KeychainSecretStore;
use crate::settings::SettingsStore;
use crate::shared::{Segment, SegmentedControl};

use super::client;
use super::panel::AssistantPanel;
use super::pending::{self, PendingItem};
use super::policy;
use super::roles::{self, Bundle, DomainPack, Role, Skill};
use super::schedule::Schedule;
use super::settings::{
    HEADER_LIMIT, MODEL_LIMIT, ModelProfile, ProviderProtocol, ProxyChoice, ProxyConfig, ProxyKind,
    RequestHeader, clean_header, proxy_account,
};

/// The dialog is wide enough for a form's label and control on one line, and
/// never wider than the window it opens in.
const DIALOG_WIDTH: f32 = 760.0;
/// The margin left between the dialog and the window's edges.
const DIALOG_MARGIN: f32 = 32.0;
/// How long one model test may take before it is called a failure. A test
/// the user waits on should not outlast a slow answer.
const MODEL_TEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The pages of the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigTab {
    Model,
    Models,
    Roles,
    Proxy,
    Headers,
    Rules,
    Classifier,
    Schedules,
}

impl ConfigTab {
    const ALL: [ConfigTab; 8] = [
        ConfigTab::Model,
        ConfigTab::Models,
        ConfigTab::Roles,
        ConfigTab::Proxy,
        ConfigTab::Headers,
        ConfigTab::Rules,
        ConfigTab::Classifier,
        ConfigTab::Schedules,
    ];

    fn index(self) -> usize {
        match self {
            ConfigTab::Model => 0,
            ConfigTab::Models => 1,
            ConfigTab::Roles => 2,
            ConfigTab::Proxy => 3,
            ConfigTab::Headers => 4,
            ConfigTab::Rules => 5,
            ConfigTab::Classifier => 6,
            ConfigTab::Schedules => 7,
        }
    }

    fn label(self) -> SharedString {
        match self {
            ConfigTab::Model => t!("assistant.config.tab.model"),
            ConfigTab::Models => t!("assistant.config.tab.models"),
            ConfigTab::Roles => t!("assistant.config.tab.roles"),
            ConfigTab::Proxy => t!("assistant.config.tab.proxy"),
            ConfigTab::Headers => t!("assistant.config.tab.headers"),
            ConfigTab::Rules => t!("assistant.config.tab.rules"),
            ConfigTab::Classifier => t!("assistant.config.tab.classifier"),
            ConfigTab::Schedules => t!("assistant.config.tab.schedules"),
        }
    }
}

/// The proxy the tab is editing: the global default, or one provider's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProxyScope {
    Default,
    Provider(ProviderProtocol),
}

impl ProxyScope {
    fn index(self) -> usize {
        match self {
            ProxyScope::Default => 0,
            ProxyScope::Provider(ProviderProtocol::OpenAi) => 1,
            ProxyScope::Provider(ProviderProtocol::Anthropic) => 2,
        }
    }

    fn from_index(index: usize) -> Self {
        match index {
            1 => ProxyScope::Provider(ProviderProtocol::OpenAi),
            2 => ProxyScope::Provider(ProviderProtocol::Anthropic),
            _ => ProxyScope::Default,
        }
    }

    /// The scope's name in the keychain account.
    fn key(self) -> &'static str {
        match self {
            ProxyScope::Default => "default",
            ProxyScope::Provider(protocol) => protocol.key(),
        }
    }
}

/// The last connectivity check of the proxy tab.
enum ProxyCheck {
    Running,
    /// The proxy answered; the status is what the service returned.
    Passed(u16),
    Failed(SharedString),
}

/// The last test of the models tab's form: whether the service answered at
/// all, and whether it took the picture. The line is built when the answer
/// arrives, so it says the same thing on every render.
enum ModelCheck {
    Running,
    Passed(SharedString),
    Failed(SharedString),
}

/// A number for a field: blank when it means the service's default.
fn empty_if_zero(value: u32) -> String {
    if value == 0 {
        String::new()
    } else {
        value.to_string()
    }
}

/// A float for a field, without a trailing `.0`.
fn float_text(value: Option<f32>) -> String {
    match value {
        Some(value) if value.fract() == 0.0 => format!("{}", value as u32),
        Some(value) => format!("{value}"),
        None => String::new(),
    }
}

/// The parameters a model sends, as one line under its name.
fn model_params_line(profile: &ModelProfile) -> String {
    let mut parts: Vec<String> = Vec::new();
    if profile.max_tokens > 0 {
        parts.push(format!("max_tokens {}", profile.max_tokens));
    }
    if let Some(value) = profile.temperature {
        parts.push(format!("temperature {value}"));
    }
    if let Some(value) = profile.top_p {
        parts.push(format!("top_p {value}"));
    }
    if let Some(value) = profile.top_k {
        parts.push(format!("top_k {value}"));
    }
    if !profile.thinking_param.is_empty() && !profile.thinking_effort.is_empty() {
        parts.push(format!(
            "{} {}",
            profile.thinking_param, profile.thinking_effort
        ));
    }
    if profile.tool_rounds > 0 {
        parts.push(
            t!(
                "assistant.settings.models.tool_rounds_summary",
                rounds = profile.tool_rounds
            )
            .to_string(),
        );
    }
    if !profile.stop.is_empty() {
        parts.push(
            t!(
                "assistant.settings.models.stop_summary",
                count = profile.stop.len()
            )
            .to_string(),
        );
    }
    if !profile.instruction.is_empty() {
        parts.push(t!("assistant.settings.models.instruction_summary").to_string());
    }
    if !profile.extra.is_empty() {
        parts.push(t!("assistant.settings.models.extra_summary").to_string());
    }
    if parts.is_empty() {
        t!("assistant.settings.models.no_params").to_string()
    } else {
        parts.join(" · ")
    }
}

/// The body of the assistant's settings dialog.
pub struct AssistantConfigDialog {
    settings: Entity<SettingsStore>,
    /// The panel owns the scheduled tasks and the review queue; the dialog
    /// only reads them and asks the panel to change them.
    panel: Entity<AssistantPanel>,
    tab: ConfigTab,
    /// The protocol this page is showing; switching saves it straight away.
    protocol: ProviderProtocol,
    base_url: Entity<InputState>,
    model: Entity<InputState>,
    api_key: Entity<InputState>,
    blacklist_input: Entity<InputState>,
    whitelist_input: Entity<InputState>,
    header_name: Entity<InputState>,
    header_value: Entity<InputState>,
    classifier_model: Entity<InputState>,
    /// The models tab: the profile being written, or the id of the one being
    /// edited, and the fields of the form.
    model_protocol: ProviderProtocol,
    editing_model: Option<String>,
    model_label: Entity<InputState>,
    model_id: Entity<InputState>,
    model_base_url: Entity<InputState>,
    model_full_url: bool,
    /// Whether the model this form describes takes pictures; the composer
    /// keeps them out when it does not.
    model_supports_images: bool,
    model_api_key: Entity<InputState>,
    model_max_tokens: Entity<InputState>,
    model_context: Entity<InputState>,
    model_tool_rounds: Entity<InputState>,
    model_temperature: Entity<InputState>,
    model_top_p: Entity<InputState>,
    model_top_k: Entity<InputState>,
    model_thinking_param: Entity<InputState>,
    model_thinking_effort: Entity<InputState>,
    model_stop: Entity<InputState>,
    model_instruction: Entity<InputState>,
    model_extra: Entity<InputState>,
    /// The last test of the form, while it runs and after it answers.
    model_test: Option<ModelCheck>,
    /// The roles and skills tab: the role being written (or edited), and
    /// the fields of both forms.
    editing_role: Option<String>,
    role_name: Entity<InputState>,
    role_avatar: Entity<InputState>,
    role_description: Entity<InputState>,
    role_instruction: Entity<TextareaState>,
    editing_skill: Option<String>,
    skill_name: Entity<InputState>,
    skill_description: Entity<InputState>,
    skill_instruction: Entity<TextareaState>,
    schedule_command: Entity<InputState>,
    schedule_interval: Entity<InputState>,
    /// The proxy tab: which scope it edits, whether the default is on, and
    /// the unsaved values in the fields.
    proxy_scope: ProxyScope,
    proxy_enabled: bool,
    proxy_kind: ProxyKind,
    proxy_mode: ProxyChoice,
    proxy_host: Entity<InputState>,
    proxy_port: Entity<InputState>,
    proxy_username: Entity<InputState>,
    proxy_password: Entity<InputState>,
    proxy_result: Option<ProxyCheck>,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl AssistantConfigDialog {
    fn new(
        settings: Entity<SettingsStore>,
        panel: Entity<AssistantPanel>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let assistant = settings.read(cx).settings().assistant.clone();
        let protocol = assistant.protocol;
        let provider = assistant.active().clone();
        let base_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.base_url.placeholder"))
                .default_value(provider.base_url)
        });
        let model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.model.placeholder"))
                .default_value(provider.model)
        });
        let api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.api_key.placeholder"))
                .masked(true)
        });
        let blacklist_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.blacklist.placeholder"))
                .submit_on_enter(true)
        });
        let whitelist_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.whitelist.placeholder"))
                .submit_on_enter(true)
        });
        let header_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.headers.name_placeholder"))
                .submit_on_enter(true)
        });
        let header_value = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.headers.value_placeholder"))
                .submit_on_enter(true)
        });
        let classifier_model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.classifier.model_placeholder"))
                .default_value(assistant.classifier_model.clone())
        });
        let model_label = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.label.placeholder"))
        });
        let model_id = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("assistant.settings.models.id.placeholder"))
        });
        let model_base_url = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.base_url.placeholder"))
        });
        let model_api_key = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.api_key.placeholder"))
                .masked(true)
        });
        let model_max_tokens = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.output_tokens.placeholder"))
        });
        let model_context = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.context_window.placeholder"))
        });
        let model_tool_rounds = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.tool_rounds.placeholder"))
        });
        let model_temperature = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.temperature.placeholder"))
        });
        let model_top_p = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.top_p.placeholder"))
        });
        let model_top_k = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.top_k.placeholder"))
        });
        let model_thinking_param = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.thinking_param.placeholder"))
        });
        let model_thinking_effort = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.thinking_effort.placeholder"))
        });
        let model_stop = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.stop.placeholder"))
        });
        let model_instruction = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.instruction.placeholder"))
        });
        let model_extra = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.models.extra.placeholder"))
        });
        let role_name = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t!("assistant.settings.roles.name.placeholder"))
        });
        let role_avatar = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.roles.avatar.placeholder"))
        });
        let role_description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.roles.description.placeholder"))
        });
        let role_instruction = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 8)
                .placeholder(t!("assistant.settings.roles.instruction.placeholder"))
        });
        let skill_name = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.skills.name.placeholder"))
        });
        let skill_description = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.skills.description.placeholder"))
        });
        let skill_instruction = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(4, 8)
                .placeholder(t!("assistant.settings.skills.instruction.placeholder"))
        });
        let schedule_command = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.schedule.command_placeholder"))
                .submit_on_enter(true)
        });
        let schedule_interval = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.schedule.interval_placeholder"))
                .default_value("15")
        });
        let proxy = assistant.proxy.clone();
        let proxy_host = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.proxy.host.placeholder"))
                .default_value(proxy.host.clone())
        });
        let proxy_port = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.proxy.port.placeholder"))
                .default_value(proxy.port.to_string())
        });
        let proxy_username = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.proxy.username.placeholder"))
                .default_value(proxy.username.clone())
        });
        let proxy_password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.proxy.password.placeholder"))
                .masked(true)
        });
        let mut subscriptions = vec![
            cx.observe(&settings, |_, _, cx| cx.notify()),
            cx.observe(&panel, |_, _, cx| cx.notify()),
        ];
        subscriptions.push(cx.subscribe_in(
            &blacklist_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.add_blacklist(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &whitelist_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.add_whitelist(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &header_name,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.add_header(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &header_value,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.add_header(window, cx);
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &schedule_command,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                    this.add_schedule(false, window, cx);
                }
            },
        ));
        Self {
            settings,
            panel,
            tab: ConfigTab::Model,
            protocol,
            base_url,
            model,
            api_key,
            blacklist_input,
            whitelist_input,
            header_name,
            header_value,
            classifier_model,
            model_protocol: protocol,
            editing_model: None,
            model_label,
            model_id,
            model_base_url,
            model_full_url: false,
            model_supports_images: true,
            model_api_key,
            model_max_tokens,
            model_context,
            model_tool_rounds,
            model_temperature,
            model_top_p,
            model_top_k,
            model_thinking_param,
            model_thinking_effort,
            model_stop,
            model_instruction,
            model_extra,
            model_test: None,
            editing_role: None,
            role_name,
            role_avatar,
            role_description,
            role_instruction,
            editing_skill: None,
            skill_name,
            skill_description,
            skill_instruction,
            schedule_command,
            schedule_interval,
            proxy_scope: ProxyScope::Default,
            proxy_enabled: assistant.proxy_enabled,
            proxy_kind: assistant.proxy.kind,
            proxy_mode: ProxyChoice::Default,
            proxy_host,
            proxy_port,
            proxy_username,
            proxy_password,
            proxy_result: None,
            error: None,
            notice: None,
            _subscriptions: subscriptions,
        }
    }

    /// Show a problem and put the last notice away.
    fn fail(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = Some(message.into());
        self.notice = None;
        cx.notify();
    }

    fn succeed(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.error = None;
        self.notice = Some(message.into());
        cx.notify();
    }

    fn switch_protocol(
        &mut self,
        protocol: ProviderProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.protocol = protocol;
        let provider = self
            .settings
            .read(cx)
            .settings()
            .assistant
            .provider(protocol)
            .clone();
        self.base_url.update(cx, |input, cx| {
            input.set_value(provider.base_url, window, cx)
        });
        self.model
            .update(cx, |input, cx| input.set_value(provider.model, window, cx));
        self.api_key
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.protocol = protocol, cx)
        });
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn save_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let protocol = self.protocol;
        let base_url = self.base_url.read(cx).value().to_string();
        let model = self.model.read(cx).value().to_string();
        let key = self.api_key.read(cx).value().to_string();
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    let provider = settings.assistant.provider_mut(protocol);
                    provider.base_url = base_url;
                    provider.model = model;
                },
                cx,
            )
        });
        self.api_key
            .update(cx, |input, cx| input.set_value("", window, cx));
        if key.trim().is_empty() {
            self.succeed(t!("assistant.settings.key_kept"), cx);
            return;
        }
        let keychain_id = self
            .settings
            .read(cx)
            .settings()
            .assistant
            .provider(protocol)
            .keychain_id
            .clone();
        let account = super::settings::account(&keychain_id);
        let key = Zeroizing::new(key);
        self.succeed(t!("assistant.settings.key_saved"), cx);
        cx.spawn(async move |this, cx| {
            let written = cx
                .background_executor()
                .spawn(async move { KeychainSecretStore::write_account(&account, &key) })
                .await;
            this.update(cx, |this, cx| match written {
                Ok(()) => this.succeed(t!("assistant.settings.key_saved"), cx),
                Err(error) => this.fail(
                    t!("assistant.settings.key_failed", error = error.to_string()),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// The fields of the models tab as a profile, cleaned the way the
    /// settings file is.
    fn model_from_fields(&self, cx: &mut Context<Self>) -> Result<ModelProfile, SharedString> {
        let number = |text: String| -> Result<Option<f32>, SharedString> {
            let text = text.trim().to_string();
            if text.is_empty() {
                return Ok(None);
            }
            text.parse::<f32>()
                .map(Some)
                .map_err(|_| t!("assistant.settings.models.number_invalid"))
        };
        let max_tokens = {
            let text = self.model_max_tokens.read(cx).value().trim().to_string();
            if text.is_empty() {
                0
            } else {
                text.parse::<u32>()
                    .map_err(|_| t!("assistant.settings.models.number_invalid"))?
            }
        };
        let context_window = {
            let text = self.model_context.read(cx).value().trim().to_string();
            if text.is_empty() {
                0
            } else {
                text.parse::<u32>()
                    .map_err(|_| t!("assistant.settings.models.number_invalid"))?
            }
        };
        let tool_rounds = {
            let text = self.model_tool_rounds.read(cx).value().trim().to_string();
            if text.is_empty() {
                0
            } else {
                text.parse::<u32>()
                    .map_err(|_| t!("assistant.settings.models.number_invalid"))?
            }
        };
        let top_k = {
            let text = self.model_top_k.read(cx).value().trim().to_string();
            if text.is_empty() {
                None
            } else {
                Some(
                    text.parse::<u32>()
                        .map_err(|_| t!("assistant.settings.models.number_invalid"))?,
                )
            }
        };
        let stop = self
            .model_stop
            .read(cx)
            .value()
            .split([',', '\u{FF0C}'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut profile = ModelProfile {
            id: String::new(),
            label: self.model_label.read(cx).value().to_string(),
            protocol: self.model_protocol,
            model: self.model_id.read(cx).value().to_string(),
            base_url: self.model_base_url.read(cx).value().to_string(),
            full_url: self.model_full_url,
            instruction: self.model_instruction.read(cx).value().to_string(),
            supports_images: self.model_supports_images,
            max_tokens,
            context_window,
            tool_rounds,
            temperature: number(self.model_temperature.read(cx).value().to_string())?,
            top_p: number(self.model_top_p.read(cx).value().to_string())?,
            top_k,
            thinking_param: self.model_thinking_param.read(cx).value().to_string(),
            thinking_effort: self.model_thinking_effort.read(cx).value().to_string(),
            stop,
            extra: self.model_extra.read(cx).value().to_string(),
            ..ModelProfile::default()
        };
        profile.normalize();
        if profile.model.is_empty() {
            return Err(t!("assistant.settings.models.require_id"));
        }
        if profile
            .temperature
            .is_some_and(|value| !(0.0..=2.0).contains(&value))
        {
            return Err(t!("assistant.settings.models.temperature.invalid"));
        }
        if profile
            .top_p
            .is_some_and(|value| !(0.0..=1.0).contains(&value))
        {
            return Err(t!("assistant.settings.models.top_p.invalid"));
        }
        Ok(profile)
    }

    fn save_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut profile = match self.model_from_fields(cx) {
            Ok(profile) => profile,
            Err(message) => {
                self.fail(message, cx);
                return;
            }
        };
        let editing = self.editing_model.clone();
        let settings = self.settings.read(cx).settings().assistant.clone();
        if settings
            .models
            .iter()
            .any(|other| other.id == profile.id && Some(&other.id) != editing.as_ref())
        {
            self.fail(t!("assistant.settings.models.duplicate"), cx);
            return;
        }
        if editing.is_none() && settings.models.len() >= MODEL_LIMIT {
            self.fail(t!("assistant.settings.models.limit"), cx);
            return;
        }
        // The id is derived from protocol and model, so an edit may move it.
        profile.id = ModelProfile::id_of(profile.protocol, &profile.model);
        let new_id = profile.id.clone();
        // A model keeps the keychain entry it was created with, even when
        // its model name changes; a new model gets one of its own so its key
        // does not touch the protocol's.
        let previous_keychain = editing
            .as_ref()
            .and_then(|old| settings.model_profile(old))
            .map(|other| other.keychain_id.clone())
            .filter(|keychain_id| !keychain_id.is_empty());
        profile.keychain_id = previous_keychain.unwrap_or_else(|| format!("model:{}", profile.id));
        let keychain_id = profile.keychain_id.clone();
        let typed_key = self.model_api_key.read(cx).value().to_string();
        let was_active = editing
            .as_ref()
            .is_some_and(|old| settings.active_model == *old);
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    match &editing {
                        Some(old) => {
                            if let Some(slot) =
                                settings.assistant.models.iter_mut().find(|m| &m.id == old)
                            {
                                *slot = profile;
                            }
                        }
                        None => settings.assistant.models.push(profile),
                    }
                    if was_active {
                        settings.assistant.active_model = new_id.clone();
                    }
                },
                cx,
            )
        });
        self.clear_model_form(window, cx);
        self.succeed(t!("assistant.settings.models.saved"), cx);
        if !typed_key.trim().is_empty() {
            let account = super::settings::account(&keychain_id);
            let key = Zeroizing::new(typed_key);
            cx.spawn(async move |this, cx| {
                let written = cx
                    .background_executor()
                    .spawn(async move { KeychainSecretStore::write_account(&account, &key) })
                    .await;
                this.update(cx, |this, cx| match written {
                    Ok(()) => this.succeed(t!("assistant.settings.models.key_saved"), cx),
                    Err(error) => this.fail(
                        t!(
                            "assistant.settings.models.key_failed",
                            error = error.to_string()
                        ),
                        cx,
                    ),
                })
                .ok();
            })
            .detach();
        }
    }

    /// Test the form's model without saving it. One request asks a word;
    /// when the form says the model takes pictures, a second one carries
    /// one, and a service that refuses it turns the checkbox off, so the
    /// answer is what the model does rather than what its name suggests.
    fn test_model(&mut self, cx: &mut Context<Self>) {
        if matches!(self.model_test, Some(ModelCheck::Running)) {
            return;
        }
        let profile = match self.model_from_fields(cx) {
            Ok(profile) => profile,
            Err(message) => {
                self.model_test = Some(ModelCheck::Failed(message));
                cx.notify();
                return;
            }
        };
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let provider = assistant.provider(profile.protocol);
        let base_url = if profile.base_url.is_empty() {
            provider.base_url.clone()
        } else {
            profile.base_url.clone()
        };
        if base_url.trim().is_empty() {
            self.model_test = Some(ModelCheck::Failed(t!(
                "assistant.settings.models.test.no_base_url"
            )));
            cx.notify();
            return;
        }
        let keychain_id = self
            .editing_model
            .as_ref()
            .and_then(|id| assistant.model_profile(id))
            .map(|other| other.keychain_id.clone())
            .filter(|id| !id.is_empty())
            .unwrap_or_else(|| {
                format!(
                    "model:{}",
                    ModelProfile::id_of(profile.protocol, &profile.model)
                )
            });
        let request = client::ProbeRequest {
            protocol: profile.protocol,
            base_url,
            full_url: profile.full_url,
            model: profile.model.clone(),
            keychain_id,
            headers: provider.headers.clone(),
            proxy: assistant
                .proxy_route(profile.protocol)
                .map(client::ProxySetting::from),
            timeout: MODEL_TEST_TIMEOUT,
            images: self.model_supports_images,
        };
        let key = self.model_api_key.read(cx).value().to_string();
        self.model_test = Some(ModelCheck::Running);
        self.error = None;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move { client::probe(&request, Some(key.as_str())) })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.model_test = Some(match outcome {
                    Ok(outcome) => {
                        let ms = outcome.elapsed.as_millis();
                        let line = match &outcome.image {
                            None => t!("assistant.settings.models.test.passed", ms = ms),
                            Some(Ok(())) => {
                                t!("assistant.settings.models.test.passed_images", ms = ms)
                            }
                            Some(Err(refusal)) => {
                                let reason = refusal.error.message();
                                // A request the service rejected is the
                                // model's answer about pictures; the
                                // form should not keep claiming them.
                                if (400..=422).contains(&refusal.status) {
                                    this.model_supports_images = false;
                                    t!(
                                        "assistant.settings.models.test.images_off",
                                        reason = reason.as_str()
                                    )
                                } else {
                                    t!(
                                        "assistant.settings.models.test.images_refused",
                                        reason = reason.as_str()
                                    )
                                }
                            }
                        };
                        ModelCheck::Passed(line)
                    }
                    Err(error) => ModelCheck::Failed(t!(
                        "assistant.settings.models.test.failed",
                        reason = error.message().as_str()
                    )),
                });
                cx.notify();
            });
        })
        .detach();
    }

    fn edit_model(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self
            .settings
            .read(cx)
            .settings()
            .assistant
            .model_profile(id)
            .cloned()
        else {
            return;
        };
        self.editing_model = Some(profile.id.clone());
        self.model_protocol = profile.protocol;
        self.model_full_url = profile.full_url;
        self.model_supports_images = profile.supports_images;
        self.model_test = None;
        let values = [
            (&self.model_label, profile.label),
            (&self.model_id, profile.model),
            (&self.model_base_url, profile.base_url),
            (&self.model_max_tokens, empty_if_zero(profile.max_tokens)),
            (&self.model_context, empty_if_zero(profile.context_window)),
            (&self.model_tool_rounds, empty_if_zero(profile.tool_rounds)),
            (&self.model_temperature, float_text(profile.temperature)),
            (&self.model_top_p, float_text(profile.top_p)),
            (
                &self.model_top_k,
                profile
                    .top_k
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            (&self.model_thinking_param, profile.thinking_param),
            (&self.model_thinking_effort, profile.thinking_effort),
            (&self.model_stop, profile.stop.join(", ")),
            (&self.model_instruction, profile.instruction),
            (&self.model_extra, profile.extra),
        ];
        for (input, value) in values {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.model_api_key
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn remove_model(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let keychain_id = self
            .settings
            .read(cx)
            .settings()
            .assistant
            .model_profile(id)
            .map(|profile| profile.keychain_id.clone())
            .filter(|keychain_id| keychain_id.starts_with("model:"));
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    settings.assistant.models.retain(|profile| profile.id != id);
                    if settings.assistant.active_model == id {
                        settings.assistant.active_model.clear();
                    }
                },
                cx,
            )
        });
        // A model's own key goes with it; a provider's stays.
        if let Some(keychain_id) = keychain_id {
            let account = super::settings::account(&keychain_id);
            cx.spawn(async move |_, cx| {
                cx.background_executor()
                    .spawn(async move { KeychainSecretStore::delete_account(&account) })
                    .await
                    .ok();
            })
            .detach();
        }
        if self.editing_model.as_deref() == Some(id) {
            self.clear_model_form(window, cx);
        }
        self.succeed(t!("assistant.settings.models.removed"), cx);
    }

    fn activate_model(&mut self, id: String, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.active_model = id, cx)
        });
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn clear_model_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_model = None;
        self.model_protocol = self.protocol;
        self.model_full_url = false;
        self.model_supports_images = true;
        self.model_test = None;
        for input in [
            &self.model_label,
            &self.model_id,
            &self.model_base_url,
            &self.model_api_key,
            &self.model_max_tokens,
            &self.model_context,
            &self.model_tool_rounds,
            &self.model_temperature,
            &self.model_top_p,
            &self.model_top_k,
            &self.model_thinking_param,
            &self.model_thinking_effort,
            &self.model_stop,
            &self.model_instruction,
            &self.model_extra,
        ] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    /// The role form's fields as a role, cleaned the way the settings file
    /// is. An edit keeps the id it was opened with, so a rename does not
    /// detach the role from a conversation that loaded it.
    fn role_from_fields(&self, cx: &mut Context<Self>) -> Result<Role, SharedString> {
        let mut role = Role {
            name: self.role_name.read(cx).value().to_string(),
            avatar: self.role_avatar.read(cx).value().to_string(),
            description: self.role_description.read(cx).value().to_string(),
            instruction: self.role_instruction.read(cx).value().to_string(),
            ..Role::default()
        };
        role.normalize();
        if role.name.is_empty() {
            return Err(t!("assistant.settings.roles.require_name"));
        }
        if role.instruction.is_empty() {
            return Err(t!("assistant.settings.roles.require_instruction"));
        }
        if let Some(id) = &self.editing_role {
            role.id = id.clone();
        }
        Ok(role)
    }

    fn save_role(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let role = match self.role_from_fields(cx) {
            Ok(role) => role,
            Err(error) => {
                self.fail(error, cx);
                return;
            }
        };
        let mut roles = self.settings.read(cx).settings().assistant.roles.clone();
        match roles.iter().position(|other| other.id == role.id) {
            Some(index) => roles[index] = role,
            None if roles.len() >= roles::ROLES_LIMIT => {
                self.fail(
                    t!("assistant.settings.roles.limit", limit = roles::ROLES_LIMIT),
                    cx,
                );
                return;
            }
            None => roles.push(role),
        }
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.roles = roles, cx)
        });
        self.clear_role_form(window, cx);
        self.succeed(t!("assistant.settings.roles.saved"), cx);
    }

    fn edit_role(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(role) = self
            .settings
            .read(cx)
            .settings()
            .assistant
            .roles
            .iter()
            .find(|role| role.id == id)
            .cloned()
        else {
            return;
        };
        self.editing_role = Some(role.id.clone());
        for (input, value) in [
            (&self.role_name, role.name),
            (&self.role_avatar, role.avatar),
            (&self.role_description, role.description),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.role_instruction.update(cx, |input, cx| {
            input.set_value(role.instruction, window, cx)
        });
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn remove_role(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| settings.assistant.roles.retain(|role| role.id != id),
                cx,
            )
        });
        if self.editing_role.as_deref() == Some(id) {
            self.clear_role_form(window, cx);
        }
        self.succeed(t!("assistant.settings.roles.removed"), cx);
    }

    fn clear_role_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_role = None;
        for input in [&self.role_name, &self.role_avatar, &self.role_description] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.role_instruction
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    /// The skill form's fields as a skill; `enabled` comes from the form's
    /// own switch when it is not a new one.
    fn skill_from_fields(&self, cx: &mut Context<Self>) -> Result<Skill, SharedString> {
        let mut skill = Skill {
            name: self.skill_name.read(cx).value().to_string(),
            description: self.skill_description.read(cx).value().to_string(),
            instruction: self.skill_instruction.read(cx).value().to_string(),
            enabled: true,
            ..Skill::default()
        };
        skill.normalize();
        if skill.name.is_empty() {
            return Err(t!("assistant.settings.skills.require_name"));
        }
        if skill.instruction.is_empty() {
            return Err(t!("assistant.settings.skills.require_instruction"));
        }
        if let Some(id) = &self.editing_skill {
            skill.id = id.clone();
            if let Some(existing) = self
                .settings
                .read(cx)
                .settings()
                .assistant
                .skills
                .iter()
                .find(|other| &other.id == id)
            {
                skill.enabled = existing.enabled;
            }
        }
        Ok(skill)
    }

    fn save_skill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let skill = match self.skill_from_fields(cx) {
            Ok(skill) => skill,
            Err(error) => {
                self.fail(error, cx);
                return;
            }
        };
        let mut skills = self.settings.read(cx).settings().assistant.skills.clone();
        match skills.iter().position(|other| other.id == skill.id) {
            Some(index) => skills[index] = skill,
            None if skills.len() >= roles::SKILLS_LIMIT => {
                self.fail(
                    t!(
                        "assistant.settings.skills.limit",
                        limit = roles::SKILLS_LIMIT
                    ),
                    cx,
                );
                return;
            }
            None => skills.push(skill),
        }
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.skills = skills, cx)
        });
        self.clear_skill_form(window, cx);
        self.succeed(t!("assistant.settings.skills.saved"), cx);
    }

    fn edit_skill(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(skill) = self
            .settings
            .read(cx)
            .settings()
            .assistant
            .skills
            .iter()
            .find(|skill| skill.id == id)
            .cloned()
        else {
            return;
        };
        self.editing_skill = Some(skill.id.clone());
        for (input, value) in [
            (&self.skill_name, skill.name),
            (&self.skill_description, skill.description),
        ] {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        self.skill_instruction.update(cx, |input, cx| {
            input.set_value(skill.instruction, window, cx)
        });
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn remove_skill(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| settings.assistant.skills.retain(|skill| skill.id != id),
                cx,
            )
        });
        if self.editing_skill.as_deref() == Some(id) {
            self.clear_skill_form(window, cx);
        }
        self.succeed(t!("assistant.settings.skills.removed"), cx);
    }

    fn clear_skill_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing_skill = None;
        for input in [&self.skill_name, &self.skill_description] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.skill_instruction
            .update(cx, |input, cx| input.set_value("", window, cx));
        cx.notify();
    }

    fn toggle_skill(&mut self, id: &str, enabled: bool, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    if let Some(skill) = settings
                        .assistant
                        .skills
                        .iter_mut()
                        .find(|skill| skill.id == id)
                    {
                        skill.enabled = enabled;
                    }
                },
                cx,
            )
        });
        cx.notify();
    }

    /// Take one of the suggested skills into the settings, on.
    fn add_builtin_skill(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(skill) = roles::builtin_skills()
            .into_iter()
            .find(|skill| skill.id == id)
        else {
            return;
        };
        let mut skills = self.settings.read(cx).settings().assistant.skills.clone();
        match skills.iter().position(|other| other.id == skill.id) {
            Some(index) => skills[index].enabled = true,
            None if skills.len() >= roles::SKILLS_LIMIT => {
                self.fail(
                    t!(
                        "assistant.settings.skills.limit",
                        limit = roles::SKILLS_LIMIT
                    ),
                    cx,
                );
                return;
            }
            None => skills.push(Skill {
                builtin: false,
                enabled: true,
                ..skill
            }),
        }
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.skills = skills, cx)
        });
        self.succeed(t!("assistant.settings.skills.added"), cx);
    }

    /// Add a whole pack to the settings, replacing one with the same id.
    fn store_pack(&mut self, mut pack: DomainPack, cx: &mut Context<Self>) {
        pack.normalize();
        if pack.name.is_empty() {
            self.fail(t!("assistant.settings.packs.require_name"), cx);
            return;
        }
        let mut packs = self.settings.read(cx).settings().assistant.packs.clone();
        match packs.iter().position(|other| other.id == pack.id) {
            Some(index) => packs[index] = pack,
            None if packs.len() >= roles::PACKS_LIMIT => {
                self.fail(
                    t!("assistant.settings.packs.limit", limit = roles::PACKS_LIMIT),
                    cx,
                );
                return;
            }
            None => packs.push(pack),
        }
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.packs = packs, cx)
        });
        self.succeed(t!("assistant.settings.packs.added"), cx);
    }

    /// Take a built-in pack into the settings with its skills on, since a
    /// pack is added to be used.
    fn add_builtin_pack(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(mut pack) = roles::builtin_packs().get(index).cloned() else {
            return;
        };
        pack.builtin = false;
        for role in &mut pack.roles {
            role.builtin = false;
        }
        for skill in &mut pack.skills {
            skill.builtin = false;
            skill.enabled = true;
        }
        self.store_pack(pack, cx);
    }

    fn remove_pack(&mut self, id: &str, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| settings.assistant.packs.retain(|pack| pack.id != id),
                cx,
            )
        });
        self.succeed(t!("assistant.settings.packs.removed"), cx);
    }

    /// Read a role, a skill or a pack from a JSON file the user picks.
    fn import_bundle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("assistant.settings.bundle.import_prompt")),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = receiver.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let read = cx
                .background_spawn(async move { std::fs::read_to_string(path) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| {
                let parsed = read
                    .map_err(|error| error.to_string())
                    .and_then(|json| Bundle::parse(&json).map_err(|error| error.to_string()));
                match parsed {
                    Ok(bundle) => this.apply_bundle(bundle, cx),
                    Err(error) => this.fail(
                        t!("assistant.settings.bundle.import_failed", error = error),
                        cx,
                    ),
                }
            });
        })
        .detach();
    }

    fn apply_bundle(&mut self, bundle: Bundle, cx: &mut Context<Self>) {
        match bundle {
            Bundle::Role(mut role) => {
                if role.id.starts_with("builtin:") {
                    self.fail(t!("assistant.settings.roles.builtin_locked"), cx);
                    return;
                }
                role.normalize();
                if role.name.is_empty() || role.instruction.is_empty() {
                    self.fail(t!("assistant.settings.bundle.incomplete"), cx);
                    return;
                }
                let mut roles = self.settings.read(cx).settings().assistant.roles.clone();
                match roles.iter().position(|other| other.id == role.id) {
                    Some(index) => roles[index] = role,
                    None if roles.len() >= roles::ROLES_LIMIT => {
                        self.fail(
                            t!("assistant.settings.roles.limit", limit = roles::ROLES_LIMIT),
                            cx,
                        );
                        return;
                    }
                    None => roles.push(role),
                }
                self.settings.update(cx, |store, cx| {
                    store.update(|settings| settings.assistant.roles = roles, cx)
                });
                self.succeed(t!("assistant.settings.roles.imported"), cx);
            }
            Bundle::Skill(mut skill) => {
                skill.normalize();
                if skill.name.is_empty() || skill.instruction.is_empty() {
                    self.fail(t!("assistant.settings.bundle.incomplete"), cx);
                    return;
                }
                let mut skills = self.settings.read(cx).settings().assistant.skills.clone();
                match skills.iter().position(|other| other.id == skill.id) {
                    Some(index) => skills[index] = skill,
                    None if skills.len() >= roles::SKILLS_LIMIT => {
                        self.fail(
                            t!(
                                "assistant.settings.skills.limit",
                                limit = roles::SKILLS_LIMIT
                            ),
                            cx,
                        );
                        return;
                    }
                    None => skills.push(skill),
                }
                self.settings.update(cx, |store, cx| {
                    store.update(|settings| settings.assistant.skills = skills, cx)
                });
                self.succeed(t!("assistant.settings.skills.imported"), cx);
            }
            Bundle::Pack(pack) => self.store_pack(pack, cx),
        }
    }

    /// Write a role, a skill or a pack to the file the user picks.
    fn export_bundle(
        &mut self,
        bundle: Bundle,
        file_name: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let dir = crate::app::data_dir();
        let receiver = cx.prompt_for_new_path(&dir, Some(file_name));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            let json = bundle.to_json();
            let written = cx
                .background_spawn(async move { std::fs::write(path, json) })
                .await;
            let _ = this.update_in(cx, |this, _, cx| match written {
                Ok(()) => this.succeed(t!("assistant.settings.bundle.exported"), cx),
                Err(error) => this.fail(
                    t!(
                        "assistant.settings.bundle.export_failed",
                        error = error.to_string()
                    ),
                    cx,
                ),
            });
        })
        .detach();
    }

    /// Whether the scope's proxy has a password in the keychain.
    fn proxy_password_saved(&self, cx: &mut Context<Self>) -> bool {
        let assistant = &self.settings.read(cx).settings().assistant;
        match self.proxy_scope {
            ProxyScope::Default => assistant.proxy.password_saved,
            ProxyScope::Provider(protocol) => assistant.provider(protocol).proxy.password_saved,
        }
    }

    fn set_password_saved(&mut self, scope: ProxyScope, saved: bool, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| match scope {
                    ProxyScope::Default => settings.assistant.proxy.password_saved = saved,
                    ProxyScope::Provider(protocol) => {
                        settings
                            .assistant
                            .provider_mut(protocol)
                            .proxy
                            .password_saved = saved;
                    }
                },
                cx,
            )
        });
    }

    /// Put the selected scope's saved proxy into the fields.
    fn load_proxy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let (config, mode) = match self.proxy_scope {
            ProxyScope::Default => (assistant.proxy, ProxyChoice::Default),
            ProxyScope::Provider(protocol) => {
                let provider = assistant.provider(protocol);
                (provider.proxy.clone(), provider.proxy_mode)
            }
        };
        self.proxy_enabled = assistant.proxy_enabled;
        self.proxy_mode = mode;
        self.proxy_kind = config.kind;
        self.proxy_host
            .update(cx, |input, cx| input.set_value(config.host, window, cx));
        self.proxy_port.update(cx, |input, cx| {
            input.set_value(config.port.to_string(), window, cx)
        });
        self.proxy_username
            .update(cx, |input, cx| input.set_value(config.username, window, cx));
        self.proxy_password
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.proxy_result = None;
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    /// The fields as a proxy config, cleaned the way the settings file is.
    /// A scope that is not in force may be saved without an address.
    fn proxy_config_from_fields(
        &self,
        cx: &mut Context<Self>,
        require_host: bool,
    ) -> Result<ProxyConfig, SharedString> {
        let host = self.proxy_host.read(cx).value().to_string();
        let port = self.proxy_port.read(cx).value().trim().to_string();
        let username = self.proxy_username.read(cx).value().to_string();
        if !port.is_empty() && !matches!(port.parse::<u16>(), Ok(port) if port > 0) {
            return Err(t!("assistant.settings.proxy.port.invalid"));
        }
        let mut config = ProxyConfig {
            kind: self.proxy_kind,
            host,
            port: port.parse().unwrap_or(0),
            username,
            password_saved: self.proxy_password_saved(cx),
        };
        config.normalize();
        if require_host && config.host.is_empty() {
            return Err(t!("assistant.settings.proxy.host.required"));
        }
        Ok(config)
    }

    fn save_proxy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let scope = self.proxy_scope;
        let in_force = match scope {
            ProxyScope::Default => self.proxy_enabled,
            ProxyScope::Provider(_) => self.proxy_mode == ProxyChoice::Custom,
        };
        let config = match self.proxy_config_from_fields(cx, in_force) {
            Ok(config) => config,
            Err(message) => {
                self.fail(message, cx);
                return;
            }
        };
        let typed = self.proxy_password.read(cx).value().to_string();
        let username_empty = config.username.is_empty();
        // A typed password only counts once the keychain takes it; with no
        // account there is nothing left to authenticate with.
        let had_password = config.password_saved && !username_empty;
        let account = proxy_account(scope.key());
        let cleaned = (
            config.host.clone(),
            config.port.to_string(),
            config.username.clone(),
        );
        let mut stored = config;
        stored.password_saved = had_password && typed.trim().is_empty();
        let enabled = self.proxy_enabled;
        let mode = self.proxy_mode;
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| match scope {
                    ProxyScope::Default => {
                        settings.assistant.proxy_enabled = enabled;
                        settings.assistant.proxy = stored;
                    }
                    ProxyScope::Provider(protocol) => {
                        let provider = settings.assistant.provider_mut(protocol);
                        provider.proxy_mode = mode;
                        provider.proxy = stored;
                    }
                },
                cx,
            )
        });
        self.proxy_host
            .update(cx, |input, cx| input.set_value(&cleaned.0, window, cx));
        self.proxy_port
            .update(cx, |input, cx| input.set_value(&cleaned.1, window, cx));
        self.proxy_username
            .update(cx, |input, cx| input.set_value(&cleaned.2, window, cx));
        self.proxy_password
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.proxy_result = None;
        if username_empty {
            self.set_password_saved(scope, false, cx);
            if had_password {
                cx.spawn(async move |this, cx| {
                    let removed = cx
                        .background_executor()
                        .spawn(async move { KeychainSecretStore::delete_account(&account) })
                        .await;
                    if let Err(error) = removed {
                        this.update(cx, |this, cx| {
                            this.fail(
                                t!(
                                    "assistant.settings.proxy.password_failed",
                                    error = error.to_string()
                                ),
                                cx,
                            )
                        })
                        .ok();
                    }
                })
                .detach();
            }
            self.succeed(t!("assistant.settings.saved"), cx);
            return;
        }
        if typed.trim().is_empty() {
            self.succeed(t!("assistant.settings.proxy.password_kept"), cx);
            return;
        }
        let typed = Zeroizing::new(typed);
        self.succeed(t!("assistant.settings.saved"), cx);
        cx.spawn(async move |this, cx| {
            let written = cx
                .background_executor()
                .spawn(async move { KeychainSecretStore::write_account(&account, &typed) })
                .await;
            this.update(cx, |this, cx| match written {
                Ok(()) => {
                    this.set_password_saved(scope, true, cx);
                    this.succeed(t!("assistant.settings.proxy.password_saved"), cx);
                }
                Err(error) => this.fail(
                    t!(
                        "assistant.settings.proxy.password_failed",
                        error = error.to_string()
                    ),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Send one real request through the proxy, off the UI thread. The
    /// target is the service the assistant would call for this scope.
    fn test_proxy(&mut self, cx: &mut Context<Self>) {
        let scope = self.proxy_scope;
        let config = match self.proxy_config_from_fields(cx, true) {
            Ok(config) => config,
            Err(message) => {
                self.fail(message, cx);
                return;
            }
        };
        let typed = self.proxy_password.read(cx).value().to_string();
        let account = proxy_account(scope.key());
        let target = {
            let assistant = &self.settings.read(cx).settings().assistant;
            let base_url = match scope {
                ProxyScope::Default => assistant.active().base_url.clone(),
                ProxyScope::Provider(protocol) => assistant.provider(protocol).base_url.clone(),
            };
            if base_url.trim().is_empty() {
                "https://example.com".to_string()
            } else {
                base_url
            }
        };
        self.proxy_result = Some(ProxyCheck::Running);
        self.error = None;
        self.notice = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let typed = Zeroizing::new(typed);
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    let password = if config.username.is_empty() {
                        Ok(None)
                    } else if typed.trim().is_empty() {
                        KeychainSecretStore::read_account(&account)
                            .map(|saved| saved.map(|value| value.to_string()))
                    } else {
                        Ok(Some(typed.to_string()))
                    };
                    match password {
                        Ok(password) => client::test_proxy(&config, password.as_deref(), &target),
                        Err(error) => Err(client::ProxyTestError::Other(error.to_string())),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.proxy_result = Some(match outcome {
                    Ok(status) => ProxyCheck::Passed(status),
                    Err(error) => ProxyCheck::Failed(error.message()),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn add_blacklist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.blacklist_input.read(cx).value().to_string();
        let Some(entry) = policy::clean_blacklist_entry(&value) else {
            self.fail(t!("assistant.settings.list.invalid"), cx);
            return;
        };
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    if !settings.assistant.user_blacklist.contains(&entry) {
                        settings.assistant.user_blacklist.push(entry);
                    }
                },
                cx,
            )
        });
        self.blacklist_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        cx.notify();
    }

    fn add_whitelist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.whitelist_input.read(cx).value().to_string();
        let Some(entry) = policy::clean_whitelist_entry(&value) else {
            self.fail(t!("assistant.settings.list.invalid"), cx);
            return;
        };
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    if !settings.assistant.user_whitelist.contains(&entry) {
                        settings.assistant.user_whitelist.push(entry);
                    }
                },
                cx,
            )
        });
        self.whitelist_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        cx.notify();
    }

    fn remove_blacklist(&mut self, index: usize, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    if index < settings.assistant.user_blacklist.len() {
                        settings.assistant.user_blacklist.remove(index);
                    }
                },
                cx,
            )
        });
        cx.notify();
    }

    fn remove_whitelist(&mut self, index: usize, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    if index < settings.assistant.user_whitelist.len() {
                        settings.assistant.user_whitelist.remove(index);
                    }
                },
                cx,
            )
        });
        cx.notify();
    }

    fn add_header(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.header_name.read(cx).value().to_string();
        let value = self.header_value.read(cx).value().to_string();
        let Some(header) = clean_header(&name, &value) else {
            self.fail(t!("assistant.settings.headers.invalid"), cx);
            return;
        };
        let mut full = false;
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    let provider = settings.assistant.active_mut();
                    if provider.headers.len() >= HEADER_LIMIT {
                        full = true;
                        return;
                    }
                    if !provider
                        .headers
                        .iter()
                        .any(|kept| kept.name.eq_ignore_ascii_case(&header.name))
                    {
                        provider.headers.push(header);
                    }
                },
                cx,
            )
        });
        if full {
            self.fail(
                t!("assistant.settings.headers.full", limit = HEADER_LIMIT),
                cx,
            );
            return;
        }
        self.header_name
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.header_value
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        cx.notify();
    }

    fn remove_header(&mut self, index: usize, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    let provider = settings.assistant.active_mut();
                    if index < provider.headers.len() {
                        provider.headers.remove(index);
                    }
                },
                cx,
            )
        });
        cx.notify();
    }

    fn set_classifier(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| settings.assistant.classifier_enabled = enabled,
                cx,
            )
        });
        cx.notify();
    }

    fn save_classifier_model(&mut self, cx: &mut Context<Self>) {
        let model = self.classifier_model.read(cx).value().to_string();
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.classifier_model = model, cx)
        });
        self.succeed(t!("assistant.settings.saved"), cx);
    }

    /// Add a task on the first bound terminal. The panel owns the list and
    /// the save, so the dialog only carries the values over.
    fn add_schedule(&mut self, prompt: bool, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.schedule_command.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        let interval = self
            .schedule_interval
            .read(cx)
            .value()
            .trim()
            .parse::<u32>()
            .unwrap_or(15);
        let result = self.panel.update(cx, |panel, cx| {
            panel.add_schedule_values(&text, interval, prompt, cx)
        });
        match result {
            Ok(()) => {
                self.schedule_command
                    .update(cx, |input, cx| input.set_value("", window, cx));
                self.succeed(t!("assistant.schedule.added"), cx);
            }
            Err(message) => self.fail(message, cx),
        }
    }

    fn toggle_schedule(&mut self, id: &str, enabled: bool, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.toggle_schedule(id, enabled, cx));
        cx.notify();
    }

    fn remove_schedule(&mut self, id: &str, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.remove_schedule(id, cx));
        cx.notify();
    }

    fn rebind_schedule(&mut self, id: &str, cx: &mut Context<Self>) {
        let result = self
            .panel
            .update(cx, |panel, cx| panel.rebind_schedule(id, cx));
        match result {
            Ok(()) => self.succeed(t!("assistant.settings.saved"), cx),
            Err(message) => self.fail(message, cx),
        }
    }

    fn approve_item(&mut self, index: usize, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.approve_pending_item(index, cx));
        cx.notify();
    }

    fn deny_item(&mut self, index: usize, cx: &mut Context<Self>) {
        self.panel
            .update(cx, |panel, cx| panel.deny_pending_item(index, cx));
        cx.notify();
    }

    fn render_model(&self, cx: &mut Context<Self>) -> AnyElement {
        let protocols = SegmentedControl::new("assistant-config-protocol")
            .selected_index(
                ProviderProtocol::ALL
                    .iter()
                    .position(|protocol| *protocol == self.protocol),
            )
            .on_change(cx.listener(|this, index: &usize, window, cx| {
                if let Some(protocol) = ProviderProtocol::ALL.get(*index).copied() {
                    this.switch_protocol(protocol, window, cx);
                }
            }))
            .segments(ProviderProtocol::ALL.map(|protocol| Segment::new(protocol.label())));
        let footer = h_flex().items_center().gap_2().child(
            Button::new("assistant-config-save")
                .small()
                .primary()
                .label(t!("assistant.action.save"))
                .on_click(cx.listener(|this, _, window, cx| this.save_provider(window, cx))),
        );
        Form::new()
            .child(
                Field::new()
                    .label(t!("assistant.settings.protocol"))
                    .child(protocols),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.base_url.label"))
                    .description(t!("assistant.settings.base_url.hint"))
                    .child(Input::new(&self.base_url).id("assistant-config-base-url")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.model.label"))
                    .child(Input::new(&self.model).id("assistant-config-model")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.api_key.label"))
                    .description(t!("assistant.settings.api_key.hint"))
                    .child(Input::new(&self.api_key).id("assistant-config-api-key")),
            )
            .footer(footer)
            .into_any_element()
    }

    /// 模型: the models that can be picked in a conversation, and the
    /// parameters each sends. The provider tab says where they are sent and
    /// with which key.
    fn render_models(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;
        let danger = cx.theme().danger;
        let border = cx.theme().border;
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let editing = self.editing_model.is_some();
        let protocols = SegmentedControl::new("assistant-config-model-protocol")
            .selected_index(
                ProviderProtocol::ALL
                    .iter()
                    .position(|protocol| *protocol == self.model_protocol),
            )
            .on_change(cx.listener(|this, index: &usize, _, cx| {
                if let Some(protocol) = ProviderProtocol::ALL.get(*index).copied() {
                    this.model_protocol = protocol;
                    cx.notify();
                }
            }))
            .segments(ProviderProtocol::ALL.map(|protocol| Segment::new(protocol.label())));
        let footer = h_flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("assistant-config-model-test")
                    .small()
                    .label(t!("assistant.settings.models.test.action"))
                    .disabled(matches!(self.model_test, Some(ModelCheck::Running)))
                    .on_click(cx.listener(|this, _, _, cx| this.test_model(cx))),
            )
            .child(
                Button::new("assistant-config-model-save")
                    .small()
                    .primary()
                    .label(if editing {
                        t!("assistant.settings.models.save")
                    } else {
                        t!("assistant.settings.models.add")
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.save_model(window, cx))),
            )
            .when(editing, |row| {
                row.child(
                    Button::new("assistant-config-model-cancel")
                        .small()
                        .label(t!("common.cancel"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear_model_form(window, cx);
                        })),
                )
            });
        let chip_row = |field: &'static str,
                        target: Entity<InputState>,
                        values: &[(&'static str, &'static str)],
                        cx: &mut Context<Self>| {
            let mut row = h_flex().flex_wrap().gap_1();
            for (label_key, value) in values {
                let target = target.clone();
                let value = *value;
                row = row.child(
                    Button::new(format!("assistant-config-{field}-chip-{value}"))
                        .xsmall()
                        .ghost()
                        .label(t!(label_key))
                        .on_click(cx.listener(move |_, _, window, cx| {
                            target.update(cx, |input, cx| input.set_value(value, window, cx));
                        })),
                );
            }
            row
        };
        let basic = Form::new()
            .child(
                Field::new()
                    .label(t!("assistant.settings.models.protocol"))
                    .child(protocols),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.models.base_url"))
                    .description(t!("assistant.settings.models.base_url.hint"))
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                Input::new(&self.model_base_url)
                                    .id("assistant-config-model-base-url"),
                            )
                            .child(
                                Checkbox::new("assistant-config-model-full-url")
                                    .label(t!("assistant.settings.models.full_url"))
                                    .checked(self.model_full_url)
                                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                        this.model_full_url = *checked;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.models.id"))
                    .description(t!("assistant.settings.models.id.hint"))
                    .child(Input::new(&self.model_id).id("assistant-config-model-id")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.models.label"))
                    .description(t!("assistant.settings.models.label.hint"))
                    .child(Input::new(&self.model_label).id("assistant-config-model-label")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.models.api_key"))
                    .description(t!("assistant.settings.models.api_key.hint"))
                    .child(Input::new(&self.model_api_key).id("assistant-config-model-api-key")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.models.images"))
                    .description(t!("assistant.settings.models.images.hint"))
                    .child(
                        Checkbox::new("assistant-config-model-images")
                            .label(t!("assistant.settings.models.images.on"))
                            .checked(self.model_supports_images)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.model_supports_images = *checked;
                                cx.notify();
                            })),
                    ),
            );
        let advanced = Accordion::new("assistant-config-model-advanced")
            .bordered(false)
            .item(|item| {
                item.title(t!("assistant.settings.models.advanced")).child(
                    v_flex()
                        .w_full()
                        .gap_4()
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.context_window"))
                                .description(t!("assistant.settings.models.context_window.hint"))
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(
                                            Input::new(&self.model_context)
                                                .id("assistant-config-model-context"),
                                        )
                                        .child(chip_row(
                                            "context",
                                            self.model_context.clone(),
                                            &[
                                                ("assistant.settings.models.preset.128k", "128000"),
                                                ("assistant.settings.models.preset.256k", "256000"),
                                                ("assistant.settings.models.preset.512k", "512000"),
                                                ("assistant.settings.models.preset.1m", "1000000"),
                                            ],
                                            cx,
                                        )),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.output_tokens"))
                                .description(t!("assistant.settings.models.output_tokens.hint"))
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(
                                            Input::new(&self.model_max_tokens)
                                                .id("assistant-config-model-max-tokens"),
                                        )
                                        .child(chip_row(
                                            "output",
                                            self.model_max_tokens.clone(),
                                            &[
                                                ("assistant.settings.models.preset.4k", "4096"),
                                                ("assistant.settings.models.preset.16k", "16384"),
                                                ("assistant.settings.models.preset.32k", "32768"),
                                                ("assistant.settings.models.preset.128k", "131072"),
                                            ],
                                            cx,
                                        )),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.tool_rounds"))
                                .description(t!("assistant.settings.models.tool_rounds.hint"))
                                .child(
                                    Input::new(&self.model_tool_rounds)
                                        .id("assistant-config-model-tool-rounds"),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.thinking"))
                                .description(t!("assistant.settings.models.thinking.hint"))
                                .child(
                                    v_flex()
                                        .gap_2()
                                        .child(
                                            Input::new(&self.model_thinking_param)
                                                .id("assistant-config-model-thinking-param"),
                                        )
                                        .child(
                                            Input::new(&self.model_thinking_effort)
                                                .id("assistant-config-model-thinking-effort"),
                                        )
                                        .child(chip_row(
                                            "thinking",
                                            self.model_thinking_effort.clone(),
                                            &[
                                                ("assistant.settings.models.thinking.default", ""),
                                                (
                                                    "assistant.settings.models.thinking.disabled",
                                                    "none",
                                                ),
                                                ("assistant.settings.models.thinking.low", "low"),
                                                (
                                                    "assistant.settings.models.thinking.medium",
                                                    "medium",
                                                ),
                                                ("assistant.settings.models.thinking.high", "high"),
                                            ],
                                            cx,
                                        )),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.temperature"))
                                .child(
                                    Input::new(&self.model_temperature)
                                        .id("assistant-config-model-temperature"),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.top_p"))
                                .child(
                                    Input::new(&self.model_top_p)
                                        .id("assistant-config-model-top-p"),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.top_k"))
                                .child(
                                    Input::new(&self.model_top_k)
                                        .id("assistant-config-model-top-k"),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.stop"))
                                .child(
                                    Input::new(&self.model_stop).id("assistant-config-model-stop"),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.instruction"))
                                .description(t!("assistant.settings.models.instruction.hint"))
                                .child(
                                    Input::new(&self.model_instruction)
                                        .id("assistant-config-model-instruction"),
                                ),
                        )
                        .child(
                            Field::new()
                                .label(t!("assistant.settings.models.extra"))
                                .description(t!("assistant.settings.models.extra.hint"))
                                .child(
                                    Input::new(&self.model_extra)
                                        .id("assistant-config-model-extra"),
                                ),
                        ),
                )
            });
        let form = v_flex()
            .gap_4()
            .child(basic)
            .child(advanced)
            .child(footer)
            .when_some(self.model_test.as_ref(), |form, check| {
                let (color, text) = match check {
                    ModelCheck::Running => (muted, t!("assistant.settings.models.test.running")),
                    ModelCheck::Passed(text) => (success, text.clone()),
                    ModelCheck::Failed(text) => (danger, text.clone()),
                };
                form.child(
                    div()
                        .id("assistant-config-model-test-result")
                        .test_support()
                        .text_sm()
                        .text_color(color)
                        .child(text),
                )
            })
            .into_any_element();
        let active = assistant.active_model.clone();
        let mut list = v_flex().w_full().gap_1();
        if assistant.models.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.models.empty")),
            );
        }
        for (index, profile) in assistant.models.iter().enumerate() {
            let id = profile.id.clone();
            let is_active = active == profile.id;
            let params = model_params_line(profile);
            list = list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div().text_sm().child(profile.display_name().to_string()),
                                    )
                                    .child(div().text_xs().text_color(muted).child(format!(
                                        "{} · {}",
                                        profile.model,
                                        profile.protocol.label()
                                    )))
                                    .when(is_active, |row| {
                                        row.child(
                                            div()
                                                .text_xs()
                                                .text_color(success)
                                                .child(t!("assistant.settings.models.active")),
                                        )
                                    }),
                            )
                            .child(div().text_xs().text_color(muted).child(params)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .when(!is_active, |row| {
                                let id = id.clone();
                                row.child(
                                    Button::new(format!("assistant-config-model-use-{index}"))
                                        .xsmall()
                                        .ghost()
                                        .icon(CatalogIcon::CircleCheck)
                                        .label(t!("assistant.settings.models.set_active"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.activate_model(id.clone(), cx);
                                        })),
                                )
                            })
                            .child({
                                let edit = id.clone();
                                Button::new(format!("assistant-config-model-edit-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Pencil)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_model(&edit, window, cx);
                                    }))
                            })
                            .child(
                                Button::new(format!("assistant-config-model-remove-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Trash)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_model(&id, window, cx);
                                    })),
                            ),
                    ),
            );
        }
        v_flex()
            .gap_4()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.models.hint")),
            )
            .child(form)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().child(t!("assistant.settings.models.list"))),
            )
            .child(list)
            .into_any_element()
    }

    /// 角色与技能: the roles a conversation can answer as, the skills it can
    /// be told to follow, and the packs that bring both. Built-in items are
    /// shown but cannot be edited; they can be exported to start from.
    fn render_roles(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let editing_role = self.editing_role.is_some();
        let editing_skill = self.editing_skill.is_some();

        let role_footer = h_flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("assistant-config-role-save")
                    .small()
                    .primary()
                    .label(if editing_role {
                        t!("assistant.settings.roles.save")
                    } else {
                        t!("assistant.settings.roles.add")
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.save_role(window, cx))),
            )
            .when(editing_role, |row| {
                row.child(
                    Button::new("assistant-config-role-cancel")
                        .small()
                        .label(t!("common.cancel"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear_role_form(window, cx);
                        })),
                )
            });
        let role_form = Form::new()
            .child(
                Field::new()
                    .label(t!("assistant.settings.roles.name"))
                    .child(Input::new(&self.role_name).id("assistant-config-role-name")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.roles.avatar"))
                    .description(t!("assistant.settings.roles.avatar.hint"))
                    .child(Input::new(&self.role_avatar).id("assistant-config-role-avatar")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.roles.description"))
                    .child(
                        Input::new(&self.role_description).id("assistant-config-role-description"),
                    ),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.roles.instruction"))
                    .description(t!("assistant.settings.roles.instruction.hint"))
                    .child(Textarea::new(&self.role_instruction).h(px(80.)).text_sm()),
            )
            .footer(role_footer)
            .into_any_element();

        let mut role_list = v_flex().w_full().gap_1();
        for (index, role) in roles::builtin_roles().iter().enumerate() {
            let bundle = Bundle::Role(role.clone());
            let file_name = format!("role-{}.json", index);
            role_list = role_list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_sm().child(format!(
                                        "{} {}",
                                        role.initial(),
                                        role.name
                                    )))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(t!("assistant.settings.roles.builtin")),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(role.description.clone()),
                            ),
                    )
                    .child(
                        Button::new(format!("assistant-config-builtin-role-export-{index}"))
                            .xsmall()
                            .ghost()
                            .icon(CatalogIcon::Download)
                            .tooltip(t!("assistant.settings.bundle.export"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.export_bundle(bundle.clone(), &file_name, window, cx);
                            })),
                    ),
            );
        }
        if assistant.roles.is_empty() {
            role_list = role_list.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.roles.empty")),
            );
        }
        for (index, role) in assistant.roles.iter().enumerate() {
            let id = role.id.clone();
            let edit = id.clone();
            let bundle = Bundle::Role(role.clone());
            let file_name = format!("role-{}.json", index + 1);
            role_list = role_list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(div().text_sm().child(format!(
                                "{} {}",
                                role.initial(),
                                role.name
                            )))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(role.description.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(format!("assistant-config-role-edit-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Pencil)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_role(&edit, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("assistant-config-role-remove-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Trash)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_role(&id, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("assistant-config-role-export-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Download)
                                    .tooltip(t!("assistant.settings.bundle.export"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.export_bundle(bundle.clone(), &file_name, window, cx);
                                    })),
                            ),
                    ),
            );
        }

        let mut skill_list = v_flex().w_full().gap_1();
        if assistant.skills.is_empty() {
            skill_list = skill_list.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.skills.empty")),
            );
        }
        for (index, skill) in assistant.skills.iter().enumerate() {
            let id = skill.id.clone();
            let toggle = id.clone();
            let edit = id.clone();
            let bundle = Bundle::Skill(skill.clone());
            let file_name = format!("skill-{}.json", index);
            skill_list = skill_list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                Checkbox::new(format!("assistant-config-skill-enabled-{index}"))
                                    .label(skill.name.clone())
                                    .checked(skill.enabled)
                                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                        this.toggle_skill(&toggle, *checked, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(skill.description.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(format!("assistant-config-skill-edit-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Pencil)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.edit_skill(&edit, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("assistant-config-skill-remove-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Trash)
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.remove_skill(&id, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("assistant-config-skill-export-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Download)
                                    .tooltip(t!("assistant.settings.bundle.export"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.export_bundle(bundle.clone(), &file_name, window, cx);
                                    })),
                            ),
                    ),
            );
        }
        for (index, skill) in roles::builtin_skills().iter().enumerate() {
            if assistant.skills.iter().any(|added| added.id == skill.id) {
                continue;
            }
            let id = skill.id.clone();
            skill_list = skill_list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_sm().child(skill.name.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(t!("assistant.settings.skills.suggested")),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(skill.description.clone()),
                            ),
                    )
                    .child(
                        Button::new(format!("assistant-config-skill-add-{index}"))
                            .xsmall()
                            .ghost()
                            .icon(CatalogIcon::Plus)
                            .label(t!("assistant.settings.skills.add"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.add_builtin_skill(&id, cx);
                            })),
                    ),
            );
        }

        let skill_footer = h_flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("assistant-config-skill-save")
                    .small()
                    .primary()
                    .label(if editing_skill {
                        t!("assistant.settings.skills.save")
                    } else {
                        t!("assistant.settings.skills.add_new")
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.save_skill(window, cx))),
            )
            .when(editing_skill, |row| {
                row.child(
                    Button::new("assistant-config-skill-cancel")
                        .small()
                        .label(t!("common.cancel"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.clear_skill_form(window, cx);
                        })),
                )
            });
        let skill_form = Form::new()
            .child(
                Field::new()
                    .label(t!("assistant.settings.skills.name"))
                    .child(Input::new(&self.skill_name).id("assistant-config-skill-name")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.skills.description"))
                    .child(
                        Input::new(&self.skill_description)
                            .id("assistant-config-skill-description"),
                    ),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.skills.instruction"))
                    .description(t!("assistant.settings.skills.instruction.hint"))
                    .child(Textarea::new(&self.skill_instruction).h(px(80.)).text_sm()),
            )
            .footer(skill_footer)
            .into_any_element();

        let mut pack_list = v_flex().w_full().gap_1();
        for (index, pack) in roles::builtin_packs().iter().enumerate() {
            let bundle = Bundle::Pack(pack.clone());
            let file_name = format!("pack-{}.json", index);
            let counts = t!(
                "assistant.settings.packs.counts",
                roles = pack.roles.len(),
                skills = pack.skills.len()
            );
            pack_list = pack_list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_sm().child(pack.name.clone()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(t!("assistant.settings.roles.builtin")),
                                    )
                                    .child(div().text_xs().text_color(muted).child(counts)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(pack.description.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(format!("assistant-config-pack-add-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Plus)
                                    .label(t!("assistant.settings.packs.add"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.add_builtin_pack(index, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!("assistant-config-pack-export-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Download)
                                    .tooltip(t!("assistant.settings.bundle.export"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.export_bundle(bundle.clone(), &file_name, window, cx);
                                    })),
                            ),
                    ),
            );
        }
        if assistant.packs.is_empty() {
            pack_list = pack_list.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.packs.empty")),
            );
        }
        for (index, pack) in assistant.packs.iter().enumerate() {
            let id = pack.id.clone();
            let bundle = Bundle::Pack(pack.clone());
            let file_name = format!("pack-imported-{}.json", index);
            let counts = t!(
                "assistant.settings.packs.counts",
                roles = pack.roles.len(),
                skills = pack.skills.len()
            );
            pack_list = pack_list.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(div().text_sm().child(pack.name.clone()))
                                    .child(div().text_xs().text_color(muted).child(counts)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(pack.description.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(format!("assistant-config-pack-remove-{index}"))
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Trash)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_pack(&id, cx);
                                    })),
                            )
                            .child(
                                Button::new(format!(
                                    "assistant-config-pack-export-imported-{index}"
                                ))
                                .xsmall()
                                .ghost()
                                .icon(CatalogIcon::Download)
                                .tooltip(t!("assistant.settings.bundle.export"))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        this.export_bundle(bundle.clone(), &file_name, window, cx);
                                    },
                                )),
                            ),
                    ),
            );
        }

        let header = |title: SharedString, hint: SharedString| {
            v_flex()
                .gap_1()
                .child(div().text_sm().child(title))
                .child(div().text_xs().text_color(muted).child(hint))
        };
        v_flex()
            .gap_6()
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(header(
                                t!("assistant.settings.roles.title"),
                                t!("assistant.settings.roles.hint"),
                            ))
                            .child(
                                Button::new("assistant-config-role-import")
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Download)
                                    .label(t!("assistant.settings.bundle.import"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.import_bundle(window, cx);
                                    })),
                            ),
                    )
                    .child(role_list)
                    .child(role_form),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(header(
                                t!("assistant.settings.skills.title"),
                                t!("assistant.settings.skills.hint"),
                            ))
                            .child(
                                Button::new("assistant-config-skill-import")
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Download)
                                    .label(t!("assistant.settings.bundle.import"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.import_bundle(window, cx);
                                    })),
                            ),
                    )
                    .child(skill_list)
                    .child(skill_form),
            )
            .child(
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .gap_2()
                            .child(header(
                                t!("assistant.settings.packs.title"),
                                t!("assistant.settings.packs.hint"),
                            ))
                            .child(
                                Button::new("assistant-config-pack-import")
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Download)
                                    .label(t!("assistant.settings.bundle.import"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.import_bundle(window, cx);
                                    })),
                            ),
                    )
                    .child(pack_list),
            )
            .into_any_element()
    }

    fn render_proxy(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let success = cx.theme().success;
        let danger = cx.theme().danger;
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let scopes = SegmentedControl::new("assistant-config-proxy-scope")
            .segments([
                Segment::new(t!("assistant.settings.proxy.scope.default")),
                Segment::new(ProviderProtocol::OpenAi.label()),
                Segment::new(ProviderProtocol::Anthropic.label()),
            ])
            .selected_index(Some(self.proxy_scope.index()))
            .on_change(cx.listener(|this, index: &usize, window, cx| {
                this.proxy_scope = ProxyScope::from_index(*index);
                this.load_proxy(window, cx);
            }));
        let modes = SegmentedControl::new("assistant-config-proxy-mode")
            .segments(ProxyChoice::ALL.map(|choice| Segment::new(choice.label())))
            .selected_index(
                ProxyChoice::ALL
                    .iter()
                    .position(|choice| *choice == self.proxy_mode),
            )
            .on_change(cx.listener(|this, index: &usize, _, cx| {
                if let Some(choice) = ProxyChoice::ALL.get(*index).copied() {
                    this.proxy_mode = choice;
                    cx.notify();
                }
            }));
        let kinds = SegmentedControl::new("assistant-config-proxy-kind")
            .segments(ProxyKind::ALL.map(|kind| Segment::new(kind.label())))
            .selected_index(
                ProxyKind::ALL
                    .iter()
                    .position(|kind| *kind == self.proxy_kind),
            )
            .on_change(cx.listener(|this, index: &usize, _, cx| {
                if let Some(kind) = ProxyKind::ALL.get(*index).copied() {
                    this.proxy_kind = kind;
                    cx.notify();
                }
            }));
        let note = match self.proxy_scope {
            ProxyScope::Default if !self.proxy_enabled => {
                Some(t!("assistant.settings.proxy.default_off").to_string())
            }
            ProxyScope::Provider(_) if self.proxy_mode == ProxyChoice::Default => {
                let url = assistant
                    .proxy_enabled
                    .then(|| assistant.proxy.url())
                    .flatten();
                Some(match url {
                    Some(url) => {
                        t!("assistant.settings.proxy.follows_default", url = url).to_string()
                    }
                    None => t!("assistant.settings.proxy.no_default").to_string(),
                })
            }
            ProxyScope::Provider(_) if self.proxy_mode == ProxyChoice::Off => {
                Some(t!("assistant.settings.proxy.provider_off").to_string())
            }
            _ => None,
        };
        let saved = match self.proxy_scope {
            ProxyScope::Default => assistant.proxy.password_saved,
            ProxyScope::Provider(protocol) => assistant.provider(protocol).proxy.password_saved,
        };
        let mut form = if matches!(self.proxy_scope, ProxyScope::Default) {
            Form::new().child(
                Field::new()
                    .label(t!("assistant.settings.proxy.default_title"))
                    .child(
                        Checkbox::new("assistant-config-proxy-enabled")
                            .label(t!("assistant.settings.proxy.enable"))
                            .checked(self.proxy_enabled)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.proxy_enabled = *checked;
                                cx.notify();
                            })),
                    ),
            )
        } else {
            Form::new().child(
                Field::new()
                    .label(t!("assistant.settings.proxy.mode"))
                    .child(modes),
            )
        };
        form = form
            .child(
                Field::new()
                    .label(t!("assistant.settings.proxy.kind"))
                    .child(kinds),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.proxy.address"))
                    .description(t!("assistant.settings.proxy.address.hint"))
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().flex_1().min_w_0().child(
                                Input::new(&self.proxy_host).id("assistant-config-proxy-host"),
                            ))
                            .child(div().w_24().child(
                                Input::new(&self.proxy_port).id("assistant-config-proxy-port"),
                            )),
                    ),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.proxy.username.label"))
                    .description(t!("assistant.settings.proxy.username.hint"))
                    .child(Input::new(&self.proxy_username).id("assistant-config-proxy-username")),
            )
            .child(
                Field::new()
                    .label(t!("assistant.settings.proxy.password.label"))
                    .description(t!("assistant.settings.proxy.password.hint"))
                    .child(Input::new(&self.proxy_password).id("assistant-config-proxy-password")),
            )
            .footer(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("assistant-config-proxy-save")
                            .small()
                            .primary()
                            .label(t!("assistant.action.save"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_proxy(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("assistant-config-proxy-test")
                            .small()
                            .label(t!("assistant.settings.proxy.test.action"))
                            .on_click(cx.listener(|this, _, _, cx| this.test_proxy(cx))),
                    )
                    .child(div().text_xs().text_color(muted).child(if saved {
                        t!("assistant.settings.proxy.password.saved")
                    } else {
                        t!("assistant.settings.proxy.password.unsaved")
                    })),
            );
        v_flex()
            .id("assistant-config-proxy")
            .gap_3()
            .child(div().text_sm().child(t!("assistant.settings.proxy.title")))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.proxy.hint")),
            )
            .child(scopes)
            .when_some(note, |body, note| {
                body.child(div().text_xs().text_color(muted).child(note))
            })
            .child(form)
            .when_some(self.proxy_result.as_ref(), |body, result| {
                let (text, color) = match result {
                    ProxyCheck::Running => (
                        t!("assistant.settings.proxy.test.running").to_string(),
                        muted,
                    ),
                    ProxyCheck::Passed(status) => (
                        t!("assistant.settings.proxy.test.passed", status = status).to_string(),
                        success,
                    ),
                    ProxyCheck::Failed(message) => (message.to_string(), danger),
                };
                body.child(div().text_xs().text_color(color).child(text))
            })
            .into_any_element()
    }

    fn render_headers(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let mono = cx.theme().mono_font_family.clone();
        let headers: Vec<RequestHeader> = self
            .settings
            .read(cx)
            .settings()
            .assistant
            .active()
            .headers
            .clone();
        let mut rows = v_flex()
            .gap_1()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(border)
            .p_2();
        if headers.is_empty() {
            rows = rows.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.list.empty")),
            );
        }
        for (index, header) in headers.iter().enumerate() {
            rows = rows.child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(mono.clone())
                            .text_xs()
                            .child(format!("{}: {}", header.name, header.value)),
                    )
                    .child(
                        Button::new(("assistant-config-header-remove", index))
                            .xsmall()
                            .ghost()
                            .icon(CatalogIcon::Trash)
                            .tooltip(t!("assistant.settings.list.remove"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.remove_header(index, cx);
                            })),
                    ),
            );
        }
        v_flex()
            .gap_3()
            .child(
                div()
                    .text_sm()
                    .child(t!("assistant.settings.headers.title")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.headers.hint")),
            )
            .child(rows)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.header_name).id("assistant-config-header-name"),
                        ),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.header_value).id("assistant-config-header-value"),
                        ),
                    )
                    .child(
                        Button::new("assistant-config-header-add")
                            .small()
                            .label(t!("assistant.settings.list.add"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_header(window, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_rules(&self, cx: &mut Context<Self>) -> AnyElement {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        v_flex()
            .gap_6()
            .child(self.render_list(
                "assistant-config-blacklist",
                t!("assistant.settings.blacklist.title"),
                t!("assistant.settings.blacklist.hint"),
                &self.blacklist_input,
                assistant.user_blacklist,
                Self::add_blacklist,
                Self::remove_blacklist,
                cx,
            ))
            .child(self.render_list(
                "assistant-config-whitelist",
                t!("assistant.settings.whitelist.title"),
                t!("assistant.settings.whitelist.hint"),
                &self.whitelist_input,
                assistant.user_whitelist,
                Self::add_whitelist,
                Self::remove_whitelist,
                cx,
            ))
            .into_any_element()
    }

    /// One of the two command lists, with its entry box and a remove button
    /// on every entry.
    #[allow(clippy::too_many_arguments)]
    fn render_list(
        &self,
        prefix: &'static str,
        title: SharedString,
        hint: SharedString,
        input: &Entity<InputState>,
        entries: Vec<String>,
        on_add: fn(&mut Self, &mut Window, &mut Context<Self>),
        on_remove: fn(&mut Self, usize, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        let mut rows = v_flex().gap_1();
        if entries.is_empty() {
            rows = rows.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.settings.list.empty")),
            );
        }
        for (index, entry) in entries.iter().enumerate() {
            rows = rows.child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(mono.clone())
                            .text_xs()
                            .child(entry.clone()),
                    )
                    .child(
                        Button::new((prefix, index))
                            .xsmall()
                            .ghost()
                            .icon(CatalogIcon::Trash)
                            .tooltip(t!("assistant.settings.list.remove"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                on_remove(this, index, cx);
                            })),
                    ),
            );
        }
        v_flex()
            .id(prefix)
            .gap_2()
            .child(div().text_sm().child(title))
            .child(div().text_xs().text_color(muted).child(hint))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(input).id(SharedString::from(format!("{prefix}-input"))),
                        ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{prefix}-add")))
                            .small()
                            .label(t!("assistant.settings.list.add"))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                on_add(this, window, cx);
                            })),
                    ),
            )
            .child(rows)
            .into_any_element()
    }

    fn render_classifier(&self, cx: &mut Context<Self>) -> AnyElement {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let mut form = Form::new().child(
            Field::new()
                .label(t!("assistant.settings.classifier.toggle"))
                .description(t!("assistant.settings.classifier.hint"))
                .child(
                    Checkbox::new("assistant-config-classifier")
                        .label(t!("assistant.settings.classifier.toggle"))
                        .checked(assistant.classifier_enabled)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_classifier(*checked, cx);
                        })),
                ),
        );
        form = form.child(
            Field::new()
                .label(t!("assistant.settings.classifier.model"))
                .description(t!("assistant.settings.classifier.model_placeholder"))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&self.classifier_model)
                                    .id("assistant-config-classifier-model"),
                            ),
                        )
                        .child(
                            Button::new("assistant-config-classifier-save")
                                .small()
                                .label(t!("assistant.action.save"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.save_classifier_model(cx)),
                                ),
                        ),
                ),
        );
        form.into_any_element()
    }

    fn render_schedules(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let tasks: Vec<Schedule> = self.panel.read(cx).schedules().to_vec();
        let items: Vec<PendingItem> = self.panel.read(cx).review_items().to_vec();
        let target = self.panel.read(cx).bound_terminals().first().cloned();
        let target_line = match &target {
            Some(terminal) => format!("{} {}", t!("assistant.schedule.target"), terminal.alias),
            None => t!("assistant.schedule.no_target").to_string(),
        };

        let mut list = v_flex().gap_1();
        if tasks.is_empty() {
            list = list.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.schedule.empty")),
            );
        }
        for (index, task) in tasks.iter().enumerate() {
            let id_for_toggle = task.id.clone();
            let id_for_remove = task.id.clone();
            let id_for_rebind = task.id.clone();
            let enabled = task.enabled;
            let due = chrono::DateTime::from_timestamp_millis(task.next_due_at)
                .map(|time| {
                    time.with_timezone(&chrono::Local)
                        .format("%m-%d %H:%M")
                        .to_string()
                })
                .unwrap_or_default();
            list = list.child(
                v_flex()
                    .gap_1()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .child(task.action.text().to_string()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child(task.target.alias.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .items_center()
                            .child(div().text_xs().text_color(muted).child(format!(
                                "{} · {}",
                                t!("assistant.schedule.next", time = due),
                                if task.needs_rebind {
                                    t!("assistant.schedule.paused").to_string()
                                } else if task.enabled {
                                    t!("assistant.schedule.enable").to_string()
                                } else {
                                    t!("assistant.schedule.disable").to_string()
                                }
                            )))
                            .child(
                                Button::new(("assistant-config-schedule-toggle", index))
                                    .xsmall()
                                    .label(if enabled {
                                        t!("assistant.schedule.disable")
                                    } else {
                                        t!("assistant.schedule.enable")
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_schedule(&id_for_toggle, !enabled, cx);
                                    })),
                            )
                            .when(task.needs_rebind, |row| {
                                row.child(
                                    Button::new(("assistant-config-schedule-rebind", index))
                                        .xsmall()
                                        .label(t!("assistant.schedule.rebind_action"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.rebind_schedule(&id_for_rebind, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new(("assistant-config-schedule-remove", index))
                                    .xsmall()
                                    .label(t!("assistant.schedule.remove"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.remove_schedule(&id_for_remove, cx);
                                    })),
                            ),
                    ),
            );
        }

        let mut review = v_flex().gap_1();
        if items.is_empty() {
            review = review.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.pending.empty")),
            );
        }
        for (index, item) in items.iter().enumerate() {
            let reason = match item.reason.as_str() {
                pending::REASON_AI_PROPOSAL => t!("assistant.pending.reason.ai_proposal"),
                pending::REASON_UNRESOLVED => t!("assistant.pending.reason.unresolved"),
                _ => t!("assistant.pending.reason.write"),
            };
            review = review.child(
                v_flex()
                    .gap_1()
                    .py_2()
                    .border_t_1()
                    .border_color(border)
                    .child(div().text_xs().text_color(muted).child(format!(
                        "{} · {}",
                        t!("assistant.pending.from", name = item.name.as_str()),
                        reason
                    )))
                    .child(
                        div()
                            .text_sm()
                            .child(format!("{}: {}", item.alias, item.command)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(("assistant-config-pending-allow", index))
                                    .xsmall()
                                    .primary()
                                    .label(t!("assistant.pending.allow"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.approve_item(index, cx);
                                    })),
                            )
                            .child(
                                Button::new(("assistant-config-pending-deny", index))
                                    .xsmall()
                                    .label(t!("assistant.pending.deny"))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.deny_item(index, cx);
                                    })),
                            ),
                    ),
            );
        }

        v_flex()
            .gap_4()
            .child(div().text_sm().child(t!("assistant.schedule.title")))
            .child(div().text_xs().text_color(muted).child(target_line))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(
                        Input::new(&self.schedule_command).id("assistant-config-schedule-command"),
                    ))
                    .child(
                        div().w_20().child(
                            Input::new(&self.schedule_interval)
                                .id("assistant-config-schedule-interval"),
                        ),
                    )
                    .child(
                        Button::new("assistant-config-schedule-add")
                            .small()
                            .label(t!("assistant.schedule.add"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_schedule(false, window, cx);
                            })),
                    )
                    .child(
                        Button::new("assistant-config-schedule-add-prompt")
                            .small()
                            .label(t!("assistant.schedule.add_prompt"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.add_schedule(true, window, cx);
                            })),
                    ),
            )
            .child(list)
            .child(
                v_flex()
                    .gap_1()
                    .pt_2()
                    .border_t_1()
                    .border_color(border)
                    .child(div().text_sm().child(t!("assistant.pending.title")))
                    .child(review),
            )
            .into_any_element()
    }
}

impl Render for AssistantConfigDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        let muted = cx.theme().muted_foreground;
        let mut tabs = TabBar::new("assistant-config-tabs")
            .underline()
            .small()
            .selected_index(self.tab.index())
            .on_click(cx.listener(|this, index: &usize, _, cx| {
                if let Some(tab) = ConfigTab::ALL.get(*index).copied() {
                    this.tab = tab;
                    this.error = None;
                    this.notice = None;
                    cx.notify();
                }
            }));
        for tab in ConfigTab::ALL {
            tabs = tabs.child(Tab::new().label(tab.label()));
        }
        v_flex()
            .id("assistant-config")
            .gap_3()
            .when_some(self.error.clone(), |body, error| {
                body.child(div().text_sm().text_color(danger).child(error))
            })
            .when_some(self.notice.clone(), |body, notice| {
                body.child(div().text_sm().text_color(muted).child(notice))
            })
            .child(tabs)
            .child(match self.tab {
                ConfigTab::Model => self.render_model(cx),
                ConfigTab::Models => self.render_models(cx),
                ConfigTab::Roles => self.render_roles(cx),
                ConfigTab::Proxy => self.render_proxy(cx),
                ConfigTab::Headers => self.render_headers(cx),
                ConfigTab::Rules => self.render_rules(cx),
                ConfigTab::Classifier => self.render_classifier(cx),
                ConfigTab::Schedules => self.render_schedules(cx),
            })
    }
}

/// Open the settings over whatever panel asked for them.
pub fn open_config_dialog(
    settings: Entity<SettingsStore>,
    panel: Entity<AssistantPanel>,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| AssistantConfigDialog::new(settings, panel, window, cx));
    window.open_dialog(cx, move |dialog, window, _| {
        let viewport = window.viewport_size();
        dialog
            .title(t!("assistant.config.title"))
            .width(px(DIALOG_WIDTH))
            .max_w(viewport.width - px(DIALOG_MARGIN))
            .max_h(viewport.height - px(DIALOG_MARGIN * 2.5))
            .overlay_closable(true)
            .child(view.clone())
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label(t!("common.close")))),
            )
    });
}
