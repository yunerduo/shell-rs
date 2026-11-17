//! The import dialog: read a file ShellAs exported, unlock it if it is
//! sealed, show what the merge would do, then do it.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    dialog::{DialogAction, DialogClose, DialogFooter},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};
use gpui_kit::*;

use crate::i18n::{join_list, t};
use crate::shared::{dismiss_form_error, form_error_notification};

use super::super::HostStore;
use super::{ImportPreview, ImportSummary, apply_import, archive_error, preview_import};
use shellrs_hosts::portable::{Archive, ArchiveError, decode};

/// What runs once an import has been written, with what it did.
pub type ArchiveImportHandler = Rc<dyn Fn(ImportSummary, &mut Window, &mut App)>;

/// Open the dialog on `path`. It reads and, when the file is sealed, asks
/// for the password before showing anything; 导入 then merges the file into
/// `store` and hands the summary to `on_imported`.
pub fn open_import_hosts_dialog(
    path: PathBuf,
    store: Entity<HostStore>,
    on_imported: ArchiveImportHandler,
    return_focus: FocusHandle,
    window: &mut Window,
    cx: &mut App,
) {
    let view = cx.new(|cx| ImportDialog::new(path, store, window, cx));
    view.update(cx, |view, cx| view.load(window, cx));
    window.open_dialog(cx, move |dialog, window, cx| {
        let viewport = window.viewport_size();
        let ready = view.read(cx).ready();
        let return_focus = return_focus.clone();
        dialog
            .title(t!("host.portable.import.title"))
            .w(rems(64.)
                .to_pixels(window.rem_size())
                .min(viewport.width * 0.94))
            .margin_top(viewport.height * 0.06)
            .max_h(viewport.height * 0.9)
            .overlay_closable(false)
            .child(view.clone())
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().trigger(|button| button.label(t!("common.cancel"))))
                    .child(
                        DialogAction::new().child(
                            Button::new("commit")
                                .primary()
                                .label(t!("host.portable.import.commit"))
                                .disabled(!ready),
                        ),
                    ),
            )
            .on_ok({
                let view = view.clone();
                let on_imported = on_imported.clone();
                move |_, window, cx| {
                    view.update(cx, |view, cx| view.commit(&on_imported, window, cx))
                }
            })
            .on_close(move |_, window, cx| {
                dismiss_form_error(window, cx);
                return_focus.focus(window, cx);
            })
    });
}

/// What the file itself turned out to be.
enum Loaded {
    Archive(Archive),
    /// Sealed: the text is kept so the password can open it.
    Locked(String),
    Damaged(ArchiveError),
    Unreadable(String),
}

struct ImportDialog {
    store: Entity<HostStore>,
    path: PathBuf,
    archive: Option<Archive>,
    /// The sealed text, kept between the first look and the password.
    sealed: Option<String>,
    loading: bool,
    failure: Option<SharedString>,
    password: Entity<InputState>,
    preview: Option<ImportPreview>,
    _store_changes: Subscription,
}

