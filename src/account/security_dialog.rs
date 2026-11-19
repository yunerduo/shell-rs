//! 修改密码 and 修改邮箱, opened from 设置 › 账号 while signed in.
//!
//! Both run their request from the form, like the login dialog: nothing in
//! the store changes until the server accepts it. 改密码 offers the current
//! password or a code mailed to the account; either way the server ends
//! every session, so the store goes back to signed out afterwards. 改邮箱
//! needs a code sent to the new address and leaves the session alone.

use std::time::{Duration, Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogClose, DialogFooter},
    form::{Field, Form},
    h_flex,
    input::{Input, InputContentType, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeroize::Zeroizing;

use super::describe::{
    account_error, change_email_error, change_password_error, code_error, code_problem,
    email_problem, new_password_problem, reset_error,
};
use super::model::{check_code, check_new_password, check_required_email};
use super::service::{AccountError, CodeSent};
use super::store::AccountStore;
use crate::i18n::t;
use crate::shared::{Segment, SegmentedControl};

/// How 改密码 proves the account: the current password, or a code mailed to
/// the account's own address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasswordMode {
    Current,
    Email,
}

/// The server's rule beyond length: at least two of lowercase, uppercase,
/// digits and other characters. Checked here so the form can say it before
/// the request instead of repeating the server's flat refusal.
fn has_two_classes(password: &str) -> bool {
    let (mut lower, mut upper, mut digit, mut other) = (false, false, false, false);
    for ch in password.chars() {
        if ch.is_ascii_lowercase() {
            lower = true;
        } else if ch.is_ascii_uppercase() {
            upper = true;
        } else if ch.is_ascii_digit() {
            digit = true;
        } else {
            other = true;
        }
    }
    lower as usize + upper as usize + digit as usize + other as usize >= 2
}

struct ChangePasswordForm {
    account: Entity<AccountStore>,
    mode: PasswordMode,
    /// The account's address, the only one the reset code may go to.
    email: String,
    current: Entity<InputState>,
    new: Entity<InputState>,
    repeat: Entity<InputState>,
    code: Entity<InputState>,
    code_sent: bool,
    sending: bool,
    working: bool,
    resend_at: Option<Instant>,
    notice: Option<SharedString>,
    error: Option<SharedString>,
}

impl ChangePasswordForm {
    fn new(
        account: Entity<AccountStore>,
        email: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let current = cx.new(|cx| InputState::new(window, cx).masked(true));
        let new = cx.new(|cx| InputState::new(window, cx).masked(true));
        let repeat = cx.new(|cx| InputState::new(window, cx).masked(true));
        let code = cx.new(|cx| InputState::new(window, cx));
        Self {
            account,
            mode: PasswordMode::Current,
            email,
            current,
            new,
            repeat,
            code,
            code_sent: false,
            sending: false,
            working: false,
            resend_at: None,
            notice: None,
            error: None,
        }
    }

    fn working(&self) -> bool {
        self.sending || self.working
    }

    fn resend_remaining(&self) -> u64 {
        self.resend_at
            .map(|at| {
                at.saturating_duration_since(Instant::now())
                    .as_millis()
                    .div_ceil(1000) as u64
            })
            .unwrap_or(0)
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        let input = match self.mode {
            PasswordMode::Current => &self.current,
            PasswordMode::Email => &self.code,
        };
        input.update(cx, |state, cx| state.focus(window, cx));
    }

    fn switch(&mut self, mode: PasswordMode, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.notice = None;
        self.error = None;
        self.focus(window, cx);
        cx.notify();
    }

