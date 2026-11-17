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

/// The popover's contents. `snapshot` is plain data: the sidebar takes it
/// from the store when it renders, so nothing here reads an entity.
pub fn account_popover(
    snapshot: AccountSnapshot,
    theme: gpui_kit::component::Theme,
) -> impl IntoElement {
    let theme = &theme;
    v_flex()
        .id("account-popover")
        .test_support()
        .gap_3()
        .w_64()
        .when(!snapshot.signed_in, |view| {
            view.child(signed_out(snapshot.notice, theme))
        })
        .when(snapshot.signed_in, |view| {
            view.child(signed_in(&snapshot, theme))
        })
}

fn signed_out(notice: Option<AccountNotice>, theme: &gpui_kit::component::Theme) -> AnyElement {
    v_flex()
        .id("account-signed-out")
        .test_support()
        .gap_3()
        .child(
            h_flex()
                .gap_2()
                .child(Avatar::new().name("?").small())
                .child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_sm()
                                .font_medium()
                                .child(t!("account.signed_out")),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(t!("account.popover.signed_out_hint")),
                        ),
                ),
        )
        .when_some(notice, |view, notice| {
            view.child(
                div()
                    .id("account-notice")
                    .test_support()
                    .text_xs()
                    .text_color(theme.warning)
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
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(SignInAccount), cx)
                        }),
                )
                .child(
                    Button::new("account-register")
                        .ghost()
                        .small()
                        .label(t!("account.register"))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(RegisterAccount), cx)
                        }),
                ),
        )
        .into_any_element()
}

fn signed_in(snapshot: &AccountSnapshot, theme: &gpui_kit::component::Theme) -> AnyElement {
    let avatar = snapshot
        .avatar
        .clone()
        .map(|path| Avatar::new().src(path).small())
        .unwrap_or_else(|| Avatar::new().name(snapshot.initial.clone()).small());
    v_flex()
        .id("account-profile")
        .test_support()
        .gap_3()
        .child(
            h_flex().gap_2().child(avatar).child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .id("account-name")
                            .test_support()
                            .text_sm()
                            .font_medium()
                            .child(snapshot.display_name.clone()),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if snapshot.email.is_empty() {
                            format!("@{}", snapshot.username)
                        } else {
                            snapshot.email.clone()
                        },
                    )),
            ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
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
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(OpenAccountSettings), cx)
                        }),
                )
                .child(
                    Button::new("account-sign-out")
                        .ghost()
                        .small()
                        .label(t!("account.sign_out"))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(SignOutAccount), cx)
                        }),
                ),
        )
        .into_any_element()
}
