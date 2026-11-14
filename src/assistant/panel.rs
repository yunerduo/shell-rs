//! The assistant's panel: who it talks to, which terminals it may use, and
//! the conversation itself. P0 is conversation only; execution comes in P1.

use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    text::TextView,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::CatalogIcon;
use crate::i18n::t;
use crate::secrets::KeychainSecretStore;
use crate::settings::SettingsStore;

use super::bindings::{AssistantTerminal, Bindings};
use super::chat::{self, ChatMessage};
use super::client::{self, StreamEvent, StreamHandle, StreamRequest};
use super::settings::ProviderProtocol;

/// How often the panel looks at the answer's channel.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// A tool panel in the sidebar. It renders itself; the sidebar only keeps
/// the entity.
pub struct AssistantPanel {
    settings: Entity<SettingsStore>,
    protocol: ProviderProtocol,
    base_url: Entity<InputState>,
    model: Entity<InputState>,
    api_key: Entity<InputState>,
    input: Entity<TextareaState>,
    bindings: Bindings,
    available: Vec<AssistantTerminal>,
    messages: Vec<ChatMessage>,
    stream: Option<StreamHandle>,
    /// The answer was stopped by the user; an empty answer is not an error
    /// then.
    stopped: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    key_note: Option<SharedString>,
    settings_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl AssistantPanel {
    pub fn new(
        settings: Entity<SettingsStore>,
        _dispatch: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let assistant = settings.read(cx).settings().assistant;
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
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .submit_on_enter(true)
                .placeholder(t!("assistant.placeholder"))
        });
        let subscriptions =
            vec![
                cx.observe(&settings, |_, _, cx| cx.notify()),
                cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                    match event {
                        InputEvent::PressEnter { shift: false, .. } => this.send(window, cx),
                        InputEvent::Change => cx.notify(),
                        _ => {}
                    }
                }),
            ];
        Self {
            settings,
            protocol,
            base_url,
            model,
            api_key,
            input,
            bindings: Bindings::default(),
            available: Vec::new(),
            messages: Vec::new(),
            stream: None,
            stopped: false,
            error: None,
            notice: None,
            key_note: None,
            settings_open: false,
            _subscriptions: subscriptions,
        }
    }

    /// Which terminals are open, and which one is in front. The front one
    /// joins an empty conversation on its own.
    pub fn set_terminals(
        &mut self,
        terminals: Vec<AssistantTerminal>,
        front: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.bindings.sync(&terminals);
        let front = front
            .as_deref()
            .and_then(|id| terminals.iter().find(|terminal| terminal.id == id));
        if front.is_some() {
            self.bindings.follow_front(front);
        }
        self.available = terminals;
        cx.notify();
    }

    fn toggle_terminal(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(terminal) = self.available.iter().find(|terminal| terminal.id == id) else {
            return;
        };
        self.bindings.toggle(terminal);
        self.error = None;
        cx.notify();
    }

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    fn switch_protocol(
        &mut self,
        protocol: ProviderProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.protocol = protocol;
        let assistant = self.settings.read(cx).settings().assistant;
        let provider = assistant.provider(protocol).clone();
        self.base_url.update(cx, |input, cx| {
            input.set_value(provider.base_url, window, cx)
        });
        self.model
            .update(cx, |input, cx| input.set_value(provider.model, window, cx));
        self.api_key
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.key_note = None;
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.protocol = protocol, cx)
        });
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
            self.key_note = Some(t!("assistant.settings.key_kept"));
            cx.notify();
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
        let key = zeroize::Zeroizing::new(key);
        self.key_note = Some(t!("assistant.settings.key_saved"));
        cx.spawn(async move |this, cx| {
            let written = cx
                .background_executor()
                .spawn(async move { KeychainSecretStore::write_account(&account, &key) })
                .await;
            this.update(cx, |this, cx| {
                this.key_note = Some(match written {
                    Ok(()) => t!("assistant.settings.key_saved"),
                    Err(error) => t!("assistant.settings.key_failed", error = error.to_string()),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Ask the model. The panel keeps the conversation; the worker keeps
    /// the key.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stream.is_some() {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        if self.bindings.is_empty() {
            self.error = Some(t!("assistant.error.no_terminal"));
            cx.notify();
            return;
        }
        let assistant = self.settings.read(cx).settings().assistant;
        let provider = assistant.active().clone();
        if provider.base_url.trim().is_empty() || provider.model.trim().is_empty() {
            self.error = Some(t!("assistant.error.no_provider"));
            self.settings_open = true;
            cx.notify();
            return;
        }

        self.messages.push(ChatMessage::user(text));
        self.messages.push(ChatMessage::assistant(String::new()));
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        self.notice = None;
        self.stopped = false;

        let request = StreamRequest {
            protocol: assistant.protocol,
            base_url: provider.base_url,
            model: provider.model,
            keychain_id: provider.keychain_id,
            messages: self.messages[..self.messages.len() - 1].to_vec(),
            max_tokens: provider.max_tokens,
            timeout: Duration::from_secs(assistant.timeout_seconds),
        };
        self.stream = Some(client::start(request));
        self.begin_poll(cx);
        cx.notify();
    }

    fn begin_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let finished = this.update(cx, |this, cx| this.poll(cx)).unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// Take what the worker sent since the last look; `true` when the
    /// answer is over, however it ended.
    fn poll(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(stream) = &self.stream else {
            return true;
        };
        let mut finished = false;
        let mut changed = false;
        loop {
            match client::next_event(&stream.rx) {
                Some(StreamEvent::Delta(text)) => {
                    if let Some(message) = self.messages.last_mut() {
                        message.text.push_str(&text);
                        changed = true;
                    }
                }
                Some(StreamEvent::Done) => {
                    finished = true;
                    break;
                }
                Some(StreamEvent::Failed(error)) => {
                    self.error = Some(error.message());
                    finished = true;
                    break;
                }
                None => break,
            }
        }
        if finished {
            self.stream = None;
            let empty = self
                .messages
                .last()
                .is_some_and(|message| message.text.trim().is_empty());
            if empty && self.error.is_none() && !self.stopped {
                self.error = Some(t!("assistant.error.empty"));
            }
            cx.notify();
        } else if changed {
            cx.notify();
        }
        finished
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(stream) = self.stream.take() {
            stream.cancel();
        }
        self.stopped = true;
        self.notice = Some(t!("assistant.error.cancelled"));
        cx.notify();
    }

    fn copy_markdown(&mut self, cx: &mut Context<Self>) {
        let markdown = chat::to_markdown(&self.messages, &t!("assistant.role.user"));
        if markdown.is_empty() {
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(markdown));
        self.notice = Some(t!("assistant.action.copied"));
        cx.notify();
    }

    fn render_terminals(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let mut row = h_flex()
            .id("assistant-terminals")
            .flex_wrap()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.terminals.label")),
            );
        if self.available.is_empty() {
            return row.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.empty_terminals")),
            );
        }
        for (index, terminal) in self.available.iter().enumerate() {
            let bound = self.bindings.is_bound(&terminal.id);
            let label = self
                .bindings
                .entries()
                .iter()
                .find(|entry| entry.id == terminal.id)
                .map_or_else(|| terminal.title.clone(), |entry| entry.alias.clone());
            let id = terminal.id.clone();
            row = row.child(
                Button::new(("assistant-terminal", index))
                    .xsmall()
                    .selected(bound)
                    .tooltip(terminal.title.clone())
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_terminal(&id, cx))),
            );
        }
        row
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let mut protocols = h_flex().gap_1();
        for (index, protocol) in ProviderProtocol::ALL.into_iter().enumerate() {
            protocols = protocols.child(
                Button::new(("assistant-protocol", index))
                    .xsmall()
                    .selected(self.protocol == protocol)
                    .label(protocol.label())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.switch_protocol(protocol, window, cx)
                    })),
            );
        }
        v_flex()
            .id("assistant-settings")
            .gap_2()
            .px_2()
            .py_2()
            .border_b_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("assistant.settings.protocol")),
                    )
                    .child(protocols),
            )
            .child(Input::new(&self.base_url).id("assistant-base-url").small())
            .child(Input::new(&self.model).id("assistant-model").small())
            .child(Input::new(&self.api_key).id("assistant-api-key").small())
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("assistant.settings.hint")),
                    )
                    .child(
                        Button::new("assistant-save")
                            .xsmall()
                            .primary()
                            .label(t!("assistant.action.save"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.save_provider(window, cx)),
                            ),
                    ),
            )
            .when_some(self.key_note.clone(), |form, note| {
                form.child(div().text_xs().text_color(muted).child(note))
            })
    }

    fn render_messages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let streaming = self.stream.is_some();
        let mut list = v_flex()
            .id("assistant-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap_3()
            .px_2()
            .py_2();
        if self.messages.is_empty() {
            return list.child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("assistant.empty_state")),
            );
        }
        for (index, message) in self.messages.iter().enumerate() {
            let role = match message.role {
                chat::Role::User => t!("assistant.role.user"),
                chat::Role::Assistant | chat::Role::System => t!("assistant.role.assistant"),
            };
            let mut turn = v_flex()
                .id(("assistant-turn", index))
                .w_full()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(role));
            if message.role == chat::Role::Assistant && message.text.is_empty() {
                if streaming {
                    turn = turn.child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(t!("assistant.streaming")),
                    );
                }
            } else if message.role == chat::Role::Assistant {
                turn = turn.child(
                    TextView::markdown(("assistant-markdown", index), message.text.clone())
                        .selectable(true),
                );
            } else {
                turn = turn.child(div().text_sm().child(message.text.clone()));
            }
            list = list.child(turn);
        }
        list
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let streaming = self.stream.is_some();
        let empty = self.input.read(cx).value().trim().is_empty();
        v_flex()
            .id("assistant-composer")
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(border)
            .child(
                Textarea::new(&self.input)
                    .h(px(72.))
                    .text_sm()
                    .aria_label(t!("assistant.placeholder")),
            )
            .child(h_flex().justify_end().gap_1().child(if streaming {
                Button::new("assistant-stop")
                    .small()
                    .icon(CatalogIcon::CircleStop)
                    .label(t!("assistant.action.stop"))
                    .on_click(cx.listener(|this, _, _, cx| this.stop(cx)))
            } else {
                Button::new("assistant-send")
                    .small()
                    .primary()
                    .icon(CatalogIcon::Send)
                    .label(t!("assistant.action.send"))
                    .disabled(empty)
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
            }))
    }
}