    fn send_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.working() || self.resend_remaining() > 0 {
            return;
        }
        if self.email.trim().is_empty() {
            self.error = Some(t!("account.change_password.no_email"));
            cx.notify();
            return;
        }
        let Some(service) = self.account.read(cx).service() else {
            self.error = Some(account_error(&AccountError::Network(String::new())));
            cx.notify();
            return;
        };
        self.error = None;
        self.notice = None;
        self.sending = true;
        cx.notify();
        let email = self.email.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { service.request_password_code(&email) })
                .await;
            this.update_in(cx, |form, window, cx| form.apply_code(result, window, cx))
                .ok();
        })
        .detach();
    }

    fn apply_code(
        &mut self,
        result: Result<CodeSent, AccountError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sending = false;
        match result {
            Ok(sent) => {
                self.error = None;
                self.code_sent = true;
                self.notice = Some(t!("account.code.sent"));
                self.start_resend(sent.resend_after, cx);
                self.code.update(cx, |state, cx| state.focus(window, cx));
            }
            Err(error) => {
                self.notice = None;
                if matches!(error, AccountError::TooMany(_)) {
                    self.start_resend(60, cx);
                }
                self.error = Some(code_error(&error));
            }
        }
        cx.notify();
    }

    fn start_resend(&mut self, seconds: u64, cx: &mut Context<Self>) {
        self.resend_at = Some(Instant::now() + Duration::from_secs(seconds.max(1)));
        cx.notify();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let waiting = this.update(cx, |form, cx| {
                    let waiting = form.resend_remaining() > 0;
                    if !waiting {
                        form.resend_at = None;
                    }
                    cx.notify();
                    waiting
                });
                if !waiting.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.working() {
            return false;
        }
        let new = Zeroizing::new(self.new.read(cx).value().to_string());
        if let Err(problem) = check_new_password(&new) {
            self.error = Some(new_password_problem(problem));
            cx.notify();
            return false;
        }
        if !has_two_classes(&new) {
            self.error = Some(t!("account.form.password.classes"));
            cx.notify();
            return false;
        }
        if new.as_str() != self.repeat.read(cx).value().as_str() {
            self.error = Some(t!("account.form.password.mismatch"));
            cx.notify();
            return false;
        }
        let Some(service) = self.account.read(cx).service() else {
            self.error = Some(account_error(&AccountError::Network(String::new())));
            cx.notify();
            return false;
        };
        let Some(token) = self.account.read(cx).token() else {
            self.error = Some(account_error(&AccountError::Unauthorized));
            cx.notify();
            return false;
        };
        self.error = None;
        self.notice = None;
        self.working = true;
        cx.notify();
        match self.mode {
            PasswordMode::Current => {
                let current = Zeroizing::new(self.current.read(cx).value().to_string());
                if current.is_empty() {
                    self.working = false;
                    self.error = Some(t!("account.form.password.empty"));
                    cx.notify();
                    return false;
                }
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            service.change_password(token.as_str(), current.as_str(), new.as_str())
                        })
                        .await;
                    this.update_in(cx, |form, window, cx| form.apply_change(result, window, cx))
                        .ok();
                })
                .detach();
            }
            PasswordMode::Email => {
                if self.email.trim().is_empty() {
                    self.working = false;
                    self.error = Some(t!("account.change_password.no_email"));
                    cx.notify();
                    return false;
                }
                let code = self.code.read(cx).value().to_string();
                if let Err(problem) = check_code(&code) {
                    self.working = false;
                    self.error = Some(code_problem(problem));
                    cx.notify();
                    return false;
                }
                let email = self.email.clone();
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_spawn(async move {
                            service.reset_password(&email, code.trim(), new.as_str())
                        })
                        .await;
                    this.update_in(cx, |form, window, cx| form.apply_change(result, window, cx))
                        .ok();
                })
                .detach();
            }
        }
        false
    }

    fn apply_change(
        &mut self,
        result: Result<(), AccountError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.working = false;
        match result {
            Ok(()) => {
                // Every session is gone, this one included.
                self.account
                    .update(cx, |account, cx| account.finish_password_change(cx));
                window.close_dialog(cx);
            }
            Err(error) => {
                self.error = Some(match self.mode {
                    PasswordMode::Current => change_password_error(&error),
                    PasswordMode::Email => reset_error(&error),
                });
                if matches!(error, AccountError::EmailCodeInvalid) {
                    self.code.update(cx, |state, cx| state.focus(window, cx));
                }
            }
        }
        cx.notify();
    }
}

