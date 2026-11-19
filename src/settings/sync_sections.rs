//! What of 应用设置 follows the account to another computer, one group at a
//! time.
//!
//! The settings document keeps only the groups the user checked, and a
//! computer that reads it applies only those: an unchecked group stays what
//! this computer had. Two fields never leave: `sync` is where *this*
//! computer syncs (a folder path or an account entry), and
//! `terminal_background` names picture files only this computer has. The
//! choice itself lives in `account.json`, not in the settings.

use std::collections::BTreeSet;

use gpui_kit::SharedString;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::AppSettings;
use crate::i18n::t;

/// One group of 应用设置, as the cloud sync offers them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConfigSection {
    Appearance,
    TerminalFont,
    TerminalInteraction,
    KeywordHighlight,
    Shortcuts,
    Sftp,
    Notifications,
    LocalTerminal,
    Sidebar,
    Window,
    Maintenance,
    Privacy,
    Assistant,
}

impl ConfigSection {
    /// Every group, in the order the settings page lists them.
    pub const ALL: [Self; 13] = [
        Self::Appearance,
        Self::TerminalFont,
        Self::TerminalInteraction,
        Self::KeywordHighlight,
        Self::Shortcuts,
        Self::Sftp,
        Self::Notifications,
        Self::LocalTerminal,
        Self::Sidebar,
        Self::Window,
        Self::Maintenance,
        Self::Privacy,
        Self::Assistant,
    ];

