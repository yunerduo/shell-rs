//! 退出前的确认: terminals and uploads still in use, and the system's
//! request to end the session.
//!
//! A quit holds back while something is still running, the same way it holds
//! back for an editor with changes not saved: one dialog asks about both. On
//! Windows the system asks the window before it ends the session; the window
//! procedure says no (see `app::shutdown`) and the request comes here, where
//! it is put to the user like any other quit.

use std::time::Duration;

use gpui_kit::*;

use crate::i18n::{join_list, t, tn};
use crate::terminal::TerminalLifecycle;

use super::Workspace;

/// How many host names a confirmation lists before it says how many there
/// are in all.
const LISTED: usize = 5;

/// What a quit would stop: terminals still connected, and hosts whose upload
/// is running.
pub(super) struct InUse {
    /// One entry per connected terminal, so two tabs of one host count as
    /// two connections.
    terminals: Vec<String>,
    /// One entry per host with an upload under way.
    uploads: Vec<String>,
}

impl InUse {
    pub(super) fn is_empty(&self) -> bool {
        self.terminals.is_empty() && self.uploads.is_empty()
    }
}

impl Workspace {
    /// The connections a quit would break, remote and local terminals first,
    /// then the uploads running.
    pub(super) fn in_use(&self, cx: &App) -> InUse {
        let mut terminals: Vec<String> = self
            .terminals
            .values()
            .filter(|panel| panel.read(cx).connection_state(cx).is_connected())
            .map(|panel| panel.read(cx).host().name.to_string())
            .collect();
        terminals.extend(
            self.local_terminals
                .values()
                .filter(|panel| {
                    matches!(
                        panel.read(cx).status(cx).lifecycle(),
                        TerminalLifecycle::Running
                    )
                })
                .map(|panel| panel.read(cx).default_title().to_string()),
        );
        terminals.sort();
        let mut uploads: Vec<String> = self
            .explorers
            .values()
            .filter(|panel| panel.read(cx).running_uploads() > 0)
            .map(|panel| panel.read(cx).host().name.to_string())
            .collect();
        uploads.sort();
        InUse { terminals, uploads }
    }
}

/// What a quit with `in_use` still running would stop, as the description of
/// the confirmation, or `None` when nothing is in use.
pub(super) fn describe_in_use(in_use: &InUse) -> Option<String> {
    if in_use.is_empty() {
        return None;
    }
    let mut lines = Vec::new();
    if !in_use.terminals.is_empty() {
        lines.push(
            tn!(
                "workspace.quit.terminals",
                in_use.terminals.len(),
                names = names(&in_use.terminals)
            )
            .to_string(),
        );
    }
    if !in_use.uploads.is_empty() {
        lines.push(
            tn!(
                "workspace.quit.uploads",
                in_use.uploads.len(),
                names = names(&in_use.uploads)
            )
            .to_string(),
        );
    }
    lines.push(t!("workspace.quit.consequence").to_string());
    Some(lines.join(" "))
}

/// `names` without repetitions, in order, at most [`LISTED`] of them: the
/// rest become a count of hosts.
fn names(names: &[String]) -> String {
    let mut unique: Vec<&str> = Vec::new();
    for name in names {
        if !unique.contains(&name.as_str()) {
            unique.push(name);
        }
    }
    if unique.len() <= LISTED {
        return join_list(&unique);
    }
    t!(
        "workspace.quit.more",
        listed = join_list(&unique[..LISTED]),
        count = unique.len()
    )
    .to_string()
}

/// Answer the system's request to end the session the way the close button
/// is answered: ask about what is still running, and quit only once the user
/// agrees.
///
/// Windows only: it is the one platform here whose window procedure is asked
/// before the session ends. The window procedure could only say no, so it
/// leaves the request for this loop, which asks as soon as no other dialog
/// is in the way.
///
/// The loop only reads the flag the window procedure sets: what is in use is
/// worked out while the window draws (`Workspace::render`), so the loop never
/// touches a panel. Touching them from a timer woke tests up mid-action.
#[cfg(windows)]
pub fn watch_system_shutdown(window: &mut Window, cx: &mut Context<Workspace>) {
    use gpui_kit::component::WindowExt as _;

    cx.spawn_in(window, async move |this, cx| {
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            if !crate::app::system_shutdown_requested() {
                continue;
            }
            let watching = this
                .update_in(cx, |this, window, cx| {
                    if !crate::app::system_shutdown_requested() || window.has_active_dialog(cx) {
                        return true;
                    }
                    // The session end was canceled, not postponed: the
                    // shutdown screen's reason is done with once the question
                    // is ours.
                    crate::app::release_shutdown_block(window);
                    if !crate::app::take_system_shutdown_request() {
                        return true;
                    }
                    if !this.ask_before_quit(window, cx) {
                        cx.quit();
                        return false;
                    }
                    true
                })
                .unwrap_or(false);
            if !watching {
                return;
            }
        }
    })
    .detach();
}
