//! The assistant's panel: who it talks to, which terminals it may use, and
//! the conversation itself. From P1 on it also runs what the user's policy
//! allows, one command at a time, with a card for anything not whitelisted.

use std::collections::VecDeque;
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
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
use crate::terminal::{
    ExecDetailResult, RunFailure, RunHandle, RunProgress, RunTiming, TerminalEvent, TerminalRun,
    TerminalView,
};

use super::audit;
use super::bindings::{AssistantTerminal, Bindings};
use super::chat::{self, ChatMessage, RunCommandArgs, RunMode, ToolCall};
use super::client::{self, StreamEvent, StreamHandle, StreamRequest};
use super::inject::{self, InjectOutcome, Route};
use super::policy::{self, AuthorizationLevel, Decision, Policy, Risk, Verdict};
use super::protocol::ToolTerminal;
use super::settings::ProviderProtocol;
use super::tool_result::{
    call_line, denied_result, tool_card_parts, tool_error, tool_result_body, unsupported_result,
};

/// How often the panel looks at the answer's channel.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long one command may run beside the shell, and how much of its two
/// streams is kept. The transport clamps both.
const EXEC_TIMEOUT: Duration = Duration::from_secs(30);
const EXEC_LIMIT: usize = 256 * 1024;
/// How much of a typed command's output the conversation keeps. The
/// terminal has all of it anyway.
const INJECT_LIMIT: usize = 256 * 1024;

/// How many model turns one exchange may take. Eight is enough for a
/// question, a look, a follow-up look, and an answer, with room to spare;
/// more than that is a loop, not a conversation.
const MAX_ROUNDS: u32 = 8;

/// A call waiting for the user's answer. Its command is the one the card
/// showed; the hash is checked again before it runs.
struct PendingCall {
    call: ToolCall,
    alias: String,
    terminal: AssistantTerminal,
    command: String,
    decision: Decision,
    hash: String,
    /// The call asked for a background run: only injection can start one.
    background: bool,
    /// The card's takeover checkbox, unchecked until the user clicks it.
    takeover: bool,
}

/// What a call resolved to before it runs: where, what, and how it was
/// judged.
struct Granted {
    call_id: String,
    alias: String,
    terminal: AssistantTerminal,
    command: String,
    decision: Decision,
    approver: &'static str,
}

/// One command running beside the shell now.
struct ExecRun {
    call_id: String,
    alias: String,
    decision: Decision,
    approver: &'static str,
    receiver: std::sync::mpsc::Receiver<ExecDetailResult>,
}

/// One command typed into a terminal now.
struct InjectRun {
    call_id: String,
    alias: String,
    terminal_id: String,
    decision: Decision,
    approver: &'static str,
    /// Kept here so the stop button and the user's own typing can send ^C.
    handle: RunHandle,
    receiver: std::sync::mpsc::Receiver<InjectMessage>,
    cancelled_by: Option<&'static str>,
}

/// What the injection thread sends back once.
enum InjectMessage {
    /// The run ended, or never started.
    Done(InjectOutcome),
    /// The command is running; the terminal keeps it, and the panel keeps
    /// the handle so the user can stop it.
    Background(RunHandle, InjectOutcome),
}

/// A command left running in a terminal by the background mode.
struct BackgroundRun {
    alias: String,
    terminal_id: String,
    handle: RunHandle,
}

/// The parts of one command list's section in the settings.
struct ListSection<'a> {
    prefix: &'static str,
    title: SharedString,
    hint: SharedString,
    input: &'a Entity<InputState>,
    entries: &'a [String],
    on_add: fn(&mut AssistantPanel, &mut Window, &mut Context<AssistantPanel>),
    on_remove: fn(&mut AssistantPanel, usize, &mut Context<AssistantPanel>),
}