impl Render for ChangePasswordForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let working = self.working();
        let current_mode = self.mode == PasswordMode::Current;
        let remaining = self.resend_remaining();
        let form = cx.entity();
        let description = if current_mode {
            t!("account.change_password.description_current")
        } else {
            t!("account.change_password.description_email")
        };
        let mut fields = Form::new();
        if current_mode {
            fields = fields.child(
                Field::new()
                    .label(t!("account.change_password.current"))
                    .required(true)
                    .child(
                        Input::new(&self.current)
                            .id("account-password-current")
                            .mask_toggle()
                            .content_type(InputContentType::Password)
                            .disabled(working),
                    ),
            );
        } else {
            fields = fields.child(
                Field::new()
                    .label(t!("account.field.verification_code"))
                    .required(true)
                    .description(t!(
                        "account.change_password.email_hint",
                        email = self.email.clone()
                    ))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.code)
                                        .id("account-password-code")
                                        .disabled(working),
                                ),
                            )
                            .child(code_button(
                                "account-password-send-code",
                                working,
                                remaining,
                                self.code_sent,
                                {
                                    let form = form.clone();
                                    move |window, cx| {
                                        form.update(cx, |form, cx| form.send_code(window, cx))
                                    }
                                },
                            )),
                    ),
            );
        }
        fields = fields
            .child(
                Field::new()
                    .label(t!("account.change_password.new"))
                    .required(true)
                    .child(
                        Input::new(&self.new)
                            .id("account-password-new")
                            .mask_toggle()
                            .content_type(InputContentType::Password)
                            .disabled(working),
                    ),
            )
            .child(
                Field::new()
                    .label(t!("account.change_password.repeat"))
                    .required(true)
                    .child(
                        Input::new(&self.repeat)
                            .id("account-password-repeat")
                            .mask_toggle()
                            .content_type(InputContentType::Password)
                            .disabled(working),
                    ),
            );

        v_flex()
            .w_full()
            .gap_4()
            .child(
                SegmentedControl::new("account-password-mode")
                    .selected_index(Some(if current_mode { 0 } else { 1 }))
                    .on_change({
                        let form = form.clone();
                        move |index, window, cx| {
                            let mode = if *index == 1 {
                                PasswordMode::Email
                            } else {
                                PasswordMode::Current
                            };
                            form.update(cx, |form, cx| form.switch(mode, window, cx));
                        }
                    })
                    .segments([
                        Segment::new(t!("account.change_password.mode_current")).disabled(working),
                        Segment::new(t!("account.change_password.mode_email")).disabled(working),
                    ]),
            )
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(description),
            )
            .child(fields)
            .when_some(self.notice.clone(), |view, notice| {
                view.child(message_block(
                    "account-password-notice",
                    notice,
                    theme.success,
                    theme.radius,
                ))
            })
            .when_some(self.error.clone(), |view, error| {
                view.child(message_block(
                    "account-password-error",
                    error,
                    theme.danger,
                    theme.radius,
                ))
            })
    }
}

struct ChangeEmailForm {
    account: Entity<AccountStore>,
    /// The address in use now, to refuse a no-op change before the request.
    current: String,
    email: Entity<InputState>,
    code: Entity<InputState>,
    /// The address the last code went to; a later edit makes that code stale.
    sent_to: Option<String>,
    code_sent: bool,
    sending: bool,
    working: bool,
    resend_at: Option<Instant>,
    notice: Option<SharedString>,
    error: Option<SharedString>,
}

impl ChangeEmailForm {
    fn new(
        account: Entity<AccountStore>,
        current: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let email = cx.new(|cx| InputState::new(window, cx));
        let code = cx.new(|cx| InputState::new(window, cx));
        Self {
            account,
            current,
            email,
            code,
            sent_to: None,
            code_sent: false,
            sending: false,
            working: false,
            resend_at: None,
            notice: None,
            error: None,
        }
    }

    fn working(&self) -> bool {
        self.sending || self.working
    }

    fn resend_remaining(&self) -> u64 {
        self.resend_at
            .map(|at| {
                at.saturating_duration_since(Instant::now())
                    .as_millis()
                    .div_ceil(1000) as u64
            })
            .unwrap_or(0)
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.email.update(cx, |state, cx| state.focus(window, cx));
    }

    /// Check the typed address the same way both buttons need it.
    fn checked_email(&self, cx: &App) -> Result<String, SharedString> {
        let email = self.email.read(cx).value().trim().to_string();
        check_required_email(&email).map_err(email_problem)?;
        if email.eq_ignore_ascii_case(self.current.trim()) {
            return Err(t!("account.change_email.same"));
        }
        Ok(email)
    }

