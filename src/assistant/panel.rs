//! The assistant's panel: who it talks to, which terminals it may use, and
//! the conversation itself. From P1 on it also runs what the user's policy
//! allows, one command at a time, with a card for anything not whitelisted.

use std::cell::Cell;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _, DropdownButton},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState, Textarea, TextareaState},
    menu::{DropdownMenu as _, PopupMenuItem},
    scroll::{ScrollableElement as _, Scrollbar, ScrollbarMode},
    text::TextView,
    v_flex,
};
use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::CatalogIcon;
use crate::i18n::{join_list, t};
use crate::settings::SettingsStore;
use crate::terminal::{
    ExecDetailResult, RunFailure, RunHandle, RunProgress, RunTiming, TerminalEvent, TerminalRun,
    TerminalView,
};

use super::attachment::{self, Attachment, AttachmentError, AttachmentKind};
use super::audit;
use super::bindings::{self, AssistantTerminal, Bindings, SavedBinding};
use super::chat::{
    self, AskOption, AskUserArgs, ChatMessage, RunCommandArgs, RunMode, ToolCall, WaitForOutputArgs,
};
use super::classifier::{self, Judged};
use super::client::{self, StreamEvent, StreamHandle, StreamRequest};
use super::compact;
use super::inject::{self, InjectOutcome, Route};
use super::pending::{self, PendingItem};
use super::policy::{self, AuthorizationLevel, Classified, Decision, Policy, Risk, Verdict};
use super::protocol::ToolTerminal;
use super::schedule::{self, ResumePlan, Schedule, ScheduleAction, ScheduleStore, ScheduleTarget};
use super::sessions::{Compaction, SessionEvent, Sessions};
use super::settings::ModelSelection;
use super::tool_result::{
    ResultFacts, call_parts, call_summary, denied_result, result_facts, tool_card_parts,
    tool_error, tool_result_body, unknown_terminal, unsupported_result,
};
use super::trial;
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

/// How often the scheduler looks for a task that has come due.
const SCHEDULE_TICK: Duration = Duration::from_secs(30);

/// How long a trial status is trusted before the 30-second loop asks again.
/// Quota is also refreshed at once after an answer ends.
const TRIAL_STATUS_TTL: Duration = Duration::from_secs(300);

/// The longest a scheduled command or answer may take, whatever the file
/// says.
const SCHEDULE_TIMEOUT_MAX: u64 = 600;

/// How many model turns one scheduled answer may take. A report needs a
/// look and an answer; more than that is a loop.
const SCHEDULE_AI_ROUNDS: u32 = 4;

/// How wide the composer card must be before its bottom row keeps the
/// bound-terminal chip, and then the context readout. The terminal chip goes
/// first because the header already names the bound terminals; the readout
/// follows because it is the last thing worth reading there. Both are
/// compared against the width the card was painted at, not the window.
const COMPOSER_TERMINAL_WIDTH: f32 = 520.;
const COMPOSER_CONTEXT_WIDTH: f32 = 420.;

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
    /// When it was started, for the card's duration.
    started: Instant,
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
    /// When it was started, for the card's duration.
    started: Instant,
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

/// One command a trigger — or an approved review item — started beside its
/// shell. Its audit waits for the exit code.
struct ScheduledRun {
    name: String,
    alias: String,
    decision: Decision,
    /// `scheduled` for a trigger, `user` for an approved review item.
    approver: &'static str,
    receiver: std::sync::mpsc::Receiver<ExecDetailResult>,
}

/// One scheduled prompt being answered. Its tools read the screen and the
/// host facts; a command it suggests only goes to the review list.
struct ScheduledAiRun {
    name: String,
    alias: String,
    target: ScheduleTarget,
    messages: Vec<ChatMessage>,
    stream: StreamHandle,
    text: String,
    rounds: u32,
    deadline: Instant,
}

/// One summarizing call in flight. Its text becomes the summary of the
/// messages before `split`.
struct CompactRun {
    stream: StreamHandle,
    text: String,
    split: usize,
    /// The cumulative number of stored messages the summary will stand for.
    count: usize,
    started: Instant,
}

/// A tool panel in the sidebar. It renders itself; the sidebar only keeps
/// the entity.
pub struct AssistantPanel {
    settings: Entity<SettingsStore>,
    /// The optional account. Signed in, the assistant calls the user by
    /// their nickname or username instead of 「你」; signed out, it keeps
    /// the neutral wording.
    account: Entity<crate::account::AccountStore>,
    input: Entity<TextareaState>,
    bindings: Bindings,
    /// The roles loaded into this conversation, by id.
    roles: Vec<String>,
    /// The thinking blocks and call cards the user opened, by call id or
    /// `thinking-<index>`. Everything is folded until it is opened.
    expanded: HashSet<String>,
    /// The width the composer was last painted at. The bottom row drops the
    /// terminal and then the usage readout as the panel narrows; the value
    /// is measured, not guessed from the window.
    composer_width: Rc<Cell<Option<Pixels>>>,
    /// What the platform prompt adds for those roles and the skills on:
    /// rebuilt from the library whenever a turn is sent.
    conversation_note: String,
    /// Whether the aliases still waiting for a terminal are listed. A
    /// restart that placed none of them would otherwise open on a wall of
    /// warnings; the count says as much in one line.
    pending_open: bool,
    available: Vec<AssistantTerminal>,
    /// The conversations kept between launches; `messages` is the open one.
    sessions: Sessions,
    messages: Vec<ChatMessage>,
    /// The summary an earlier compaction left behind, and how many stored
    /// messages it stands for.
    compaction: Option<Compaction>,
    /// The summarizing call in flight, if any.
    compact: Option<CompactRun>,
    /// The conversation's scroll position. Tracked so new words can follow
    /// the tail while the reader is there, and so the scrollbar has a handle.
    scroll: ScrollHandle,
    /// The tail moved on while the reader was above it: the jump button
    /// shows until they come back or ask for it.
    behind: bool,
    /// The tail's shape when the last frame was built, so a repaint for any
    /// other reason does not claim new words.
    painted_tail: (usize, usize),
    /// What the user pasted or dropped into the composer, not sent yet.
    attachments: Vec<Attachment>,
    stream: Option<StreamHandle>,
    /// The built-in trial's last status; `None` when signed out, offline or
    /// the server says it is off.
    trial: Option<trial::Status>,
    /// The availability probe on its way.
    trial_probe: Option<Receiver<Option<trial::Status>>>,
    /// When the status last came back, so the 30-second loop does not ask
    /// again for a while.
    trial_checked: Option<Instant>,
    /// The answer being streamed is the trial's, so its end refreshes the
    /// quota.
    stream_trial: bool,
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
    /// A question is being answered: more turns may follow. It ends when
    /// the answer has no calls left or the user stops it, so an unrelated
    /// event cannot start a turn of its own.
    exchange: bool,
    /// The answer was stopped by the user; an empty answer is not an error
    /// then.
    stopped: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    /// Where a finished task's notice goes.
    window: AnyWindowHandle,
    /// The tasks and the commands waiting for the user.
    schedules: ScheduleStore,
    pending_items: Vec<PendingItem>,
    /// The commands a trigger started beside the shell.
    scheduled: Vec<ScheduledRun>,
    /// The answer a prompt task is waiting on.
    scheduled_ai: Option<ScheduledAiRun>,
    /// The 30-second loop is running.
    schedule_ticking: bool,
    _subscriptions: Vec<Subscription>,
}

