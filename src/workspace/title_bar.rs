use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::{GlobalState, TextSelection};
use gpui_kit::component::{
    ActiveTheme as _, IconName, Selectable as _, Sizable as _, TITLE_BAR_HEIGHT, TitleBar,
    avatar::Avatar,
    button::{Button, ButtonGroup, ButtonVariants as _},
    h_flex,
    popover::Popover,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::account::{AccountSnapshot, account_popover};
use crate::app::{
    CatalogIcon, NewLocalTerminal, NewTemporaryConnection, OpenSettings, QuickConnect,
    ShowCredentials, ShowForwards, ShowHosts, ShowUpdate, ToggleHostPanel, ToggleTheme,
};
use crate::i18n::t;
use crate::shared::tinted;
use crate::update::UpdateBadge;

use super::sidebar::SidebarMode;

/// The custom title bar: the app name and the sidebar's switch on the left,
/// window-level commands on the right. Every button dispatches an action on
/// `target` (the workspace's focus handle) so the workspace handles it even
/// when nothing is focused.
///
/// `sidebar` is what the left dock is showing, or `None` while it is hidden;
/// `forwards` is how many port forwards are running; `update` is the
/// newer ShellRS waiting to be installed, when there is one; `account` is the
/// account's plain snapshot, and `width` is the window's, which decides how
/// much of the bar is dropped as it narrows.
///
/// `switch_end` learns where the list switch ends, and `view` is redrawn
/// when that moves.
#[allow(clippy::too_many_arguments)]
pub fn render_title_bar(
    sidebar: Option<SidebarMode>,
    forwards: usize,
    update: Option<UpdateBadge>,
    account: &AccountSnapshot,
    width: Pixels,
    switch_end: &SwitchEnd,
    view: EntityId,
    target: &FocusHandle,
    cx: &App,
) -> impl IntoElement {
    // Narrowing costs the app name first, then the account's name, and
    // finally the account's mailbox. The right-hand buttons never wrap.
    let compact = width < px(700.);
    let show_account_name = width >= px(880.);
    let show_account_email = width >= px(1120.);
    let dark = cx.theme().is_dark();
    let (mode, quick, temporary, new_local, theme, show_update, settings) = (
        target.clone(),
        target.clone(),
        target.clone(),
        target.clone(),
        target.clone(),
        target.clone(),
        target.clone(),
    );
    let account = account.clone();
    let account_target = target.clone();
    let account_theme = cx.theme().clone();
    // The nickname, else the username, with the mailbox when there is room.
    // A narrow window keeps only the picture: the bar never grows for it.
    let account_label: Option<SharedString> = if !account.signed_in || !show_account_name {
        None
    } else if show_account_email && !account.email.is_empty() {
        Some(t!(
            "account.button.label_with_email",
            name = account.display_name.clone(),
            email = account.email.clone()
        ))
    } else {
        Some(account.display_name.clone().into())
    };
    let account_trigger = if account.signed_in {
        let picture = account
            .avatar
            .clone()
            .map(|path| Avatar::new().src(path).small())
            .unwrap_or_else(|| Avatar::new().name(account.initial.clone()).small());
        let trigger = Button::new("account-button").ghost().child(picture);
        match account_label {
            Some(label) => trigger.accessibility_label(label.clone()).child(
                div()
                    .min_w_0()
                    .max_w(px(220.))
                    .truncate()
                    .text_sm()
                    .child(label),
            ),
            None => trigger,
        }
    } else {
        Button::new("account-button")
            .ghost()
            .icon(CatalogIcon::CircleUser)
    };
    // The title bar drags the window on mouse down; keep clicks on its
    // controls from starting a drag.
    let no_drag = |_: &MouseDownEvent, _: &mut Window, cx: &mut App| cx.stop_propagation();
    // Dragging the window takes the pointer down into the platform's own
    // move loop, which the app hears no mouse up from. A text selection that
    // press started would therefore never end, and would go on growing over
    // whatever text the pointer crossed next — the assistant's, say.
    div()
        .flex_shrink_0()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            GlobalState::suppress_text_selection(cx);
        })
        .child(
            TitleBar::new()
                .overflow_x_hidden()
                .child(
                    h_flex()
                        .min_w_0()
                        .flex_1()
                        .gap_3()
                        .when(!compact, |row| {
                            row.child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(crate::app::WINDOW_TITLE),
                            )
                        })
                        .child(
                            // Which list the sidebar shows. The choice stays marked
                            // while that list is up, and a hidden sidebar marks none.
                            div()
                                .relative()
                                .on_mouse_down(MouseButton::Left, no_drag)
                                .child(switch_end.measure(view))
                                .child(
                                    ButtonGroup::new("sidebar-mode")
                                        .ghost()
                                        .small()
                                        .child(
                                            Button::new("show-hosts")
                                                .icon(CatalogIcon::Server)
                                                .tooltip(t!("workspace.title_bar.hosts"))
                                                .accessibility_label(t!(
                                                    "workspace.title_bar.hosts"
                                                ))
                                                .selected(sidebar == Some(SidebarMode::Hosts)),
                                        )
                                        .child(
                                            Button::new("show-forwards")
                                                .icon(CatalogIcon::ArrowLeftRight)
                                                .tooltip(t!("workspace.title_bar.forwards"))
                                                .selected(sidebar == Some(SidebarMode::Forwards))
                                                // Forwards run whether or not their list
                                                // is showing, so the count is said here.
                                                .map(|button| match forwards {
                                                    0 => button.accessibility_label(t!(
                                                        "workspace.title_bar.forwards"
                                                    )),
                                                    running => button
                                                        .label(running.to_string())
                                                        .accessibility_label(t!(
                                                            "workspace.title_bar.forwards_running",
                                                            count = running
                                                        )),
                                                }),
                                        )
                                        .child(
                                            Button::new("show-credentials")
                                                .icon(CatalogIcon::KeyRound)
                                                .tooltip(t!("workspace.title_bar.credentials"))
                                                .accessibility_label(t!(
                                                    "workspace.title_bar.credentials"
                                                ))
                                                .selected(
                                                    sidebar == Some(SidebarMode::Credentials),
                                                ),
                                        )
                                        .on_click(move |picked: &Vec<usize>, window, cx| {
                                            match picked.first() {
                                                Some(0) => {
                                                    mode.dispatch_action(&ShowHosts, window, cx)
                                                }
                                                Some(1) => {
                                                    mode.dispatch_action(&ShowForwards, window, cx)
                                                }
                                                Some(2) => mode.dispatch_action(
                                                    &ShowCredentials,
                                                    window,
                                                    cx,
                                                ),
                                                _ => {}
                                            }
                                        }),
                                ),
                        ),
                )
                .child(
                    h_flex()
                        .flex_shrink_0()
                        .justify_end()
                        .px_2()
                        .gap_1()
                        .on_mouse_down(MouseButton::Left, no_drag)
                        .child(
                            Button::new("title-quick-connect")
                                .ghost()
                                .icon(IconName::Search)
                                .tooltip(t!("workspace.title_bar.quick_connect"))
                                .accessibility_label(t!("workspace.title_bar.quick_connect"))
                                .on_click(move |_, window, cx| {
                                    quick.dispatch_action(&QuickConnect, window, cx)
                                }),
                        )
                        .child(
                            // Saved hosts are made in the host list; up here is the
                            // quick way in, to a host that is not saved.
                            Button::new("temporary-connection")
                                .ghost()
                                .icon(CatalogIcon::Zap)
                                .accessibility_label(t!("workspace.title_bar.temporary_connection"))
                                .tooltip(t!("workspace.title_bar.temporary_connection_tooltip"))
                                .on_click(move |_, window, cx| {
                                    temporary.dispatch_action(&NewTemporaryConnection, window, cx)
                                }),
                        )
                        .child(
                            Button::new("new-local-terminal")
                                .ghost()
                                .icon(CatalogIcon::Terminal)
                                .accessibility_label(t!("workspace.title_bar.local_terminal"))
                                .tooltip_with_action(
                                    t!("workspace.title_bar.new_local_terminal"),
                                    &NewLocalTerminal,
                                    None,
                                )
                                .on_click(move |_, window, cx| {
                                    new_local.dispatch_action(&NewLocalTerminal, window, cx)
                                }),
                        )
                        .children(update.map(|update| {
                            // The one coloured button up here, in the colour of
                            // what it says: the only sign that an update waits.
                            Button::new("update-available")
                                .custom(tinted(
                                    if update.trouble {
                                        cx.theme().warning
                                    } else {
                                        cx.theme().success
                                    },
                                    cx,
                                ))
                                .icon(CatalogIcon::CircleArrowUp)
                                .tooltip(update.label.clone())
                                .accessibility_label(update.label)
                                .on_click(move |_, window, cx| {
                                    show_update.dispatch_action(&ShowUpdate, window, cx)
                                })
                        }))
                        .child(
                            // 设置 and the optional account live in the corner,
                            // where the window-level commands are.
                            Button::new("open-settings")
                                .ghost()
                                .icon(CatalogIcon::Settings)
                                .label(t!("workspace.sidebar.settings"))
                                .tooltip_with_action(
                                    t!("workspace.sidebar.open_settings"),
                                    &OpenSettings,
                                    None,
                                )
                                .on_click(move |_, window, cx| {
                                    settings.dispatch_action(&OpenSettings, window, cx)
                                }),
                        )
                        .child(
                            Popover::new("account-button-popover")
                                .anchor(Anchor::TopRight)
                                .trigger(
                                    account_trigger
                                        .tooltip(t!("account.button.tooltip"))
                                        .accessibility_label(t!("account.button.tooltip")),
                                )
                                .content(move |_, _, _| {
                                    account_popover(
                                        account.clone(),
                                        account_target.clone(),
                                        account_theme.clone(),
                                    )
                                }),
                        )
                        .child(
                            Button::new("theme-toggle")
                                .ghost()
                                .icon(if dark { IconName::Sun } else { IconName::Moon })
                                .tooltip(if dark {
                                    t!("workspace.title_bar.to_light")
                                } else {
                                    t!("workspace.title_bar.to_dark")
                                })
                                .on_click(move |_, window, cx| {
                                    theme.dispatch_action(&ToggleTheme, window, cx)
                                }),
                        ),
                ),
        )
        .child(render_selection_frame_guard())
}