impl ImportDialog {
    fn new(
        path: PathBuf,
        store: Entity<HostStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t!("host.portable.import.password"))
        });
        cx.subscribe_in(
            &password,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.unlock(window, cx);
                }
            },
        )
        .detach();
        Self {
            store: store.clone(),
            path,
            archive: None,
            sealed: None,
            loading: true,
            failure: None,
            password,
            preview: None,
            _store_changes: cx.observe(&store, |this, store, cx| this.refresh(store, cx)),
        }
    }

    fn ready(&self) -> bool {
        self.archive.is_some()
    }

    /// Read the file and take a first look: an archive, a sealed one, or
    /// what is wrong with it.
    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.clone();
        self.loading = true;
        self.archive = None;
        self.sealed = None;
        self.failure = None;
        self.preview = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    match std::fs::read_to_string(&path) {
                        Ok(text) => match decode(&text, None) {
                            Ok(archive) => Loaded::Archive(archive),
                            Err(ArchiveError::NeedsPassword) => Loaded::Locked(text),
                            Err(error) => Loaded::Damaged(error),
                        },
                        Err(error) => Loaded::Unreadable(error.to_string()),
                    }
                })
                .await;
            this.update_in(cx, |this, _, cx| this.loaded(loaded, cx))
                .ok();
        })
        .detach();
    }

    fn loaded(&mut self, loaded: Loaded, cx: &mut Context<Self>) {
        self.loading = false;
        match loaded {
            Loaded::Archive(archive) => {
                self.archive = Some(archive);
                self.refresh_preview(cx);
            }
            Loaded::Locked(text) => self.sealed = Some(text),
            Loaded::Damaged(error) => self.failure = Some(archive_error(error)),
            Loaded::Unreadable(error) => {
                self.failure = Some(t!(
                    "host.portable.import.unreadable",
                    path = self.path.to_string_lossy(),
                    error = error
                ))
            }
        }
        cx.notify();
    }

    /// Open a sealed file with the password typed so far.
    fn unlock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.sealed.clone() else {
            return;
        };
        let password = self.password.read(cx).value().to_string();
        if password.is_empty() {
            window.push_notification(
                form_error_notification(t!("host.portable.import.password_needed")),
                cx,
            );
            return;
        }
        self.loading = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let opened = cx
                .background_spawn(async move { decode(&text, Some(&password)) })
                .await;
            this.update_in(cx, |this, window, cx| match opened {
                Ok(archive) => {
                    this.archive = Some(archive);
                    this.sealed = None;
                    this.loading = false;
                    this.refresh_preview(cx);
                    cx.notify();
                }
                Err(ArchiveError::WrongPassword) => {
                    this.loading = false;
                    this.password
                        .update(cx, |password, cx| password.set_value("", window, cx));
                    cx.notify();
                    window.defer(cx, |window, cx| {
                        window.push_notification(
                            form_error_notification(t!("host.portable.import.wrong_password")),
                            cx,
                        );
                    });
                }
                Err(error) => {
                    this.loading = false;
                    this.sealed = None;
                    this.failure = Some(archive_error(error));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn refresh(&mut self, store: Entity<HostStore>, cx: &mut Context<Self>) {
        let _ = store;
        self.refresh_preview(cx);
        cx.notify();
    }

    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        self.preview = self
            .archive
            .as_ref()
            .map(|archive| preview_import(self.store.read(cx), archive));
    }

    fn choose_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t!("host.portable.import.choose_prompt")),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await
                && let Some(path) = paths.into_iter().next()
            {
                this.update_in(cx, |this, window, cx| {
                    this.path = path;
                    this.load(window, cx);
                })
                .ok();
            }
        })
        .detach();
    }

    fn commit(
        &mut self,
        on_imported: &ArchiveImportHandler,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(archive) = self.archive.clone() else {
            return false;
        };
        let summary = self
            .store
            .update(cx, |store, cx| apply_import(store, &archive, cx));
        on_imported(summary, window, cx);
        true
    }
}

impl Render for ImportDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let mut body = v_flex()
            .id("import-hosts-dialog")
            .w_full()
            .gap_3()
            .child(
                h_flex()
                    .test_support()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_sm()
                            .child(t!("host.portable.import.file")),
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
                        Button::new("import-choose-file")
                            .small()
                            .label(t!("host.portable.import.choose"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_file(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("host.portable.import.rules")),
            );

        if self.loading {
            return body.child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t!("host.portable.import.loading")),
            );
        }
        if let Some(failure) = &self.failure {
            return body.child(
                div()
                    .id("import-error")
                    .test_support()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(failure.clone()),
            );
        }
        if self.sealed.is_some() {
            return body
                .child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(t!("host.portable.import.locked")),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            div()
                                .w(rems(18.))
                                .child(Input::new(&self.password).id("import-password").small()),
                        )
                        .child(
                            Button::new("import-unlock")
                                .small()
                                .label(t!("host.portable.import.unlock"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.unlock(window, cx);
                                })),
                        ),
                );
        }
        let Some(preview) = &self.preview else {
            return body;
        };
        body = body.child(div().text_sm().child(t!(
            "host.portable.import.summary",
            hosts = preview.hosts,
            groups = preview.groups
        )));
        if preview.groups_reused > 0 {
            body = body.child(div().text_sm().text_color(muted).child(t!(
                "host.portable.import.groups_merged",
                count = preview.groups_reused
            )));
        }
        if !preview.conflicts.is_empty() {
            let names: Vec<String> = preview
                .conflicts
                .iter()
                .map(|name| name.to_string())
                .collect();
            body = body.child(
                div()
                    .id("import-conflicts")
                    .test_support()
                    .text_sm()
                    .text_color(theme.warning)
                    .child(t!(
                        "host.portable.import.conflicts",
                        count = preview.conflicts.len(),
                        names = join_list(&names)
                    )),
            );
        }
        if !preview.needs_password.is_empty() {
            let names: Vec<String> = preview
                .needs_password
                .iter()
                .map(|name| name.to_string())
                .collect();
            body = body.child(
                div()
                    .id("import-needs-password")
                    .test_support()
                    .text_sm()
                    .text_color(muted)
                    .child(t!(
                        "host.portable.import.needs_password",
                        count = preview.needs_password.len(),
                        names = join_list(&names)
                    )),
            );
        }
        if !preview.needs_key.is_empty() {
            let names: Vec<String> = preview
                .needs_key
                .iter()
                .map(|name| name.to_string())
                .collect();
            body = body.child(
                div()
                    .id("import-needs-key")
                    .test_support()
                    .text_sm()
                    .text_color(muted)
                    .child(t!(
                        "host.portable.import.needs_key",
                        count = preview.needs_key.len(),
                        names = join_list(&names)
                    )),
            );
        }
        body
    }
}
