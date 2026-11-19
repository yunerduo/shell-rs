//! 设置 › 账号. Signed out, the page explains what an account is for and
//! offers the two ways in; signed in, it edits the avatar and nickname and
//! holds the cloud sync switches. No row here is needed to use ShellAs.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    dialog::{DialogClose, DialogFooter},
    form::{Field, Form},
    h_flex,
    input::{Input, InputContentType, InputState},
    scroll::ScrollableElement as _,
    setting::{SettingField, SettingGroup, SettingItem},
    switch::Switch,
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeroize::Zeroizing;

use super::describe::notice_text;
use super::store::AccountStore;
use crate::app::{
    ChangeAccountAvatar, ChangeAccountEmail, ChangeAccountPassword, EditAccountNickname,
    OpenSyncContent, RegisterAccount, RemoveAccountAvatar, SetCloudConfig, SignInAccount,
    UploadCloudSyncNow,
};
use crate::i18n::t;
use crate::settings::{ConfigSection, SettingsPanel};
use crate::shared::commit_footer;

/// The 账号 category: one group signed out, three signed in.
pub fn account_groups(panel: &SettingsPanel, cx: &App) -> Vec<SettingGroup> {
    let account = panel.account().clone();
    let snapshot = account.read(cx).snapshot();
    if !snapshot.signed_in {
        return signed_out_groups(&account, snapshot.notice);
    }
    vec![
        profile_group(&account),
        cloud_group(&account),
        sync_content_group(&account),
    ]
}

fn signed_out_groups(
    _account: &Entity<AccountStore>,
    notice: Option<super::model::AccountNotice>,
) -> Vec<SettingGroup> {
    let field = SettingField::render(|_, _, _| {
        h_flex()
            .gap_2()
            .child(
                Button::new("account-settings-sign-in")
                    .primary()
                    .small()
                    .label(t!("account.sign_in"))
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(SignInAccount), cx)),
            )
            .child(
                Button::new("account-settings-register")
                    .ghost()
                    .small()
                    .label(t!("account.register"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(RegisterAccount), cx)
                    }),
            )
    });
    let mut items = vec![
        SettingItem::new(t!("settings.account.signed_out"), field)
            .description(t!("settings.account.signed_out_description"))
            .layout(Axis::Vertical),
        SettingItem::new(
            t!("settings.account.local_first"),
            SettingField::render(|_, _, _| {
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_sm()
                    .child(t!("settings.account.local_first_description"))
            }),
        ),
    ];
    if let Some(notice) = notice {
        items.push(SettingItem::new(
            t!("settings.account.notice"),
            SettingField::render(move |_, _, _| {
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_sm()
                    .child(notice_text(notice))
            }),
        ));
    }
    vec![
        SettingGroup::new()
            .title(t!("settings.account.group.account"))
            .items(items),
    ]
}