/// Keeps the window's own frame — everything that is not content — out of the
/// window-wide text selection.
///
/// The resize border lies outside the content, so a press there starts a
/// platform resize loop, and the app hears no mouse up from it: a selection
/// seeded by that press could never end, and would keep growing over whatever
/// text the pointer crossed afterwards. Should a release be swallowed anyway,
/// the first move without a button held ends the gesture before it grows.
///
/// The canvas paints nothing; it is here for its paint step, which runs after
/// the selection layer's, so these listeners see the press before the
/// selection's do.
fn render_selection_frame_guard() -> impl IntoElement {
    canvas(
        |_, _, _| {},
        |_, _, window, _cx| {
            window.on_mouse_event(|event: &MouseDownEvent, phase, window, cx| {
                if !phase.bubble() || event.button != MouseButton::Left {
                    return;
                }
                let size = window.viewport_size();
                let position = event.position;
                // Frame presses come in negative, or past the content's size.
                if position.x < px(0.)
                    || position.y < px(0.)
                    || position.x >= size.width
                    || position.y >= size.height
                {
                    GlobalState::suppress_text_selection(cx);
                }
            });
            window.on_mouse_event(|event: &MouseMoveEvent, phase, window, cx| {
                if phase.bubble() && event.pressed_button.is_none() {
                    TextSelection::end(window, cx);
                }
            });
        },
    )
    .absolute()
    .size_full()
}

