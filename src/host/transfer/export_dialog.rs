//! The export dialog: what goes into the file, whether it is sealed, and
//! where it is written.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    dialog::{DialogAction, DialogClose, DialogFooter},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use zeroize::Zeroizing;

use crate::i18n::t;
use crate::shared::{dismiss_form_error, form_error_notification};

use super::super::HostStore;
use super::{ExportRequest, default_export_dir, plan_export};

/// What runs once the dialog's user has decided on an export.
pub type ExportHandler = Rc<dyn Fn(ExportRequest, &mut Window, &mut App)>;

/// The suggested file name, so the picker lands on something sensible.
const FILE_NAME: &str = "shellas-hosts.json";

/// Open the export dialog for `store`. Choosing 导出 hands the request to
/// `on_export`, which writes the file away from the UI thread, and closes
/// the dialog.
pub fn open_export_hosts_dialog(
    store: Entity<HostStore>,
    on_export: ExportHandler,
    return_focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| ExportDialog::new(store, window, cx));
    window.open_dialog(cx, move |dialog, window, _| {
        let viewport = window.viewport_size();
        let return_focus = return_focus.clone();
        dialog
            .title(t!("host.portable.export.title"))
            .w(rems(56.)
                .to_pixels(window.rem_size())
                .min(viewport.width * 0.94))
            .margin_top(viewport.height * 0.08)
            .overlay_closable(false)
            .child(view.clone())
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label(t!("common.cancel"))))
                    .child(
                        DialogAction::new().child(
                            Button::new("commit")
                                .primary()
                                .label(t!("host.portable.export.commit")),
                        ),
                    ),
            )
            .on_ok({
                let view = view.clone();
                let on_export = on_export.clone();
                move |_, window, cx| view.update(cx, |view, cx| view.commit(&on_export, window, cx))
            })
            .on_close(move |_, window, cx| {
                dismiss_form_error(window, cx);
                return_focus.focus(window, cx);
            })
    });
}

struct ExportDialog {
    store: Entity<HostStore>,
    hosts: usize,
    groups: usize,
    skipped: usize,
    encrypt: bool,
    secrets: bool,
    password: Entity<InputState>,
    confirm: Entity<InputState>,
    path: PathBuf,
}

impl ExportDialog {
    fn new(store: Entity<HostStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let plan = plan_export(store.read(cx), false);
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("host.portable.export.password"))
        });
        let confirm = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("host.portable.export.password_confirm"))
        });
        Self {
            store,
            hosts: plan.hosts(),
            groups: plan.groups(),
            skipped: plan.skipped,
            encrypt: false,
            secrets: false,
            password,
            confirm,
            path: default_export_dir().join(FILE_NAME),
        }
    }

    fn commit(
        &mut self,
        on_export: &ExportHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let password = if self.encrypt {
            let password = self.password.read(cx).value().to_string();
            if password.is_empty() {
                window.push_notification(
                    form_error_notification(t!("host.portable.export.password_needed")),
                    cx,
                );
                return false;
            }
            if password.as_str() != self.confirm.read(cx).value().as_ref() {
                window.push_notification(
                    form_error_notification(t!("host.portable.export.password_mismatch")),
                    cx,
                );
                return false;
            }
            Some(Zeroizing::new(password))
        } else {
            None
        };
        // Secrets only travel sealed: the checkbox is disabled without one,
        // and this is the same rule written where it is obeyed.
        let include_secrets = self.secrets && self.encrypt;
        let plan = plan_export(self.store.read(cx), include_secrets);
        if plan.hosts() == 0 && plan.groups() == 0 {
            window.push_notification(
                form_error_notification(t!("host.portable.export.nothing")),
                cx,
            );
            return false;
        }
        on_export(
            ExportRequest {
                plan,
                password,
                path: self.path.clone(),
            },
            window,
            cx,
        );
        true
    }

    fn choose_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_new_path(
            &default_export_dir(),
            Some(
                self.path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or(FILE_NAME),
            ),
        );
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(path))) = receiver.await {
                this.update_in(cx, |this, _, cx| {
                    this.path = path;
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }
}

impl Render for ExportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .id("export-hosts-dialog")
            .w_full()
            .gap_3()
            .child(div().text_sm().child(t!(
                "host.portable.export.note",
                hosts = self.hosts,
                groups = self.groups
            )))
            .when(self.skipped > 0, |this| {
                this.child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(t!("host.portable.export.skipped", count = self.skipped)),
                )
            })
            .child(
                Checkbox::new("export-encrypt")
                    .checked(self.encrypt)
                    .label(t!("host.portable.export.encrypt"))
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.encrypt = *checked;
                        if !this.encrypt {
                            this.secrets = false;
                        }
                        cx.notify();
                    })),
            )
            .when(self.encrypt, |this| {
                this.child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .w(rems(16.))
                                .child(Input::new(&self.password).id("export-password").small()),
                        )
                        .child(
                            div().w(rems(16.)).child(
                                Input::new(&self.confirm)
                                    .id("export-password-confirm")
                                    .small(),
                            ),
                        ),
                )
            })
            .child(
                Checkbox::new("export-secrets")
                    .checked(self.secrets)
                    .disabled(!self.encrypt)
                    .label(t!("host.portable.export.secrets"))
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.secrets = *checked;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("host.portable.export.secrets_hint")),
            )
            .child(
                h_flex()
                    .id("export-path")
                    .test_support()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .child(t!("host.portable.export.path")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(self.path.to_string_lossy().to_string()),
                    )
                    .child(
                        Button::new("export-choose-path")
                            .small()
                            .label(t!("host.portable.export.choose"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_path(window, cx);
                            })),
                    ),
            )
    }
}