fn profile_group(account: &Entity<AccountStore>) -> SettingGroup {
    let avatar_reader = account.clone();
    let avatar = SettingField::render(move |_, _, cx| {
        let snapshot = avatar_reader.read(cx).snapshot();
        let picture = snapshot
            .avatar
            .map(|path| Avatar::new().src(path).small())
            .unwrap_or_else(|| Avatar::new().name(snapshot.initial).small());
        h_flex()
            .gap_2()
            .child(picture)
            .child(
                Button::new("account-avatar-change")
                    .outline()
                    .small()
                    .label(t!("settings.account.change_avatar"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ChangeAccountAvatar), cx)
                    }),
            )
            .when(snapshot.custom_avatar, |row| {
                row.child(
                    Button::new("account-avatar-remove")
                        .ghost()
                        .small()
                        .label(t!("settings.account.remove_avatar"))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(RemoveAccountAvatar), cx)
                        }),
                )
            })
    });
    let nickname_reader = account.clone();
    let nickname = SettingField::render(move |_, _, cx| {
        let snapshot = nickname_reader.read(cx).snapshot();
        let shown = if snapshot.nickname.trim().is_empty() {
            t!("settings.account.nickname_empty")
        } else {
            snapshot.nickname.into()
        };
        h_flex()
            .min_w_0()
            .gap_2()
            .child(div().min_w_0().whitespace_normal().text_sm().child(shown))
            .child(
                Button::new("account-nickname-edit")
                    .outline()
                    .small()
                    .label(t!("settings.account.edit_nickname"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(EditAccountNickname), cx)
                    }),
            )
    });
    let name_reader = account.clone();
    let name = SettingField::render(move |_, _, cx| {
        let snapshot = name_reader.read(cx).snapshot();
        div()
            .min_w_0()
            .whitespace_normal()
            .text_sm()
            .child(format!("@{}", snapshot.username))
    });
    let email_reader = account.clone();
    let email = SettingField::render(move |_, _, cx| {
        let snapshot = email_reader.read(cx).snapshot();
        let shown = if snapshot.email.is_empty() {
            t!("settings.account.email_empty")
        } else {
            snapshot.email.into()
        };
        h_flex()
            .min_w_0()
            .gap_2()
            .child(div().min_w_0().whitespace_normal().text_sm().child(shown))
            .child(
                Button::new("account-change-email")
                    .outline()
                    .small()
                    .label(t!("settings.account.change_email"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(ChangeAccountEmail), cx)
                    }),
            )
    });
    let email_avatar_reader = account.clone();
    let email_avatar_writer = account.clone();
    let email_avatar = SettingField::switch(
        move |cx| email_avatar_reader.read(cx).snapshot().email_avatar,
        move |enabled, cx| {
            email_avatar_writer.update(cx, |account, cx| account.set_email_avatar(enabled, cx))
        },
    )
    .default_value(true);
    SettingGroup::new()
        .title(t!("settings.account.group.account"))
        .items([
            SettingItem::new(t!("settings.account.avatar"), avatar)
                .description(t!("settings.account.avatar_description")),
            SettingItem::new(t!("settings.account.email_avatar"), email_avatar)
                .description(t!("settings.account.email_avatar_description")),
            SettingItem::new(t!("settings.account.nickname"), nickname)
                .description(t!("settings.account.nickname_description")),
            SettingItem::new(t!("settings.account.username"), name),
            SettingItem::new(t!("settings.account.email"), email)
                .description(t!("settings.account.email_description")),
            SettingItem::new(
                t!("settings.account.password"),
                SettingField::render(|_, _, _| {
                    h_flex().gap_2().child(
                        Button::new("account-change-password")
                            .outline()
                            .small()
                            .label(t!("settings.account.change_password"))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(Box::new(ChangeAccountPassword), cx)
                            }),
                    )
                }),
            )
            .description(t!("settings.account.password_description")),
        ])
}

fn cloud_group(account: &Entity<AccountStore>) -> SettingGroup {
    let cloud_reader = account.clone();
    let cloud_writer = account.clone();
    let cloud_sync = SettingField::switch(
        move |cx| cloud_reader.read(cx).snapshot().cloud_sync,
        move |enabled, cx| {
            cloud_writer.update(cx, |account, cx| account.set_cloud_sync(enabled, cx))
        },
    )
    .default_value(false);
    let remember_reader = account.clone();
    let remember_writer = account.clone();
    let remember = SettingField::switch(
        move |cx| remember_reader.read(cx).snapshot().remember_sync_password,
        move |enabled, cx| {
            remember_writer.update(cx, |account, cx| {
                account.set_remember_sync_password(enabled, cx)
            })
        },
    )
    .default_value(false);
    let uploader = account.clone();
    let upload = SettingField::render(move |_, _, cx| {
        let snapshot = uploader.read(cx).snapshot();
        // A stale cloud copy outranks "off": there is something up there
        // that this account can no longer open, and the user should know.
        let hint = if snapshot.sync_stale {
            t!("settings.account.sync_stale")
        } else if snapshot.cloud_sync {
            snapshot
                .last_sync_at
                .clone()
                .map(|at| t!("settings.account.last_sync", at = at))
                .unwrap_or_else(|| t!("settings.account.never_synced"))
        } else {
            t!("settings.account.sync_off")
        };
        let hint_color = if snapshot.sync_stale {
            cx.theme().warning
        } else {
            cx.theme().muted_foreground
        };
        h_flex()
            .min_w_0()
            .gap_2()
            .child(
                Button::new("account-upload")
                    .outline()
                    .small()
                    .label(t!("settings.account.upload_now"))
                    .disabled(!snapshot.cloud_sync)
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(UploadCloudSyncNow), cx)
                    }),
            )
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_xs()
                    .text_color(hint_color)
                    .child(hint),
            )
    });
    SettingGroup::new()
        .title(t!("settings.account.group.cloud"))
        .items([
            SettingItem::new(t!("settings.account.cloud_sync"), cloud_sync)
                .description(t!("settings.account.cloud_sync_description")),
            SettingItem::new(t!("settings.account.remember_password"), remember)
                .description(t!("settings.account.remember_password_description")),
            SettingItem::new(t!("settings.account.sync_now"), upload)
                .description(t!("settings.account.sync_now_description"))
                .layout(gpui_kit::Axis::Vertical),
        ])
}

