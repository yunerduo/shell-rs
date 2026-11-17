//! 账号: signed out is the default, and it blocks nothing. The account
//! corner and 设置 › 账号 both say so; neither asks for a login on start-up.
//! No server or keychain is touched: the workspace's account has no path and
//! no service.

use crate::support::*;
use shellrs::app::{OpenAccountSettings, RegisterAccount};

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

#[gpui_kit::test]
fn the_dialog_switches_between_signing_in_and_registering(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(RegisterAccount), cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // One switch, and the fields registration adds are there.
        assert!(window.within("account-mode").find(1usize).visible());
        assert!(window.find("account-email").visible());
        assert!(window.find("account-phone").visible());
        assert_eq!(window.find("account-commit").label(), Some("创建账号"));
        // A number that is not a mainland mobile is refused before
        // anything is sent.
        window.click("account-username", cx);
        window.input("alice", cx);
        window.click("account-password", cx);
        window.input("hunter2hunter2", cx);
        window.click("account-phone", cx);
        window.input("12345", cx);
        window.click("account-commit", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("account-error").label(),
            Some("手机号应为 11 位，以 1 开头")
        );
        window.within("account-mode").click(0usize, cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Signing in has neither the e-mail nor the phone, and the error
        // belonged to the registration.
        assert!(window.try_find("account-email").is_none());
        assert!(window.try_find("account-phone").is_none());
        assert!(window.try_find("account-error").is_none());
        assert_eq!(window.find("account-commit").label(), Some("登录"));
        window.within("account-mode").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // What was typed is still there, so switching costs nothing.
        assert_eq!(window.find("account-phone").value(), Some("12345"));
        assert_eq!(window.find("account-username").value(), Some("alice"));
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_forgot_password_flow_has_a_way_back(cx: &mut TestAppContext) {
    let (handle, _) = open_workspace(cx);

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("account-button", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("account-sign-in", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Signing in offers 忘记密码; it starts on the e-mail step.
        assert!(window.find("account-forgot").visible());
        window.click("account-forgot", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find("account-email").visible());
        assert!(window.try_find("account-password").is_none());
        assert!(window.try_find("account-username").is_none());
        assert_eq!(window.find("account-commit").label(), Some("发送验证码"));
        // Nothing is sent with an empty address.
        window.click("account-commit", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("account-error").label(), Some("请输入邮箱"));
        window.click("account-reset-back", cx);
    })
    .unwrap();
    cx.run_until_parked();

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Back where it started, with nothing from the reset left over.
        assert!(window.find("account-username").visible());
        assert!(window.find("account-forgot").visible());
        assert!(window.try_find("account-error").is_none());
        assert_eq!(window.find("account-commit").label(), Some("登录"));
    })
    .unwrap();
}