impl Render for AssistantPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let danger = cx.theme().danger;
        v_flex()
            .id("assistant-panel")
            .size_full()
            .min_h_0()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .min_w_0()
                            .child(Icon::new(CatalogIcon::Bot).small())
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(t!("assistant.title")),
                            )
                            .when_some(self.notice.clone(), |header, notice| {
                                header.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(notice),
                                )
                            }),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("assistant-copy")
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Copy)
                                    .tooltip(t!("assistant.action.copy"))
                                    .disabled(self.messages.is_empty())
                                    .on_click(cx.listener(|this, _, _, cx| this.copy_markdown(cx))),
                            )
                            .child(
                                Button::new("assistant-settings-toggle")
                                    .xsmall()
                                    .ghost()
                                    .selected(self.settings_open)
                                    .icon(CatalogIcon::Settings)
                                    .tooltip(t!("assistant.action.settings"))
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.toggle_settings(cx)),
                                    ),
                            ),
                    ),
            )
            .child(self.render_terminals(cx))
            .when(self.settings_open, |panel| {
                panel.child(self.render_settings(cx))
            })
            .when_some(self.error.clone(), |panel, error| {
                panel.child(
                    div()
                        .id("assistant-error")
                        .px_2()
                        .py_1()
                        .text_sm()
                        .text_color(danger)
                        .child(error),
                )
            })
            .child(self.render_messages(cx))
            .child(self.render_composer(cx))
    }
}