/// 同步内容: one master switch for 软件配置, with the way into the list of
/// groups it would carry beside it. Hosts and credentials are not behind
/// that switch, so the description says so.
fn sync_content_group(account: &Entity<AccountStore>) -> SettingGroup {
    let reader = account.clone();
    let row = SettingField::render(move |_, _, cx| {
        let snapshot = reader.read(cx).snapshot();
        let status = if snapshot.cloud_config {
            t!(
                "settings.account.sync_config_on",
                count = snapshot.cloud_sections.len()
            )
        } else {
            t!("settings.account.sync_config_off")
        };
        h_flex()
            .min_w_0()
            .gap_2()
            .child(
                Switch::new("account-sync-config")
                    .checked(snapshot.cloud_config)
                    .accessibility_label(t!("settings.account.sync_config"))
                    .on_click(|next, window, cx| {
                        window.dispatch_action(Box::new(SetCloudConfig { enabled: *next }), cx)
                    }),
            )
            .child(
                Button::new("account-sync-content")
                    .outline()
                    .small()
                    .label(t!("settings.account.choose_sync_content"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(OpenSyncContent), cx)
                    }),
            )
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(status),
            )
    });
    SettingGroup::new()
        .title(t!("settings.account.group.sync_content"))
        .description(t!("settings.account.sync_content_summary"))
        .items([SettingItem::new(t!("settings.account.sync_config"), row)])
}

/// 选择同步内容: the groups of 软件配置 the settings document carries. Every
/// switch writes the choice straight away, so there is nothing to save and
/// closing the dialog loses nothing.
struct SyncContentForm {
    account: Entity<AccountStore>,
    _subscription: Subscription,
}

impl SyncContentForm {
    fn new(account: Entity<AccountStore>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&account, |_, _, cx| cx.notify());
        Self {
            account,
            _subscription: subscription,
        }
    }
}

impl Render for SyncContentForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let selected = self.account.read(cx).snapshot().cloud_sections;
        let account = self.account.clone();
        let rows = ConfigSection::ALL.into_iter().map(move |section| {
            let writer = account.clone();
            let checked = selected.contains(&section);
            h_flex()
                .id(format!("account-sync-section-row-{}", section.key()))
                .test_support()
                .justify_between()
                .items_start()
                .gap_4()
                .child(
                    v_flex()
                        .min_w_0()
                        .gap_1()
                        .child(div().text_sm().child(section.label()))
                        .child(
                            div()
                                .min_w_0()
                                .whitespace_normal()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(section.description()),
                        ),
                )
                .child(
                    Switch::new(format!("account-sync-section-{}", section.key()))
                        .checked(checked)
                        .accessibility_label(section.label())
                        .on_click(move |next, _, cx| {
                            writer.update(cx, |account, cx| {
                                account.set_cloud_section(section, *next, cx)
                            });
                        }),
                )
        });
        v_flex()
            .gap_4()
            .w_full()
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("settings.account.sync_content_description")),
            )
            .child(
                v_flex()
                    .gap_3()
                    .max_h(window.viewport_size().height * 0.6)
                    .overflow_y_scrollbar()
                    .children(rows),
            )
    }
}

/// Open 选择同步内容.
pub fn open_sync_content_dialog(account: Entity<AccountStore>, window: &mut Window, cx: &mut App) {
    if !account.read(cx).is_signed_in() {
        return;
    }
    let form = cx.new(|cx| SyncContentForm::new(account, cx));
    window.open_dialog(cx, {
        let form = form.clone();
        move |dialog, window, _| {
            dialog
                .title(t!("settings.account.sync_content_title"))
                .w(rems(36.).to_pixels(window.rem_size()))
                .max_h(window.viewport_size().height * 0.9)
                .overlay_closable(true)
                .child(form.clone())
                .footer(DialogFooter::new().child(
                    DialogClose::new().trigger(|button| button.primary().label(t!("common.close"))),
                ))
        }
    });
}

