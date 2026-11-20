//! The workspace's share of updating: the commands, what a restart would
//! interrupt, and the word after an update.

use std::rc::Rc;

use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::*;

use crate::app::{
    CheckForUpdates, DownloadUpdate, OpenChangelog, OpenDownloadPage, OpenWebsite, RestartToUpdate,
    ShowUpdate,
};
use crate::host::ConnectionState;
use crate::i18n::t;
use crate::terminal::TerminalLifecycle;
use crate::update::build_info::{WEBSITE, changelog_page};
use crate::update::{Phase, RestartImpact, UpdateFailure, UpdaterEvent, open_update_dialog};

use super::workspace_view::Workspace;

impl Workspace {
    pub(super) fn on_check_for_updates(
        &mut self,
        _: &CheckForUpdates,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.updater.update(cx, |updater, cx| updater.check(cx));
    }

    pub(super) fn on_download_update(
        &mut self,
        _: &DownloadUpdate,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.updater.update(cx, |updater, cx| updater.download(cx));
    }

    pub(super) fn on_show_update(
        &mut self,
        _: &ShowUpdate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.updater.read(cx).snapshot().offered_version().is_none() {
            return;
        }
        self.open_update(window, cx);
    }

    fn open_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = cx.weak_entity();
        let impact = Rc::new(move |cx: &App| {
            workspace
                .read_with(cx, |workspace, cx| workspace.restart_impact(cx))
                .unwrap_or_default()
        });
        open_update_dialog(
            self.updater.clone(),
            impact,
            self.focus_handle.clone(),
            window,
            cx,
        );
    }

    /// A version just found opens the dialog once, saying what changed and
    /// leaving the download to the person; 强制更新 opens it too, and cannot
    /// be dismissed. Once opened, the same version is never announced again
    /// — closing the dialog is an answer.
    fn announce_found_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.updater.read(cx).snapshot();
        if !matches!(snapshot.phase, Phase::Available) && !snapshot.blocking() {
            return;
        }
        let Some(version) = snapshot.offered_version() else {
            return;
        };
        if self.announced_update.as_deref() == Some(version.as_str()) {
            return;
        }
        self.announced_update = Some(version);
        self.open_update(window, cx);
    }

    pub(super) fn on_restart_to_update(
        &mut self,
        _: &RestartToUpdate,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.updater.update(cx, |updater, cx| updater.restart(cx));
    }

    pub(super) fn on_open_download_page(
        &mut self,
        _: &OpenDownloadPage,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let page = self.updater.read(cx).download_page();
        cx.open_url(&page);
    }

    /// The whole changelog, not one version's section.
    pub(super) fn on_open_changelog(
        &mut self,
        _: &OpenChangelog,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.open_url(changelog_page(crate::i18n::locale()));
    }

    pub(super) fn on_open_website(
        &mut self,
        _: &OpenWebsite,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.open_url(WEBSITE);
    }

    /// The title bar shows the update button only in some phases; the
    /// version after an update is said once the new one runs.
    pub(super) fn on_updater_event(
        &mut self,
        event: &UpdaterEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            UpdaterEvent::Changed => {
                cx.notify();
                self.announce_found_update(window, cx);
            }
            UpdaterEvent::Updated {
                to,
                completed: true,
                ..
            } => window.push_notification(
                Notification::success(t!("workspace.update.done", version = to))
                    .title(t!("workspace.update.done_title")),
                cx,
            ),
            UpdaterEvent::Updated {
                from,
                to,
                failure: Some(UpdateFailure::InstallerFailed { exit_code }),
                ..
            } => window.push_notification(
                Notification::error(t!(
                    "workspace.update.installer_failed",
                    code = exit_code,
                    to = to,
                    from = from
                ))
                .title(t!("workspace.update.failed_title")),
                cx,
            ),
            UpdaterEvent::Updated {
                from,
                to,
                failure: Some(UpdateFailure::InstalledElsewhere),
                ..
            } => window.push_notification(
                Notification::error(t!(
                    "workspace.update.installed_elsewhere",
                    to = to,
                    from = from
                ))
                .title(t!("workspace.update.failed_title")),
                cx,
            ),
            UpdaterEvent::Updated { from, to, .. } => window.push_notification(
                Notification::error(t!("workspace.update.failed", to = to, from = from))
                    .title(t!("workspace.update.failed_title")),
                cx,
            ),
        }
    }

    /// 更新渠道, from the settings.
    pub(super) fn sync_updater(&self, cx: &mut App) {
        let settings = self.settings().read(cx).settings().update.clone();
        self.updater.update(cx, |updater, cx| {
            updater.set_channel(settings.channel, cx);
        });
    }

    /// What restarting now would close or stop.
    fn restart_impact(&self, cx: &App) -> RestartImpact {
        RestartImpact {
            remote_terminals: self
                .terminals
                .values()
                .filter(|panel| panel.read(cx).lifecycle(cx) == TerminalLifecycle::Running)
                .count(),
            sftp_tabs: self
                .explorers
                .values()
                .filter(|panel| {
                    let panel = panel.read(cx);
                    panel.connection_state() == ConnectionState::Connected && !panel.is_sidebar()
                })
                .count(),
            local_terminals: self
                .local_terminals
                .values()
                .filter(|panel| {
                    panel
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .lifecycle(cx)
                        .accepts_input()
                })
                .count(),
            transfers: self
                .explorers
                .values()
                .filter(|panel| panel.read(cx).is_transferring())
                .count(),
            forwards: self.forwards.read(cx).active_count(),
            unsaved_files: self.all_unsaved_files(cx).len(),
        }
    }
}
