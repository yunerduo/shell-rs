//! The workspace's share of exporting and importing the saved hosts:
//! opening the dialogs, writing the file away from the UI thread, and
//! saying what an import did.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::*;

use crate::app::{ExportHosts, ImportHostsFile};
use crate::host::{
    ExportDone, ExportRequest, ImportSummary, finish_export, join_sentences,
    open_export_hosts_dialog, open_import_hosts_dialog,
};
use crate::i18n::{join_list, t};

use super::workspace_view::Workspace;

impl Workspace {
    pub(super) fn on_export_hosts(
        &mut self,
        _: &ExportHosts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let workspace = cx.entity().downgrade();
        let on_export = Rc::new(
            move |request: ExportRequest, window: &mut Window, cx: &mut App| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.export_hosts(request, window, cx)
                    })
                    .ok();
            },
        );
        let return_focus = self.host_panel.read(cx).focus_handle(cx);
        open_export_hosts_dialog(self.store.clone(), on_export, return_focus, window, cx);
    }

    /// Write the file on the background executor: it reads the keychain.
    /// The workspace, not the dialog, waits, so closing the dialog does not
    /// cancel the write.
    fn export_hosts(
        &mut self,
        request: ExportRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let secrets = self.store.read(cx).secrets();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { finish_export(request, &secrets) })
                .await;
            this.update_in(cx, |_, window, cx| match result {
                Ok(done) => window.push_notification(exported(done), cx),
                Err(error) => window.push_notification(
                    Notification::error(error.message()).title(t!("host.portable.export.failed")),
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn on_import_hosts_file(
        &mut self,
        _: &ImportHostsFile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                this.update_in(cx, |workspace, window, cx| {
                    workspace.open_archive_import(path, window, cx)
                })
                .ok();
            }
        })
        .detach();
    }

    fn open_archive_import(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.entity().downgrade();
        let on_imported = Rc::new(
            move |summary: ImportSummary, window: &mut Window, cx: &mut App| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.imported_archive(summary, window, cx)
                    })
                    .ok();
            },
        );
        let return_focus = self.host_panel.read(cx).focus_handle(cx);
        open_import_hosts_dialog(
            path,
            self.store.clone(),
            on_imported,
            return_focus,
            window,
            cx,
        );
    }

    /// Say what the import added, what it skipped and what still has to be
    /// supplied on this machine.
    fn imported_archive(
        &mut self,
        summary: ImportSummary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = t!("host.portable.import.done_title");
        let mut sentences = vec![t!(
            "host.portable.import.done",
            hosts = summary.hosts_added,
            skipped = summary.hosts_skipped
        )];
        if summary.groups_added > 0 || summary.groups_reused > 0 {
            sentences.push(t!(
                "host.portable.import.done_groups",
                added = summary.groups_added,
                reused = summary.groups_reused
            ));
        }
        if !summary.needs_password.is_empty() {
            let names: Vec<String> = summary
                .needs_password
                .iter()
                .map(|name| name.to_string())
                .collect();
            sentences.push(t!(
                "host.portable.import.done_needs_password",
                names = join_list(&names)
            ));
        }
        if !summary.needs_key.is_empty() {
            let names: Vec<String> = summary
                .needs_key
                .iter()
                .map(|name| name.to_string())
                .collect();
            sentences.push(t!(
                "host.portable.import.done_needs_key",
                names = join_list(&names)
            ));
        }
        let body = join_sentences(sentences).unwrap_or_else(|| title.clone());
        window.push_notification(Notification::success(body).title(title), cx);
    }
}

fn exported(done: ExportDone) -> Notification {
    let title = t!("host.portable.export.done_title");
    let mut sentences = vec![t!(
        "host.portable.export.done",
        hosts = done.hosts,
        groups = done.groups
    )];
    if done.skipped > 0 {
        sentences.push(t!(
            "host.portable.export.done_skipped",
            count = done.skipped
        ));
    }
    let body = join_sentences(sentences).unwrap_or_else(|| title.clone());
    Notification::success(body).title(title)
}
