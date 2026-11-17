//! The sign-in and registration dialog. One dialog, one switch between the
//! two forms; the account is optional, so this is only ever opened from a
//! button the user pressed.
//!
//! The dialog stays open while the request runs, exits when the store says
//! it is signed in, and shows the server's reason in a block above the
//! footer when it is not. 忘记密码 is the third form: an e-mail, then the
//! code from the message and a new password.

use std::time::{Duration, Instant};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
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
    account_error, code_error, code_problem, email_problem, new_password_problem, phone_problem,
    reset_error, username_problem,
};
use super::model::{
    AccountPhase, check_code, check_email, check_login_password, check_new_password, check_phone,
    check_required_email, check_username,
};
use super::service::{AccountError, CodeSent};
use super::store::AccountStore;
use crate::i18n::t;
use crate::shared::{Segment, SegmentedControl};

/// Which form the dialog opens on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMode {
    SignIn,
    Register,
    /// 忘记密码: the account's e-mail first, then a code and a new password.
    Reset,
}

/// Which half of the reset the dialog is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResetStep {
    Email,
    Code,
}

/// Which flow asked for the code, since the two answer differently: one
/// stays on its form, the other moves to the second step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CodePurpose {
    Register,
    Reset,
}

struct LoginForm {
    account: Entity<AccountStore>,
    mode: LoginMode,
    reset_step: ResetStep,
    /// Registration asked the server and got `email_verification_required`:
    /// the code field is out and the e-mail is now required.
    code_required: bool,
    /// A code has gone out at least once, so the button says 重新发送.
    code_sent: bool,
    username: Entity<InputState>,
    password: Entity<InputState>,
    email: Entity<InputState>,
    phone: Entity<InputState>,
    code: Entity<InputState>,
    /// A code request is in flight.
    code_sending: bool,
    /// A password reset is in flight.
    resetting: bool,
    /// When another code may be asked for, while the countdown runs.
    resend_at: Option<Instant>,
    /// A success line: the code went out, or the password was reset.
    notice: Option<SharedString>,
    error: Option<SharedString>,
    _subscription: Subscription,
}

impl LoginForm {
    fn new(
        account: Entity<AccountStore>,
        mode: LoginMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let username = cx.new(|cx| InputState::new(window, cx));
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let email = cx.new(|cx| InputState::new(window, cx));
        let phone = cx.new(|cx| InputState::new(window, cx));
        let code = cx.new(|cx| InputState::new(window, cx));
        let subscription = cx.observe_in(&account, window, |this, account, window, cx| {
            let (signed_in, error) = {
                let account = account.read(cx);
                (
                    account.phase() == AccountPhase::SignedIn,
                    account.login_error().cloned(),
                )
            };
            if signed_in {
                window.close_dialog(cx);
                return;
            }
            match error {
                // The server asked for a code the first time the
                // registration was sent: show the field and put the cursor
                // in it, instead of leaving the request as a dead end.
                Some(AccountError::EmailVerificationRequired) if !this.code_required => {
                    this.code_required = true;
                    this.error = None;
                    this.notice = None;
                    this.code.update(cx, |state, cx| state.focus(window, cx));
                }
                // The switch was turned off between the attempt and the
                // code request: the plain form goes through again.
                Some(AccountError::EmailVerificationDisabled) => this.code_required = false,
                // A wrong code is worth retyping, so the cursor goes back
                // to it. A stale or void one needs a new message instead.
                Some(AccountError::EmailCodeInvalid) if this.code_required => {
                    this.code.update(cx, |state, cx| state.focus(window, cx));
                }
                _ => {}
            }
            cx.notify();
        });
        Self {
            account,
            mode,
            reset_step: ResetStep::Email,
            code_required: false,
            code_sent: false,
            username,
            password,
            email,
            phone,
            code,
            code_sending: false,
            resetting: false,
            resend_at: None,
            notice: None,
            error: None,
            _subscription: subscription,
        }
    }

    fn working(&self, cx: &App) -> bool {
        self.account.read(cx).phase() == AccountPhase::SigningIn
            || self.code_sending
            || self.resetting
    }

