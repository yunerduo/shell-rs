//! What this build is, as stamped by the release workflow.
//!
//! Only the release workflow sets `SHELLAS_UPDATE_CHANNEL`, so a `cargo run`
//! build has no channel and never looks for updates: it is not something a
//! published package could replace. A release build starts on its own
//! channel; 设置 › 关于 › 更新渠道 switches it.

use serde::{Deserialize, Serialize};

/// This build's version, from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The commit the release was built from.
pub const COMMIT: Option<&str> = option_env!("SHELLAS_COMMIT");

/// The day the release was built, `YYYY-MM-DD`.
pub const BUILD_DATE: Option<&str> = option_env!("SHELLAS_BUILD_DATE");

/// The Apple team that signs this app: a new bundle must come from it.
pub const APPLE_TEAM_ID: Option<&str> = option_env!("SHELLAS_APPLE_TEAM_ID");

/// This fork's signed update manifests. `{channel}` is replaced with
/// `stable` or `beta`; a development build can override the address with
/// `SHELLAS_UPDATE_URL`.
pub const RELEASES_API: &str = "https://shellas.agree.host/update/v1/{channel}.json";

/// The project page.
pub const WEBSITE: &str = "https://github.com/yunerduo/shell-rs";

/// Where a person downloads the app by hand.
pub const DOWNLOAD_PAGE: &str = "https://github.com/yunerduo/shell-rs/releases";

/// What each version changed: the releases page carries every changelog,
/// in the language it was written in.
pub fn changelog_page(_locale: &str) -> &'static str {
    DOWNLOAD_PAGE
}

/// The minisign public keys a manifest may be signed with: the key in use,
/// and a spare kept offline so the first can be replaced. Rotating means
/// shipping a release that knows the new key before signing with it.
///
/// The release workflow refuses to build a release while this is empty: a
/// build without keys rejects every manifest.
pub const TRUSTED_KEYS: &[&str] = &[
    // In use: the release workflow signs with it (key id 9A7DE51998612035).
    "RWSafeUZmGEgNRjcj/kFGvjlZlIQhZUbPwuZfE9bIndPfFF2xkNFEXnB",
    // Spare, kept offline (key id F3B9F08DE80AF399).
    "RWSZ8wrojfC584RC+h7Frcp/pDW0pd7TaB4pO6bTVVOgSj6mo1KFs0sh",
];

/// Which stream of releases a copy follows. Beta also gets every stable
/// release that is not behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Stable,
    Beta,
}

impl Channel {
    /// The name in a manifest's signature and in the settings file.
    pub fn key(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "stable" => Some(Self::Stable),
            "beta" => Some(Self::Beta),
            _ => None,
        }
    }

    /// The channel this build follows; `None` for a development build.
    pub fn of_this_build() -> Option<Self> {
        option_env!("SHELLAS_UPDATE_CHANNEL").and_then(Self::from_key)
    }

    /// The address of this channel's signed manifest.
    pub fn releases_url(self) -> String {
        RELEASES_API.replace("{channel}", self.key())
    }
}

/// This build's version, parsed.
pub fn version() -> semver::Version {
    semver::Version::parse(VERSION).expect("Cargo.toml holds a semver version")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_channel_asks_shellas_for_its_manifest() {
        assert_eq!(
            Channel::Stable.releases_url(),
            "https://shellas.agree.host/update/v1/stable.json"
        );
        assert_eq!(
            Channel::Beta.releases_url(),
            "https://shellas.agree.host/update/v1/beta.json"
        );
        assert!(Channel::Stable.releases_url().starts_with("https://"));
        assert_eq!(Channel::from_key("nightly"), None);
    }

    #[test]
    fn the_package_version_parses() {
        assert_eq!(version().to_string(), VERSION);
    }
}
