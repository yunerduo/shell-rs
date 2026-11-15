//! The assistant's panel: who it talks to, which terminals it may use, and
//! the conversation itself. From P1 on it also runs what the user's policy
//! allows, one command at a time, with a card for anything not whitelisted.

use std::collections::VecDeque;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

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
use super::bindings::{self, AssistantTerminal, Bindings};
use super::chat::{
    self, AskOption, AskUserArgs, ChatMessage, RunCommandArgs, RunMode, ToolCall, WaitForOutputArgs,
};
use super::classifier::{self, Judged};
use super::client::{self, StreamEvent, StreamHandle, StreamRequest};
use super::inject::{self, InjectOutcome, Route};
use super::policy::{self, AuthorizationLevel, Classified, Decision, Policy, Risk, Verdict};
use super::protocol::ToolTerminal;
use super::settings::ProviderProtocol;
use super::tool_result::{
    call_line, denied_result, tool_card_parts, tool_error, tool_result_body, unknown_terminal,
    unsupported_result,
};
use super::wait::{self, Watcher};

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

/// Full access waits this long before its confirmation can be accepted: the
/// second click is a decision, not a reflex.
const FULL_ACCESS_HOLD: Duration = Duration::from_secs(10);

/// How many classifier calls one exchange may make. Past that, an
/// unplaceable command keeps the rules' R2.
const CLASSIFIER_LIMIT: u32 = 20;

/// How many terminals may run a command at the same time. One terminal
/// still runs one command; different terminals run side by side.
const GLOBAL_RUNS: usize = 3;

/// How often a wait looks at its screen. Fast enough for a service that
/// starts in a second, slow enough not to reread the grid every tick.
const WAIT_CHECK: Duration = Duration::from_millis(200);

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
    /// The level the command was judged at.
    level: &'static str,
    /// The classifier forbade it and the user may still run it once.
    overridable: bool,
}

/// One question the conversation waits on. Asking runs nothing.
struct AskRun {
    call_id: String,
    question: String,
    options: Vec<AskOption>,
    allow_free_text: bool,
}

/// One classifier call in flight: the call it belongs to, and the answer
/// being waited for.
struct ClassifyRun {
    call: ToolCall,
    alias: String,
    terminal: AssistantTerminal,
    command: String,
    decision: Decision,
    hash: String,
    background: bool,
    level: &'static str,
    judge: classifier::Judge,
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
    /// The level it was judged at, for the audit.
    level: &'static str,
    /// Why the user ran it against a refusal, if they did.
    override_reason: Option<&'static str>,
}

/// One command running beside its shell now.
struct ExecRun {
    call_id: String,
    alias: String,
    /// The terminal it runs on: one run per terminal.
    terminal_id: String,
    decision: Decision,
    approver: &'static str,
    level: &'static str,
    override_reason: Option<&'static str>,
    receiver: std::sync::mpsc::Receiver<ExecDetailResult>,
}