impl AssistantPanel {
    pub fn new(
        settings: Entity<SettingsStore>,
        account: Entity<crate::account::AccountStore>,
        _dispatch: FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .submit_on_enter(true)
                .placeholder(t!("assistant.placeholder"))
        });
        let ask_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("assistant.ask.placeholder"))
                .submit_on_enter(true)
        });
        let subscriptions =
            vec![
                cx.observe(&settings, |_, _, cx| cx.notify()),
                cx.observe(&account, |this, _, cx| this.trial_changed(cx)),
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
            ];
        let sessions = Sessions::load(&crate::app::data_dir(), schedule::now_millis());
        let (messages, compaction) = match sessions.active_id() {
            Some(id) => sessions.messages(id),
            None => (Vec::new(), None),
        };
        let mut panel = Self {
            settings,
            account,
            input,
            bindings: Bindings::default(),
            roles: Vec::new(),
            expanded: HashSet::new(),
            composer_width: Rc::default(),
            conversation_note: String::new(),
            available: Vec::new(),
            sessions,
            messages,
            compaction,
            compact: None,
            scroll: ScrollHandle::new(),
            behind: false,
            painted_tail: (0, 0),
            attachments: Vec::new(),
            stream: None,
            trial: None,
            trial_probe: None,
            trial_checked: None,
            stream_trial: false,
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
            pending_open: false,
            level: AuthorizationLevel::Restricted,
            covered: Vec::new(),
            full_access_card: None,
            ask: None,
            ask_input,
            classify: None,
            classifier_calls: 0,
            classifier_cache: Vec::new(),
            rounds: 0,
            exchange: false,
            stopped: false,
            error: None,
            notice: None,
            window: window.window_handle(),
            schedules: ScheduleStore::load(&Self::schedules_path()),
            pending_items: pending::load(&Self::pending_path()),
            scheduled: Vec::new(),
            scheduled_ai: None,
            schedule_ticking: false,
            _subscriptions: subscriptions,
        };
        // A task that came due while the app was not running is counted and
        // moved forward, never run late.
        panel.resume_schedules(cx);
        panel.ensure_schedule_tick(cx);
        panel.install_paste_interceptor(cx);
        panel.refresh_trial(cx, true);
        panel
    }

    /// Ctrl+V with a picture or files on the clipboard becomes an attachment.
    /// Anything else falls through to the field's own paste.
    fn install_paste_interceptor(&mut self, cx: &mut Context<Self>) {
        let weak = cx.entity().downgrade();
        let subscription = cx.intercept_keystrokes(move |event, window, cx| {
            let keystroke = &event.keystroke;
            if !keystroke.key.eq_ignore_ascii_case("v")
                || (!keystroke.modifiers.platform && !keystroke.modifiers.control)
                || keystroke.modifiers.alt
            {
                return;
            }
            let mut taken = false;
            let _ = weak.update(cx, |this, cx| {
                if !this.input.read(cx).focus_handle(cx).is_focused(window) {
                    return;
                }
                taken = this.attach_clipboard(cx);
            });
            if taken {
                cx.stop_propagation();
            }
        });
        self._subscriptions.push(subscription);
    }

    fn schedules_path() -> std::path::PathBuf {
        schedule::path_in(&crate::app::data_dir())
    }

    fn pending_path() -> std::path::PathBuf {
        pending::path_in(&crate::app::data_dir())
    }

    /// The user's lists, as the policy reads them.
    fn policy(&self, cx: &App) -> Policy {
        let assistant = &self.settings.read(cx).settings().assistant;
        Policy::new(
            assistant.user_blacklist.clone(),
            assistant.user_whitelist.clone(),
        )
    }

    /// The tasks the settings dialog shows.
    pub(super) fn schedules(&self) -> &[Schedule] {
        &self.schedules.schedules
    }

    /// The commands waiting for the user, which the settings dialog shows.
    pub(super) fn review_items(&self) -> &[PendingItem] {
        &self.pending_items
    }

    /// The terminals this conversation can use right now.
    pub(super) fn bound_terminals(&self) -> &[AssistantTerminal] {
        self.bindings.entries()
    }

    /// Open the settings over this panel.
    pub fn open_config(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        super::config_dialog::open_config_dialog(self.settings.clone(), cx.entity(), window, cx);
    }

    /// Count what came due while the app was not running and move every
    /// task forward. No task runs late, at any level and for any action.
    fn resume_schedules(&mut self, cx: &mut Context<Self>) {
        let policy = self.policy(cx);
        let now = chrono::Local::now();
        let mut events = Vec::new();
        let mut missed_total = 0;
        for task in &mut self.schedules.schedules {
            if !task.enabled {
                continue;
            }
            let plan: ResumePlan = schedule::resume_at(now, task.next_due_at, task.recurrence);
            if plan.missed == 0 && !plan.rewound {
                continue;
            }
            task.next_due_at = plan.next_due;
            if plan.missed > 0 {
                let decision =
                    policy::decide(task.action.text(), AuthorizationLevel::Restricted, &policy);
                events.push((task.target.alias.clone(), decision));
                missed_total += plan.missed;
            }
        }
        if events.is_empty() {
            return;
        }
        let _ = self.schedules.save(&Self::schedules_path());
        for (alias, decision) in events {
            self.record_raw(&alias, &decision, "missed", None, "scheduled");
        }
        if missed_total > 0 {
            self.notice = Some(t!("assistant.schedule.missed", count = missed_total));
        }
    }

    /// Start the once-a-30-seconds look for due tasks. The loop lives as
    /// long as the panel, which is as long as the workspace.
    fn ensure_schedule_tick(&mut self, cx: &mut Context<Self>) {
        if self.schedule_ticking {
            return;
        }
        self.schedule_ticking = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SCHEDULE_TICK).await;
                let keep = this
                    .update(cx, |this, cx| {
                        this.schedule_tick(cx);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        })
        .detach();
    }

    fn schedule_tick(&mut self, cx: &mut Context<Self>) {
        let now = schedule::now_millis();
        let due: Vec<String> = self
            .schedules
            .schedules
            .iter()
            .filter(|task| task.is_due(now))
            .map(|task| task.id.clone())
            .collect();
        for id in due {
            self.trigger_schedule(&id, now, cx);
        }
        self.poll_scheduled(cx);
        self.poll_scheduled_ai(cx);
        self.poll_trial(cx);
        self.refresh_trial(cx, false);
        if !self.scheduled.is_empty() || self.scheduled_ai.is_some() {
            self.ensure_tick(cx);
        }
    }

    /// Run one due task. Its due time moves before it starts, so a long run
    /// is not started twice.
    fn trigger_schedule(&mut self, id: &str, now_ms: i64, cx: &mut Context<Self>) {
        let Some(task) = self.schedules.get(id).cloned() else {
            return;
        };
        let plan = schedule::resume_at(chrono::Local::now(), task.next_due_at, task.recurrence);
        if let Some(stored) = self.schedules.get_mut(id) {
            stored.next_due_at = plan.next_due;
            stored.last_run_at = Some(now_ms);
            stored.updated_at = now_ms;
        }
        let _ = self.schedules.save(&Self::schedules_path());
        let Some(terminal) = self.resolve_target(&task.target) else {
            // The terminal this task names is not open any more. It is not
            // moved to another card; it waits to be bound again.
            if let Some(stored) = self.schedules.get_mut(id) {
                stored.needs_rebind = true;
                stored.updated_at = now_ms;
            }
            let _ = self.schedules.save(&Self::schedules_path());
            self.notify_system(
                cx,
                t!("assistant.schedule.rebind", name = task.name.as_str()),
                true,
            );
            return;
        };
        if !terminal.can_exec() {
            let decision = policy::decide(
                task.action.text(),
                AuthorizationLevel::Restricted,
                &self.policy(cx),
            );
            self.record_raw(
                &task.target.alias,
                &decision,
                "unsupported",
                None,
                "scheduled",
            );
            self.notify_system(
                cx,
                t!("assistant.schedule.unsupported", name = task.name.as_str()),
                true,
            );
            return;
        }
        match &task.action {
            ScheduleAction::FixedCommand { command } => {
                self.trigger_command(&task, &terminal, command, cx);
            }
            ScheduleAction::FixedPrompt { prompt } => {
                self.trigger_prompt(&task, &terminal, prompt, cx);
            }
        }
    }

    /// A fixed command, judged as if no one were there: only the whitelist's
    /// R0 runs, a write waits for review, a refusal fails.
    fn trigger_command(
        &mut self,
        task: &Schedule,
        terminal: &AssistantTerminal,
        command: &str,
        cx: &mut Context<Self>,
    ) {
        let policy = self.policy(cx);
        match schedule::unattended_action(command, &policy) {
            schedule::Unattended::Run => {
                let decision = policy::decide(command, AuthorizationLevel::Restricted, &policy);
                let timeout = u64::from(task.timeout_seconds).min(SCHEDULE_TIMEOUT_MAX);
                self.start_scheduled(
                    &task.id,
                    &task.name,
                    terminal,
                    command,
                    &decision,
                    "scheduled",
                    timeout,
                    cx,
                );
            }
            schedule::Unattended::Review(decision) => {
                let reason = if decision.unresolved {
                    pending::REASON_UNRESOLVED
                } else {
                    pending::REASON_WRITE
                };
                self.queue_pending(task, terminal, command, &decision, reason);
                self.notify_system(
                    cx,
                    t!("assistant.schedule.waiting", name = task.name.as_str()),
                    false,
                );
            }
            schedule::Unattended::Refuse(decision) => {
                self.record_raw(&task.target.alias, &decision, "denied", None, "policy");
                self.notify_system(
                    cx,
                    t!("assistant.schedule.refused", name = task.name.as_str()),
                    true,
                );
            }
        }
    }

    /// Start one scheduled answer. It reads the terminal and reports; a
    /// command it suggests waits in the review list.
    fn trigger_prompt(
        &mut self,
        task: &Schedule,
        terminal: &AssistantTerminal,
        prompt: &str,
        cx: &mut Context<Self>,
    ) {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let selection = assistant.selection();
        if selection.base_url.trim().is_empty() || selection.model.trim().is_empty() {
            let decision =
                policy::decide(prompt, AuthorizationLevel::Restricted, &Policy::default());
            self.record_raw(
                &task.target.alias,
                &decision,
                "unsupported",
                None,
                "scheduled",
            );
            self.notify_system(cx, t!("assistant.error.no_provider"), true);
            return;
        }
        let timeout =
            Duration::from_secs(assistant.timeout_seconds.clamp(10, SCHEDULE_TIMEOUT_MAX));
        let request = StreamRequest {
            protocol: selection.protocol,
            base_url: selection.base_url,
            full_url: selection.full_url,
            model: selection.model,
            keychain_id: selection.keychain_id,
            messages: vec![ChatMessage::user(prompt.to_string())],
            max_tokens: selection.max_tokens,
            timeout,
            terminals: vec![ToolTerminal {
                alias: terminal.alias.clone(),
                title: terminal.title.clone(),
            }],
            scheduled: true,
            plain: false,
            headers: selection.headers,
            proxy: assistant.proxy_route(selection.protocol).map(Into::into),
            temperature: selection.temperature,
            top_p: selection.top_p,
            top_k: selection.top_k,
            thinking_param: selection.thinking_param,
            thinking_effort: selection.thinking_effort,
            stop: selection.stop,
            extra: selection.extra,
            instruction: selection.instruction,
            trial: false,
        };
        self.scheduled_ai = Some(ScheduledAiRun {
            name: task.name.clone(),
            alias: task.target.alias.clone(),
            target: task.target.clone(),
            messages: vec![ChatMessage::user(prompt.to_string())],
            stream: client::start(request),
            text: String::new(),
            rounds: 1,
            deadline: Instant::now() + timeout,
        });
        self.ensure_tick(cx);
        cx.notify();
    }

    /// Start one command beside its shell and keep its channel until the
    /// exit code arrives.
    #[allow(clippy::too_many_arguments)]
    fn start_scheduled(
        &mut self,
        schedule_id: &str,
        name: &str,
        terminal: &AssistantTerminal,
        command: &str,
        decision: &Decision,
        approver: &'static str,
        timeout_seconds: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(view) = terminal.view.as_ref().and_then(|weak| weak.upgrade()) else {
            self.record_raw(&terminal.alias, decision, "unsupported", None, approver);
            self.notify_system(cx, t!("assistant.schedule.unsupported", name = name), true);
            return;
        };
        let timeout = Duration::from_secs(timeout_seconds.clamp(1, SCHEDULE_TIMEOUT_MAX));
        let Some(receiver) =
            view.read(cx)
                .exec_detail(command.to_string(), timeout, EXEC_LIMIT, cx)
        else {
            self.record_raw(&terminal.alias, decision, "unsupported", None, approver);
            return;
        };
        let _ = schedule_id;
        self.scheduled.push(ScheduledRun {
            name: name.to_string(),
            alias: terminal.alias.clone(),
            decision: decision.clone(),
            approver,
            receiver,
        });
        self.ensure_tick(cx);
        cx.notify();
    }

    /// Put a command on the review list; it has not run.
    fn queue_pending(
        &mut self,
        task: &Schedule,
        terminal: &AssistantTerminal,
        command: &str,
        decision: &Decision,
        reason: &str,
    ) {
        let now = schedule::now_millis();
        let item = PendingItem::new(
            format!("pending-{now}-{}", self.pending_items.len()),
            &task.id,
            &task.name,
            &task.target.alias,
            command,
            reason,
            now,
            decision.risk.name(),
            decision.rule.name(),
        )
        .on_terminal(&terminal.id, &terminal.title, terminal.local);
        self.pending_items.push(item);
        let _ = pending::save(&Self::pending_path(), &self.pending_items);
        self.record_raw(&task.target.alias, decision, "pending", None, "scheduled");
    }

    /// Take the outcome of every scheduled run that has one.
    fn poll_scheduled(&mut self, cx: &mut Context<Self>) {
        let mut index = 0;
        while index < self.scheduled.len() {
            let result = match self.scheduled[index].receiver.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => {
                    index += 1;
                    continue;
                }
                Err(TryRecvError::Disconnected) => {
                    Err(t!("assistant.tool.transport_closed").to_string())
                }
            };
            let run = self.scheduled.remove(index);
            let (_, exit_code) = tool_result_body(result);
            self.record_raw(&run.alias, &run.decision, "exec", exit_code, run.approver);
            let ok = exit_code == Some(0);
            let message = if ok {
                t!("assistant.schedule.finished", name = run.name.as_str())
            } else {
                t!("assistant.schedule.failed", name = run.name.as_str())
            };
            self.notify_system(cx, message, !ok);
        }
    }

    /// Take what the scheduled answer has so far. A tool call is answered
    /// from the terminal or the review list, never run.
    fn poll_scheduled_ai(&mut self, cx: &mut Context<Self>) {
        let Some(mut run) = self.scheduled_ai.take() else {
            return;
        };
        let policy = self.policy(cx);
        let mut calls = None;
        let mut done = false;
        let mut failed = None;
        loop {
            match run.stream.rx.try_recv() {
                Ok(StreamEvent::Delta(text)) => run.text.push_str(&text),
                Ok(StreamEvent::Thinking(_)) => {}
                Ok(StreamEvent::ToolCalls(list)) => {
                    calls = Some(list);
                    break;
                }
                Ok(StreamEvent::Done) => {
                    done = true;
                    break;
                }
                Ok(StreamEvent::Failed(error)) => {
                    failed = Some(error.message().to_string());
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        if Instant::now() > run.deadline {
            run.stream.cancel();
            failed = Some(t!("assistant.schedule.ai_timeout").to_string());
        }
        if let Some(error) = failed {
            self.finish_ai_failed(run, error, cx);
            return;
        }
        let Some(list) = calls else {
            if done {
                self.finish_ai(run, cx);
            } else {
                self.scheduled_ai = Some(run);
            }
            return;
        };
        if list.is_empty() || run.rounds >= SCHEDULE_AI_ROUNDS {
            self.finish_ai(run, cx);
            return;
        }
        run.messages.push(ChatMessage::assistant_calls(
            std::mem::take(&mut run.text),
            list.clone(),
        ));
        let screen = self.screen_of(&run.target, cx);
        for call in &list {
            let result = match call.name.as_str() {
                "read_terminal" => screen
                    .clone()
                    .unwrap_or_else(|| tool_error("terminal_gone")),
                "host_facts" => serde_json::json!({
                    "alias": run.alias,
                    "title": run.target.title,
                    "local": run.target.local,
                })
                .to_string(),
                "propose_command" => {
                    if let Ok(args) = serde_json::from_str::<RunCommandArgs>(&call.arguments) {
                        self.queue_ai_proposal(&run, &args.command, &policy);
                    }
                    r#"{"status":"pending_review"}"#.to_string()
                }
                _ => tool_error("unknown_tool"),
            };
            run.messages
                .push(ChatMessage::tool_result(&call.id, result));
        }
        run.rounds += 1;
        self.start_ai_round(run, cx);
    }

    /// Start the next turn of a scheduled answer with what the tools said.
    fn start_ai_round(&mut self, mut run: ScheduledAiRun, cx: &mut Context<Self>) {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let selection = assistant.selection();
        let timeout =
            Duration::from_secs(assistant.timeout_seconds.clamp(10, SCHEDULE_TIMEOUT_MAX));
        let request = StreamRequest {
            protocol: selection.protocol,
            base_url: selection.base_url,
            full_url: selection.full_url,
            model: selection.model,
            keychain_id: selection.keychain_id,
            messages: run.messages.clone(),
            max_tokens: selection.max_tokens,
            timeout,
            terminals: vec![ToolTerminal {
                alias: run.alias.clone(),
                title: run.target.title.clone(),
            }],
            scheduled: true,
            plain: false,
            headers: selection.headers,
            proxy: assistant.proxy_route(selection.protocol).map(Into::into),
            temperature: selection.temperature,
            top_p: selection.top_p,
            top_k: selection.top_k,
            thinking_param: selection.thinking_param,
            thinking_effort: selection.thinking_effort,
            stop: selection.stop,
            extra: selection.extra,
            instruction: selection.instruction,
            trial: false,
        };
        run.stream = client::start(request);
        run.deadline = Instant::now() + timeout;
        self.scheduled_ai = Some(run);
        self.ensure_tick(cx);
    }

    /// A command the model suggested in a task. It has not run and will not
    /// run until the user allows it.
    fn queue_ai_proposal(&mut self, run: &ScheduledAiRun, command: &str, policy: &Policy) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        let decision = policy::decide(command, AuthorizationLevel::Restricted, policy);
        if decision.verdict == Verdict::Deny {
            self.record_raw(&run.alias, &decision, "denied", None, "scheduled");
            return;
        }
        let now = schedule::now_millis();
        let item = PendingItem::new(
            format!("pending-{now}-{}", self.pending_items.len()),
            "",
            run.name.clone(),
            run.alias.clone(),
            command,
            pending::REASON_AI_PROPOSAL,
            now,
            decision.risk.name(),
            decision.rule.name(),
        )
        .on_terminal(&run.target.terminal_id, &run.target.title, run.target.local);
        self.pending_items.push(item);
        let _ = pending::save(&Self::pending_path(), &self.pending_items);
        self.record_raw(&run.alias, &decision, "pending", None, "scheduled");
    }

    /// The visible screen of the task's terminal, as the read tool returns
    /// it. A screen that is gone says so; it is never replaced by another
    /// terminal.
    fn screen_of(&self, target: &ScheduleTarget, cx: &App) -> Option<String> {
        let terminal = self.resolve_target(target)?;
        let view = terminal.view.as_ref().and_then(|weak| weak.upgrade())?;
        let screen = view.read(cx).screen_text(cx);
        Some(screen.chars().take(12_000).collect())
    }

    /// Write the answer as a report; the file name is what the notice says.
    fn finish_ai(&mut self, run: ScheduledAiRun, cx: &mut Context<Self>) {
        let text = run.text.trim().to_string();
        if text.is_empty() {
            self.finish_ai_failed(run, t!("assistant.schedule.ai_empty").to_string(), cx);
            return;
        }
        let dir = crate::app::data_dir().join("assistant").join("reports");
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
        let file = dir.join(format!("{stamp}-{}.md", report_slug(&run.name)));
        let body = format!("# {}\n\n{text}\n", run.name);
        let saved = std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::write(&file, body))
            .is_ok();
        let _ = audit::append(&audit::AuditEntry {
            alias: &run.alias,
            command: &run.name,
            risk: "R0",
            rule: "schedule_report",
            channel: "report",
            exit_code: None,
            level: AuthorizationLevel::Restricted.name(),
            approver: "scheduled",
            override_reason: None,
        });
        if saved {
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            self.notify_system(cx, t!("assistant.report.saved", name = name), false);
        } else {
            self.notify_system(cx, t!("assistant.report.failed"), true);
        }
    }

    fn finish_ai_failed(&mut self, run: ScheduledAiRun, reason: String, cx: &mut Context<Self>) {
        let _ = audit::append(&audit::AuditEntry {
            alias: &run.alias,
            command: &run.name,
            risk: "R0",
            rule: "schedule_report",
            channel: "failed",
            exit_code: None,
            level: AuthorizationLevel::Restricted.name(),
            approver: "scheduled",
            override_reason: None,
        });
        self.notify_system(
            cx,
            t!(
                "assistant.schedule.ai_failed",
                name = run.name.as_str(),
                reason = reason
            ),
            true,
        );
    }

    /// The one terminal a task names: its id this session, or the single
    /// open terminal with that name. Two of the name is not a match.
    fn resolve_target(&self, target: &ScheduleTarget) -> Option<AssistantTerminal> {
        if let Some(terminal) = self
            .available
            .iter()
            .find(|terminal| terminal.id == target.terminal_id)
        {
            return Some(terminal.clone());
        }
        let mut candidates = self.available.iter().filter(|terminal| {
            terminal.local == target.local && terminal.title.eq_ignore_ascii_case(&target.title)
        });
        let first = candidates.next()?;
        if candidates.next().is_some() {
            return None;
        }
        Some(first.clone())
    }

    /// A task notice goes to the system when the window is not in front and
    /// to the window otherwise; the panel is not necessarily on screen.
    fn notify_system(&self, cx: &mut Context<Self>, message: SharedString, error: bool) {
        let window = self.window;
        let _ = cx.update_window(window, move |_, window, cx| {
            let notification = if error {
                Notification::error(message)
            } else {
                Notification::success(message)
            };
            window.push_notification(notification, cx);
        });
    }

    /// One audit line with no tool call behind it: a missed trigger, a
    /// refusal, a review item.
    fn record_raw(
        &mut self,
        alias: &str,
        decision: &Decision,
        channel: &str,
        exit_code: Option<i32>,
        approver: &str,
    ) {
        self.record(
            alias,
            decision,
            channel,
            exit_code,
            approver,
            AuthorizationLevel::Restricted.name(),
            None,
        );
    }

    /// Add a task on the first bound terminal. The dialog carries the
    /// values; the panel owns the list and the save. The command or prompt
    /// is frozen here, so later edits elsewhere cannot change it.
    pub(super) fn add_schedule_values(
        &mut self,
        text: &str,
        interval: u32,
        prompt: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let text = text.trim().to_string();
        if text.is_empty() {
            return Ok(());
        }
        let Some(terminal) = self.bindings.entries().first().cloned() else {
            return Err(t!("assistant.schedule.no_target"));
        };
        if !terminal.can_exec() {
            return Err(if terminal.local {
                t!("assistant.schedule.local")
            } else {
                t!("assistant.schedule.shell_only")
            });
        }
        let interval = interval.clamp(
            schedule::MIN_INTERVAL_MINUTES,
            schedule::MAX_INTERVAL_MINUTES,
        );
        let target = ScheduleTarget::new(
            &terminal.id,
            &terminal.title,
            &terminal.alias,
            terminal.local,
        );
        let name: String = text.chars().take(32).collect();
        let recurrence = schedule::Recurrence::EveryMinutes { n: interval };
        let task = if prompt {
            Schedule::fixed_prompt(chrono::Local::now(), name, target, text, recurrence)
        } else {
            Schedule::fixed_command(chrono::Local::now(), name, target, text, recurrence)
        };
        self.schedules.add(task);
        let _ = self.schedules.save(&Self::schedules_path());
        cx.notify();
        Ok(())
    }

    pub(super) fn toggle_schedule(&mut self, id: &str, enabled: bool, cx: &mut Context<Self>) {
        if let Some(task) = self.schedules.get_mut(id) {
            task.enabled = enabled;
            task.updated_at = schedule::now_millis();
        }
        let _ = self.schedules.save(&Self::schedules_path());
        cx.notify();
    }

    pub(super) fn remove_schedule(&mut self, id: &str, cx: &mut Context<Self>) {
        self.schedules.remove(id);
        let _ = self.schedules.save(&Self::schedules_path());
        cx.notify();
    }

    /// Point a waiting task at the first bound terminal, when the user says
    /// so. Nothing is moved on its own.
    pub(super) fn rebind_schedule(
        &mut self,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Result<(), SharedString> {
        let Some(terminal) = self.bindings.entries().first().cloned() else {
            return Err(t!("assistant.schedule.no_target"));
        };
        if !terminal.can_exec() {
            return Err(if terminal.local {
                t!("assistant.schedule.local")
            } else {
                t!("assistant.schedule.shell_only")
            });
        }
        let target = ScheduleTarget::new(
            &terminal.id,
            &terminal.title,
            &terminal.alias,
            terminal.local,
        );
        if let Some(task) = self.schedules.get_mut(id) {
            if task.bind_target(target).is_err() {
                return Err(t!("assistant.schedule.second_target"));
            }
            task.updated_at = schedule::now_millis();
            task.next_due_at =
                schedule::next_due_after(chrono::Local::now(), task.recurrence).timestamp_millis();
        }
        let _ = self.schedules.save(&Self::schedules_path());
        cx.notify();
        Ok(())
    }

    /// Run a review item the user allowed. It is judged again at the
    /// restricted level; a refusal is not overridable here.
    pub(super) fn approve_pending_item(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.pending_items.len() {
            return;
        }
        let item = self.pending_items.remove(index);
        let _ = pending::save(&Self::pending_path(), &self.pending_items);
        let target = ScheduleTarget::new(&item.terminal_id, &item.title, &item.alias, item.local);
        let Some(terminal) = self.resolve_target(&target) else {
            self.pending_items.push(item);
            let _ = pending::save(&Self::pending_path(), &self.pending_items);
            self.notify_system(cx, t!("assistant.schedule.rebind_short"), true);
            return;
        };
        let decision = policy::decide(
            &item.command,
            AuthorizationLevel::Restricted,
            &self.policy(cx),
        );
        if decision.verdict == Verdict::Deny || !terminal.can_exec() {
            let channel = if decision.verdict == Verdict::Deny {
                "denied"
            } else {
                "unsupported"
            };
            self.record_raw(&item.alias, &decision, channel, None, "user");
            self.notify_system(
                cx,
                t!("assistant.schedule.refused", name = item.name.as_str()),
                true,
            );
            return;
        }
        self.start_scheduled(
            &item.schedule_id,
            &item.name,
            &terminal,
            &item.command,
            &decision,
            "user",
            EXEC_TIMEOUT.as_secs(),
            cx,
        );
    }

    pub(super) fn deny_pending_item(&mut self, index: usize, cx: &mut Context<Self>) {
        if index >= self.pending_items.len() {
            return;
        }
        let item = self.pending_items.remove(index);
        let _ = pending::save(&Self::pending_path(), &self.pending_items);
        let decision = policy::decide(
            &item.command,
            AuthorizationLevel::Restricted,
            &self.policy(cx),
        );
        self.record_raw(&item.alias, &decision, "denied", None, "user");
        self.notify_system(
            cx,
            t!("assistant.schedule.refused", name = item.name.as_str()),
            true,
        );
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

    /// A command runs, a card waits, or a question is being answered: the
    /// next turn would have nothing to read yet, so the composer holds back
    /// sending and switching models.
    fn busy(&self) -> bool {
        !self.execs.is_empty()
            || !self.injects.is_empty()
            || !self.waits.is_empty()
            || !self.waiting.is_empty()
            || !self.pending.is_empty()
            || self.ask.is_some()
            || self.classify.is_some()
            || self.compact.is_some()
    }

    /// What the model menu may do with the built-in trial now.
    fn trial_offer(&self, cx: &App) -> trial::Offer {
        trial::offer(self.account.read(cx).is_signed_in(), self.trial.as_ref())
    }

    /// Whether the saved choice is the trial. Missing means the user has not
    /// chosen yet, and a fresh setup starts on the trial.
    fn trial_selected(&self, cx: &App) -> bool {
        self.settings.read(cx).settings().assistant.prefers_trial()
    }

    /// What the next request uses: the trial when it is the saved choice and
    /// still offered, otherwise the saved model.
    fn selection(&self, cx: &App) -> ModelSelection {
        if self.trial_selected(cx)
            && self.trial_offer(cx) == trial::Offer::Ready
            && let Some(status) = &self.trial
        {
            return trial::selection(status);
        }
        self.settings.read(cx).settings().assistant.selection()
    }

    /// Why the picked trial cannot answer now, if it cannot.
    fn trial_blocked(&self, cx: &App) -> Option<SharedString> {
        if !self.trial_selected(cx) {
            return None;
        }
        match self.trial_offer(cx) {
            trial::Offer::Ready => None,
            trial::Offer::Exhausted => Some(t!("assistant.trial.error.limit")),
            trial::Offer::Hidden if !self.account.read(cx).is_signed_in() => None,
            trial::Offer::Hidden if self.trial.is_some() => {
                Some(t!("assistant.trial.error.disabled"))
            }
            trial::Offer::Hidden => Some(t!("assistant.trial.error.unavailable")),
        }
    }

    /// The account came or went: the trial goes with it.
    fn trial_changed(&mut self, cx: &mut Context<Self>) {
        if !self.account.read(cx).is_signed_in() {
            self.trial = None;
            self.trial_probe = None;
            self.trial_checked = None;
        } else {
            // The first probe after a sign-in is immediate: there is no
            // fresh status to trust yet.
            self.refresh_trial(cx, false);
        }
        cx.notify();
    }

    /// Pick the trial for later turns. A running answer or a waiting card
    /// holds it back, like any other model switch.
    fn select_trial(&mut self, cx: &mut Context<Self>) {
        if self.stream.is_some() || self.busy() {
            return;
        }
        if self.trial_offer(cx) != trial::Offer::Ready {
            return;
        }
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| settings.assistant.trial_selected = Some(true),
                cx,
            )
        });
        self.refresh_trial(cx, true);
        cx.notify();
    }

    /// Ask the service whether the trial is on, unless a probe is already
    /// on its way or the last answer is still fresh.
    fn refresh_trial(&mut self, cx: &mut Context<Self>, force: bool) {
        if !self.account.read(cx).is_signed_in() {
            self.trial = None;
            self.trial_probe = None;
            self.trial_checked = None;
            return;
        }
        if self.trial_probe.is_some() {
            return;
        }
        if !force
            && self
                .trial_checked
                .is_some_and(|at| at.elapsed() < TRIAL_STATUS_TTL)
        {
            return;
        }
        self.trial_probe = Some(trial::probe());
        self.begin_trial_poll(cx);
    }

    fn begin_trial_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let done = this
                    .update(cx, |this, cx| this.poll_trial(cx))
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        })
        .detach();
    }

    /// Take the probe's answer; `true` when nothing more is coming.
    fn poll_trial(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(probe) = &self.trial_probe else {
            return true;
        };
        let result = match probe.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => None,
        };
        self.trial_probe = None;
        self.trial_checked = Some(Instant::now());
        self.trial = result;
        cx.notify();
        true
    }

    /// Switch which model answers: one of the configured profiles, or the
    /// empty id for the active provider's own. It takes effect on the next
    /// turn; the conversation so far stays as it is.
    fn set_model(&mut self, id: String, cx: &mut Context<Self>) {
        if self.stream.is_some() || self.busy() {
            return;
        }
        if self.settings.read(cx).settings().assistant.active_model == id
            && !self.trial_selected(cx)
        {
            return;
        }
        self.settings.update(cx, |store, cx| {
            store.update(
                |settings| {
                    settings.assistant.active_model = id;
                    settings.assistant.trial_selected = Some(false);
                },
                cx,
            )
        });
        cx.notify();
    }

    /// Load the role into this conversation, or take it out. The note the
    /// next request adds is rebuilt from the library, so a role deleted in
    /// the settings falls away here too.
    fn toggle_role(&mut self, id: String, cx: &mut Context<Self>) {
        if let Some(index) = self.roles.iter().position(|role| *role == id) {
            self.roles.remove(index);
        } else {
            self.roles.push(id);
        }
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let library = super::roles::library(&assistant);
        self.roles
            .retain(|id| super::roles::role(&library, id).is_some());
        self.conversation_note = super::roles::instructions(&library, &self.roles);
        cx.notify();
    }

    /// Drop every role; the default assistant answers again.
    fn clear_roles(&mut self, cx: &mut Context<Self>) {
        self.roles.clear();
        self.conversation_note.clear();
        cx.notify();
    }

    /// Ask the model. The panel keeps the conversation; the worker keeps
    /// the key. A question starts a new exchange, so the round count starts
    /// again.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.stream.is_some() || self.busy() {
            return;
        }
        let text = self.input.read(cx).value().trim().to_string();
        if text.is_empty() && self.attachments.is_empty() {
            return;
        }
        if self.bindings.is_empty() {
            self.error = Some(t!("assistant.error.no_terminal"));
            cx.notify();
            return;
        }
        if let Some(message) = self.trial_blocked(cx) {
            self.error = Some(message);
            cx.notify();
            return;
        }
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let selection = self.selection(cx);
        if !selection.supports_images && self.attachments.iter().any(Attachment::is_image) {
            // The model was marked text-only after the picture was added;
            // sending it would only come back as a provider error.
            self.error = Some(t!("assistant.attachment.images_off"));
            cx.notify();
            return;
        }
        if selection.base_url.trim().is_empty() || selection.model.trim().is_empty() {
            self.error = Some(t!("assistant.error.no_provider"));
            self.open_config(window, cx);
            cx.notify();
            return;
        }
        // A role can be loaded from the menu or mentioned right here with
        // `@name`; both apply to this conversation. Mentions do not stick.
        let library = super::roles::library(&assistant);
        let mut loaded = self.roles.clone();
        for id in super::roles::mentioned_roles(&text, &library.roles) {
            if !loaded.contains(&id) {
                loaded.push(id);
            }
        }
        self.conversation_note = super::roles::instructions(&library, &loaded);

        let attachments = std::mem::take(&mut self.attachments);
        if let Some(dir) = self
            .sessions
            .active_id()
            .map(|id| self.sessions.attachment_dir(id))
        {
            for attachment in &attachments {
                if attachment.save(&dir).is_err() {
                    self.notice = Some(t!("assistant.attachment.save_failed"));
                }
            }
        }
        let references = attachments
            .iter()
            .map(|attachment| attachment.reference())
            .collect();
        let at = schedule::now_millis();
        self.messages
            .push(ChatMessage::user_with(text.clone(), attachments).with_time(at));
        self.session_append(SessionEvent::User {
            text,
            attachments: references,
            at,
        });
        self.queue.clear();
        self.pending.clear();
        self.rounds = 0;
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.error = None;
        self.notice = None;
        self.stopped = false;
        self.exchange = true;

        self.request_round(cx);
        cx.notify();
    }

    /// Take pictures or files off the clipboard. Returns whether anything was
    /// taken, so plain text still pastes into the field.
    fn attach_clipboard(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let name = t!("assistant.attachment.pasted_image").to_string();
        let mut images = Vec::new();
        let mut files = Vec::new();
        for entry in item.entries {
            match entry {
                ClipboardEntry::Image(image) => images.push(image),
                ClipboardEntry::ExternalPaths(paths) => files.extend(paths.paths().iter().cloned()),
                ClipboardEntry::String(_) => {}
            }
        }
        let taken = !images.is_empty() || !files.is_empty();
        for image in images {
            self.add_attachment(Attachment::from_image(name.clone(), image), cx);
        }
        if !files.is_empty() {
            self.attach_paths(files, cx);
        }
        taken
    }

    /// Read dropped or chosen files on the background executor; the model
    /// only ever sees the result.
    pub fn attach_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let room = attachment::MAX_ATTACHMENTS.saturating_sub(self.attachments.len());
        if room == 0 {
            self.notice = Some(t!(
                "assistant.attachment.too_many",
                limit = attachment::MAX_ATTACHMENTS
            ));
            cx.notify();
            return;
        }
        let paths: Vec<PathBuf> = paths
            .into_iter()
            .filter(|path| path.is_file())
            .take(room)
            .collect();
        if paths.is_empty() {
            return;
        }
        let task = cx.background_spawn(async move {
            paths
                .iter()
                .map(|path| Attachment::from_path(path))
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let results = task.await;
            let _ = this.update(cx, |this, cx| {
                for result in results {
                    this.add_attachment(result, cx);
                }
            });
        })
        .detach();
    }

    fn add_attachment(
        &mut self,
        result: Result<Attachment, AttachmentError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(attachment) => {
                if self.attachments.len() >= attachment::MAX_ATTACHMENTS {
                    self.notice = Some(t!(
                        "assistant.attachment.too_many",
                        limit = attachment::MAX_ATTACHMENTS
                    ));
                } else if attachment.is_image() && !self.selection(cx).supports_images {
                    // The model's profile says it cannot see; a picture
                    // would come back as a provider error, so say it here.
                    self.notice = Some(t!("assistant.attachment.images_off"));
                } else {
                    self.attachments.push(attachment);
                }
            }
            Err(error) => self.notice = Some(self.describe_attachment_error(error)),
        }
        cx.notify();
    }

    fn describe_attachment_error(&self, error: AttachmentError) -> SharedString {
        match error {
            AttachmentError::UnsupportedFormat => t!("assistant.attachment.unsupported"),
            AttachmentError::TooLarge { limit } => t!(
                "assistant.attachment.too_large",
                limit = attachment::size_label(limit)
            ),
            AttachmentError::Binary => t!("assistant.attachment.binary"),
            AttachmentError::Read(error) => {
                t!(
                    "assistant.attachment.unreadable",
                    reason = error.to_string()
                )
            }
            AttachmentError::LimitReached => t!(
                "assistant.attachment.too_many",
                limit = attachment::MAX_ATTACHMENTS
            ),
        }
    }

    fn choose_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let choice = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t!("assistant.attachment.add")),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = choice.await {
                let _ = this.update_in(cx, |this, _window, cx| this.attach_paths(paths, cx));
            }
        })
        .detach();
    }

    fn remove_attachment(&mut self, id: &str, cx: &mut Context<Self>) {
        self.attachments.retain(|attachment| attachment.id != id);
        cx.notify();
    }

    fn preview_attachment(
        &mut self,
        attachment: Attachment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.open_dialog(cx, move |dialog, _window, cx| {
            let title = attachment.name.clone();
            let body = match &attachment.kind {
                AttachmentKind::Image(_) => match attachment.image_source() {
                    Some(source) => img(source)
                        .max_w(px(720.))
                        .max_h(px(560.))
                        .object_fit(ObjectFit::Contain)
                        .into_any_element(),
                    None => div().into_any_element(),
                },
                AttachmentKind::Text { text, truncated } => v_flex()
                    .gap_2()
                    .child(
                        div()
                            .id("assistant-attachment-preview")
                            .max_h(px(560.))
                            .overflow_y_scroll()
                            .text_sm()
                            .child(text.clone()),
                    )
                    .when(*truncated, |column| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(t!("assistant.attachment.truncated")),
                        )
                    })
                    .into_any_element(),
            };
            dialog.title(title).child(body)
        });
    }

    /// Ask for one more model turn. Nothing is asked while a command is
    /// running or a card is waiting: the answer would have no result to
    /// read yet.
    fn request_round(&mut self, cx: &mut Context<Self>) {
        if !self.exchange
            || self.stream.is_some()
            || !self.execs.is_empty()
            || !self.injects.is_empty()
            || !self.waits.is_empty()
            || !self.waiting.is_empty()
            || self.classify.is_some()
            || self.ask.is_some()
            || self.compact.is_some()
            || !self.pending.is_empty()
            || !self.queue.is_empty()
        {
            return;
        }
        if let Some(message) = self.trial_blocked(cx) {
            self.error = Some(message);
            self.exchange = false;
            cx.notify();
            return;
        }
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let selection = self.selection(cx);
        let round_limit = selection.tool_rounds.max(1);
        if self.rounds >= round_limit {
            self.notice = Some(t!("assistant.tool.round_limit", rounds = round_limit));
            return;
        }
        if selection.base_url.trim().is_empty() || selection.model.trim().is_empty() {
            self.error = Some(t!("assistant.error.no_provider"));
            return;
        }
        // Near the window, the older turns become a summary before this
        // round is asked for. The summary call ends by calling back here.
        if self.maybe_compact(cx) {
            return;
        }
        self.rounds += 1;
        self.messages
            .push(ChatMessage::assistant(String::new()).with_time(schedule::now_millis()));
        // The model's own instruction first, then what the roles and the
        // enabled skills add: the roles are the last word on style.
        let mut instruction = selection.instruction.trim().to_string();
        let note = self.conversation_note.trim();
        if !note.is_empty() {
            if instruction.is_empty() {
                instruction = note.to_string();
            } else {
                instruction.push_str("\n\n");
                instruction.push_str(note);
            }
        }
        // Signed in, the user has a name the assistant may use once; it is
        // a reference, not a required form of address, because a wording
        // that demanded it made replies repeat the name in every sentence.
        // Signed out — the default, and the only state offline — the
        // neutral wording stays.
        if let Some(name) = self.account.read(cx).assistant_addressing() {
            if !instruction.is_empty() {
                instruction.push_str("\n\n");
            }
            instruction.push_str(&t!("assistant.instruction.address_user", name = name));
        }
        // The turns that are no longer in the message list travel as a
        // summary the compaction wrote.
        if let Some(compaction) = &self.compaction
            && !compaction.summary.trim().is_empty()
        {
            if !instruction.is_empty() {
                instruction.push_str("\n\n");
            }
            instruction.push_str("Summary of the earlier conversation:\n");
            instruction.push_str(compaction.summary.trim());
        }
        let request = StreamRequest {
            protocol: selection.protocol,
            base_url: selection.base_url,
            full_url: selection.full_url,
            model: selection.model,
            keychain_id: selection.keychain_id,
            messages: self.messages[..self.messages.len() - 1].to_vec(),
            max_tokens: selection.max_tokens,
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
            scheduled: false,
            plain: false,
            headers: selection.headers,
            proxy: assistant.proxy_route(selection.protocol).map(Into::into),
            temperature: selection.temperature,
            top_p: selection.top_p,
            top_k: selection.top_k,
            thinking_param: selection.thinking_param,
            thinking_effort: selection.thinking_effort,
            stop: selection.stop,
            extra: selection.extra,
            instruction,
            trial: selection.trial,
        };
        self.stream_trial = selection.trial;
        self.stream = Some(client::start(request));
        self.begin_poll(cx);
    }

    /// Start a summarizing call when the next turn would come too close to
    /// the window. `true` when one is on its way.
    fn maybe_compact(&mut self, cx: &mut Context<Self>) -> bool {
        let (used, window) = self.context_usage(cx);
        if window == 0 || used * 100 < window * compact::TRIGGER_PERCENT {
            return false;
        }
        let Some(plan) = compact::plan(
            &self.messages,
            self.compaction
                .as_ref()
                .map(|current| current.summary.as_str()),
        ) else {
            return false;
        };
        let selection = self.selection(cx);
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let timeout = Duration::from_secs(
            assistant
                .timeout_seconds
                .clamp(30, compact::SUMMARY_TIMEOUT.as_secs()),
        );
        let request = compact::request(
            &selection,
            plan.prompt,
            timeout,
            assistant.proxy_route(selection.protocol).map(Into::into),
        );
        let count = self.compaction.as_ref().map_or(0, |current| current.count) + plan.count;
        self.compact = Some(CompactRun {
            stream: client::start(request),
            text: String::new(),
            split: plan.split,
            count,
            started: Instant::now(),
        });
        self.begin_compact_poll(cx);
        cx.notify();
        true
    }

    fn begin_compact_poll(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_INTERVAL).await;
                let finished = this
                    .update(cx, |this, cx| this.poll_compact(cx))
                    .unwrap_or(true);
                if finished {
                    break;
                }
            }
        })
        .detach();
    }

    /// Take what the summarizing answer sent. `true` when it is over,
    /// however it ended: a slow or failed summary never holds the
    /// conversation.
    fn poll_compact(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(mut run) = self.compact.take() else {
            return true;
        };
        let mut finished = false;
        let mut failed = false;
        let mut changed = false;
        loop {
            match client::next_event(&run.stream.rx) {
                Some(StreamEvent::Delta(text)) => {
                    run.text.push_str(&text);
                    changed = true;
                }
                Some(StreamEvent::Thinking(_)) | Some(StreamEvent::ToolCalls(_)) => {}
                Some(StreamEvent::Done) => {
                    finished = true;
                    break;
                }
                Some(StreamEvent::Failed(_)) => {
                    failed = true;
                    finished = true;
                    break;
                }
                None => break,
            }
        }
        if !finished && run.started.elapsed() < compact::SUMMARY_TIMEOUT {
            self.compact = Some(run);
            if changed {
                cx.notify();
            }
            return false;
        }
        if !finished {
            run.stream.cancel();
        }
        let summary = if failed || run.text.trim().is_empty() {
            None
        } else {
            Some(run.text.trim().to_string())
        };
        self.finish_compaction(run, summary, cx);
        true
    }

    /// Replace the messages before the cut with the summary, then continue
    /// the exchange. `summary` is `None` when the model could not write one:
    /// the same messages are simply dropped.
    fn finish_compaction(
        &mut self,
        run: CompactRun,
        summary: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let split = run.split.min(self.messages.len());
        self.messages.drain(..split);
        self.expanded.clear();
        let trimmed = summary.is_none();
        let summary = summary.unwrap_or_else(|| compact::FALLBACK_SUMMARY.to_string());
        self.compaction = Some(Compaction {
            summary,
            count: run.count,
            trimmed,
        });

        // The kept tail alone may still be too large (one huge answer, for
        // example). Drop older turns from it rather than let the request
        // fail.
        let selection = self.selection(cx);
        let standing = self.standing_chars(&selection, cx);
        let window = selection.context_window as usize;
        if window > 0
            && compact::estimate_tokens(&self.messages, standing) * 100
                >= window * compact::TRIGGER_PERCENT
        {
            let removed = compact::trim_to_target(&mut self.messages, standing, window);
            if removed > 0
                && let Some(compaction) = &mut self.compaction
            {
                compaction.count += removed;
                compaction.trimmed = true;
                compaction.summary.push_str("\n\n");
                compaction.summary.push_str(compact::FALLBACK_SUMMARY);
            }
        }
        self.painted_tail = (self.messages.len(), 0);

        let compacted = self.compaction.as_ref().map(|compaction| {
            (
                compaction.summary.clone(),
                compaction.count,
                compaction.trimmed,
            )
        });
        if let Some((summary, count, trimmed)) = compacted {
            self.session_append(SessionEvent::Compact {
                summary,
                count,
                trimmed,
            });
            if trimmed {
                self.notice = Some(t!("assistant.compact.failed"));
            }
        }
        if self.exchange {
            self.request_round(cx);
        } else {
            cx.notify();
        }
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
        let mut failure: Option<SharedString> = None;
        loop {
            match client::next_event(&stream.rx) {
                Some(StreamEvent::Delta(text)) => {
                    if let Some(message) = self.messages.last_mut() {
                        message.text.push_str(&text);
                        changed = true;
                    }
                }
                Some(StreamEvent::Thinking(text)) => {
                    if let Some(message) = self.messages.last_mut() {
                        message.reasoning.push_str(&text);
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
                    failure = self.error.clone();
                    // A turn that failed asks for nothing: running calls it
                    // got out would only start a new turn on a broken one.
                    self.queue.clear();
                    self.exchange = false;
                    finished = true;
                    break;
                }
                None => break,
            }
        }
        if finished {
            self.stream = None;
            // The trial's usage moved; the menu should say so now.
            if std::mem::take(&mut self.stream_trial) {
                self.refresh_trial(cx, true);
            }
            let empty = self.messages.last().is_some_and(|message| {
                message.text.trim().is_empty() && message.tool_calls.is_empty()
            });
            if empty && self.error.is_none() && !self.stopped {
                self.error = Some(t!("assistant.error.empty"));
            }
            // The turn is over: its words and, if it failed, the reason go
            // into the journal. An empty turn asked for tools only.
            let answered = self
                .messages
                .last()
                .filter(|message| message.role == chat::Role::Assistant)
                .map(|message| (message.text.clone(), message.at))
                .filter(|(text, _)| !text.trim().is_empty());
            if let Some((text, at)) = answered {
                self.session_append(SessionEvent::Assistant { text, at });
            }
            if let Some(text) = failure {
                self.session_append(SessionEvent::Error {
                    text: text.to_string(),
                });
            }
            if !self.queue.is_empty() {
                self.advance(cx);
            } else if self.pending.is_empty() && self.ask.is_none() && self.classify.is_none() {
                // Nothing is left to run or ask: the answer is the whole
                // answer, and no later event continues the exchange.
                self.exchange = false;
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
            // What the model had said before the stop is still part of the
            // conversation.
            let answered = self
                .messages
                .last()
                .filter(|message| message.role == chat::Role::Assistant)
                .map(|message| (message.text.clone(), message.at))
                .filter(|(text, _)| !text.trim().is_empty());
            if let Some((text, at)) = answered {
                self.session_append(SessionEvent::Assistant { text, at });
            }
        }
        // A cancelled trial answer still counted against the daily quota.
        if std::mem::take(&mut self.stream_trial) {
            self.refresh_trial(cx, true);
        }
        self.stopped = true;
        self.exchange = false;
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
        self.poll_scheduled(cx);
        self.poll_scheduled_ai(cx);
        self.poll_waits(cx);
        self.pump(cx);
        let busy = !self.execs.is_empty()
            || !self.injects.is_empty()
            || !self.scheduled.is_empty()
            || self.scheduled_ai.is_some()
            || !self.waits.is_empty();
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
            started: Instant::now(),
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
        self.exchange = false;
        self.notice = Some(t!("assistant.full_access.emergency_done"));
        cx.notify();
    }

    fn push_result(&mut self, call_id: String, body: String) {
        self.messages
            .push(ChatMessage::tool_result(call_id, body).with_time(schedule::now_millis()));
    }

    /// Add how long it took and how the policy judged it to a result body.
    /// The card's expanded view reads these; the model sees the same facts,
    /// which are data, not instructions. A body that is not a JSON object
    /// is left as it is.
    fn annotate(body: String, elapsed: Duration, decision: &Decision) -> String {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&body) else {
            return body;
        };
        let Some(object) = value.as_object_mut() else {
            return body;
        };
        object.insert(
            "elapsed_ms".to_string(),
            serde_json::json!(elapsed.as_millis() as u64),
        );
        object.insert("risk".to_string(), serde_json::json!(decision.risk.name()));
        object.insert("rule".to_string(), serde_json::json!(decision.rule.name()));
        object.insert(
            "reason".to_string(),
            serde_json::json!(decision.rule.label().to_string()),
        );
        value.to_string()
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
            let body = Self::annotate(body, run.started.elapsed(), &run.decision);
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
            let body = Self::annotate(outcome.body(), run.started.elapsed(), &run.decision);
            self.push_result(run.call_id, body);
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
            started: Instant::now(),
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

    /// Add one line to the open session's journal. A failure is not worth a
    /// card: the conversation on screen is still there.
    fn session_append(&mut self, event: SessionEvent) {
        let now = schedule::now_millis();
        let Some(id) = self.sessions.active_id().map(str::to_string) else {
            return;
        };
        let _ = self.sessions.append(&id, &event, now);
    }

    /// Whether a conversation is mid-flight: a turn, a command, a wait, a
    /// card or a question. Switching then would split the task in two.
    fn session_busy(&self) -> bool {
        self.exchange
            || self.stream.is_some()
            || !self.execs.is_empty()
            || !self.injects.is_empty()
            || !self.waits.is_empty()
            || !self.waiting.is_empty()
            || !self.pending.is_empty()
            || !self.backgrounds.is_empty()
            || self.classify.is_some()
            || self.ask.is_some()
            || self.compact.is_some()
    }

    /// Show the open session's words. Everything that belonged to the last
    /// exchange's screen is dropped; nothing is replayed or re-run.
    fn load_session(&mut self, cx: &mut Context<Self>) {
        (self.messages, self.compaction) = match self.sessions.active_id() {
            Some(id) => self.sessions.messages(id),
            None => (Vec::new(), None),
        };
        self.compact = None;
        self.expanded.clear();
        self.attachments.clear();
        self.queue.clear();
        self.pending.clear();
        self.waiting.clear();
        self.waits.clear();
        self.ask = None;
        self.classify = None;
        self.exchange = false;
        self.stopped = false;
        self.rounds = 0;
        self.error = None;
        self.notice = None;
        cx.notify();
    }

    fn switch_session(&mut self, forward: bool, cx: &mut Context<Self>) {
        if self.session_busy() {
            self.notice = Some(t!("assistant.session.busy"));
            cx.notify();
            return;
        }
        if self.sessions.cycle(forward) {
            self.load_session(cx);
        }
    }

    fn new_session(&mut self, cx: &mut Context<Self>) {
        if self.session_busy() {
            self.notice = Some(t!("assistant.session.busy"));
            cx.notify();
            return;
        }
        self.sessions.create(schedule::now_millis());
        self.load_session(cx);
    }

    fn delete_session(&mut self, cx: &mut Context<Self>) {
        if self.session_busy() {
            self.notice = Some(t!("assistant.session.busy"));
            cx.notify();
            return;
        }
        let Some(id) = self.sessions.active_id().map(str::to_string) else {
            return;
        };
        self.sessions.delete(&id, schedule::now_millis());
        self.load_session(cx);
    }

    /// The conversation as a report: a title from the session, the facts a
    /// reader needs, then the turns.
    fn export_text(&self, cx: &mut Context<Self>) -> String {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let provider = assistant.active();
        let title = self
            .sessions
            .active()
            .map(|session| session.title.clone())
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| t!("assistant.report.title").to_string());
        let terminals = self
            .bindings
            .entries()
            .iter()
            .map(|entry| entry.alias.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let meta = vec![
            (
                t!("assistant.report.meta.time").to_string(),
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            ),
            (
                t!("assistant.report.meta.protocol").to_string(),
                assistant.protocol.label().to_string(),
            ),
            (
                t!("assistant.report.meta.model").to_string(),
                provider.model.clone(),
            ),
            (t!("assistant.report.meta.terminals").to_string(), terminals),
        ];
        chat::export_markdown(&self.messages, &t!("assistant.role.user"), &title, &meta)
    }

    fn copy_markdown(&mut self, cx: &mut Context<Self>) {
        let markdown = self.export_text(cx);
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
        let markdown = self.export_text(cx);
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
        // Milliseconds, so two reports in one second do not overwrite each
        // other.
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S-%3f");
        let name = format!("assistant-{stamp}.md");
        match std::fs::write(dir.join(&name), markdown) {
            Ok(()) => self.notice = Some(t!("assistant.report.saved", name = name)),
            Err(_) => self.notice = Some(t!("assistant.report.failed")),
        }
        cx.notify();
    }

    /// The terminals this conversation may use, one entry per alias, for the
    /// header line: bound aliases are chips, and an alias whose terminal
    /// could not be identified after a restart is warned about below.
    fn render_bindings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let success = theme.success;
        let danger = theme.danger;
        let accent = theme.accent;
        let radius = theme.radius;

        let open = self.open_terminals();

        let mut row = h_flex()
            .id("assistant-bindings")
            .flex_1()
            .min_w_0()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.terminals.label")),
            );
        if self.bindings.is_empty() && self.bindings.pending().is_empty() {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(t!("assistant.terminals.none")),
            );
        }
        for (index, entry) in self.bindings.entries().iter().enumerate() {
            let id = entry.id.clone();
            let covered = self.level == AuthorizationLevel::FullAccess
                && self.covered.iter().any(|covered| covered == &entry.id);
            let (dot, state) = if covered {
                (danger, t!("assistant.terminals.covered"))
            } else if !entry.can_inject() {
                (muted, t!("assistant.terminals.unavailable"))
            } else {
                (success, t!("assistant.terminals.bound"))
            };
            row = row.child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .pl_1p5()
                    .pr_1()
                    .py_0p5()
                    .rounded(radius)
                    .bg(accent)
                    .child(div().size_1p5().rounded_full().bg(dot))
                    .child(
                        div()
                            .max_w(px(110.))
                            .truncate()
                            .text_xs()
                            .child(entry.alias.clone()),
                    )
                    .child(
                        Button::new(("assistant-unbind", index))
                            .xsmall()
                            .ghost()
                            .icon(CatalogIcon::Unplug)
                            .tooltip(format!("{} · {}", state, t!("assistant.terminals.unbind")))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.toggle_terminal(&id, cx);
                            })),
                    ),
            );
        }
        if !open.is_empty() {
            if self.bindings.has_room() {
                row = row.child(self.render_bind_menu(cx, open.clone()));
            } else {
                row = row.child(
                    Button::new("assistant-bind-full")
                        .xsmall()
                        .ghost()
                        .icon(CatalogIcon::Plug)
                        .label(t!("assistant.terminals.add"))
                        .disabled(true)
                        .tooltip(t!(
                            "assistant.terminals.limit",
                            limit = bindings::MAX_BINDINGS
                        )),
                );
            }
        }
        row
    }

    /// The terminals still open for binding: not bound yet.
    fn open_terminals(&self) -> Vec<AssistantTerminal> {
        self.available
            .iter()
            .filter(|terminal| !self.bindings.is_bound(&terminal.id))
            .cloned()
            .collect()
    }

    /// The aliases a restart could not place, as one line: how many are
    /// waiting, and the rows that answer them when the line is opened.
    fn render_pending_list(
        &self,
        open: &[AssistantTerminal],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let warning = theme.warning;
        let border = theme.border;
        let expanded = self.pending_open;
        let count = self.bindings.pending().len();

        let column = v_flex()
            .id("assistant-pending")
            .border_t_1()
            .border_color(border)
            .bg(warning.opacity(0.08))
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .child(
                        Icon::new(CatalogIcon::CircleAlert)
                            .small()
                            .text_color(warning),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .child(t!("assistant.terminals.pending_count", count = count)),
                    )
                    .child(
                        Button::new("assistant-pending-toggle")
                            .xsmall()
                            .ghost()
                            .icon(if expanded {
                                IconName::ChevronUp
                            } else {
                                IconName::ChevronDown
                            })
                            .tooltip(if expanded {
                                t!("assistant.terminals.pending_hide")
                            } else {
                                t!("assistant.terminals.pending_show")
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.pending_open = !this.pending_open;
                                cx.notify();
                            })),
                    ),
            );
        if !expanded {
            return column;
        }
        let mut rows = v_flex().id("assistant-pending-rows").max_h(px(168.));
        rows = rows.child(
            div()
                .px_2()
                .pb_1()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(t!("assistant.terminals.pending_hint")),
        );
        for (index, wanted) in self.bindings.pending().iter().enumerate() {
            rows = rows.child(self.render_pending_binding(index, wanted, open, cx));
        }
        column.child(rows.overflow_y_scrollbar())
    }

    /// The menu that binds one more open terminal.
    fn render_bind_menu(&self, cx: &mut Context<Self>, open: Vec<AssistantTerminal>) -> AnyElement {
        let panel = cx.entity();
        DropdownButton::new("assistant-bind-menu")
            .button(
                Button::new("assistant-bind")
                    .xsmall()
                    .ghost()
                    .icon(CatalogIcon::Plug)
                    .label(t!("assistant.terminals.add")),
            )
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for terminal in &open {
                    let id = terminal.id.clone();
                    let panel = panel.clone();
                    let label = if terminal.can_inject() {
                        terminal.title.clone()
                    } else {
                        format!(
                            "{} · {}",
                            terminal.title,
                            t!("assistant.terminals.unavailable")
                        )
                    };
                    menu = menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                        panel.update(cx, |panel, cx| panel.toggle_terminal(&id, cx));
                    }));
                }
                menu
            })
            .into_any_element()
    }

    /// One saved alias waiting for its terminal: the alias, the picker that
    /// binds it in one step, and the way to drop it. The note that explains
    /// the wait is the alias's tooltip, so the row stays one line.
    fn render_pending_binding(
        &self,
        index: usize,
        wanted: &SavedBinding,
        open: &[AssistantTerminal],
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rebind: AnyElement = if open.is_empty() {
            Button::new(("assistant-rebind-empty", index))
                .xsmall()
                .label(t!("assistant.terminals.rebind"))
                .disabled(true)
                .tooltip(t!("assistant.terminals.add_empty"))
                .into_any_element()
        } else {
            let panel = cx.entity();
            let open = open.to_vec();
            DropdownButton::new(("assistant-rebind-menu", index))
                .button(
                    Button::new(("assistant-rebind", index))
                        .xsmall()
                        .label(t!("assistant.terminals.rebind")),
                )
                .dropdown_menu(move |menu, _, _| {
                    let mut menu = menu;
                    for terminal in &open {
                        let terminal = terminal.clone();
                        let panel = panel.clone();
                        menu = menu.item(PopupMenuItem::new(terminal.title.clone()).on_click(
                            move |_, _, cx| {
                                let terminal = terminal.clone();
                                panel.update(cx, |panel, cx| {
                                    if panel.bindings.rebind_pending(index, &terminal) {
                                        panel.save_bindings(cx);
                                    }
                                    cx.notify();
                                });
                            },
                        ));
                    }
                    menu
                })
                .into_any_element()
        };

        h_flex()
            .id(("assistant-pending-binding", index))
            .items_center()
            .gap_1()
            .px_2()
            .py_0p5()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .child(wanted.alias.clone()),
            )
            .child(rebind)
            .child(
                Button::new(("assistant-pending-dismiss", index))
                    .xsmall()
                    .ghost()
                    .icon(CatalogIcon::X)
                    .tooltip(t!("assistant.terminals.dismiss"))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.bindings.dismiss_pending(index) {
                            this.save_bindings(cx);
                        }
                        cx.notify();
                    })),
            )
    }

    /// The open conversation, and the ways to move between them.
    fn render_sessions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let title: SharedString = match self.sessions.active() {
            Some(session) if !session.title.is_empty() => session.title.clone().into(),
            Some(_) | None => t!("assistant.session.untitled"),
        };
        let total = self.sessions.sessions().len();
        h_flex()
            .id("assistant-sessions")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(muted)
                    .child(title),
            )
            .child(
                Button::new("assistant-session-prev")
                    .xsmall()
                    .ghost()
                    .icon(CatalogIcon::ArrowLeftToLine)
                    .tooltip(t!("assistant.session.previous"))
                    .disabled(total < 2)
                    .on_click(cx.listener(|this, _, _, cx| this.switch_session(false, cx))),
            )
            .child(
                Button::new("assistant-session-next")
                    .xsmall()
                    .ghost()
                    .icon(CatalogIcon::ArrowRightToLine)
                    .tooltip(t!("assistant.session.next"))
                    .disabled(total < 2)
                    .on_click(cx.listener(|this, _, _, cx| this.switch_session(true, cx))),
            )
            .child(
                Button::new("assistant-session-new")
                    .xsmall()
                    .ghost()
                    .icon(CatalogIcon::CopyPlus)
                    .tooltip(t!("assistant.session.new"))
                    .on_click(cx.listener(|this, _, _, cx| this.new_session(cx))),
            )
            .child(
                Button::new("assistant-session-delete")
                    .xsmall()
                    .ghost()
                    .icon(CatalogIcon::Trash)
                    .tooltip(t!("assistant.session.delete"))
                    .on_click(cx.listener(|this, _, _, cx| this.delete_session(cx))),
            )
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

    /// Fold or open a thinking block or a call card. The id is the call's,
    /// or `thinking-<index>` for a turn's reasoning.
    fn toggle_expanded(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    /// What the model thought before it answered, folded to its first line
    /// until the user opens it.
    fn render_thinking(
        &self,
        index: usize,
        message: &ChatMessage,
        live: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let key = format!("thinking-{index}");
        let open = self.expanded.contains(&key);
        let title = if live {
            t!("assistant.thinking.streaming")
        } else {
            t!("assistant.thinking.title")
        };
        let summary = message
            .reasoning
            .lines()
            .find(|line| !line.trim().is_empty())
            .map(|line| one_line(line, 80));
        let toggle = key.clone();
        let mut head = h_flex()
            .id(("assistant-thinking", index))
            .w_full()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(toggle.clone(), cx);
            }))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .text_color(muted),
            )
            .child(div().text_xs().text_color(muted).child(title));
        if !live && let Some(summary) = summary {
            head = head.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .flex_1()
                    .min_w_0()
                    .child(summary),
            );
        }
        let mut block = v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(border)
            .child(head);
        if open {
            block = block.child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .text_color(muted)
                    .child(message.reasoning.clone()),
            );
        }
        block.into_any_element()
    }

    /// One tool call: folded to what it is doing and where, opened to its
    /// arguments, result, duration and the policy's verdict.
    fn render_call_card(
        &self,
        index: usize,
        position: usize,
        call: &ToolCall,
        result: Option<&ChatMessage>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let open = self.expanded.contains(&call.id);
        let (summary, body) = match result {
            Some(message) => tool_card_parts(&message.text),
            None => (t!("assistant.tool.running").to_string(), String::new()),
        };
        let facts = result
            .map(|message| result_facts(&message.text))
            .unwrap_or_default();
        let parts = call_parts(call);
        let toggle = call.id.clone();
        let head = h_flex()
            .id(("assistant-call", index * 100 + position))
            .w_full()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_expanded(toggle.clone(), cx);
            }))
            .child(
                Icon::new(if open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .text_color(muted),
            )
            .child(
                div()
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_xs()
                    .flex_1()
                    .min_w_0()
                    .child(call_summary(call)),
            )
            .child(div().text_xs().text_color(muted).child(summary.clone()));
        let mut card = v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(border)
            .child(head);
        if open {
            let mut detail = v_flex().w_full().gap_1().pl_5().child(self.detail_row(
                t!("assistant.tool.details.args"),
                call.arguments.clone(),
                true,
                cx,
            ));
            if let Some(terminal) = parts.terminal {
                detail = detail.child(self.detail_row(
                    t!("assistant.tool.details.terminal"),
                    terminal,
                    false,
                    cx,
                ));
            }
            if let Some(command) = parts.detail {
                detail = detail.child(self.detail_row(
                    t!("assistant.tool.details.command"),
                    command,
                    true,
                    cx,
                ));
            }
            let mut outcome = summary;
            if !body.is_empty() {
                outcome.push('\n');
                outcome.push_str(&body);
            }
            detail = detail.child(self.detail_row(
                t!("assistant.tool.details.result"),
                outcome,
                true,
                cx,
            ));
            if let Some(milliseconds) = facts.elapsed_ms {
                detail = detail.child(self.detail_row(
                    t!("assistant.tool.details.elapsed"),
                    duration_text(milliseconds),
                    false,
                    cx,
                ));
            }
            if let Some(verdict) = verdict_text(&facts) {
                detail = detail.child(self.detail_row(
                    t!("assistant.tool.details.verdict"),
                    verdict,
                    false,
                    cx,
                ));
            }
            if facts.truncated {
                detail = detail.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("assistant.tool.details.truncated")),
                );
            }
            if facts.timed_out {
                detail = detail.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("assistant.tool.details.timed_out")),
                );
            }
            card = card.child(detail);
        }
        card.into_any_element()
    }

    /// One labelled row of an opened card.
    fn detail_row(
        &self,
        label: SharedString,
        value: String,
        mono: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        h_flex()
            .w_full()
            .items_start()
            .gap_2()
            .child(div().text_xs().text_color(muted).w(px(40.)).child(label))
            .child(
                div()
                    .when(mono, |text| {
                        text.font_family(cx.theme().mono_font_family.clone())
                    })
                    .text_xs()
                    .flex_1()
                    .min_w_0()
                    .child(value),
            )
            .into_any_element()
    }

    /// A rough count of the tokens this conversation sends each round: the
    /// messages, the standing prompt and the tool descriptions. It is an
    /// estimate, which the display says; the window comes from the model
    /// profile.
    fn context_usage(&self, cx: &mut Context<Self>) -> (usize, usize) {
        let selection = self.selection(cx);
        let standing = self.standing_chars(&selection, cx);
        (
            compact::estimate_tokens(&self.messages, standing),
            selection.context_window as usize,
        )
    }

    /// The characters the next request always carries beside the messages:
    /// the model's instruction, the roles and skills, the user's name, and
    /// any earlier summary.
    fn standing_chars(&self, selection: &ModelSelection, cx: &App) -> usize {
        let mut chars =
            self.conversation_note.chars().count() + selection.instruction.chars().count();
        if let Some(name) = self.account.read(cx).assistant_addressing() {
            chars += t!("assistant.instruction.address_user", name = name)
                .chars()
                .count();
        }
        if let Some(compaction) = &self.compaction {
            chars += compaction.summary.chars().count();
        }
        chars
    }

    /// The conversation area: the messages themselves, the scrollbar, and
    /// the way back to the newest words while the reader is above them.
    fn render_conversation(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.follow_tail();
        let behind = self.behind;
        v_flex()
            .relative()
            .flex_1()
            .min_h_0()
            .child(self.render_messages(cx))
            .when(behind, |area| {
                area.child(
                    div()
                        .absolute()
                        .bottom_3()
                        .left_0()
                        .right_0()
                        .flex()
                        .justify_center()
                        .child(
                            Button::new("assistant-jump-latest")
                                .small()
                                .outline()
                                .icon(IconName::ArrowDown)
                                .label(t!("assistant.scroll.latest"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.behind = false;
                                    this.scroll.scroll_to_bottom();
                                    cx.notify();
                                })),
                        ),
                )
            })
            // On the wrapper, which does not scroll: on the list the bar
            // would scroll away with the messages. Always visible, so the
            // conversation can be dragged without hunting for the bar.
            .child(
                Scrollbar::vertical(&self.scroll)
                    .id("assistant-scrollbar")
                    .mode(ScrollbarMode::Always),
            )
            .into_any_element()
    }

    /// Keep the newest words in view while the reader is at the bottom. A
    /// reader above the tail is left where they are, and the jump button
    /// says the conversation moved on. Runs before each frame is built.
    fn follow_tail(&mut self) {
        let tail = (
            self.messages.len(),
            self.messages
                .last()
                .map_or(0, |message| message.text.len() + message.reasoning.len()),
        );
        // Offsets run from zero at the top to minus the scroll range at the
        // bottom, so the sum is zero when the tail is in view.
        let at_bottom = self.scroll.offset().y + self.scroll.max_offset().y <= px(0.5);
        if at_bottom {
            self.behind = false;
            self.scroll.scroll_to_bottom();
        } else if tail != self.painted_tail {
            self.behind = true;
        }
        self.painted_tail = tail;
    }

    fn render_messages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let streaming = self.stream.is_some();
        let mut list = v_flex()
            .id("assistant-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
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
        if let Some(compaction) = &self.compaction {
            let text = if compaction.trimmed {
                t!("assistant.compact.notice.trimmed", count = compaction.count)
            } else {
                t!("assistant.compact.notice", count = compaction.count)
            };
            list = list.child(
                h_flex()
                    .id("assistant-compaction")
                    .w_full()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(div().h_px().flex_1().bg(cx.theme().border))
                    .child(text)
                    .child(div().h_px().flex_1().bg(cx.theme().border)),
            );
        }
        let account = self.account.read(cx).snapshot();
        for (index, message) in self.messages.iter().enumerate() {
            // A result is drawn inside the call card it belongs to; only a
            // result whose call is gone is shown on its own.
            if message.role == chat::Role::Tool
                && message.tool_call_id.as_deref().is_some_and(|id| {
                    self.messages.iter().any(|other| {
                        other.role == chat::Role::Assistant
                            && other.tool_calls.iter().any(|call| call.id == id)
                    })
                })
            {
                continue;
            }
            let role: SharedString = match message.role {
                chat::Role::User if account.signed_in && !account.display_name.is_empty() => {
                    account.display_name.clone().into()
                }
                chat::Role::User => t!("assistant.role.user"),
                chat::Role::Assistant | chat::Role::System => t!("assistant.role.assistant"),
                chat::Role::Tool => t!("assistant.role.tool"),
            };
            let mut turn = v_flex()
                .id(("assistant-turn", index))
                .w_full()
                .gap_1()
                .child(self.render_message_header(message, role, &account, cx));
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
                    if message.text.is_empty()
                        && message.tool_calls.is_empty()
                        && message.reasoning.is_empty() =>
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
                    if !message.reasoning.is_empty() {
                        let live = streaming && index + 1 == self.messages.len();
                        turn = turn.child(self.render_thinking(index, message, live, cx));
                    }
                    if !message.text.is_empty() {
                        turn = turn.child(message_body(index, &message.text, true));
                    }
                    for (position, call) in message.tool_calls.iter().enumerate() {
                        let result = self.messages.iter().find(|other| {
                            other.role == chat::Role::Tool
                                && other.tool_call_id.as_deref() == Some(call.id.as_str())
                        });
                        turn = turn.child(self.render_call_card(index, position, call, result, cx));
                    }
                }
                _ => {
                    if !message.text.is_empty() {
                        turn = turn.child(message_body(index, &message.text, false));
                    }
                    if !message.attachments.is_empty() {
                        turn = turn.child(self.render_sent_attachments(index, message, cx));
                    }
                }
            }
            list = list.child(turn);
        }
        list
    }

    /// The line above a message's words: the speaker's badge, the name, and
    /// when it was said. A tool result has no speaker, so it gets no badge.
    fn render_message_header(
        &self,
        message: &ChatMessage,
        role: SharedString,
        account: &crate::account::AccountSnapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let mut header = h_flex().w_full().items_center().gap_2();
        if let Some(avatar) = self.render_speaker_avatar(message.role, account, cx) {
            header = header.child(avatar);
        }
        let header = header.child(
            div()
                .text_base()
                .font_weight(FontWeight::MEDIUM)
                .child(role),
        );
        match message_time(message.at) {
            Some(time) => header
                .child(div().text_xs().text_color(muted).child(time))
                .into_any_element(),
            None => header.into_any_element(),
        }
    }

    /// The badge in front of a message's name: the assistant's bot on the
    /// quiet surface, and a placeholder for the user until signing in can
    /// supply the account's picture — when it can, return that image here
    /// for the user arm and leave the rest of the header alone. A tool
    /// result is nobody speaking, so it gets none.
    fn render_speaker_avatar(
        &self,
        role: chat::Role,
        account: &crate::account::AccountSnapshot,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let (ink, glyph) = match role {
            chat::Role::User => {
                if account.signed_in {
                    if let Some(path) = &account.avatar {
                        return Some(Avatar::new().src(path.clone()).small().into_any_element());
                    }
                    if !account.initial.is_empty() {
                        return Some(
                            Avatar::new()
                                .name(account.initial.clone())
                                .small()
                                .into_any_element(),
                        );
                    }
                }
                (muted, Icon::new(CatalogIcon::User))
            }
            chat::Role::Assistant | chat::Role::System => {
                (cx.theme().foreground, Icon::new(CatalogIcon::Bot))
            }
            chat::Role::Tool => return None,
        };
        Some(
            h_flex()
                .size_5()
                .flex_shrink_0()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(cx.theme().muted)
                .text_color(ink)
                .child(glyph.xsmall())
                .into_any_element(),
        )
    }

    /// The pictures and files waiting to be sent, each with a way to look at
    /// it and a way to drop it.
    fn render_attachments(&self, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .id("assistant-attachments")
            .flex_wrap()
            .gap_2()
            .children(
                self.attachments
                    .iter()
                    .enumerate()
                    .map(|(index, attachment)| self.render_attachment(index, attachment, cx)),
            )
            .into_any_element()
    }

    /// One waiting attachment. Its id is the slot, not the attachment's own
    /// id, so the strip reads the same way in a test and on screen.
    fn render_attachment(
        &self,
        index: usize,
        attachment: &Attachment,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let preview_id = attachment.id.clone();
        let remove_id = attachment.id.clone();
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let body: AnyElement = match attachment.image_source() {
            Some(source) => img(source)
                .w(px(56.))
                .h(px(56.))
                .rounded_md()
                .object_fit(ObjectFit::Cover)
                .into_any_element(),
            None => h_flex()
                .h(px(56.))
                .max_w(px(200.))
                .px_2()
                .gap_1()
                .items_center()
                .child(Icon::new(CatalogIcon::FileText).small().text_color(muted))
                .child(
                    v_flex()
                        .gap_1()
                        .child(div().text_xs().truncate().child(attachment.name.clone()))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(attachment::size_label(attachment.size())),
                        ),
                )
                .into_any_element(),
        };
        div()
            .id(("assistant-attachment", index))
            .test_support()
            .relative()
            .rounded_md()
            .border_1()
            .border_color(border)
            .overflow_hidden()
            .cursor_pointer()
            .child(body)
            .child(
                h_flex()
                    .id(("assistant-attachment-remove", index))
                    .test_support()
                    .absolute()
                    .top(px(1.))
                    .right(px(1.))
                    .w(px(18.))
                    .h(px(18.))
                    .rounded_full()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(border)
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(Icon::new(CatalogIcon::X).xsmall())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.remove_attachment(&remove_id, cx);
                        cx.stop_propagation();
                    })),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(attachment) = this
                    .attachments
                    .iter()
                    .find(|attachment| attachment.id == preview_id)
                    .cloned()
                {
                    this.preview_attachment(attachment, window, cx);
                }
            }))
            .into_any_element()
    }

    /// What a sent message carried, shown as the same thumbnails the
    /// composer uses, minus the remove button.
    fn render_sent_attachments(
        &self,
        index: usize,
        message: &ChatMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .id(("assistant-sent-attachments", index))
            .flex_wrap()
            .gap_2()
            .children(
                message
                    .attachments
                    .iter()
                    .enumerate()
                    .map(|(position, attachment)| {
                        let attachment = attachment.clone();
                        let size = attachment::size_label(attachment.size());
                        let muted = cx.theme().muted_foreground;
                        let content: AnyElement = match attachment.image_source() {
                            Some(source) => img(source)
                                .w(px(56.))
                                .h(px(56.))
                                .rounded_md()
                                .object_fit(ObjectFit::Cover)
                                .into_any_element(),
                            None => h_flex()
                                .h(px(56.))
                                .max_w(px(200.))
                                .px_2()
                                .gap_1()
                                .items_center()
                                .child(Icon::new(CatalogIcon::FileText).small().text_color(muted))
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_xs()
                                                .truncate()
                                                .child(attachment.name.clone()),
                                        )
                                        .child(div().text_xs().text_color(muted).child(size)),
                                )
                                .into_any_element(),
                        };
                        div()
                            .id(("assistant-sent-attachment", index * 100 + position))
                            .rounded_md()
                            .border_1()
                            .border_color(cx.theme().border)
                            .overflow_hidden()
                            .cursor_pointer()
                            .child(content)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.preview_attachment(attachment.clone(), window, cx);
                            }))
                            .into_any_element()
                    }),
            )
            .into_any_element()
    }

    fn render_composer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        let streaming = self.stream.is_some();
        let injecting = !self.injects.is_empty();
        let busy = self.busy();
        let empty = self.input.read(cx).value().trim().is_empty();
        let (used, window) = self.context_usage(cx);
        let percent = (used * 100).checked_div(window).unwrap_or(0).min(100);
        let can_compact = compact::can_plan(&self.messages);
        let tone = if percent >= 95 {
            cx.theme().danger
        } else {
            cx.theme().warning
        };
        // The card's width was measured last frame; until then everything
        // shows. The chip goes before the readout as the row narrows, in the
        // order the card's own controls matter.
        let width = self.composer_width.get().map(f32::from);
        let show_terminal = width.is_none_or(|width| width >= COMPOSER_TERMINAL_WIDTH);
        let show_context = width.is_none_or(|width| width >= COMPOSER_CONTEXT_WIDTH);
        v_flex()
            .id("assistant-composer")
            .gap_1()
            .p_2()
            .when(self.classify.is_some(), |composer| {
                composer.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(t!("assistant.tool.classifying")),
                )
            })
            .when(self.compact.is_some(), |composer| {
                composer.child(
                    div()
                        .id("assistant-compacting")
                        .text_xs()
                        .text_color(muted)
                        .child(t!("assistant.compact.running")),
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
            // One card holds the whole composer: the text, then the
            // controls that decide who answers, what it may do, and where
            // the question goes.
            .child(
                v_flex()
                    .id("assistant-composer-card")
                    .relative()
                    .gap_1()
                    .p_2()
                    .rounded(cx.theme().radius_lg)
                    .border_1()
                    .border_color(border)
                    .bg(cx.theme().group_box)
                    .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                        this.attach_paths(paths.paths().to_vec(), cx);
                    }))
                    // How wide the card was painted decides what the bottom
                    // row keeps; a new width takes effect on the next frame.
                    .child({
                        let measured = self.composer_width.clone();
                        let panel = cx.entity_id();
                        canvas(
                            move |bounds, window, _| {
                                if measured.get() != Some(bounds.size.width) {
                                    measured.set(Some(bounds.size.width));
                                    window.on_next_frame(move |_, cx| cx.notify(panel));
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full()
                    })
                    .when(!self.attachments.is_empty(), |card| {
                        card.child(self.render_attachments(cx))
                    })
                    // Who answers sits above the text, left: on the bottom
                    // row these two would push the send button out first.
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_1()
                            .child(self.render_mode_menu(cx))
                            .child(self.render_role_menu(cx)),
                    )
                    .child(
                        Textarea::new(&self.input)
                            .h(px(72.))
                            .text_sm()
                            .appearance(false)
                            .bordered(false)
                            .aria_label(t!("assistant.input.aria")),
                    )
                    .when(percent >= 80, |card| {
                        let warning = if can_compact {
                            t!("assistant.context.warning", percent = percent)
                        } else {
                            t!("assistant.context.warning.manual", percent = percent)
                        };
                        card.child(
                            div()
                                .id("assistant-context-warning")
                                .text_xs()
                                .text_color(tone)
                                .child(warning),
                        )
                    })
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap_1()
                            .child(
                                Button::new("assistant-attach")
                                    .xsmall()
                                    .ghost()
                                    .icon(CatalogIcon::Paperclip)
                                    .tooltip(t!("assistant.attachment.add"))
                                    .disabled(streaming || busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.choose_files(window, cx)
                                    })),
                            )
                            .child(self.render_model_menu(streaming || busy, cx))
                            .when(show_terminal, |row| row.child(self.render_terminal_chip()))
                            .when(show_context, |row| {
                                row.child(
                                    div()
                                        .id("assistant-context")
                                        .text_xs()
                                        .text_color(if percent >= 80 { tone } else { muted })
                                        .child(t!(
                                            "assistant.context.usage",
                                            percent = percent,
                                            used = compact_tokens(used),
                                            limit = compact_tokens(window)
                                        )),
                                )
                            })
                            .child(div().flex_1())
                            .child(if streaming {
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
                                    .disabled(empty && self.attachments.is_empty() || busy)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.send(window, cx)),
                                    )
                            }),
                    ),
            )
    }

    /// The authorization level in force, with both levels in its menu.
    /// Turning full access on still goes through the confirmation card that
    /// waits out its hold.
    fn render_mode_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let full = self.level == AuthorizationLevel::FullAccess;
        let button = Button::new("assistant-mode")
            .xsmall()
            .ghost()
            .icon(if full {
                CatalogIcon::CircleAlert
            } else {
                CatalogIcon::Lock
            })
            .label(if full {
                t!("assistant.full_access.button")
            } else {
                t!("assistant.mode.restricted")
            })
            .tooltip(t!("assistant.mode.tooltip"))
            .dropdown_caret(true);
        let button = if full { button.danger() } else { button };
        let panel = cx.entity();
        button
            .dropdown_menu(move |menu, _, _| {
                let restricted = panel.clone();
                let whole = panel.clone();
                menu.item(
                    PopupMenuItem::new(t!("assistant.mode.restricted"))
                        .checked(!full)
                        .on_click(move |_, _, cx| {
                            restricted.update(cx, |panel, cx| panel.close_full_access(cx));
                        }),
                )
                .item(
                    PopupMenuItem::new(t!("assistant.full_access.button"))
                        .checked(full)
                        .on_click(move |_, _, cx| {
                            whole.update(cx, |panel, cx| {
                                if panel.level != AuthorizationLevel::FullAccess {
                                    panel.toggle_full_access(cx);
                                }
                            });
                        }),
                )
            })
            .into_any_element()
    }

    /// The roles this conversation answers as: the default assistant, or the
    /// roles loaded from the menu. A role can also be mentioned with `@name`
    /// in the message, which applies to that message only.
    fn render_role_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let library = super::roles::library(&assistant);
        let loaded = self.roles.clone();
        let names: Vec<&str> = loaded
            .iter()
            .filter_map(|id| super::roles::role(&library, id))
            .map(|role| role.name.as_str())
            .collect();
        let label: SharedString = match names.first() {
            None => t!("assistant.role.default"),
            Some(first) if names.len() == 1 => (*first).to_string().into(),
            Some(first) => format!("{first} +{}", names.len() - 1).into(),
        };
        let tooltip: SharedString = if names.is_empty() {
            t!("assistant.role.tooltip")
        } else {
            t!("assistant.role.loaded_tip", names = join_list(&names))
        };
        let panel = cx.entity();
        let panel_for_default = panel.clone();
        let panel_for_configure = panel.clone();
        Button::new("assistant-role")
            .xsmall()
            .ghost()
            .icon(CatalogIcon::User)
            .label(label)
            .tooltip(tooltip)
            .dropdown_caret(true)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                let empty = loaded.is_empty();
                let clear = panel_for_default.clone();
                menu = menu.item(
                    PopupMenuItem::new(t!("assistant.role.default"))
                        .checked(empty)
                        .on_click(move |_, _, cx| {
                            clear.update(cx, |panel, cx| panel.clear_roles(cx));
                        }),
                );
                for role in &library.roles {
                    let id = role.id.clone();
                    let checked = loaded.contains(&id);
                    let label = format!("{} {}", role.initial(), role.name);
                    let panel = panel.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            let id = id.clone();
                            panel.update(cx, |panel, cx| panel.toggle_role(id, cx));
                        },
                    ));
                }
                let configure = panel_for_configure.clone();
                menu.separator()
                    .item(PopupMenuItem::new(t!("assistant.role.manage")).on_click(
                        move |_, window, cx| {
                            configure.update(cx, |panel, cx| panel.open_config(window, cx));
                        },
                    ))
            })
            .into_any_element()
    }

    /// The configured models, with the current one checked. Switching takes
    /// effect on the next turn; a running answer or a waiting card holds it
    /// back. The built-in trial heads the list while the service offers it.
    fn render_model_menu(&self, locked: bool, cx: &mut Context<Self>) -> AnyElement {
        let assistant = self.settings.read(cx).settings().assistant.clone();
        let active = assistant.active_profile();
        let provider_model = assistant.active().model.trim();
        let offer = self.trial_offer(cx);
        let trial_selected = self.trial_selected(cx) && offer != trial::Offer::Hidden;
        let label: SharedString = if trial_selected {
            t!("assistant.trial.name")
        } else {
            match (active, provider_model) {
                (Some(profile), _) => profile.display_name().to_string().into(),
                (None, "") => t!("assistant.model.none"),
                (None, model) => model.to_string().into(),
            }
        };
        let tooltip: SharedString = if locked {
            t!("assistant.model.locked")
        } else if trial_selected {
            match &self.trial {
                Some(status) if status.available => t!(
                    "assistant.trial.quota",
                    requests = status.requests_remaining_today,
                    tokens = compact_tokens(status.tokens_used_today as usize)
                ),
                _ => t!("assistant.trial.menu.exhausted"),
            }
        } else {
            t!("assistant.model.switch")
        };
        // Hidden means the entry is not there at all; Exhausted keeps it so
        // the user can see why it stopped.
        let trial_item: Option<(SharedString, bool, bool)> = match (&self.trial, offer) {
            (Some(status), trial::Offer::Ready) => Some((
                t!(
                    "assistant.trial.menu",
                    remaining = status.requests_remaining_today
                ),
                true,
                trial_selected,
            )),
            (Some(_), trial::Offer::Exhausted) => {
                Some((t!("assistant.trial.menu.exhausted"), false, trial_selected))
            }
            _ => None,
        };
        let profiles = assistant.models.clone();
        let configured = !profiles.is_empty();
        let panel = cx.entity();
        let panel_for_default = panel.clone();
        let panel_for_configure = panel.clone();
        let default_label: SharedString = if provider_model.is_empty() {
            t!("assistant.model.default_none")
        } else {
            t!("assistant.model.default", model = provider_model)
        };
        let default_checked = assistant.active_model.is_empty() && !trial_selected;
        Button::new("assistant-model")
            .xsmall()
            .ghost()
            .icon(CatalogIcon::Cpu)
            .label(label)
            .tooltip(tooltip)
            .dropdown_caret(true)
            .disabled(locked)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                if let Some((label, selectable, checked)) = trial_item.clone() {
                    let panel = panel.clone();
                    let mut item = PopupMenuItem::new(label).checked(checked);
                    if selectable {
                        item = item.on_click(move |_, _, cx| {
                            panel.update(cx, |panel, cx| panel.select_trial(cx));
                        });
                    } else {
                        item = item.disabled(true);
                    }
                    menu = menu.item(item).separator();
                }
                if !configured {
                    menu = menu.item(PopupMenuItem::new(t!("assistant.model.none")).disabled(true));
                }
                for profile in &profiles {
                    let id = profile.id.clone();
                    let label =
                        format!("{} · {}", profile.display_name(), profile.protocol.label());
                    let checked = assistant.active_model == id && !trial_selected;
                    let panel = panel.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            let id = id.clone();
                            panel.update(cx, |panel, cx| panel.set_model(id, cx));
                        },
                    ));
                }
                let panel = panel_for_default.clone();
                menu = menu.item(
                    PopupMenuItem::new(default_label.clone())
                        .checked(default_checked)
                        .on_click(move |_, _, cx| {
                            panel.update(cx, |panel, cx| panel.set_model(String::new(), cx));
                        }),
                );
                let panel = panel_for_configure.clone();
                menu.separator().item(
                    PopupMenuItem::new(t!("assistant.model.configure")).on_click(
                        move |_, window, cx| {
                            panel.update(cx, |panel, cx| panel.open_config(window, cx));
                        },
                    ),
                )
            })
            .into_any_element()
    }

    /// The terminals this conversation is bound to, in one chip: the first
    /// alias and how many others there are, all of them in the tooltip.
    fn render_terminal_chip(&self) -> AnyElement {
        let names: Vec<&str> = self
            .bindings
            .entries()
            .iter()
            .map(|entry| entry.alias.as_str())
            .collect();
        let label: SharedString = match names.first() {
            None => t!("assistant.composer.terminal.none"),
            Some(first) if names.len() == 1 => (*first).to_string().into(),
            Some(first) => format!("{first} +{}", names.len() - 1).into(),
        };
        let tooltip: SharedString = if names.is_empty() {
            t!("assistant.composer.terminal.none")
        } else {
            t!("assistant.composer.terminal.tip", names = join_list(&names))
        };
        Button::new("assistant-composer-terminal")
            .xsmall()
            .ghost()
            .icon(CatalogIcon::Terminal)
            .label(label)
            .tooltip(tooltip)
            .into_any_element()
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
        let open = self.open_terminals();
        v_flex()
            .id("assistant-panel")
            .size_full()
            .min_h_0()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _window, cx| {
                this.attach_paths(paths.paths().to_vec(), cx);
            }))
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(border)
                    // The terminals it may use share the line with the
                    // actions: a row of their own cost the conversation a
                    // strip for chips that are usually absent.
                    .child(self.render_bindings(cx))
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_1()
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
                                    .icon(CatalogIcon::Settings)
                                    .tooltip(t!("assistant.action.settings"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_config(window, cx)
                                    })),
                            ),
                    ),
            )
            .when_some(self.notice.clone(), |panel, notice| {
                panel.child(
                    div()
                        .id("assistant-notice")
                        .test_support()
                        .px_2()
                        .py_1()
                        .truncate()
                        .text_xs()
                        .text_color(muted)
                        .child(notice),
                )
            })
            .when(!self.bindings.pending().is_empty(), |panel| {
                panel.child(self.render_pending_list(&open, cx))
            })
            .child(self.render_sessions(cx))
            .when_some(self.full_access_card, |panel, since| {
                panel.child(self.render_full_access_dialog(since, cx))
            })
            .when(self.level == AuthorizationLevel::FullAccess, |panel| {
                panel.child(self.render_full_access_bar(cx))
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
            .child(self.render_conversation(cx))
            .when_some(self.ask.as_ref(), |panel, ask| {
                panel.child(self.render_ask(ask, cx))
            })
            .when(!self.pending.is_empty(), |panel| {
                panel.child(self.render_pending(cx))
            })
            .child(self.render_composer(cx))
    }
}

