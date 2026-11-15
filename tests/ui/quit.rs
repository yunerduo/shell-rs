//! Quitting with connections still in use: the question comes first, and
//! nothing is asked when there is nothing to stop. The terminals are the
//! fixture's, over fake transports: no server or keychain is touched.

use std::time::Duration;

use shellrs::app::Quit;
use shellrs::host::HostStore;
use shellrs::terminal::{RemoteTerminalId, TerminalLifecycle};

use crate::support::*;

const WEB: RemoteTerminalId = RemoteTerminalId(INITIAL_WEB_TERMINAL);

#[gpui_kit::test]
async fn quitting_with_an_active_connection_asks_first(cx: &mut TestAppContext) {
    let (handle, workspace) = open_workspace(cx);
    let view = workspace.clone();
    cx.wait_for(handle.into(), Duration::from_secs(2), move |window, cx| {
        window.render_frame(cx);
        remote_lifecycle(&view, WEB, cx) == TerminalLifecycle::Running
    })
    .await;

    cx.update(|cx| cx.dispatch_action(&Quit));
    wait_for(cx, handle, "ok").await;

    // 取消 keeps the app and its connection; only 仍然退出 quits.
    in_frame(cx, handle, |window, cx| window.click("cancel", cx));
    let lifecycle = cx.update(|cx| remote_lifecycle(&workspace, WEB, cx));
    assert_eq!(lifecycle, TerminalLifecycle::Running);
}

#[gpui_kit::test]
async fn quitting_without_active_connections_does_not_ask(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace_with_store(cx, HostStore::empty());
    cx.run_until_parked();

    cx.update(|cx| cx.dispatch_action(&Quit));
    cx.run_until_parked();

    in_frame(cx, handle, |window, _| {
        assert!(window.try_find("ok").is_none())
    });
}

async fn wait_for(cx: &mut TestAppContext, handle: WindowHandle<Root>, id: &'static str) {
    cx.wait_for(handle.into(), Duration::from_secs(2), move |window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .await;
}