    fn send_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.working() || self.resend_remaining() > 0 {
            return;
        }
        let email = match self.checked_email(cx) {
            Ok(email) => email,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return;
            }
        };
        let Some(service) = self.account.read(cx).service() else {
            self.error = Some(account_error(&AccountError::Network(String::new())));
            cx.notify();
            return;
        };
        let Some(token) = self.account.read(cx).token() else {
            self.error = Some(account_error(&AccountError::Unauthorized));
            cx.notify();
            return;
        };
        self.error = None;
        self.notice = None;
        self.sending = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let request_email = email.clone();
            let result = cx
                .background_spawn(async move {
                    service.request_email_change_code(token.as_str(), &request_email)
                })
                .await;
            this.update_in(cx, |form, window, cx| {
                form.apply_code(email, result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn apply_code(
        &mut self,
        email: String,
        result: Result<CodeSent, AccountError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sending = false;
        match result {
            Ok(sent) => {
                self.error = None;
                self.code_sent = true;
                self.sent_to = Some(email);
                self.notice = Some(t!("account.change_email.code_sent"));
                self.start_resend(sent.resend_after, cx);
                self.code.update(cx, |state, cx| state.focus(window, cx));
            }
            Err(error) => {
                self.notice = None;
                if matches!(error, AccountError::TooMany(_)) {
                    self.start_resend(60, cx);
                }
                self.error = Some(change_email_error(&error));
            }
        }
        cx.notify();
    }

    fn start_resend(&mut self, seconds: u64, cx: &mut Context<Self>) {
        self.resend_at = Some(Instant::now() + Duration::from_secs(seconds.max(1)));
        cx.notify();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let waiting = this.update(cx, |form, cx| {
                    let waiting = form.resend_remaining() > 0;
                    if !waiting {
                        form.resend_at = None;
                    }
                    cx.notify();
                    waiting
                });
                if !waiting.unwrap_or(false) {
                    break;
                }
            }
        })
        .detach();
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.working() {
            return false;
        }
        let email = match self.checked_email(cx) {
            Ok(email) => email,
            Err(error) => {
                self.error = Some(error);
                cx.notify();
                return false;
            }
        };
        let code = self.code.read(cx).value().to_string();
        if let Err(problem) = check_code(&code) {
            self.error = Some(code_problem(problem));
            cx.notify();
            return false;
        }
        if self.sent_to.as_deref() != Some(email.as_str()) {
            self.error = Some(t!("account.change_email.code_stale"));
            cx.notify();
            return false;
        }
        let Some(service) = self.account.read(cx).service() else {
            self.error = Some(account_error(&AccountError::Network(String::new())));
            cx.notify();
            return false;
        };
        let Some(token) = self.account.read(cx).token() else {
            self.error = Some(account_error(&AccountError::Unauthorized));
            cx.notify();
            return false;
        };
        self.error = None;
        self.notice = None;
        self.working = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let changed = email.clone();
            let result = cx
                .background_spawn(async move {
                    service.change_email(token.as_str(), &email, code.trim())
                })
                .await;
            this.update_in(cx, |form, window, cx| {
                form.apply_change(changed, result, window, cx)
            })
            .ok();
        })
        .detach();
        false
    }

    fn apply_change(
        &mut self,
        email: String,
        result: Result<(), AccountError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.working = false;
        match result {
            Ok(()) => {
                // The session stays: the mailbox is not the token.
                self.account
                    .update(cx, |account, cx| account.apply_email(email, cx));
                window.close_dialog(cx);
            }
            Err(error) => {
                self.error = Some(change_email_error(&error));
                if matches!(error, AccountError::EmailCodeInvalid) {
                    self.code.update(cx, |state, cx| state.focus(window, cx));
                }
            }
        }
        cx.notify();
    }
}

impl Render for ChangeEmailForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let working = self.working();
        let remaining = self.resend_remaining();
        let form = cx.entity();
        v_flex()
            .w_full()
            .gap_4()
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(t!("account.change_email.description")),
            )
            .child(
                Form::new()
                    .child(
                        Field::new()
                            .label(t!("account.change_email.field"))
                            .required(true)
                            .child(
                                Input::new(&self.email)
                                    .id("account-email-new")
                                    .content_type(InputContentType::EmailAddress)
                                    .disabled(working),
                            ),
                    )
                    .child(
                        Field::new()
                            .label(t!("account.field.verification_code"))
                            .required(true)
                            .description(t!("account.change_email.hint"))
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .child(
                                        div().flex_1().min_w_0().child(
                                            Input::new(&self.code)
                                                .id("account-email-code")
                                                .disabled(working),
                                        ),
                                    )
                                    .child(code_button(
                                        "account-email-send-code",
                                        working,
                                        remaining,
                                        self.code_sent,
                                        {
                                            let form = form.clone();
                                            move |window, cx| {
                                                form.update(cx, |form, cx| {
                                                    form.send_code(window, cx)
                                                })
                                            }
                                        },
                                    )),
                            ),
                    ),
            )
            .when_some(self.notice.clone(), |view, notice| {
                view.child(message_block(
                    "account-email-notice",
                    notice,
                    theme.success,
                    theme.radius,
                ))
            })
            .when_some(self.error.clone(), |view, error| {
                view.child(message_block(
                    "account-email-error",
                    error,
                    theme.danger,
                    theme.radius,
                ))
            })
    }
}