/// Where the title bar's list switch ends, as the last frame drew it: past
/// a narrow sidebar's edge on macOS, after the window buttons.
#[derive(Clone, Default)]
pub(super) struct SwitchEnd(Rc<Cell<Pixels>>);

impl SwitchEnd {
    /// Fills the switch's box to learn its right edge as it is drawn, and
    /// redraws `view` when that moves.
    fn measure(&self, view: EntityId) -> impl IntoElement + use<> {
        let end = self.0.clone();
        canvas(
            move |bounds, window, _| {
                if end.get() != bounds.right() {
                    end.set(bounds.right());
                    window.on_next_frame(move |_, cx| cx.notify(view));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

/// Position against the workspace, whose left edge is also the dock's,
/// rather than against TitleBar's platform-dependent traffic-light inset.
/// It sits just past the list switch, so「凭据」and 显示/隐藏侧栏 read as one
/// group and stay together however narrow the window gets.
pub(super) fn render_sidebar_toggle(
    open: bool,
    switch_end: &SwitchEnd,
    target: &FocusHandle,
) -> impl IntoElement {
    let target = target.clone();
    let label = if open {
        t!("workspace.title_bar.hide_sidebar")
    } else {
        t!("workspace.title_bar.show_sidebar")
    };
    h_flex()
        .absolute()
        .top_0()
        .left(switch_end.0.get())
        .h(TITLE_BAR_HEIGHT)
        .pl_1()
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .child(
            Button::new("toggle-hosts")
                .ghost()
                .small()
                .icon(if open {
                    IconName::PanelLeft
                } else {
                    IconName::PanelLeftOpen
                })
                .tooltip(label.clone())
                .accessibility_label(label)
                .on_click(move |_, window, cx| {
                    target.dispatch_action(&ToggleHostPanel, window, cx)
                }),
        )
}