    /// Whole seconds left on the resend countdown, rounded up; zero when the
    /// button may be pressed again.
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
        self.username
            .update(cx, |state, cx| state.focus(window, cx));
    }

    fn switch(&mut self, mode: LoginMode, cx: &mut Context<Self>) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        self.reset_step = ResetStep::Email;
        // A sign-in failure has nothing to say about a registration.
        self.error = None;
        self.notice = None;
        self.account
            .update(cx, |account, cx| account.clear_login_error(cx));
        cx.notify();
    }

    fn forgot(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switch(LoginMode::Reset, cx);
        // The sign-in password is not a new one.
        self.password
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.email.update(cx, |state, cx| state.focus(window, cx));
    }

    fn back_to_sign_in(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.switch(LoginMode::SignIn, cx);
        self.focus(window, cx);
    }

    fn change_email(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reset_step = ResetStep::Email;
        self.error = None;
        self.notice = None;
        self.email.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Check the fields, then start the request. Never closes the dialog
    /// itself: the store's signed-in state does.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.working(cx) {
            return false;
        }
        match self.mode {
            LoginMode::SignIn => self.sign_in(cx),
            LoginMode::Register => self.register(cx),
            LoginMode::Reset => match self.reset_step {
                ResetStep::Email => self.send_reset_code(window, cx),
                ResetStep::Code => self.submit_reset(window, cx),
            },
        }
        false
    }

    fn sign_in(&mut self, cx: &mut Context<Self>) {
        let username = self.username.read(cx).value().to_string();
        if let Err(problem) = check_username(&username) {
            self.error = Some(username_problem(problem));
            cx.notify();
            return;
        }
        let password = Zeroizing::new(self.password.read(cx).value().to_string());
        if let Err(problem) = check_login_password(&password) {
            self.error = Some(super::describe::login_password_problem(problem));
            cx.notify();
            return;
        }
        self.error = None;
        self.notice = None;
        self.account
            .update(cx, |account, cx| account.login(username, password, cx));
        cx.notify();
    }

    fn register(&mut self, cx: &mut Context<Self>) {
        let username = self.username.read(cx).value().to_string();
        if let Err(problem) = check_username(&username) {
            self.error = Some(username_problem(problem));
            cx.notify();
            return;
        }
        let password = Zeroizing::new(self.password.read(cx).value().to_string());
        if let Err(problem) = check_new_password(&password) {
            self.error = Some(new_password_problem(problem));
            cx.notify();
            return;
        }
        let email = self.email.read(cx).value().to_string();
        let checked = if self.code_required {
            check_required_email(&email)
        } else {
            check_email(&email)
        };
        if let Err(problem) = checked {
            self.error = Some(email_problem(problem));
            cx.notify();
            return;
        }
        let phone = self.phone.read(cx).value().to_string();
        if let Err(problem) = check_phone(&phone) {
            self.error = Some(phone_problem(problem));
            cx.notify();
            return;
        }
        // With the code field out, an empty one is a form mistake, not a
        // reason to ask the server again.
        let code = if self.code_required {
            let code = self.code.read(cx).value().to_string();
            if let Err(problem) = check_code(&code) {
                self.error = Some(code_problem(problem));
                cx.notify();
                return;
            }
            Some(code)
        } else {
            None
        };
        self.error = None;
        self.notice = None;
        self.account.update(cx, |account, cx| {
            account.register(username, password, email, phone, code, cx)
        });
        cx.notify();
    }

    fn send_register_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.code_sending || self.resend_remaining() > 0 {
            return;
        }
        let email = self.email.read(cx).value().to_string();
        if let Err(problem) = check_required_email(&email) {
            self.error = Some(email_problem(problem));
            cx.notify();
            return;
        }
        let username = self.username.read(cx).value().trim().to_string();
        let Some(service) = self.account.read(cx).service() else {
            self.error = Some(account_error(&AccountError::Network(String::new())));
            cx.notify();
            return;
        };
        self.error = None;
        self.code_sending = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let email = email.trim().to_string();
            let result = cx
                .background_spawn(async move {
                    service.request_email_code(
                        &email,
                        (!username.is_empty()).then_some(username.as_str()),
                    )
                })
                .await;
            this.update_in(cx, |form, window, cx| {
                form.apply_code_result(CodePurpose::Register, result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn send_reset_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.code_sending || self.resend_remaining() > 0 {
            return;
        }
        let email = self.email.read(cx).value().to_string();
        if let Err(problem) = check_required_email(&email) {
            self.error = Some(email_problem(problem));
            cx.notify();
            return;
        }
        let Some(service) = self.account.read(cx).service() else {
            self.error = Some(account_error(&AccountError::Network(String::new())));
            cx.notify();
            return;
        };
        self.error = None;
        self.code_sending = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let email = email.trim().to_string();
            let result = cx
                .background_spawn(async move { service.request_password_code(&email) })
                .await;
            this.update_in(cx, |form, window, cx| {
                form.apply_code_result(CodePurpose::Reset, result, window, cx)
            })
            .ok();
        })
        .detach();
    }

    fn apply_code_result(
        &mut self,
        purpose: CodePurpose,
        result: Result<CodeSent, AccountError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.code_sending = false;
        match result {
            Ok(sent) => {
                self.error = None;
                self.code_sent = true;
                match purpose {
                    CodePurpose::Register => self.notice = Some(t!("account.code.sent")),
                    CodePurpose::Reset => {
                        // The same words whether or not the address has an
                        // account: the server does not tell, and neither do
                        // we.
                        self.reset_step = ResetStep::Code;
                        self.notice = None;
                        self.code.update(cx, |state, cx| state.focus(window, cx));
                    }
                }
                self.start_resend(sent.resend_after, cx);
            }
            Err(error) => {
                self.notice = None;
                if matches!(error, AccountError::EmailVerificationDisabled) {
                    self.code_required = false;
                }
                // The server's interval is a minute; count it down here so
                // the button cannot be hammered while it refuses.
                if matches!(error, AccountError::TooMany(_)) {
                    self.start_resend(60, cx);
                }
                self.error = Some(code_error(&error));
            }
        }
        cx.notify();
    }

    fn submit_reset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let email = self.email.read(cx).value().to_string();
        if let Err(problem) = check_required_email(&email) {
            self.error = Some(email_problem(problem));
            cx.notify();
            return;
        }
        let code = self.code.read(cx).value().to_string();
        if let Err(problem) = check_code(&code) {
            self.error = Some(code_problem(problem));
            cx.notify();
            return;
        }
        let password = Zeroizing::new(self.password.read(cx).value().to_string());
        if let Err(problem) = check_new_password(&password) {
            self.error = Some(new_password_problem(problem));
            cx.notify();
            return;
        }
        let Some(service) = self.account.read(cx).service() else {
            self.error = Some(account_error(&AccountError::Network(String::new())));
            cx.notify();
            return;
        };
        self.error = None;
        self.resetting = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let email = email.trim().to_string();
            let code = code.trim().to_string();
            let result = cx
                .background_spawn(async move {
                    service.reset_password(&email, &code, password.as_str())
                })
                .await;
            this.update_in(cx, |form, window, cx| form.apply_reset(result, window, cx))
                .ok();
        })
        .detach();
    }

    fn apply_reset(
        &mut self,
        result: Result<(), AccountError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.resetting = false;
        match result {
            Ok(()) => {
                // The server ended every session of the account, including
                // any token this computer kept.
                self.account
                    .update(cx, |account, cx| account.forget_after_reset(cx));
                self.mode = LoginMode::SignIn;
                self.reset_step = ResetStep::Email;
                self.error = None;
                self.resend_at = None;
                self.notice = Some(t!("account.forgot.done"));
                self.password
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.code
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.focus(window, cx);
            }
            Err(error) => {
                if matches!(error, AccountError::EmailCodeInvalid) {
                    self.code.update(cx, |state, cx| state.focus(window, cx));
                }
                self.error = Some(reset_error(&error));
            }
        }
        cx.notify();
    }

    /// Keep the resend button disabled until the server's interval is over,
    /// then let it be pressed again. Every second is a repaint; the loop ends
    /// when the countdown does or when the dialog closes.
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

    fn username_field(&self, working: bool) -> Field {
        Field::new()
            .label(t!("account.field.username"))
            .required(true)
            .child(
                Input::new(&self.username)
                    .id("account-username")
                    .disabled(working),
            )
    }

    fn password_field(&self, label: SharedString, working: bool) -> Field {
        Field::new().label(label).required(true).child(
            Input::new(&self.password)
                .id("account-password")
                .mask_toggle()
                .content_type(InputContentType::Password)
                .disabled(working),
        )
    }

    fn email_field(&self, required: bool, working: bool) -> Field {
        let label = if required {
            t!("account.field.email_required")
        } else {
            t!("account.field.email")
        };
        Field::new().label(label).required(required).child(
            Input::new(&self.email)
                .id("account-email")
                .content_type(InputContentType::EmailAddress)
                .disabled(working),
        )
    }

    fn code_field(&self, working: bool, description: SharedString, button: Button) -> Field {
        Field::new()
            .label(t!("account.field.verification_code"))
            .required(true)
            .description(description)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&self.code).id("account-code").disabled(working)),
                    )
                    .child(button),
            )
    }

    /// 发送验证码 or its countdown, wired to the flow's sender.
    fn code_button(
        &self,
        working: bool,
        remaining: u64,
        form: Entity<LoginForm>,
        purpose: CodePurpose,
    ) -> Button {
        let label = if remaining > 0 {
            t!("account.button.resend_code_wait", seconds = remaining)
        } else if self.code_sent {
            t!("account.button.resend_code")
        } else {
            t!("account.button.send_code")
        };
        let button = Button::new("account-send-code")
            .label(label)
            .disabled(working || remaining > 0);
        if working || remaining > 0 {
            return button;
        }
        button.on_click(move |_, window, cx| {
            form.update(cx, |form, cx| match purpose {
                CodePurpose::Register => form.send_register_code(window, cx),
                CodePurpose::Reset => form.send_reset_code(window, cx),
            });
        })
    }
}

