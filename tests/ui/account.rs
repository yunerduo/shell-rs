//! 账号: signed out is the default, and it blocks nothing. The account
//! corner and 设置 › 账号 both say so; neither asks for a login on start-up.
//! No server or keychain is touched: the workspace's account has no path and
//! no service.

use crate::support::*;
use shellrs::app::OpenAccountSettings;

#[gpui_kit::test]
fn signed_out_blocks_nothing_and_offers_a_way_in(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // The host list is the first screen; there is no login wall.
        assert!(window.find("workspace-sidebar").visible());
        assert!(window.find("host-search").visible());
        window.click("account-button", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Signed out is a state, not an error: no spinner, just the two
        // ways in.
        assert!(window.find("account-signed-out").visible());
        assert!(window.find("account-sign-in").visible());
        assert!(window.find("account-register").visible());
    })
    .unwrap();
}

#[gpui_kit::test]
fn account_settings_explains_the_local_features_need_no_account(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(OpenAccountSettings), cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("settings-tab").label(), Some("设置"));
        assert!(window.find("account-settings-sign-in").visible());
        assert!(window.find("account-settings-register").visible());
    })
    .unwrap();
}