/// One command typed into a terminal now.
struct InjectRun {
    call_id: String,
    alias: String,
    terminal_id: String,
    decision: Decision,
    approver: &'static str,
    level: &'static str,
    override_reason: Option<&'static str>,
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

/// A judged call waiting for its terminal or for a slot under the global
/// limit. Calls keep their order per terminal.
struct WaitingCall {
    granted: Granted,
    background: bool,
    route: Route,
    terminal_id: String,
}

/// One wait watching a terminal's screen. It sends no bytes and runs no
/// command; only the screen reaches it.
struct WaitRun {
    call_id: String,
    terminal_id: String,
    view: Entity<TerminalView>,
    watcher: Watcher,
    started: Instant,
    timeout: Duration,
    /// When the screen was last read, so reads stay at one per
    /// [`WAIT_CHECK`].
    last_check: Instant,
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
    /// The classifier's model name, edited beside its switch.
    classifier_model: Entity<InputState>,
    bindings: Bindings,
    available: Vec<AssistantTerminal>,
    messages: Vec<ChatMessage>,
    stream: Option<StreamHandle>,
    /// The calls of this exchange not yet handled.
    queue: VecDeque<ToolCall>,
    /// Calls waiting for the user, oldest first.
    pending: VecDeque<PendingCall>,
    /// The commands running beside their shells, one per terminal.
    execs: Vec<ExecRun>,
    /// The commands typed into their terminals, one per terminal.
    injects: Vec<InjectRun>,
    /// Judged calls waiting for their terminal or for a free slot.
    waiting: VecDeque<WaitingCall>,
    /// Waits watching a terminal's screen.
    waits: Vec<WaitRun>,
    /// One tick serves every run and wait.
    ticking: bool,
    /// Commands the background mode left running.
    backgrounds: Vec<BackgroundRun>,
    /// The command last typed into a terminal, for echo suppression.
    echoes: Vec<(String, String)>,
    /// Terminal views whose input the panel already watches.
    watched: Vec<String>,
    /// Whether the saved bindings were read in; later syncs only retry
    /// what is still pending.
    bindings_restored: bool,
    /// Restricted unless the user confirmed full access.
    level: AuthorizationLevel,
    /// The terminals full access covers: those bound when it was confirmed.
    /// A terminal bound later stays restricted.
    covered: Vec<String>,
    /// When the full-access confirmation opened, while it waits out its
    /// hold. `None` when no dialog is shown.
    full_access_card: Option<Instant>,
    /// A question the conversation waits on.
    ask: Option<AskRun>,
    /// The rebuilt answer box for an ask's free text.
    ask_input: Entity<InputState>,
    /// The classifier call in flight.
    classify: Option<ClassifyRun>,
    /// Classifier calls made in this exchange, and what they said about
    /// commands already asked about.
    classifier_calls: u32,
    classifier_cache: Vec<(String, Classified)>,
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
        let ask_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.ask.placeholder"))
                .submit_on_enter(true)
        });
        let classifier_model = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.settings.classifier.model_placeholder"))
                .default_value(
                    settings
                        .read(cx)
                        .settings()
                        .assistant
                        .classifier_model
                        .clone(),
                )
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
                    &ask_input,
                    window,
                    |this, _, event: &InputEvent, window, cx| {
                        if matches!(event, InputEvent::PressEnter { shift: false, .. }) {
                            this.answer_ask_text(window, cx);
                        }
                    },
                ),
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
            classifier_model,
            bindings: Bindings::default(),
            available: Vec::new(),
            messages: Vec::new(),
            stream: None,
            queue: VecDeque::new(),
            pending: VecDeque::new(),
            execs: Vec::new(),
            injects: Vec::new(),
            waiting: VecDeque::new(),
            waits: Vec::new(),
            ticking: false,
            backgrounds: Vec::new(),
            echoes: Vec::new(),
            watched: Vec::new(),
            bindings_restored: false,
            level: AuthorizationLevel::Restricted,
            covered: Vec::new(),
            full_access_card: None,
            ask: None,
            ask_input,
            classify: None,
            classifier_calls: 0,
            classifier_cache: Vec::new(),
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
    /// joins an empty conversation on its own. The list holds every open
    /// terminal, not just the ones in sight, so switching tabs or groups
    /// changes no binding; a terminal that is gone is closed here once.
    pub fn set_terminals(
        &mut self,
        terminals: Vec<AssistantTerminal>,
        front: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let closed: Vec<String> = self
            .available
            .iter()
            .map(|terminal| terminal.id.clone())
            .filter(|id| !terminals.iter().any(|terminal| &terminal.id == id))
            .collect();
        if !self.bindings_restored {
            self.bindings_restored = true;
            self.bindings
                .restore(Bindings::load(&Self::bindings_path()));
        }
        self.bindings.sync(&terminals);
        let mut changed = self.bindings.match_pending(&terminals);
        let front = front
            .as_deref()
            .and_then(|id| terminals.iter().find(|terminal| terminal.id == id));
        changed |= self.bindings.follow_front(front);
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
        for id in closed {
            self.close_terminal(&id, cx);
        }
        if changed {
            self.save_bindings(cx);
        }
        cx.notify();
    }

    fn bindings_path() -> std::path::PathBuf {
        bindings::path_in(&crate::app::data_dir())
    }

    /// Keep the aliases for the next session. A failed write is a notice,
    /// not a reason to change what is bound now.
    fn save_bindings(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.bindings.save(&Self::bindings_path()) {
            self.notice = Some(t!(
                "assistant.terminals.save_failed",
                reason = error.to_string()
            ));
        }
        cx.notify();
    }

    /// A terminal was closed: its cards, calls, runs, background command and
    /// waits end with it, and no other terminal is touched.
    fn close_terminal(&mut self, terminal_id: &str, cx: &mut Context<Self>) {
        self.covered.retain(|id| id != terminal_id);
        self.echoes.retain(|(id, _)| id != terminal_id);

        let mut pending = VecDeque::new();
        while let Some(call) = self.pending.pop_front() {
            if call.terminal.id == terminal_id {
                self.record(
                    &call.alias,
                    &call.decision,
                    "denied",
                    None,
                    "user",
                    call.level,
                    Some("terminal_closed"),
                );
                self.push_result(call.call.id, tool_error("terminal_closed"));
            } else {
                pending.push_back(call);
            }
        }
        self.pending = pending;

        let mut waiting = VecDeque::new();
        while let Some(call) = self.waiting.pop_front() {
            if call.terminal_id == terminal_id {
                self.push_result(
                    call.granted.call_id,
                    wait::cancelled_body("terminal_closed"),
                );
            } else {
                waiting.push_back(call);
            }
        }
        self.waiting = waiting;

        let mut execs = Vec::new();
        for run in std::mem::take(&mut self.execs) {
            if run.terminal_id == terminal_id {
                self.push_result(run.call_id, wait::cancelled_body("terminal_closed"));
            } else {
                execs.push(run);
            }
        }
        self.execs = execs;

        let mut injects = Vec::new();
        for run in std::mem::take(&mut self.injects) {
            if run.terminal_id == terminal_id {
                run.handle.cancel();
                self.push_result(run.call_id, wait::cancelled_body("terminal_closed"));
            } else {
                injects.push(run);
            }
        }
        self.injects = injects;

        let mut backgrounds = Vec::new();
        for run in std::mem::take(&mut self.backgrounds) {
            if run.terminal_id == terminal_id {
                run.handle.cancel();
            } else {
                backgrounds.push(run);
            }
        }
        self.backgrounds = backgrounds;

        let mut waits = Vec::new();
        for run in std::mem::take(&mut self.waits) {
            if run.terminal_id == terminal_id {
                self.push_result(run.call_id, wait::cancelled_body("terminal_closed"));
            } else {
                waits.push(run);
            }
        }
        self.waits = waits;

        self.advance(cx);
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

    /// The user typed in one terminal the assistant was using: that
    /// terminal's takeover is over. Its injection gets its ^C, its queued
    /// calls and waits end, and its cards are refused; other terminals keep
    /// running. A background run is left alone, because stopping it is the
    /// user's own choice on the card.
    fn user_took_over(&mut self, terminal_id: &str, cx: &mut Context<Self>) {
        let mut taken = false;
        if let Some(run) = self
            .injects
            .iter_mut()
            .find(|run| run.terminal_id == terminal_id)
        {
            run.cancelled_by = Some("user");
            run.handle.cancel();
            taken = true;
        }

        let mut waiting = VecDeque::new();
        while let Some(call) = self.waiting.pop_front() {
            if call.terminal_id == terminal_id {
                self.push_result(call.granted.call_id, wait::cancelled_body("user_takeover"));
                taken = true;
            } else {
                waiting.push_back(call);
            }
        }
        self.waiting = waiting;

        let mut waits = Vec::new();
        for run in std::mem::take(&mut self.waits) {
            if run.terminal_id == terminal_id {
                self.push_result(run.call_id, wait::cancelled_body("user_takeover"));
                taken = true;
            } else {
                waits.push(run);
            }
        }
        self.waits = waits;

        let mut pending = VecDeque::new();
        while let Some(call) = self.pending.pop_front() {
            if call.terminal.id == terminal_id {
                self.record(
                    &call.alias,
                    &call.decision,
                    "denied",
                    None,
                    "user",
                    call.level,
                    Some("user_takeover"),
                );
                self.push_result(call.call.id, denied_result(&call.decision));
                taken = true;
            } else {
                pending.push_back(call);
            }
        }
        self.pending = pending;

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
            self.advance(cx);
        }
    }

    fn toggle_terminal(&mut self, id: &str, cx: &mut Context<Self>) {
        let Some(terminal) = self.available.iter().find(|terminal| terminal.id == id) else {
            return;
        };
        let terminal = terminal.clone();
        let bound = self.bindings.toggle(&terminal);
        self.error = None;
        if !bound && !self.bindings.has_room() {
            self.notice = Some(t!(
                "assistant.terminals.limit",
                limit = bindings::MAX_BINDINGS
            ));
        }
        self.save_bindings(cx);
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
            || !self.execs.is_empty()
            || !self.injects.is_empty()
            || !self.waits.is_empty()
            || !self.waiting.is_empty()
            || self.classify.is_some()
            || self.ask.is_some()
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
            || !self.execs.is_empty()
            || !self.injects.is_empty()
            || !self.waits.is_empty()
            || !self.waiting.is_empty()
            || self.classify.is_some()
            || self.ask.is_some()
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

    /// Handle every queued call that can be handled now, start what is
    /// waiting where there is room, and let the model answer once every
    /// result is in the conversation. A card, a question or a classifier
    /// call stops the dispatch: the conversation waits on the user or the
    /// classifier.
    fn advance(&mut self, cx: &mut Context<Self>) {
        while let Some(call) = self.queue.pop_front() {
            self.handle_call(call, cx);
            if self.classify.is_some() || self.ask.is_some() || !self.pending.is_empty() {
                cx.notify();
                return;
            }
        }
        self.pump(cx);
        if !self.execs.is_empty()
            || !self.injects.is_empty()
            || !self.waits.is_empty()
            || !self.waiting.is_empty()
            || self.classify.is_some()
            || self.ask.is_some()
            || !self.pending.is_empty()
            || self.stream.is_some()
        {
            cx.notify();
            return;
        }
        self.request_round(cx);
        cx.notify();
    }

    /// Start the waiting calls whose terminal is free, while the global
    /// limit has room. Order is kept: a call never passes one ahead of it on
    /// the same terminal.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let mut index = 0;
        while index < self.waiting.len() {
            if self.execs.len() + self.injects.len() >= GLOBAL_RUNS {
                break;
            }
            let terminal_id = self.waiting[index].terminal_id.clone();
            if self.terminal_busy(&terminal_id) {
                index += 1;
                continue;
            }
            let Some(call) = self.waiting.remove(index) else {
                break;
            };
            self.start(call, cx);
        }
    }

    /// One terminal runs one command at a time; different terminals do not
    /// wait for each other.
    fn terminal_busy(&self, terminal_id: &str) -> bool {
        self.execs.iter().any(|run| run.terminal_id == terminal_id)
            || self
                .injects
                .iter()
                .any(|run| run.terminal_id == terminal_id)
    }

    fn start(&mut self, call: WaitingCall, cx: &mut Context<Self>) {
        match call.route {
            Route::Exec => self.execute(call.granted, cx),
            Route::Inject => self.execute_inject(call.granted, call.background, cx),
            // A call is only queued with a route that can start.
            Route::NeedTakeover | Route::Unavailable(_) => {
                self.push_result(call.granted.call_id, unsupported_result("not_started"));
            }
        }
        self.ensure_tick(cx);
    }

    /// Put a judged call in line. It runs now when its terminal is free and
    /// the global limit has room; otherwise it waits for its turn.
    fn schedule(
        &mut self,
        granted: Granted,
        background: bool,
        route: Route,
        cx: &mut Context<Self>,
    ) {
        let terminal_id = granted.terminal.id.clone();
        // A background run holds its terminal: rather than queue behind it
        // for as long as the user leaves it running, the model is told to
        // have it stopped first.
        if self
            .backgrounds
            .iter()
            .any(|background| background.terminal_id == terminal_id)
        {
            self.push_result(granted.call_id, tool_error("background_running"));
            return;
        }
        self.waiting.push_back(WaitingCall {
            granted,
            background,
            route,
            terminal_id,
        });
        self.pump(cx);
    }

    /// One tick serves every run and wait. It polls each channel, starts
    /// what may start, and asks for the next turn once nothing is in flight.
    fn ensure_tick(&mut self, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let keep = this.update(cx, |this, cx| this.tick(cx)).unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    fn tick(&mut self, cx: &mut Context<Self>) -> bool {
        self.poll_execs(cx);
        self.poll_injects(cx);
        self.poll_waits(cx);
        self.pump(cx);
        let busy = !self.execs.is_empty() || !self.injects.is_empty() || !self.waits.is_empty();
        if busy {
            cx.notify();
        } else {
            self.ticking = false;
            self.advance(cx);
        }
        busy
    }

    /// Judge one call and either refuse it, ask, or run it.
    fn handle_call(&mut self, call: ToolCall, cx: &mut Context<Self>) {
        if call.name == "ask_user" {
            match serde_json::from_str::<AskUserArgs>(&call.arguments) {
                Ok(args) if args.usable() => {
                    self.ask = Some(AskRun {
                        call_id: call.id,
                        question: args.question.trim().to_string(),
                        options: args.options,
                        allow_free_text: args.allow_free_text,
                    });
                    cx.notify();
                }
                // A question nobody can answer runs nothing and is over at
                // once.
                _ => {
                    self.push_result(call.id, tool_error("bad_arguments"));
                }
            }
            return;
        }
        if call.name == "wait_for_output" {
            match serde_json::from_str::<WaitForOutputArgs>(&call.arguments) {
                Ok(args) if args.usable() => self.start_wait(call.id, args, cx),
                _ => {
                    self.push_result(call.id, tool_error("bad_arguments"));
                }
            }
            return;
        }
        if call.name != "run_command" {
            self.push_result(call.id, tool_error("unknown_tool"));
            return;
        }
        let Ok(args) = serde_json::from_str::<RunCommandArgs>(&call.arguments) else {
            self.push_result(call.id, tool_error("bad_arguments"));
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
            self.push_result(call.id, unknown_terminal(&self.aliases()));
            return;
        };
        let Some(terminal) = self
            .available
            .iter()
            .find(|terminal| terminal.id == entry.id)
            .cloned()
        else {
            self.push_result(call.id, tool_error("terminal_closed"));
            return;
        };

        let assistant = self.settings.read(cx).settings().assistant.clone();
        let policy = Policy::new(
            assistant.user_blacklist.clone(),
            assistant.user_whitelist.clone(),
        );
        let level = self.level_for(&entry.id);
        let decision = policy::decide(&args.command, level, &policy);
        let hash = audit::command_hash(&args.command);
        let background = args.mode == RunMode::Background;
        let level = level.name();

        // The classifier only looks where the rules could not place the
        // command, and only when the user turned it on.
        if decision.unresolved && assistant.classifier_enabled {
            let known = self
                .classifier_cache
                .iter()
                .find(|(key, _)| *key == hash)
                .map(|(_, verdict)| *verdict);
            if let Some(verdict) = known {
                let decision = policy::apply_classifier(decision, verdict);
                let overridable = verdict == Classified::Forbid;
                self.resolve(
                    call,
                    entry.alias,
                    terminal,
                    args.command,
                    decision,
                    background,
                    hash,
                    level,
                    overridable,
                    cx,
                );
                return;
            }
            if self.classifier_calls < CLASSIFIER_LIMIT {
                let judge = classifier::start(classifier::request(&assistant, &args.command, ""));
                self.classify = Some(ClassifyRun {
                    call,
                    alias: entry.alias,
                    terminal,
                    command: args.command,
                    decision,
                    hash,
                    background,
                    level,
                    judge,
                });
                self.begin_classify_poll(cx);
                cx.notify();
                return;
            }
            self.notice = Some(t!(
                "assistant.tool.classifier_limit",
                limit = CLASSIFIER_LIMIT
            ));
        }

        self.resolve(
            call,
            entry.alias,
            terminal,
            args.command,
            decision,
            background,
            hash,
            level,
            false,
            cx,
        );
    }

    /// The level this terminal is judged at. Full access covers only the
    /// terminals that were bound when the user confirmed it.
    fn level_for(&self, terminal_id: &str) -> AuthorizationLevel {
        if self.level == AuthorizationLevel::FullAccess
            && self.covered.iter().any(|id| id == terminal_id)
        {
            AuthorizationLevel::FullAccess
        } else {
            AuthorizationLevel::Restricted
        }
    }

    /// The aliases the model may name, in binding order.
    fn aliases(&self) -> Vec<String> {
        self.bindings
            .entries()
            .iter()
            .map(|entry| entry.alias.clone())
            .collect()
    }

    /// Watch one terminal's screen for the model. A wait sends nothing to
    /// the shell and runs no command; only one waits on a terminal at a
    /// time, so its results cannot be mixed up.
    fn start_wait(&mut self, call_id: String, args: WaitForOutputArgs, cx: &mut Context<Self>) {
        let Some(entry) = self
            .bindings
            .entries()
            .iter()
            .find(|entry| entry.alias == args.terminal)
            .cloned()
        else {
            self.push_result(call_id, unknown_terminal(&self.aliases()));
            return;
        };
        let Some(terminal) = self
            .available
            .iter()
            .find(|terminal| terminal.id == entry.id)
            .cloned()
        else {
            self.push_result(call_id, tool_error("terminal_closed"));
            return;
        };
        if self
            .waits
            .iter()
            .any(|wait| wait.terminal_id == terminal.id)
        {
            self.push_result(call_id, tool_error("wait_busy"));
            return;
        }
        let Some(view) = terminal.view.as_ref().and_then(|weak| weak.upgrade()) else {
            self.push_result(call_id, tool_error("closed"));
            return;
        };
        let screen = view.read(cx).screen_text(cx);
        let mut watcher = Watcher::new(&args.pattern, args.r#match, args.ignore_case, &screen);
        if let Some((_, command)) = self.echoes.iter().find(|(id, _)| id == &terminal.id) {
            watcher.watched_command(command);
        }
        self.waits.push(WaitRun {
            call_id,
            terminal_id: terminal.id,
            view,
            watcher,
            started: Instant::now(),
            timeout: Duration::from_secs(u64::from(args.timeout_seconds())),
            last_check: Instant::now(),
        });
        self.ensure_tick(cx);
        cx.notify();
    }

    /// Route one judged call: refuse it, put it on a card, or run it.
    #[allow(clippy::too_many_arguments)]
    fn resolve(
        &mut self,
        call: ToolCall,
        alias: String,
        terminal: AssistantTerminal,
        command: String,
        decision: Decision,
        background: bool,
        hash: String,
        level: &'static str,
        overridable: bool,
        cx: &mut Context<Self>,
    ) {
        match decision.verdict {
            // The classifier's refusal comes with one card: the user may
            // still run the command once.
            Verdict::Deny if overridable => {
                self.pending.push_back(PendingCall {
                    call,
                    alias,
                    terminal,
                    command,
                    decision,
                    hash,
                    background,
                    takeover: false,
                    level,
                    overridable: true,
                });
                cx.notify();
            }
            Verdict::Deny => {
                self.record(&alias, &decision, "denied", None, "policy", level, None);
                self.push_result(call.id, denied_result(&decision));
            }
            Verdict::Confirm => {
                self.pending.push_back(PendingCall {
                    call,
                    alias,
                    terminal,
                    command,
                    decision,
                    hash,
                    background,
                    takeover: false,
                    level,
                    overridable: false,
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
                    Route::Exec => self.schedule(
                        Granted {
                            call_id: call.id,
                            alias,
                            terminal,
                            command,
                            decision,
                            approver: "auto",
                            level,
                            override_reason: None,
                        },
                        background,
                        Route::Exec,
                        cx,
                    ),
                    // A background run, and a terminal with no channel
                    // beside the shell, need the takeover card even when
                    // the policy let the command run alone.
                    Route::NeedTakeover => {
                        self.pending.push_back(PendingCall {
                            call,
                            alias,
                            terminal,
                            command,
                            decision,
                            hash,
                            background,
                            takeover: false,
                            level,
                            overridable: false,
                        });
                        cx.notify();
                    }
                    Route::Unavailable(reason) => {
                        self.record(&alias, &decision, "unsupported", None, "auto", level, None);
                        self.push_result(call.id, unsupported_result(reason));
                    }
                    Route::Inject => unreachable!("no takeover was given"),
                }
            }
        }
    }

    fn begin_classify_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let finished = this
                    .update(cx, |this, cx| this.poll_classify(cx))
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// Take the classifier's answer; `true` when there is nothing more to
    /// wait for. A failed call leaves the rules' own verdict in place.
    fn poll_classify(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(run) = self.classify.as_mut() else {
            return true;
        };
        let Some(judged) = run.judge.poll() else {
            return false;
        };
        let Some(run) = self.classify.take() else {
            return true;
        };
        self.classifier_calls += 1;
        let (decision, overridable) = match judged {
            Judged::Verdict { verdict, .. } => {
                self.classifier_cache.push((run.hash.clone(), verdict));
                let decision = policy::apply_classifier(run.decision, verdict);
                (decision, verdict == Classified::Forbid)
            }
            Judged::Failed(_) => {
                self.notice = Some(t!("assistant.tool.classifier_failed"));
                (run.decision, false)
            }
        };
        self.resolve(
            run.call,
            run.alias,
            run.terminal,
            run.command,
            decision,
            run.background,
            run.hash,
            run.level,
            overridable,
            cx,
        );
        self.advance(cx);
        cx.notify();
        true
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
            level,
            override_reason,
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
            self.record(
                &alias,
                &decision,
                "unsupported",
                None,
                approver,
                level,
                None,
            );
            self.push_result(call_id, unsupported_result(reason));
            return;
        };
        let Some(receiver) = view
            .read(cx)
            .exec_detail(command, EXEC_TIMEOUT, EXEC_LIMIT, cx)
        else {
            self.record(
                &alias,
                &decision,
                "unsupported",
                None,
                approver,
                level,
                None,
            );
            self.push_result(call_id, unsupported_result("not_running"));
            return;
        };
        self.execs.push(ExecRun {
            call_id,
            alias,
            terminal_id: terminal.id,
            decision,
            approver,
            level,
            override_reason,
            receiver,
        });
        cx.notify();
    }

    fn approve_pending(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.pop_front() else {
            return;
        };
        // The card showed a command; the one that runs must be that one.
        if audit::command_hash(&pending.command) != pending.hash {
            self.record(
                &pending.alias,
                &pending.decision,
                "denied",
                None,
                "user",
                pending.level,
                None,
            );
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
            level: pending.level,
            // Running against the classifier's refusal is the one override
            // the trail keeps a reason for.
            override_reason: pending.overridable.then_some("classifier_forbid"),
        };
        match route {
            Route::Exec | Route::Inject => {
                self.schedule(granted, background, route, cx);
                self.advance(cx);
            }
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
        self.record(
            &pending.alias,
            &pending.decision,
            "denied",
            None,
            "user",
            pending.level,
            None,
        );
        self.push_result(pending.call.id, denied_result(&pending.decision));
        self.advance(cx);
        cx.notify();
    }

    /// Answer the waiting question and let the conversation go on.
    fn answer_ask(
        &mut self,
        answer: String,
        option_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ask) = self.ask.take() else {
            return;
        };
        let body = serde_json::json!({
            "status": "ok",
            "answer": answer,
            "option_id": option_id,
        })
        .to_string();
        self.ask_input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.push_result(ask.call_id, body);
        self.advance(cx);
        cx.notify();
    }

    /// The typed answer, when the ask lets the user type one.
    fn answer_ask_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.ask_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.answer_ask(text, None, window, cx);
    }

    /// Whether full access is in force, for whatever shows a red bar.
    pub fn full_access(&self) -> bool {
        self.level == AuthorizationLevel::FullAccess
    }

    /// Open or close full access. Opening only shows the confirmation; the
    /// level changes when the user accepts it after the hold.
    fn toggle_full_access(&mut self, cx: &mut Context<Self>) {
        if self.level == AuthorizationLevel::FullAccess {
            self.close_full_access(cx);
            return;
        }
        if self.full_access_card.is_some() {
            self.full_access_card = None;
            cx.notify();
            return;
        }
        self.full_access_card = Some(Instant::now());
        self.notice = None;
        self.begin_full_access_tick(cx);
        cx.notify();
    }

    /// Turn full access off, whether it was on or only waiting.
    fn close_full_access(&mut self, cx: &mut Context<Self>) {
        self.full_access_card = None;
        if self.level != AuthorizationLevel::FullAccess {
            cx.notify();
            return;
        }
        self.level = AuthorizationLevel::Restricted;
        self.covered.clear();
        self.notice = Some(t!("assistant.full_access.off"));
        cx.notify();
    }

    /// Accept the confirmation, once its hold has passed. Full access then
    /// covers the terminals bound at this moment and no others.
    fn confirm_full_access(&mut self, cx: &mut Context<Self>) {
        let Some(since) = self.full_access_card else {
            return;
        };
        if since.elapsed() < FULL_ACCESS_HOLD {
            return;
        }
        self.full_access_card = None;
        self.level = AuthorizationLevel::FullAccess;
        self.covered = self
            .bindings
            .entries()
            .iter()
            .map(|entry| entry.id.clone())
            .collect();
        self.notice = Some(t!("assistant.full_access.on"));
        cx.notify();
    }

    fn begin_full_access_tick(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let holding = this
                    .update(cx, |this, cx| {
                        if this.full_access_card.is_none() {
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !holding {
                    break;
                }
            }
        })
        .detach();
    }

    /// The emergency stop: everything in flight ends, the pending cards are
    /// refused, and full access goes back to restricted at once.
    fn emergency_stop(&mut self, cx: &mut Context<Self>) {
        if let Some(stream) = self.stream.take() {
            stream.cancel();
        }
        for run in &mut self.injects {
            run.cancelled_by = Some("stop");
            run.handle.cancel();
        }
        for run in self.backgrounds.drain(..) {
            run.handle.cancel();
        }
        while let Some(pending) = self.pending.pop_front() {
            self.record(
                &pending.alias,
                &pending.decision,
                "denied",
                None,
                "user",
                pending.level,
                Some("user_stop"),
            );
            self.push_result(pending.call.id, denied_result(&pending.decision));
        }
        // A call that never ran still gets an answer, so the conversation
        // the user continues later has one for every tool call.
        while let Some(call) = self.queue.pop_front() {
            self.push_result(call.id, tool_error("user_stop"));
        }
        while let Some(call) = self.waiting.pop_front() {
            self.push_result(call.granted.call_id, wait::cancelled_body("user_stop"));
        }
        for run in std::mem::take(&mut self.waits) {
            self.push_result(run.call_id, wait::cancelled_body("user_stop"));
        }
        self.classify = None;
        if let Some(ask) = self.ask.take() {
            self.push_result(ask.call_id, tool_error("user_stop"));
        }
        self.level = AuthorizationLevel::Restricted;
        self.covered.clear();
        self.full_access_card = None;
        // The exchange ends here: no turn continues after a stop.
        self.stopped = true;
        self.notice = Some(t!("assistant.full_access.emergency_done"));
        cx.notify();
    }

    fn push_result(&mut self, call_id: String, body: String) {
        self.messages.push(ChatMessage::tool_result(call_id, body));
    }

    /// Write one decision to the trail. A failed write is a notice: the
    /// command the user approved still runs.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        alias: &str,
        decision: &Decision,
        channel: &str,
        exit_code: Option<i32>,
        approver: &str,
        level: &str,
        override_reason: Option<&str>,
    ) {
        let entry = audit::AuditEntry {
            alias,
            command: &decision.masked_command,
            risk: decision.risk.name(),
            rule: decision.rule.name(),
            channel,
            exit_code,
            level,
            approver,
            override_reason,
        };
        if audit::append(&entry).is_err() {
            self.notice = Some(t!("assistant.tool.audit_failed"));
        }
    }

    /// Take the answers of every exec run that has one. A run whose channel
    /// closed is reported, not left waiting.
    fn poll_execs(&mut self, cx: &mut Context<Self>) {
        let mut index = 0;
        while index < self.execs.len() {
            let result = match self.execs[index].receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    index += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Err(t!("assistant.tool.transport_closed").to_string())
                }
            };
            let run = self.execs.remove(index);
            let (body, exit_code) = tool_result_body(result);
            self.record(
                &run.alias,
                &run.decision,
                "exec",
                exit_code,
                run.approver,
                run.level,
                run.override_reason,
            );
            self.push_result(run.call_id, body);
        }
        cx.notify();
    }

    /// Take the outcome of every injection that has one, and keep a
    /// background run's handle so the user can stop it later.
    fn poll_injects(&mut self, cx: &mut Context<Self>) {
        let mut index = 0;
        while index < self.injects.len() {
            let message = match self.injects[index].receiver.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => {
                    index += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => InjectMessage::Done(InjectOutcome {
                    failure: Some(RunFailure::Disconnected),
                    ..InjectOutcome::default()
                }),
            };
            let run = self.injects.remove(index);
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
                run.level,
                run.override_reason,
            );
            self.push_result(run.call_id, outcome.body());
            if let Some(handle) = finished_handle {
                self.backgrounds.push(BackgroundRun {
                    alias: run.alias,
                    terminal_id: run.terminal_id,
                    handle,
                });
            }
        }
        cx.notify();
    }

    /// End a wait whose text appeared, whose time ran out, or both; the
    /// screen is read at most once every [`WAIT_CHECK`].
    fn poll_waits(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut index = 0;
        while index < self.waits.len() {
            let answer = {
                let run = &mut self.waits[index];
                if now.duration_since(run.last_check) < WAIT_CHECK {
                    index += 1;
                    continue;
                }
                run.last_check = now;
                let screen = run.view.read(cx).screen_text(cx);
                let elapsed = now.duration_since(run.started);
                match run.watcher.observe(&screen) {
                    Some(hit) => Some(wait::hit_body(&hit, elapsed)),
                    None if elapsed >= run.timeout => Some(wait::timeout_body(
                        run.timeout,
                        &wait::recent(&screen),
                        run.watcher.echo_seen(),
                    )),
                    None => None,
                }
            };
            let Some(body) = answer else {
                index += 1;
                continue;
            };
            let run = self.waits.remove(index);
            self.push_result(run.call_id, body);
        }
        cx.notify();
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
            level,
            override_reason,
        } = granted;
        let Some(view) = terminal.view.as_ref().and_then(|weak| weak.upgrade()) else {
            self.record(
                &alias,
                &decision,
                "unsupported",
                None,
                approver,
                level,
                None,
            );
            self.push_result(call_id, unsupported_result("takeover_closed"));
            return;
        };
        // What the terminal will echo; a wait must not read it as output.
        let typed = command.clone();
        let (run, events) = TerminalRun::new(command);
        let handle = run.handle();
        if let Err(failure) = view.read(cx).run_command(handle.clone(), cx) {
            // The run never typed anything; dropping it cancels nothing.
            self.record(
                &alias,
                &decision,
                "unsupported",
                None,
                approver,
                level,
                None,
            );
            self.push_result(call_id, inject::failure_body(&failure));
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
        self.echoes.retain(|(id, _)| id != &terminal_id);
        self.echoes.push((terminal_id.clone(), typed));
        self.injects.push(InjectRun {
            call_id,
            alias,
            terminal_id,
            decision,
            approver,
            level,
            override_reason,
            handle,
            receiver,
            cancelled_by: None,
        });
        cx.notify();
    }

    /// Stop every foreground injection with a ^C. Their results come back
    /// like any other, marked with who stopped them.
    fn stop_inject(&mut self, cx: &mut Context<Self>) {
        for run in &mut self.injects {
            run.cancelled_by = Some("stop");
            run.handle.cancel();
        }
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

    /// Write the conversation to a Markdown file beside the audit trail, so
    /// a report can be handed to someone without the window.
    fn save_report(&mut self, cx: &mut Context<Self>) {
        let markdown = chat::to_markdown(&self.messages, &t!("assistant.role.user"));
        if markdown.is_empty() {
            return;
        }
        let trail = audit::path_in(&crate::app::data_dir());
        let dir = trail
            .parent()
            .map(|parent| parent.join("reports"))
            .unwrap_or_else(|| trail.clone());
        if std::fs::create_dir_all(&dir).is_err() {
            self.notice = Some(t!("assistant.report.failed"));
            cx.notify();
            return;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default();
        let name = format!("assistant-{stamp}.md");
        match std::fs::write(dir.join(&name), markdown) {
            Ok(()) => self.notice = Some(t!("assistant.report.saved", name = name)),
            Err(_) => self.notice = Some(t!("assistant.report.failed")),
        }
        cx.notify();
    }

    fn set_classifier(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| settings.assistant.classifier_enabled = enabled,
                cx,
            )
        });
    }

    fn save_classifier_model(&mut self, cx: &mut Context<Self>) {
        let model = self.classifier_model.read(cx).value().to_string();
        self.settings.update(cx, |store, cx| {
            store.update(|settings| settings.assistant.classifier_model = model, cx)
        });
        self.notice = Some(t!("assistant.settings.saved"));
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
            let covered = self.level == AuthorizationLevel::FullAccess
                && self.covered.iter().any(|covered| covered == &terminal.id);
            let mut button = Button::new(("assistant-terminal", index))
                .xsmall()
                .selected(bound)
                .tooltip(terminal.title.clone())
                .label(label)
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_terminal(&id, cx)));
            if covered {
                button = button.danger();
            }
            row = row.child(button);
        }
        // Aliases saved from an earlier session whose terminal could not be
        // identified: shown, never used, and dismissed with a click.
        for (index, wanted) in self.bindings.pending().iter().enumerate() {
            row = row.child(
                Button::new(("assistant-rebind", index))
                    .xsmall()
                    .ghost()
                    .label(t!(
                        "assistant.terminals.pending",
                        alias = wanted.alias.as_str()
                    ))
                    .tooltip(t!(
                        "assistant.terminals.pending_hint",
                        title = wanted.title.as_str()
                    ))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.bindings.dismiss_pending(index) {
                            this.save_bindings(cx);
                        }
                        cx.notify();
                    })),
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
            .child(
                v_flex()
                    .id("assistant-classifier-settings")
                    .gap_1()
                    .child(
                        Checkbox::new("assistant-classifier")
                            .small()
                            .label(t!("assistant.settings.classifier.toggle"))
                            .checked(assistant.classifier_enabled)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.set_classifier(*checked, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("assistant.settings.classifier.hint")),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.classifier_model)
                                        .id("assistant-classifier-model")
                                        .small(),
                                ),
                            )
                            .child(
                                Button::new("assistant-classifier-save")
                                    .xsmall()
                                    .label(t!("assistant.action.save"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_classifier_model(cx);
                                    })),
                            ),
                    ),
            )
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

    /// The second confirmation full access needs, with its hold counting
    /// down and the terminals it would cover named.
    fn render_full_access_dialog(
        &self,
        since: Instant,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let danger = cx.theme().danger;
        let muted = cx.theme().muted_foreground;
        let remaining = FULL_ACCESS_HOLD.saturating_sub(since.elapsed());
        let ready = remaining.is_zero();
        let covers = self
            .bindings
            .entries()
            .iter()
            .map(|entry| entry.alias.clone())
            .collect::<Vec<_>>()
            .join(", ");
        v_flex()
            .id("assistant-full-access-dialog")
            .gap_1()
            .mx_2()
            .my_1()
            .p_2()
            .border_1()
            .border_color(danger)
            .bg(danger.opacity(0.08))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(danger)
                    .child(t!("assistant.full_access.confirm_title")),
            )
            .child(
                div()
                    .text_xs()
                    .child(t!("assistant.full_access.confirm_body")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.full_access.covers", terminals = covers)),
            )
            .child(div().text_xs().text_color(danger).child(if ready {
                t!("assistant.full_access.ready")
            } else {
                t!(
                    "assistant.full_access.wait",
                    seconds = remaining.as_secs() + 1
                )
            }))
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("assistant-full-access-cancel")
                            .xsmall()
                            .label(t!("assistant.action.cancel"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_full_access(cx))),
                    )
                    .child(
                        Button::new("assistant-full-access-accept")
                            .xsmall()
                            .danger()
                            .label(t!("assistant.full_access.accept"))
                            .disabled(!ready)
                            .on_click(cx.listener(|this, _, _, cx| this.confirm_full_access(cx))),
                    ),
            )
    }

    /// The red bar that stays while full access is on, with the emergency
    /// stop always in reach.
    fn render_full_access_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let danger = cx.theme().danger;
        h_flex()
            .id("assistant-full-access")
            .justify_between()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .bg(danger.opacity(0.15))
            .border_b_1()
            .border_color(danger)
            .child(
                div()
                    .text_xs()
                    .text_color(danger)
                    .child(t!("assistant.full_access.bar")),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("assistant-full-access-exit")
                            .xsmall()
                            .label(t!("assistant.full_access.exit"))
                            .on_click(cx.listener(|this, _, _, cx| this.close_full_access(cx))),
                    )
                    .child(
                        Button::new("assistant-emergency-stop")
                            .xsmall()
                            .danger()
                            .label(t!("assistant.full_access.emergency"))
                            .on_click(cx.listener(|this, _, _, cx| this.emergency_stop(cx))),
                    ),
            )
    }

    /// The question the conversation waits on: its choices, and a box to
    /// type an answer when the ask allows one.
    fn render_ask(&self, ask: &AskRun, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let mut card = v_flex()
            .id("assistant-ask")
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(t!("assistant.ask.title")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(t!("assistant.ask.waiting")),
                    ),
            )
            .child(div().text_sm().child(ask.question.clone()));
        for (index, option) in ask.options.iter().enumerate() {
            let answer = option.label.clone();
            let id = option.id.clone();
            card = card.child(
                Button::new(("assistant-ask-option", index))
                    .xsmall()
                    .label(option.label.clone())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.answer_ask(answer.clone(), Some(id.as_str()), window, cx);
                    })),
            );
        }
        if ask.allow_free_text {
            card = card.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.ask_input)
                                .id("assistant-ask-input")
                                .small(),
                        ),
                    )
                    .child(
                        Button::new("assistant-ask-send")
                            .xsmall()
                            .primary()
                            .label(t!("assistant.action.send"))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.answer_ask_text(window, cx)),
                            ),
                    ),
            );
        }
        card
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
        let injecting = !self.injects.is_empty();
        // A command runs, or a card waits: a new question would have no
        // turn to run in.
        let busy = !self.execs.is_empty()
            || injecting
            || !self.waits.is_empty()
            || !self.waiting.is_empty()
            || !self.pending.is_empty()
            || self.ask.is_some()
            || self.classify.is_some();
        let empty = self.input.read(cx).value().trim().is_empty();
        v_flex()
            .id("assistant-composer")
            .gap_1()
            .p_2()
            .border_t_1()
            .border_color(border)
            .when(self.classify.is_some(), |composer| {
                composer.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("assistant.tool.classifying")),
                )
            })
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
            .when(pending.overridable, |card| {
                card.child(
                    div()
                        .text_xs()
                        .text_color(danger)
                        .child(t!("assistant.tool.classifier_forbid")),
                )
            })
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
                            .label(if pending.overridable {
                                t!("assistant.tool.allow_once")
                            } else {
                                t!("assistant.tool.allow")
                            })
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
                            .when(self.level == AuthorizationLevel::FullAccess, |bar| {
                                bar.child(
                                    Button::new("assistant-full-access-toggle")
                                        .xsmall()
                                        .ghost()
                                        .danger()
                                        .selected(true)
                                        .label(t!("assistant.full_access.exit"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.toggle_full_access(cx)
                                        })),
                                )
                            })
                            .when(self.level != AuthorizationLevel::FullAccess, |bar| {
                                bar.child(
                                    Button::new("assistant-full-access-toggle")
                                        .xsmall()
                                        .ghost()
                                        .label(t!("assistant.full_access.button"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.toggle_full_access(cx)
                                        })),
                                )
                            })
                            .child(
                                Button::new("assistant-report")
                                    .xsmall()
                                    .ghost()
                                    .tooltip(t!("assistant.action.report"))
                                    .label(t!("assistant.action.report"))
                                    .disabled(self.messages.is_empty())
                                    .on_click(cx.listener(|this, _, _, cx| this.save_report(cx))),
                            )
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
            .when_some(self.full_access_card, |panel, since| {
                panel.child(self.render_full_access_dialog(since, cx))
            })
            .when(self.level == AuthorizationLevel::FullAccess, |panel| {
                panel.child(self.render_full_access_bar(cx))
            })
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
            .when_some(self.ask.as_ref(), |panel, ask| {
                panel.child(self.render_ask(ask, cx))
            })
            .when(!self.pending.is_empty(), |panel| {
                panel.child(self.render_pending(cx))
            })
            .child(self.render_composer(cx))
    }
}