impl Render for LoginForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let working = self.working(cx);
        let register = self.mode == LoginMode::Register;
        let reset = self.mode == LoginMode::Reset;
        let resetting_code = reset && self.reset_step == ResetStep::Code;
        let remaining = self.resend_remaining();
        let form = cx.entity();
        // The store's reason (wrong password, server down) after a request;
        // the form's own message while the fields are being checked. The
        // request for a code is not an error: the code field explains it.
        let error = self.error.clone().or_else(|| {
            self.account
                .read(cx)
                .login_error()
                .filter(|error| !matches!(error, AccountError::EmailVerificationRequired))
                .map(account_error)
        });
        let description = match (self.mode, self.reset_step) {
            (LoginMode::Reset, ResetStep::Email) => t!("account.forgot.description"),
            (LoginMode::Reset, ResetStep::Code) => t!("account.forgot.sent"),
            _ => t!("account.dialog.description"),
        };

        let mut fields = Form::new();
        match self.mode {
            LoginMode::SignIn => {
                fields = fields
                    .child(self.username_field(working))
                    .child(self.password_field(t!("account.field.password"), working));
            }
            LoginMode::Register => {
                let send =
                    self.code_button(working, remaining, form.clone(), CodePurpose::Register);
                fields = fields
                    .child(self.username_field(working))
                    .child(self.password_field(t!("account.field.new_password"), working))
                    .child(self.email_field(self.code_required, working))
                    // The code belongs with the address it was sent to.
                    .when(self.code_required, |fields| {
                        fields.child(self.code_field(working, t!("account.code.required"), send))
                    })
                    .child(
                        Field::new()
                            .label(t!("account.field.phone"))
                            .description(t!("account.field.phone_hint"))
                            .child(
                                Input::new(&self.phone)
                                    .id("account-phone")
                                    .content_type(InputContentType::TelephoneNumber)
                                    .disabled(working),
                            ),
                    );
            }
            LoginMode::Reset if resetting_code => {
                let send = self.code_button(working, remaining, form.clone(), CodePurpose::Reset);
                fields = fields
                    .child(self.code_field(working, t!("account.field.code_hint"), send))
                    .child(self.password_field(t!("account.field.new_password"), working));
            }
            LoginMode::Reset => {
                fields = fields.child(self.email_field(true, working));
            }
        }

        v_flex()
            .w_full()
            .gap_4()
            .when(!reset, |view| {
                view.child(
                    SegmentedControl::new("account-mode")
                        .selected_index(Some(if register { 1 } else { 0 }))
                        .on_change({
                            let form = form.clone();
                            move |index, _, cx| {
                                let mode = if *index == 1 {
                                    LoginMode::Register
                                } else {
                                    LoginMode::SignIn
                                };
                                form.update(cx, |form, cx| form.switch(mode, cx));
                            }
                        })
                        .segments([
                            Segment::new(t!("account.tab.sign_in")).disabled(working),
                            Segment::new(t!("account.tab.register")).disabled(working),
                        ]),
                )
            })
            .when(reset, |view| {
                view.child(
                    h_flex().w_full().child(
                        Button::new("account-reset-back")
                            .ghost()
                            .label(t!("account.forgot.back"))
                            .disabled(working)
                            .on_click({
                                let form = form.clone();
                                move |_, window, cx| {
                                    form.update(cx, |form, cx| form.back_to_sign_in(window, cx));
                                }
                            }),
                    ),
                )
            })
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
            .when(self.mode == LoginMode::SignIn, |view| {
                view.child(
                    h_flex().w_full().justify_end().child(
                        Button::new("account-forgot")
                            .ghost()
                            .small()
                            .label(t!("account.forgot.link"))
                            .disabled(working)
                            .on_click({
                                let form = form.clone();
                                move |_, window, cx| {
                                    form.update(cx, |form, cx| form.forgot(window, cx));
                                }
                            }),
                    ),
                )
            })
            .when(resetting_code, |view| {
                view.child(
                    h_flex().w_full().child(
                        Button::new("account-change-email")
                            .ghost()
                            .small()
                            .label(t!("account.forgot.change_email"))
                            .disabled(working)
                            .on_click({
                                let form = form.clone();
                                move |_, window, cx| {
                                    form.update(cx, |form, cx| form.change_email(window, cx));
                                }
                            }),
                    ),
                )
            })
            .when_some(self.notice.clone(), |view, notice| {
                view.child(message_block(
                    "account-notice",
                    notice,
                    theme.success,
                    theme.radius,
                ))
            })
            .when_some(error, |view, error| {
                view.child(message_block(
                    "account-error",
                    error,
                    theme.danger,
                    theme.radius,
                ))
            })
    }
}