/// When a message was sent, in the reader's own clock: the hour and minute
/// for today, the date too for an older turn. Nothing for a message whose
/// time was never kept, as in journals written before times were recorded.
fn message_time(at: i64) -> Option<SharedString> {
    use chrono::Datelike as _;
    if at <= 0 {
        return None;
    }
    let time = chrono::DateTime::from_timestamp_millis(at)?.with_timezone(&chrono::Local);
    let today = chrono::Local::now().date_naive();
    let date = time.date_naive();
    let text = if date == today {
        time.format("%H:%M").to_string()
    } else if date.year() == today.year() {
        time.format("%m-%d %H:%M").to_string()
    } else {
        time.format("%Y-%m-%d %H:%M").to_string()
    };
    Some(text.into())
}

/// Text on one line, cut to a readable length for a folded card.
fn one_line(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= limit {
        return flat;
    }
    let mut cut: String = flat.chars().take(limit).collect();
    cut.push('…');
    cut
}

/// The user's words as HTML for a selectable text view: the tags are
/// escaped, and each newline becomes a break so the paragraphs survive.
fn escape_html_text(text: &str) -> String {
    let mut html = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => html.push_str("&amp;"),
            '<' => html.push_str("&lt;"),
            '>' => html.push_str("&gt;"),
            '\n' => html.push_str("<br>"),
            _ => html.push(character),
        }
    }
    html
}