/// 修改昵称: one field, kept local to the account. The server takes
/// 1–24 characters, and an empty field clears it.
struct NicknameForm {
    account: Entity<AccountStore>,
    input: Entity<InputState>,
    error: Option<SharedString>,
}

impl NicknameForm {
    fn new(account: Entity<AccountStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let value = account.read(cx).snapshot().nickname;
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |state, cx| state.set_value(value, window, cx));
        Self {
            account,
            input,
            error: None,
        }
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let value = self.input.read(cx).value().to_string();
        if value.trim().chars().count() > 24 {
            self.error = Some(t!("settings.account.nickname_too_long"));
            cx.notify();
            return false;
        }
        self.account
            .update(cx, |account, cx| account.set_nickname(value, cx));
        window.close_dialog(cx);
        true
    }
}

impl Render for NicknameForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .w_full()
            .child(
                Form::new().child(
                    Field::new()
                        .label(t!("settings.account.nickname"))
                        .child(Input::new(&self.input).id("account-nickname").small()),
                ),
            )
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(t!("settings.account.nickname_description")),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    div()
                        .min_w_0()
                        .whitespace_normal()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
    }
}

/// Open the nickname dialog.
pub fn open_nickname_dialog(account: Entity<AccountStore>, window: &mut Window, cx: &mut App) {
    if !account.read(cx).is_signed_in() {
        return;
    }
    let form = cx.new(|cx| NicknameForm::new(account, window, cx));
    window.open_dialog(cx, {
        let form = form.clone();
        move |dialog, _, _| {
            dialog
                .title(t!("settings.account.nickname_title"))
                .overlay_closable(true)
                .child(form.clone())
                .footer(commit_footer(
                    "account-nickname-commit",
                    t!("settings.account.save_nickname"),
                ))
                .on_ok({
                    let form = form.clone();
                    move |_, window, cx| form.update(cx, |form, cx| form.commit(window, cx))
                })
        }
    });
}

/// 云同步密码: the login password a remembered token does not carry, asked
/// for once when the first upload after a restart needs it. The account is
/// optional, so the dialog can be dismissed and the upload simply waits.
struct SyncPasswordForm {
    account: Entity<AccountStore>,
    input: Entity<InputState>,
    error: Option<SharedString>,
}

impl SyncPasswordForm {
    fn new(account: Entity<AccountStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).masked(true));
        Self {
            account,
            input,
            error: None,
        }
    }

    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let password = Zeroizing::new(self.input.read(cx).value().to_string());
        if password.is_empty() {
            self.error = Some(t!("account.form.password.empty"));
            cx.notify();
            return false;
        }
        self.account
            .update(cx, |account, cx| account.set_sync_password(password, cx));
        window.close_dialog(cx);
        true
    }
}

impl Render for SyncPasswordForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_3()
            .w_full()
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("account.sync_password.description")),
            )
            .child(
                Form::new().child(
                    Field::new()
                        .label(t!("account.field.password"))
                        .required(true)
                        .child(
                            Input::new(&self.input)
                                .id("account-sync-password")
                                .small()
                                .mask_toggle()
                                .content_type(InputContentType::Password),
                        ),
                ),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    div()
                        .id("account-sync-password-error")
                        .test_support()
                        .min_w_0()
                        .whitespace_normal()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
    }
}

/// Open the cloud sync password dialog.
pub fn open_sync_password_dialog(account: Entity<AccountStore>, window: &mut Window, cx: &mut App) {
    if !account.read(cx).is_signed_in() {
        return;
    }
    let form = cx.new(|cx| SyncPasswordForm::new(account, window, cx));
    window.open_dialog(cx, {
        let form = form.clone();
        move |dialog, _, _| {
            dialog
                .title(t!("account.sync_password.title"))
                .overlay_closable(true)
                .child(form.clone())
                .footer(commit_footer(
                    "account-sync-password-commit",
                    t!("account.sync_password.commit"),
                ))
                .on_ok({
                    let form = form.clone();
                    move |_, window, cx| form.update(cx, |form, cx| form.commit(window, cx))
                })
        }
    });
}