/// A wrapped, full-width line for good news or bad: long server messages and
/// long addresses break instead of leaving the box.
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

/// The dialog footer, which has to follow the switch: 登录 becomes 创建账号,
/// and the reset swaps 发送验证码 for 重置密码. The dialog's own footer is
/// built once, so it is a small view of its own.
struct LoginFooter {
    form: Entity<LoginForm>,
    _subscription: Subscription,
}

impl LoginFooter {
    fn new(form: Entity<LoginForm>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&form, |_, _, cx| cx.notify());
        Self {
            form,
            _subscription: subscription,
        }
    }
}

impl Render for LoginFooter {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (mode, step, working, remaining) = {
            let form = self.form.read(cx);
            (
                form.mode,
                form.reset_step,
                form.working(cx),
                form.resend_remaining(),
            )
        };
        let (label, disabled) = match (mode, step) {
            (LoginMode::SignIn, _) => (t!("account.dialog.sign_in"), working),
            (LoginMode::Register, _) => (t!("account.dialog.register"), working),
            (LoginMode::Reset, ResetStep::Email) => {
                (t!("account.button.send_code"), working || remaining > 0)
            }
            (LoginMode::Reset, ResetStep::Code) => (t!("account.forgot.reset"), working),
        };
        DialogFooter::new()
            .child(DialogClose::new().trigger(|button| button.label(t!("common.cancel"))))
            .child(
                DialogAction::new().child(
                    Button::new("account-commit")
                        .primary()
                        .label(label)
                        .disabled(disabled),
                ),
            )
    }
}

/// Open the dialog on `mode`'s form.
pub fn open_login_dialog(
    account: Entity<AccountStore>,
    mode: LoginMode,
    window: &mut Window,
    cx: &mut App,
) {
    let form = cx.new(|cx| LoginForm::new(account.clone(), mode, window, cx));
    let footer = cx.new(|cx| LoginFooter::new(form.clone(), cx));
    window.open_dialog(cx, {
        let form = form.clone();
        let account = account.clone();
        move |dialog, _, _| {
            dialog
                .title(t!("account.dialog.title"))
                // The account is optional: a click beside the dialog is a
                // way out, not a mistake.
                .overlay_closable(true)
                .child(form.clone())
                .footer(footer.clone())
                .on_ok({
                    let form = form.clone();
                    move |_, window, cx| form.update(cx, |form, cx| form.commit(window, cx))
                })
                // The next opening starts clean instead of repeating the
                // last attempt's reason.
                .on_close({
                    let account = account.clone();
                    move |_, _, cx| {
                        account.update(cx, |account, cx| account.clear_login_error(cx));
                    }
                })
        }
    });
    form.update(cx, |form, cx| form.focus(window, cx));
}