/// 发送验证码 or its countdown, shared by both forms.
fn code_button(
    id: &'static str,
    working: bool,
    remaining: u64,
    code_sent: bool,
    on_click: impl Fn(&mut Window, &mut App) + 'static,
) -> Button {
    let label = if remaining > 0 {
        t!("account.button.resend_code_wait", seconds = remaining)
    } else if code_sent {
        t!("account.button.resend_code")
    } else {
        t!("account.button.send_code")
    };
    let button = Button::new(id)
        .label(label)
        .disabled(working || remaining > 0);
    if working || remaining > 0 {
        return button;
    }
    button.on_click(move |_, window, cx| on_click(window, cx))
}

/// A wrapped, full-width line for good news or bad.
fn message_block(
    id: &'static str,
    text: SharedString,
    tone: Hsla,
    radius: Pixels,
) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .aria_label(text.clone())
        .w_full()
        .min_w_0()
        .rounded(radius)
        .border_1()
        .border_color(tone.opacity(0.35))
        .bg(tone.opacity(0.08))
        .px_3()
        .py_2()
        .text_sm()
        .text_color(tone)
        .whitespace_normal()
        .child(text)
}

/// Open 修改密码.
pub fn open_change_password_dialog(
    account: Entity<AccountStore>,
    window: &mut Window,
    cx: &mut App,
) {
    if account.read(cx).token().is_none() {
        return;
    }
    let email = account.read(cx).snapshot().email;
    let form = cx.new(|cx| ChangePasswordForm::new(account, email, window, cx));
    window.open_dialog(cx, {
        let form = form.clone();
        move |dialog, _, cx| {
            let working = form.read(cx).working();
            let commit = form.clone();
            let cancel = form.clone();
            dialog
                .title(t!("account.change_password.title"))
                .overlay_closable(true)
                .keyboard(!working)
                .close_button(!working)
                .child(form.clone())
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(move |button| {
                            button.label(t!("common.cancel")).disabled(working)
                        }))
                        .child(
                            DialogAction::new().child(
                                Button::new("account-password-commit")
                                    .primary()
                                    .label(t!("account.change_password.commit"))
                                    .disabled(working)
                                    .loading(working),
                            ),
                        ),
                )
                .on_ok(move |_, window, cx| commit.update(cx, |form, cx| form.commit(window, cx)))
                .on_cancel(move |_, _, cx| !cancel.read(cx).working())
        }
    });
    form.update(cx, |form, cx| form.focus(window, cx));
}

/// Open 修改邮箱.
pub fn open_change_email_dialog(account: Entity<AccountStore>, window: &mut Window, cx: &mut App) {
    if account.read(cx).token().is_none() {
        return;
    }
    let current = account.read(cx).snapshot().email;
    let form = cx.new(|cx| ChangeEmailForm::new(account, current, window, cx));
    window.open_dialog(cx, {
        let form = form.clone();
        move |dialog, _, cx| {
            let working = form.read(cx).working();
            let commit = form.clone();
            let cancel = form.clone();
            dialog
                .title(t!("account.change_email.title"))
                .overlay_closable(true)
                .keyboard(!working)
                .close_button(!working)
                .child(form.clone())
                .footer(
                    DialogFooter::new()
                        .child(DialogClose::new().trigger(move |button| {
                            button.label(t!("common.cancel")).disabled(working)
                        }))
                        .child(
                            DialogAction::new().child(
                                Button::new("account-email-commit")
                                    .primary()
                                    .label(t!("account.change_email.commit"))
                                    .disabled(working)
                                    .loading(working),
                            ),
                        ),
                )
                .on_ok(move |_, window, cx| commit.update(cx, |form, cx| form.commit(window, cx)))
                .on_cancel(move |_, _, cx| !cancel.read(cx).working())
        }
    });
    form.update(cx, |form, cx| form.focus(window, cx));
}

#[cfg(test)]
mod tests {
    use super::has_two_classes;

    #[test]
    fn a_new_password_needs_two_kinds_of_character() {
        assert!(has_two_classes("password1"));
        assert!(has_two_classes("Password"));
        assert!(has_two_classes("p@ssword"));
        assert!(!has_two_classes("password"));
        assert!(!has_two_classes("12345678"));
    }
}
