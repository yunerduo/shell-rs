//! The sign-in and registration dialog. One dialog, two tabs; the account
//! is optional, so this is only ever opened from a button the user pressed.
//!
//! The dialog stays open while the request runs, exits when the store says
//! it is signed in, and shows the server's reason inline when it is not.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    form::{Field, Form},
    input::{Input, InputContentType, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeroize::Zeroizing;

use super::describe::{account_error, email_problem, new_password_problem, username_problem};
use super::model::{
    AccountPhase, check_email, check_login_password, check_new_password, check_username,
};
use super::store::AccountStore;
use crate::i18n::t;
use crate::shared::{commit_footer, dismiss_form_error};

/// Which tab the dialog opens on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMode {
    SignIn,
    Register,
}

struct LoginForm {
    account: Entity<AccountStore>,
    mode: LoginMode,
    username: Entity<InputState>,
    password: Entity<InputState>,
    email: Entity<InputState>,
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
        let subscription = cx.observe_in(&account, window, |_this, account, window, cx| {
            if account.read(cx).phase() == AccountPhase::SignedIn {
                dismiss_form_error(window, cx);
                window.close_dialog(cx);
            }
            cx.notify();
        });
        Self {
            account,
            mode,
            username,
            password,
            email,
            error: None,
            _subscription: subscription,
        }
    }

    fn working(&self, cx: &App) -> bool {
        self.account.read(cx).phase() == AccountPhase::SigningIn
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        self.username
            .update(cx, |state, cx| state.focus(window, cx));
    }

    fn switch(&mut self, mode: LoginMode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.error = None;
        cx.notify();
    }

    /// Check the fields, then start the request. Never closes the dialog
    /// itself: the store's signed-in state does.
    fn commit(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.working(cx) {
            return false;
        }
        let username = self.username.read(cx).value().to_string();
        if let Err(problem) = check_username(&username) {
            self.error = Some(username_problem(problem));
            cx.notify();
            return false;
        }
        let password = Zeroizing::new(self.password.read(cx).value().to_string());
        match self.mode {
            LoginMode::SignIn => {
                if let Err(problem) = check_login_password(&password) {
                    self.error = Some(super::describe::login_password_problem(problem));
                    cx.notify();
                    return false;
                }
                self.error = None;
                self.account
                    .update(cx, |account, cx| account.login(username, password, cx));
            }
            LoginMode::Register => {
                if let Err(problem) = check_new_password(&password) {
                    self.error = Some(new_password_problem(problem));
                    cx.notify();
                    return false;
                }
                let email = self.email.read(cx).value().to_string();
                if let Err(problem) = check_email(&email) {
                    self.error = Some(email_problem(problem));
                    cx.notify();
                    return false;
                }
                self.error = None;
                self.account.update(cx, |account, cx| {
                    account.register(username, password, email, cx)
                });
            }
        }
        cx.notify();
        false
    }
}

impl Render for LoginForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let working = self.working(cx);
        let register = self.mode == LoginMode::Register;
        // The store's reason (wrong password, server down) after a request;
        // the form's own message while the fields are being checked.
        let error = self
            .error
            .clone()
            .or_else(|| self.account.read(cx).login_error().map(account_error));
        let tab = |id: &'static str, label: SharedString, active: bool, mode: LoginMode| {
            let button = Button::new(id).small().label(label);
            let button = if active {
                button.primary()
            } else {
                button.ghost()
            };
            button
                .disabled(working)
                .on_click(cx.listener(move |this, _, _, cx| this.switch(mode, cx)))
        };
        v_flex()
            .gap_3()
            .w_full()
            .child(
                div()
                    .id("account-tabs")
                    .test_support()
                    .flex()
                    .gap_1()
                    .child(tab(
                        "account-tab-signin",
                        t!("account.tab.sign_in"),
                        !register,
                        LoginMode::SignIn,
                    ))
                    .child(tab(
                        "account-tab-register",
                        t!("account.tab.register"),
                        register,
                        LoginMode::Register,
                    )),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("account.dialog.description")),
            )
            .child(
                Form::new()
                    .child(
                        Field::new()
                            .label(t!("account.field.username"))
                            .required(true)
                            .child(
                                Input::new(&self.username)
                                    .id("account-username")
                                    .small()
                                    .disabled(working),
                            ),
                    )
                    .child(
                        Field::new()
                            .label(if register {
                                t!("account.field.new_password")
                            } else {
                                t!("account.field.password")
                            })
                            .required(true)
                            .child(
                                Input::new(&self.password)
                                    .id("account-password")
                                    .small()
                                    .mask_toggle()
                                    .content_type(InputContentType::Password)
                                    .disabled(working),
                            ),
                    )
                    .when(register, |form| {
                        form.child(
                            Field::new().label(t!("account.field.email")).child(
                                Input::new(&self.email)
                                    .id("account-email")
                                    .small()
                                    .disabled(working),
                            ),
                        )
                    }),
            )
            .when_some(error, |view, error| {
                view.child(
                    div()
                        .id("account-error")
                        .test_support()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
    }
}

/// Open the dialog on `mode`'s tab.
pub fn open_login_dialog(
    account: Entity<AccountStore>,
    mode: LoginMode,
    window: &mut Window,
    cx: &mut App,
) {
    let title = match mode {
        LoginMode::SignIn => t!("account.dialog.sign_in_title"),
        LoginMode::Register => t!("account.dialog.register_title"),
    };
    let commit_label = match mode {
        LoginMode::SignIn => t!("account.dialog.sign_in"),
        LoginMode::Register => t!("account.dialog.register"),
    };
    let form = cx.new(|cx| LoginForm::new(account, mode, window, cx));
    window.open_dialog(cx, {
        let form = form.clone();
        move |dialog, _, _| {
            dialog
                .title(title.clone())
                // The account is optional: a click beside the dialog is a
                // way out, not a mistake.
                .overlay_closable(true)
                .child(form.clone())
                .footer(commit_footer("account-commit", commit_label.clone()))
                .on_ok({
                    let form = form.clone();
                    move |_, window, cx| form.update(cx, |form, cx| form.commit(window, cx))
                })
                .on_close(|_, window, cx| dismiss_form_error(window, cx))
        }
    });
    form.update(cx, |form, cx| form.focus(window, cx));
}