    /// The key the settings document and `account.json` name it by; what
    /// serde writes for it.
    pub fn key(self) -> &'static str {
        match self {
            Self::Appearance => "appearance",
            Self::TerminalFont => "terminal-font",
            Self::TerminalInteraction => "terminal-interaction",
            Self::KeywordHighlight => "keyword-highlight",
            Self::Shortcuts => "shortcuts",
            Self::Sftp => "sftp",
            Self::Notifications => "notifications",
            Self::LocalTerminal => "local-terminal",
            Self::Sidebar => "sidebar",
            Self::Window => "window",
            Self::Maintenance => "maintenance",
            Self::Privacy => "privacy",
            Self::Assistant => "assistant",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|section| section.key() == key)
    }

    /// Whether a fresh account syncs this group: what follows the user from
    /// computer to computer, as against what describes one computer's
    /// screen or this computer's security.
    pub fn default_on(self) -> bool {
        match self {
            Self::Appearance
            | Self::TerminalFont
            | Self::TerminalInteraction
            | Self::KeywordHighlight
            | Self::Shortcuts
            | Self::Sftp
            | Self::Notifications
            | Self::LocalTerminal => true,
            Self::Sidebar | Self::Window | Self::Maintenance | Self::Privacy | Self::Assistant => {
                false
            }
        }
    }

    pub fn default_selection() -> BTreeSet<Self> {
        Self::ALL
            .into_iter()
            .filter(|section| section.default_on())
            .collect()
    }

    /// The `AppSettings` fields this group carries, by their JSON names.
    pub fn fields(self) -> &'static [&'static str] {
        match self {
            Self::Appearance => &["language", "appearance", "interface", "terminal_theme"],
            Self::TerminalFont => &["terminal_font"],
            Self::TerminalInteraction => &["terminal_interaction"],
            Self::KeywordHighlight => &["terminal_highlight"],
            Self::Shortcuts => &["shortcuts"],
            Self::Sftp => &[
                "transfers",
                "file_icon_style",
                "file_sidebar",
                "show_hidden",
                "file_size_format",
            ],
            Self::Notifications => &["notifications"],
            Self::LocalTerminal => &["local_terminal", "external_cli"],
            Self::Sidebar => &[
                "sidebar_tools",
                "sidebar_collapsed",
                "local_sidebar_tool",
                "compose_pane",
                "host_details",
            ],
            Self::Window => &["window", "sidebar_widths"],
            Self::Maintenance => &["app_lock_idle_minutes", "app_lock_allow_cli", "update"],
            Self::Privacy => &["analytics"],
            Self::Assistant => &["assistant"],
        }
    }

    /// What the switch calls it, and what it says the group carries.
    pub fn label(self) -> SharedString {
        match self {
            Self::Appearance => t!("settings.account.sync_section.appearance"),
            Self::TerminalFont => t!("settings.account.sync_section.terminal_font"),
            Self::TerminalInteraction => {
                t!("settings.account.sync_section.terminal_interaction")
            }
            Self::KeywordHighlight => t!("settings.account.sync_section.keyword_highlight"),
            Self::Shortcuts => t!("settings.account.sync_section.shortcuts"),
            Self::Sftp => t!("settings.account.sync_section.sftp"),
            Self::Notifications => t!("settings.account.sync_section.notifications"),
            Self::LocalTerminal => t!("settings.account.sync_section.local_terminal"),
            Self::Sidebar => t!("settings.account.sync_section.sidebar"),
            Self::Window => t!("settings.account.sync_section.window"),
            Self::Maintenance => t!("settings.account.sync_section.maintenance"),
            Self::Privacy => t!("settings.account.sync_section.privacy"),
            Self::Assistant => t!("settings.account.sync_section.assistant"),
        }
    }

    pub fn description(self) -> SharedString {
        match self {
            Self::Appearance => t!("settings.account.sync_section.appearance_description"),
            Self::TerminalFont => t!("settings.account.sync_section.terminal_font_description"),
            Self::TerminalInteraction => {
                t!("settings.account.sync_section.terminal_interaction_description")
            }
            Self::KeywordHighlight => {
                t!("settings.account.sync_section.keyword_highlight_description")
            }
            Self::Shortcuts => t!("settings.account.sync_section.shortcuts_description"),
            Self::Sftp => t!("settings.account.sync_section.sftp_description"),
            Self::Notifications => {
                t!("settings.account.sync_section.notifications_description")
            }
            Self::LocalTerminal => {
                t!("settings.account.sync_section.local_terminal_description")
            }
            Self::Sidebar => t!("settings.account.sync_section.sidebar_description"),
            Self::Window => t!("settings.account.sync_section.window_description"),
            Self::Maintenance => t!("settings.account.sync_section.maintenance_description"),
            Self::Privacy => t!("settings.account.sync_section.privacy_description"),
            Self::Assistant => t!("settings.account.sync_section.assistant_description"),
        }
    }

    /// Take this group's fields from `from`, leaving every other field of
    /// `to` as it was.
    pub fn apply(self, from: &AppSettings, to: &mut AppSettings) {
        let (Ok(mut target), Ok(source)) = (serde_json::to_value(&*to), serde_json::to_value(from))
        else {
            return;
        };
        let (Value::Object(target), Value::Object(source)) = (&mut target, source) else {
            return;
        };
        for field in self.fields() {
            if let Some(value) = source.get(*field) {
                target.insert((*field).to_owned(), value.clone());
            }
        }
        if let Ok(merged) = serde_json::from_value(Value::Object(target.clone())) {
            *to = merged;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applying_one_group_leaves_the_others_alone() {
        let mut local = AppSettings::default();
        local.interface.font_size = 13.0;
        local.terminal_font.size = 12.0;

        let mut remote = AppSettings::default();
        remote.interface.font_size = 18.0;
        remote.terminal_font.size = 20.0;

        ConfigSection::Appearance.apply(&remote, &mut local);

        assert_eq!(local.interface.font_size, 18.0);
        assert_eq!(local.terminal_font.size, 12.0);
    }

    #[test]
    fn the_default_selection_is_what_follows_the_user() {
        let selection = ConfigSection::default_selection();
        assert!(selection.contains(&ConfigSection::Appearance));
        assert!(selection.contains(&ConfigSection::TerminalFont));
        assert!(!selection.contains(&ConfigSection::Window));
        assert!(!selection.contains(&ConfigSection::Privacy));
    }
}