/// A message's words in a text view the user can select and copy. The
/// assistant's answer keeps its Markdown; everything else is escaped so it
/// stays text.
fn message_body(index: usize, text: &str, markdown: bool) -> AnyElement {
    if markdown {
        TextView::markdown(("assistant-markdown", index), text.to_string())
            .selectable(true)
            .text_sm()
            .into_any_element()
    } else {
        TextView::html(("assistant-user-text", index), escape_html_text(text))
            .selectable(true)
            .text_sm()
            .into_any_element()
    }
}

/// A duration for the opened card.
fn duration_text(milliseconds: u64) -> String {
    if milliseconds < 1000 {
        t!("assistant.tool.elapsed_ms", ms = milliseconds).to_string()
    } else {
        t!(
            "assistant.tool.elapsed_s",
            seconds = format!("{:.1}", milliseconds as f64 / 1000.0)
        )
        .to_string()
    }
}

/// The policy's verdict on a call: its risk, why, and the rule that matched.
fn verdict_text(facts: &ResultFacts) -> Option<String> {
    let risk = facts.risk.as_deref()?;
    Some(match (facts.reason.as_deref(), facts.rule.as_deref()) {
        (Some(reason), Some(rule)) => format!("{risk} · {reason} · {rule}"),
        (Some(reason), None) => format!("{risk} · {reason}"),
        (None, Some(rule)) => format!("{risk} · {rule}"),
        (None, None) => risk.to_string(),
    })
}

/// Thousands as the compact form a usage display uses.
fn compact_tokens(value: usize) -> String {
    if value >= 1000 {
        format!("{:.1}k", value as f64 / 1000.0)
    } else {
        value.to_string()
    }
}

/// A file-name-safe piece of a task's name. Names that have no ASCII word
/// fall back to `report`; the timestamp keeps the file unique anyway.
fn report_slug(name: &str) -> String {
    let mut slug = String::new();
    for character in name.chars() {
        if slug.len() >= 24 {
            break;
        }
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "report".to_string()
    } else {
        slug
    }
}
