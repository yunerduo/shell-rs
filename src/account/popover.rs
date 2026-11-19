//! What the account button's popover shows: signed out, it offers sign-in
//! and registration; signed in, the profile and the way to 设置 › 账号.
//! The popover never opens by itself and never asks for anything.

use gpui_kit::component::{
    Sizable as _, StyledExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::describe::notice_text;
use super::model::{AccountNotice, AccountSnapshot};
use crate::app::{OpenAccountSettings, RegisterAccount, SignInAccount, SignOutAccount};
use crate::i18n::t;

/// The popover's contents. `snapshot` is plain data: the title bar takes it
/// from the store when it renders, so nothing here reads an entity. `target`
/// is the workspace's focus handle, used the same way every title-bar button
/// dispatches: the command lands even when the focus is inside a terminal.
pub fn account_popover(
    snapshot: AccountSnapshot,
    target: FocusHandle,
    theme: gpui_kit::component::Theme,
) -> impl IntoElement {
    let theme = &theme;
    v_flex()
        .id("account-popover")
        .test_support()
        .gap_3()
        .w_72()
        .when(!snapshot.signed_in, |view| {
            view.child(signed_out(snapshot.notice, &target, theme))
        })
        .when(snapshot.signed_in, |view| {
            view.child(signed_in(&snapshot, &target, theme))
        })
}

fn signed_out(
    notice: Option<AccountNotice>,
    target: &FocusHandle,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    let (sign_in, register) = (target.clone(), target.clone());
    v_flex()
        .id("account-signed-out")
        .test_support()
        .gap_3()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .child(Avatar::new().name("?").small())
                .child(
                    v_flex()
                        .min_w_0()
                        .flex_1()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(t!("account.signed_out")),
                        )
                        .child(
                            div()
                                .min_w_0()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .whitespace_normal()
                                .child(t!("account.popover.signed_out_hint")),
                        ),
                ),
        )
        .when_some(notice, |view, notice| {
            view.child(
                div()
                    .id("account-notice")
                    .test_support()
                    .w_full()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.warning)
                    .whitespace_normal()
                    .child(notice_text(notice)),
            )
        })
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("account-sign-in")
                        .primary()
                        .small()
                        .label(t!("account.sign_in"))
                        .on_click(move |_, window, cx| {
                            sign_in.dispatch_action(&SignInAccount, window, cx)
                        }),
                )
                .child(
                    Button::new("account-register")
                        .ghost()
                        .small()
                        .label(t!("account.register"))
                        .on_click(move |_, window, cx| {
                            register.dispatch_action(&RegisterAccount, window, cx)
                        }),
                ),
        )
        .into_any_element()
}

fn signed_in(
    snapshot: &AccountSnapshot,
    target: &FocusHandle,
    theme: &gpui_kit::component::Theme,
) -> AnyElement {
    let (open_settings, sign_out) = (target.clone(), target.clone());
    let avatar = snapshot
        .avatar
        .clone()
        .map(|path| Avatar::new().src(path).with_size(px(64.)))
        .unwrap_or_else(|| {
            Avatar::new()
                .name(snapshot.initial.clone())
                .with_size(px(64.))
        });
    v_flex()
        .id("account-profile")
        .test_support()
        .gap_3()
        .child(
            h_flex().w_full().min_w_0().gap_3().child(avatar).child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .gap_1()
                    .child(
                        div()
                            .id("account-name")
                            .test_support()
                            .min_w_0()
                            .text_base()
                            .font_medium()
                            .whitespace_normal()
                            .child(snapshot.display_name.clone()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .whitespace_normal()
                            .child(if snapshot.email.is_empty() {
                                format!("@{}", snapshot.username)
                            } else {
                                snapshot.email.clone()
                            }),
                    ),
            ),
        )
        .child(
            div()
                .w_full()
                .min_w_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .whitespace_normal()
                .child(if snapshot.cloud_sync {
                    t!("account.popover.cloud_on")
                } else {
                    t!("account.popover.cloud_off")
                }),
        )
        .child(
            h_flex()
                .gap_2()
                .child(
                    Button::new("account-open-settings")
                        .outline()
                        .small()
                        .label(t!("account.settings"))
                        .on_click(move |_, window, cx| {
                            open_settings.dispatch_action(&OpenAccountSettings, window, cx)
                        }),
                )
                .child(
                    Button::new("account-sign-out")
                        .ghost()
                        .small()
                        .label(t!("account.sign_out"))
                        .on_click(move |_, window, cx| {
                            sign_out.dispatch_action(&SignOutAccount, window, cx)
                        }),
                ),
        )
        .into_any_element()
}