/// A tool panel in the sidebar. It renders itself; the sidebar only keeps
/// the entity.
pub struct AssistantPanel {
    settings: Entity<SettingsStore>,
    protocol: ProviderProtocol,
    base_url: Entity<InputState>,
    model: Entity<InputState>,
    api_key: Entity<InputState>,
    input: Entity<TextareaState>,
    blacklist_input: Entity<InputState>,
    whitelist_input: Entity<InputState>,
    bindings: Bindings,
    available: Vec<AssistantTerminal>,
    messages: Vec<ChatMessage>,
    stream: Option<StreamHandle>,
    /// The calls of this exchange not yet handled.
    queue: VecDeque<ToolCall>,
    /// Calls waiting for the user, oldest first.
    pending: VecDeque<PendingCall>,
    /// The command running now.
    exec: Option<ExecRun>,
    /// The command typed into a terminal now.
    inject: Option<InjectRun>,
    /// Commands the background mode left running.
    backgrounds: Vec<BackgroundRun>,
    /// Terminal views whose input the panel already watches.
    watched: Vec<String>,
    /// Model turns used by this exchange.
    rounds: u32,
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
                cx.subscribe_in(
                    &blacklist_input,
                    window,
                    |this, _, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                            this.add_blacklist(window, cx);
                        }
                    },
                ),
                cx.subscribe_in(
                    &whitelist_input,
                    window,
                    |this, _, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                            this.add_whitelist(window, cx);
                        }
                    },
                ),
            ];
        Self {
            settings,
            protocol,
            base_url,
            model,
            api_key,
            input,
            blacklist_input,
            whitelist_input,
            bindings: Bindings::default(),
            available: Vec::new(),
            messages: Vec::new(),
            stream: None,
            queue: VecDeque::new(),
            pending: VecDeque::new(),
            exec: None,
            inject: None,
            backgrounds: Vec::new(),
            watched: Vec::new(),
            rounds: 0,
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
        // A user's own typing takes the terminal back from an injection; the
        // panel hears it from the view, the same way the view hears the user.
        let views: Vec<(String, Entity<TerminalView>)> = self
            .available
            .iter()
            .filter_map(|terminal| {
                let view = terminal.view.as_ref()?.upgrade()?;
                Some((terminal.id.clone(), view))
            })
            .collect();
        for (id, view) in views {
            self.watch_input(&id, &view, cx);
        }
        cx.notify();
    }

    fn watch_input(&mut self, id: &str, view: &Entity<TerminalView>, cx: &mut Context<Self>) {
        if self.watched.iter().any(|watched| watched == id) {
            return;
        }
        self.watched.push(id.to_string());
        let terminal_id = id.to_string();
        let subscription = cx.subscribe(view, move |this, _, event: &TerminalEvent, cx| {
            if matches!(event, TerminalEvent::Input(_)) {
                this.user_took_over(&terminal_id, cx);
            }
        });
        self._subscriptions.push(subscription);
    }

    /// The user typed in the terminal the assistant was using: the takeover
    /// is over. A foreground run gets its ^C; a background one is left
    /// alone, because stopping it is the user's own choice on the card.
    fn user_took_over(&mut self, terminal_id: &str, cx: &mut Context<Self>) {
        let mut taken = false;
        if let Some(run) = self
            .inject
            .as_mut()
            .filter(|run| run.terminal_id == terminal_id)
        {
            run.cancelled_by = Some("user");
            run.handle.cancel();
            taken = true;
        }
        if !self.queue.is_empty() {
            // Calls not begun would run after the user took the terminal
            // back; they are dropped, not retried.
            self.queue.clear();
            taken = true;
        }
        if let Some(background) = self
            .backgrounds
            .iter()
            .find(|background| background.terminal_id == terminal_id)
        {
            let alias = background.alias.clone();
            self.notice = Some(t!("assistant.tool.takeover_closed", alias = alias));
            taken = true;
        }
        if taken {
            cx.notify();
        }
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
    /// the key. A question starts a new exchange, so the round count starts
    /// again.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stream.is_some()
            || self.exec.is_some()
            || self.inject.is_some()
            || !self.pending.is_empty()
        {
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
        self.queue.clear();
        self.pending.clear();
        self.rounds = 0;
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        self.notice = None;
        self.stopped = false;

        self.request_round(cx);
        cx.notify();
    }

    /// Ask for one more model turn. Nothing is asked while a command is
    /// running or a card is waiting: the answer would have no result to
    /// read yet.
    fn request_round(&mut self, cx: &mut Context<Self>) {
        if self.stream.is_some()
            || self.exec.is_some()
            || self.inject.is_some()
            || !self.pending.is_empty()
            || !self.queue.is_empty()
        {
            return;
        }
        if self.rounds >= MAX_ROUNDS {
            self.notice = Some(t!("assistant.tool.round_limit", rounds = MAX_ROUNDS));
            return;
        }
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let provider = assistant.active().clone();
        if provider.base_url.trim().is_empty() || provider.model.trim().is_empty() {
            self.error = Some(t!("assistant.error.no_provider"));
            self.settings_open = true;
            return;
        }
        self.rounds += 1;
        self.messages.push(ChatMessage::assistant(String::new()));
        let request = StreamRequest {
            protocol: assistant.protocol,
            base_url: provider.base_url,
            model: provider.model,
            keychain_id: provider.keychain_id,
            messages: self.messages[..self.messages.len() - 1].to_vec(),
            max_tokens: provider.max_tokens,
            timeout: Duration::from_secs(assistant.timeout_seconds),
            terminals: self
                .bindings
                .entries()
                .iter()
                .map(|entry| ToolTerminal {
                    alias: entry.alias.clone(),
                    title: entry.title.clone(),
                })
                .collect(),
        };
        self.stream = Some(client::start(request));
        self.begin_poll(cx);
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
                Some(StreamEvent::ToolCalls(calls)) => {
                    if let Some(message) = self.messages.last_mut() {
                        message.tool_calls.clone_from(&calls);
                    }
                    self.queue.extend(calls);
                    changed = true;
                }
                Some(StreamEvent::Done) => {
                    finished = true;
                    break;
                }
                Some(StreamEvent::Failed(error)) => {
                    self.error = Some(error.message());
                    // A turn that failed asks for nothing: running calls it
                    // got out would only start a new turn on a broken one.
                    self.queue.clear();
                    finished = true;
                    break;
                }
                None => break,
            }
        }
        if finished {
            self.stream = None;
            let empty = self.messages.last().is_some_and(|message| {
                message.text.trim().is_empty() && message.tool_calls.is_empty()
            });
            if empty && self.error.is_none() && !self.stopped {
                self.error = Some(t!("assistant.error.empty"));
            }
            if !self.queue.is_empty() {
                self.advance(cx);
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

    /// Handle the next queued call, or let the model answer now that every
    /// result is in the conversation.
    fn advance(&mut self, cx: &mut Context<Self>) {
        if let Some(call) = self.queue.pop_front() {
            self.handle_call(call, cx);
            return;
        }
        if self.exec.is_some()
            || self.inject.is_some()
            || !self.pending.is_empty()
            || self.stream.is_some()
        {
            return;
        }
        self.request_round(cx);
        cx.notify();
    }

    /// Judge one call and either refuse it, ask, or run it.
    fn handle_call(&mut self, call: ToolCall, cx: &mut Context<Self>) {
        if call.name != "run_command" {
            self.push_result(call.id, tool_error("unknown_tool"));
            self.advance(cx);
            return;
        }
        let Ok(args) = serde_json::from_str::<RunCommandArgs>(&call.arguments) else {
            self.push_result(call.id, tool_error("bad_arguments"));
            self.advance(cx);
            return;
        };
        // The model names an alias; the panel resolves it to the terminal
        // the user bound, and to nothing else.
        let Some(entry) = self
            .bindings
            .entries()
            .iter()
            .find(|entry| entry.alias == args.terminal)
            .cloned()
        else {
            self.push_result(call.id, tool_error("unknown_terminal"));
            self.advance(cx);
            return;
        };
        let Some(terminal) = self
            .available
            .iter()
            .find(|terminal| terminal.id == entry.id)
            .cloned()
        else {
            self.push_result(call.id, tool_error("terminal_closed"));
            self.advance(cx);
            return;
        };

        let assistant = self.settings.read(cx).settings().assistant.clone();
        let policy = Policy::new(assistant.user_blacklist, assistant.user_whitelist);
        let decision = policy::decide(&args.command, AuthorizationLevel::Restricted, &policy);
        let hash = audit::command_hash(&args.command);
        let background = args.mode == RunMode::Background;
        match decision.verdict {
            Verdict::Deny => {
                self.record(&entry.alias, &decision, "denied", None, "policy");
                self.push_result(call.id, denied_result(&decision));
                self.advance(cx);
            }
            Verdict::Confirm => {
                self.pending.push_back(PendingCall {
                    call,
                    alias: entry.alias,
                    terminal,
                    command: args.command,
                    decision,
                    hash,
                    background,
                    takeover: false,
                });
                cx.notify();
            }
            Verdict::Auto => {
                match inject::route(
                    background,
                    terminal.can_exec(),
                    terminal.can_inject(),
                    false,
                ) {
                    Route::Exec => self.execute(
                        Granted {
                            call_id: call.id,
                            alias: entry.alias,
                            terminal,
                            command: args.command,
                            decision,
                            approver: "auto",
                        },
                        cx,
                    ),
                    // A background run, and a terminal with no channel
                    // beside the shell, need the takeover card even when
                    // the policy let the command run alone.
                    Route::NeedTakeover => {
                        self.pending.push_back(PendingCall {
                            call,
                            alias: entry.alias,
                            terminal,
                            command: args.command,
                            decision,
                            hash,
                            background,
                            takeover: false,
                        });
                        cx.notify();
                    }
                    Route::Unavailable(reason) => {
                        self.record(&entry.alias, &decision, "unsupported", None, "auto");
                        self.push_result(call.id, unsupported_result(reason));
                        self.advance(cx);
                    }
                    Route::Inject => unreachable!("no takeover was given"),
                }
            }
        }
    }

    /// Run one command beside the shell and watch its channel.
    fn execute(&mut self, granted: Granted, cx: &mut Context<Self>) {
        let Granted {
            call_id,
            alias,
            terminal,
            command,
            decision,
            approver,
        } = granted;
        let Some(view) = terminal
            .view
            .as_ref()
            .filter(|_| !terminal.shell_only)
            .and_then(|weak| weak.upgrade())
        else {
            let reason = if terminal.shell_only {
                "shell_only"
            } else {
                "closed"
            };
            self.record(&alias, &decision, "unsupported", None, approver);
            self.push_result(call_id, unsupported_result(reason));
            self.advance(cx);
            return;
        };
        let Some(receiver) = view
            .read(cx)
            .exec_detail(command, EXEC_TIMEOUT, EXEC_LIMIT, cx)
        else {
            self.record(&alias, &decision, "unsupported", None, approver);
            self.push_result(call_id, unsupported_result("not_running"));
            self.advance(cx);
            return;
        };
        self.exec = Some(ExecRun {
            call_id,
            alias,
            decision,
            approver,
            receiver,
        });
        self.begin_exec_poll(cx);
        cx.notify();
    }

    fn approve_pending(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.pop_front() else {
            return;
        };
        // The card showed a command; the one that runs must be that one.
        if audit::command_hash(&pending.command) != pending.hash {
            self.record(&pending.alias, &pending.decision, "denied", None, "user");
            self.push_result(pending.call.id, tool_error("command_changed"));
            self.advance(cx);
            cx.notify();
            return;
        }
        let background = pending.background;
        let route = inject::route(
            background,
            pending.terminal.can_exec(),
            pending.terminal.can_inject(),
            pending.takeover,
        );
        if matches!(route, Route::NeedTakeover | Route::Unavailable(_)) {
            // The card's allow button is disabled until the box is ticked;
            // keep the card rather than run anything silently.
            self.pending.push_front(pending);
            self.notice = Some(t!("assistant.tool.takeover.required"));
            cx.notify();
            return;
        }
        let granted = Granted {
            call_id: pending.call.id,
            alias: pending.alias,
            terminal: pending.terminal,
            command: pending.command,
            decision: pending.decision,
            approver: "user",
        };
        match route {
            Route::Exec => self.execute(granted, cx),
            Route::Inject => self.execute_inject(granted, background, cx),
            Route::NeedTakeover | Route::Unavailable(_) => unreachable!("both were handled above"),
        }
    }

    /// Tick the takeover authorization of the card in front. It is per call,
    /// and it starts unchecked every time.
    fn set_takeover(&mut self, takeover: bool, cx: &mut Context<Self>) {
        if let Some(pending) = self.pending.front_mut()
            && pending.terminal.can_inject()
        {
            pending.takeover = takeover;
            cx.notify();
        }
    }

    fn deny_pending(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.pop_front() else {
            return;
        };
        self.record(&pending.alias, &pending.decision, "denied", None, "user");
        self.push_result(pending.call.id, denied_result(&pending.decision));
        self.advance(cx);
        cx.notify();
    }

    fn push_result(&mut self, call_id: String, body: String) {
        self.messages.push(ChatMessage::tool_result(call_id, body));
    }

    /// Write one decision to the trail. A failed write is a notice: the
    /// command the user approved still runs.
    fn record(
        &mut self,
        alias: &str,
        decision: &Decision,
        channel: &str,
        exit_code: Option<i32>,
        approver: &str,
    ) {
        let entry = audit::AuditEntry {
            alias,
            command: &decision.masked_command,
            risk: decision.risk.name(),
            rule: decision.rule.name(),
            channel,
            exit_code,
            level: "restricted",
            approver,
            override_reason: None,
        };
        if audit::append(&entry).is_err() {
            self.notice = Some(t!("assistant.tool.audit_failed"));
        }
    }

    fn begin_exec_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let finished = this
                    .update(cx, |this, cx| this.poll_exec(cx))
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// Take the command's answer once it is there; `true` when there is
    /// nothing more to wait for.
    fn poll_exec(&mut self, cx: &mut Context<Self>) -> bool {
        let result = match self.exec.as_ref() {
            None => return true,
            Some(run) => match run.receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => {
                    Err(t!("assistant.tool.transport_closed").to_string())
                }
            },
        };
        let Some(run) = self.exec.take() else {
            return true;
        };
        let (body, exit_code) = tool_result_body(result);
        self.record(&run.alias, &run.decision, "exec", exit_code, run.approver);
        self.push_result(run.call_id, body);
        self.advance(cx);
        cx.notify();
        true
    }

    /// Type one command into its terminal. The user has already authorized
    /// the takeover on the card; nothing here sends ^C until the view's own
    /// run gate accepts, and the command only exists in the terminal.
    fn execute_inject(&mut self, granted: Granted, background: bool, cx: &mut Context<Self>) {
        let Granted {
            call_id,
            alias,
            terminal,
            command,
            decision,
            approver,
        } = granted;
        let Some(view) = terminal.view.as_ref().and_then(|weak| weak.upgrade()) else {
            self.record(&alias, &decision, "unsupported", None, approver);
            self.push_result(call_id, unsupported_result("takeover_closed"));
            self.advance(cx);
            cx.notify();
            return;
        };
        let (run, events) = TerminalRun::new(command);
        let handle = run.handle();
        if let Err(failure) = view.read(cx).run_command(handle.clone(), cx) {
            // The run never typed anything; dropping it cancels nothing.
            self.record(&alias, &decision, "unsupported", None, approver);
            self.push_result(call_id, inject::failure_body(&failure));
            self.advance(cx);
            cx.notify();
            return;
        }
        let terminal_id = terminal.id;
        let thread_handle = handle.clone();
        let (tx, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = if background {
                match run.drive_background(&events, RunTiming::STANDARD) {
                    Ok(context) => {
                        let outcome = InjectOutcome {
                            context: Some(context),
                            background: true,
                            ..InjectOutcome::default()
                        };
                        let _ = tx.send(InjectMessage::Background(thread_handle, outcome));
                        return;
                    }
                    Err(failure) => InjectOutcome {
                        failure: Some(failure),
                        background: true,
                        ..InjectOutcome::default()
                    },
                }
            } else {
                let mut context = None;
                let mut output = Vec::new();
                let result = run.drive(&events, RunTiming::STANDARD, &mut |progress| {
                    match progress {
                        RunProgress::Context(found) => context = Some(found),
                        RunProgress::Output(bytes) => {
                            let room = INJECT_LIMIT.saturating_sub(output.len());
                            if room > 0 {
                                output.extend_from_slice(&bytes[..bytes.len().min(room)]);
                            }
                        }
                        RunProgress::Idle => {}
                    }
                    Ok(())
                });
                match result {
                    Ok(code) => InjectOutcome {
                        context,
                        output,
                        exit_code: Some(code),
                        ..InjectOutcome::default()
                    },
                    Err(failure) => InjectOutcome {
                        context,
                        output,
                        failure: Some(failure),
                        ..InjectOutcome::default()
                    },
                }
            };
            let _ = tx.send(InjectMessage::Done(outcome));
        });
        self.inject = Some(InjectRun {
            call_id,
            alias,
            terminal_id,
            decision,
            approver,
            handle,
            receiver,
            cancelled_by: None,
        });
        self.begin_inject_poll(cx);
        cx.notify();
    }

    /// Stop the foreground injection with a ^C. Its result comes back like
    /// any other, marked with who stopped it.
    fn stop_inject(&mut self, cx: &mut Context<Self>) {
        let Some(run) = self.inject.as_mut() else {
            return;
        };
        run.cancelled_by = Some("stop");
        run.handle.cancel();
        cx.notify();
    }

    fn stop_background(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.backgrounds.len() {
            return;
        }
        let background = self.backgrounds.remove(index);
        background.handle.cancel();
        self.notice = Some(t!(
            "assistant.tool.background_stopped",
            alias = background.alias
        ));
        cx.notify();
    }

    fn begin_inject_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let finished = this
                    .update(cx, |this, cx| this.poll_inject(cx))
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// Take the injection's outcome once it is there; `true` when there is
    /// nothing more to wait for.
    fn poll_inject(&mut self, cx: &mut Context<Self>) -> bool {
        let message = match self.inject.as_ref() {
            None => return true,
            Some(run) => match run.receiver.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => InjectMessage::Done(InjectOutcome {
                    failure: Some(RunFailure::Disconnected),
                    ..InjectOutcome::default()
                }),
            },
        };
        let Some(run) = self.inject.take() else {
            return true;
        };
        let (mut outcome, finished_handle) = match message {
            InjectMessage::Done(outcome) => (outcome, None),
            InjectMessage::Background(handle, outcome) => (outcome, Some(handle)),
        };
        outcome.cancelled_by = run.cancelled_by;
        let channel = if finished_handle.is_some() {
            "inject-background"
        } else {
            "inject"
        };
        self.record(
            &run.alias,
            &run.decision,
            channel,
            outcome.exit_code,
            run.approver,
        );
        self.push_result(run.call_id, outcome.body());
        if let Some(handle) = finished_handle {
            self.backgrounds.push(BackgroundRun {
                alias: run.alias,
                terminal_id: run.terminal_id,
                handle,
            });
        }
        self.advance(cx);
        cx.notify();
        true
    }

    fn add_blacklist(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.blacklist_input.read(cx).value().to_string();
        let Some(entry) = policy::clean_blacklist_entry(&value) else {
            self.error = Some(t!("assistant.settings.list.invalid"));
            cx.notify();
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
            self.error = Some(t!("assistant.settings.list.invalid"));
            cx.notify();
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
        // Read the lists once: the panel is asked to redraw while they are
        // edited, and the borrow must not outlive the look.
        let assistant = self.settings.read(cx).settings().assistant.clone();
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
            .child(self.render_list(
                ListSection {
                    prefix: "assistant-blacklist",
                    title: t!("assistant.settings.blacklist.title"),
                    hint: t!("assistant.settings.blacklist.hint"),
                    input: &self.blacklist_input,
                    entries: &assistant.user_blacklist,
                    on_add: Self::add_blacklist,
                    on_remove: Self::remove_blacklist,
                },
                cx,
            ))
            .child(self.render_list(
                ListSection {
                    prefix: "assistant-whitelist",
                    title: t!("assistant.settings.whitelist.title"),
                    hint: t!("assistant.settings.whitelist.hint"),
                    input: &self.whitelist_input,
                    entries: &assistant.user_whitelist,
                    on_add: Self::add_whitelist,
                    on_remove: Self::remove_whitelist,
                },
                cx,
            ))
    }

    /// One of the two command lists, with its entry box and a remove button
    /// on every entry.
    fn render_list(&self, section: ListSection<'_>, cx: &mut Context<Self>) -> impl IntoElement {
        let ListSection {
            prefix,
            title,
            hint,
            input,
            entries,
            on_add,
            on_remove,
        } = section;
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
                            .on_click(
                                cx.listener(move |this, _, _, cx| on_remove(this, index, cx)),
                            ),
                    ),
            );
        }
        v_flex()
            .id(prefix)
            .gap_1()
            .child(div().text_xs().text_color(muted).child(title))
            .child(div().text_xs().text_color(muted).child(hint))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(input)
                                .id(SharedString::from(format!("{prefix}-input")))
                                .small(),
                        ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{prefix}-add")))
                            .xsmall()
                            .label(t!("assistant.settings.list.add"))
                            .on_click(
                                cx.listener(move |this, _, window, cx| on_add(this, window, cx)),
                            ),
                    ),
            )
            .child(rows)
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
                chat::Role::Tool => t!("assistant.role.tool"),
            };
            let mut turn = v_flex()
                .id(("assistant-turn", index))
                .w_full()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(role));
            match message.role {
                chat::Role::Tool => {
                    let (summary, body) = tool_card_parts(&message.text);
                    turn = turn
                        .child(div().text_xs().text_color(muted).child(summary))
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_xs()
                                .child(body),
                        );
                }
                chat::Role::Assistant
                    if message.text.is_empty() && message.tool_calls.is_empty() =>
                {
                    if streaming {
                        turn = turn.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(t!("assistant.streaming")),
                        );
                    }
                }
                chat::Role::Assistant => {
                    if !message.text.is_empty() {
                        turn = turn.child(
                            TextView::markdown(("assistant-markdown", index), message.text.clone())
                                .selectable(true),
                        );
                    }
                    for call in &message.tool_calls {
                        turn = turn.child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_xs()
                                .text_color(muted)
                                .child(call_line(call)),
                        );
                    }
                }
                _ => {
                    turn = turn.child(div().text_sm().child(message.text.clone()));
                }
            }
            list = list.child(turn);
        }
        list
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let streaming = self.stream.is_some();
        let injecting = self.inject.is_some();
        // A command runs, or a card waits: a new question would have no
        // turn to run in.
        let busy = self.exec.is_some() || injecting || !self.pending.is_empty();
        let empty = self.input.read(cx).value().trim().is_empty();
        v_flex()
            .id("assistant-composer")
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(border)
            .when(!self.backgrounds.is_empty(), |composer| {
                composer.child(
                    v_flex()
                        .id("assistant-backgrounds")
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(t!("assistant.tool.background_list")),
                        )
                        .children(self.backgrounds.iter().enumerate().map(
                            |(index, background)| {
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().text_xs().child(background.alias.clone()))
                                    .child(
                                        Button::new(("assistant-background-stop", index))
                                            .xsmall()
                                            .label(t!("assistant.action.stop"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.stop_background(index, cx);
                                            })),
                                    )
                            },
                        )),
                )
            })
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
            } else if injecting {
                Button::new("assistant-stop-inject")
                    .small()
                    .icon(CatalogIcon::CircleStop)
                    .label(t!("assistant.action.stop"))
                    .on_click(cx.listener(|this, _, _, cx| this.stop_inject(cx)))
            } else {
                Button::new("assistant-send")
                    .small()
                    .primary()
                    .icon(CatalogIcon::Send)
                    .label(t!("assistant.action.send"))
                    .disabled(empty || busy)
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx)))
            }))
    }

    /// The card for the oldest call waiting for the user: what would run
    /// where, why it needs a card, and the two answers.
    fn render_pending(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let danger = cx.theme().danger;
        let mono = cx.theme().mono_font_family.clone();
        let mut card = v_flex()
            .id("assistant-pending")
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(border);
        let Some(pending) = self.pending.front() else {
            return card;
        };
        let risk_danger = pending.decision.risk >= Risk::R2;
        let can_inject = pending.terminal.can_inject();
        let route = inject::route(
            pending.background,
            pending.terminal.can_exec(),
            can_inject,
            pending.takeover,
        );
        let needs_takeover = matches!(route, Route::NeedTakeover | Route::Unavailable(_));
        card = card
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("assistant.tool.confirm_title")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if risk_danger { danger } else { muted })
                            .child(pending.decision.risk.name()),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("assistant.tool.alias")),
                    )
                    .child(div().text_xs().child(pending.alias.clone())),
            )
            .child(
                div()
                    .font_family(mono)
                    .text_xs()
                    .child(pending.decision.masked_command.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(pending.decision.rule.label()),
            )
            .when(pending.background, |card| {
                card.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("assistant.tool.background")),
                )
            })
            .when(can_inject, |card| {
                card.child(
                    Checkbox::new("assistant-takeover")
                        .small()
                        .label(t!("assistant.tool.takeover"))
                        .checked(pending.takeover)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_takeover(*checked, cx);
                        })),
                )
            })
            .when(needs_takeover, |card| {
                card.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(t!("assistant.tool.takeover.required")),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("assistant-deny")
                            .xsmall()
                            .label(t!("assistant.tool.deny"))
                            .on_click(cx.listener(|this, _, _, cx| this.deny_pending(cx))),
                    )
                    .child(
                        Button::new("assistant-allow")
                            .xsmall()
                            .primary()
                            .label(t!("assistant.tool.allow"))
                            .disabled(needs_takeover)
                            .on_click(cx.listener(|this, _, _, cx| this.approve_pending(cx))),
                    ),
            );
        card
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
            .when(!self.pending.is_empty(), |panel| {
                panel.child(self.render_pending(cx))
            })
            .child(self.render_composer(cx))
    }
}
