//! Reading this fork's GitHub releases as a manifest.
//!
//! The fork's releases carry no signed manifest: they are public GitHub
//! releases whose assets GitHub itself summarises with a SHA-256 digest.
//! This module turns one of those release descriptions into the [`Manifest`]
//! the rest of the updater works with, so the download and install path does
//! not know where the release came from. The digest is trusted exactly as
//! far as HTTPS and GitHub are: a package without one is refused rather than
//! installed unverified, and the signed-manifest path in `verify` stays in
//! front of this one for the day the fork serves its own signatures again.

use std::collections::BTreeMap;

use semver::Version;
use serde::Deserialize;

use super::build_info::Channel;
use super::error::UpdateError;
use super::manifest::{Asset, Manifest, SCHEMA};
use super::platform::WINDOWS_PORTABLE;
use crate::i18n::t;

/// One release as the GitHub API describes it.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<Artifact>,
}

/// One file attached to a release.
#[derive(Debug, Deserialize)]
struct Artifact {
    name: String,
    size: u64,
    browser_download_url: String,
    /// `sha256:<hex>`, as GitHub calculates it; absent on older assets.
    #[serde(default)]
    digest: Option<String>,
}

/// The API answers an object for one release and a list for many.
#[derive(Deserialize)]
#[serde(untagged)]
enum Answer {
    One(Release),
    Many(Vec<Release>),
}

/// The package each platform updates with, by the name GitHub's workflow
/// gives it.
const PACKAGES: [(&str, &str); 5] = [
    ("-macos-aarch64.app.zip", "macos-aarch64"),
    ("-macos-x86_64.app.zip", "macos-x86_64"),
    ("-windows-x86_64-portable.zip", WINDOWS_PORTABLE),
    ("-windows-x86_64-setup.exe", "windows-x86_64"),
    ("-linux-x86_64.AppImage", "linux-x86_64"),
];

/// What a person downloads to install by hand.
const INSTALLERS: [(&str, &str); 5] = [
    ("-macos-aarch64.dmg", "macos-aarch64"),
    ("-macos-x86_64.dmg", "macos-x86_64"),
    ("-windows-x86_64-setup.exe", "windows-x86_64"),
    ("-windows-x86_64-portable.zip", WINDOWS_PORTABLE),
    ("-linux-x86_64.AppImage", "linux-x86_64"),
];

/// Read the API's answer as the newest release of `channel`, with the
/// package `wanted` updates from. `NotFound` means the list holds no
/// release for this channel yet.
pub fn manifest(bytes: &[u8], channel: Channel, wanted: &str) -> Result<Manifest, UpdateError> {
    let answer: Answer = serde_json::from_slice(bytes)
        .map_err(|error| UpdateError::BadManifest(error.to_string()))?;
    let release = match answer {
        Answer::One(release) => Some(release),
        Answer::Many(releases) => newest_release(releases, channel),
    }
    .ok_or(UpdateError::NotFound)?;
    if release.draft || (channel == Channel::Stable && release.prerelease) {
        return Err(UpdateError::NotFound);
    }
    let version = version_of_tag(&release.tag_name).ok_or_else(|| {
        UpdateError::BadManifest(
            t!("update.github.bad_tag", tag = release.tag_name.clone()).to_string(),
        )
    })?;
    let (assets, installers) = packages(&release, wanted)?;
    Ok(Manifest {
        schema: SCHEMA,
        channel,
        version,
        published_at: release.published_at,
        minimum_version: None,
        rollout: 1.0,
        notes: release.body,
        notes_url: release.html_url.filter(|url| url.starts_with("https://")),
        assets,
        installers,
    })
}

/// The newest of a list, skipping drafts and, on the stable channel, betas.
fn newest_release(releases: Vec<Release>, channel: Channel) -> Option<Release> {
    let mut best: Option<(Version, Release)> = None;
    for release in releases {
        if release.draft || (channel == Channel::Stable && release.prerelease) {
            continue;
        }
        let Some(version) = version_of_tag(&release.tag_name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(known, _)| version > *known) {
            best = Some((version, release));
        }
    }
    best.map(|(_, release)| release)
}

/// A release's packages and installers, by platform key.
type Packages = (BTreeMap<String, Asset>, BTreeMap<String, String>);

/// The packages and installers of a release, with the digest this trust
/// model rests on. An artifact of `wanted` without one, or not over HTTPS,
/// is a release this copy refuses.
fn packages(release: &Release, wanted: &str) -> Result<Packages, UpdateError> {
    let mut assets: BTreeMap<String, Asset> = BTreeMap::new();
    let mut installers: BTreeMap<String, String> = BTreeMap::new();
    for artifact in &release.assets {
        let https = artifact.browser_download_url.starts_with("https://");
        if let Some(key) = key_of(&artifact.name, &PACKAGES) {
            if !https {
                if key == wanted {
                    return Err(UpdateError::Insecure);
                }
            } else if let Some(sha256) = artifact.digest.as_deref().and_then(sha256_digest) {
                assets.insert(
                    key.to_string(),
                    Asset {
                        urls: vec![artifact.browser_download_url.clone()],
                        size: artifact.size,
                        sha256,
                    },
                );
            } else if key == wanted {
                return Err(UpdateError::Unverified(artifact.name.clone()));
            }
        }
        if let Some(key) = key_of(&artifact.name, &INSTALLERS) {
            if https {
                installers.insert(key.to_string(), artifact.browser_download_url.clone());
            } else if key == wanted {
                return Err(UpdateError::Insecure);
            }
        }
    }
    Ok((assets, installers))
}

/// Which platform an artifact belongs to, matching the workflow's names.
fn key_of(name: &str, table: &'static [(&'static str, &'static str)]) -> Option<&'static str> {
    table
        .iter()
        .find(|(suffix, _)| name.ends_with(suffix))
        .map(|(_, key)| *key)
}

/// The hex half of GitHub's `sha256:<hex>` digest, when it is one.
fn sha256_digest(digest: &str) -> Option<String> {
    let hex = digest.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
}

/// `v0.1.19`, `V0.1.19` and `0.1.19` all mean 0.1.19; a tag that is not a
/// version (a rolling one) names no release to compare.
pub fn version_of_tag(tag: &str) -> Option<Version> {
    let tag = tag.trim();
    let tag = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag);
    Version::parse(tag).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn artifact(name: &str, digest: Option<&str>) -> String {
        let digest = digest.map_or("null".to_string(), |digest| format!("\"{digest}\""));
        format!(
            r#"{{"name":"{name}","size":42,"browser_download_url":"https://github.com/yunerduo/shell-rs/releases/download/v0.3.0/{name}","digest":{digest}}}"#
        )
    }

    fn release(tag: &str, extra: &str) -> String {
        format!(
            r####"{{"tag_name":"{tag}","body":"### 新增\n- 一个版本","published_at":"2026-11-01T08:00:00Z","html_url":"https://github.com/yunerduo/shell-rs/releases/tag/{tag}","draft":false,"prerelease":{extra},"assets":[{}]}}"####,
            artifact("ShellAs-0.3.0-macos-aarch64.app.zip", Some(DIGEST))
        )
    }

    fn open(json: &str, channel: Channel, wanted: &str) -> Result<Manifest, UpdateError> {
        manifest(json.as_bytes(), channel, wanted)
    }

    #[test]
    fn a_release_becomes_a_manifest_with_githubs_digest() {
        let manifest = open(
            &release("v0.3.0", "false"),
            Channel::Stable,
            "macos-aarch64",
        )
        .unwrap();
        assert_eq!(manifest.version, Version::new(0, 3, 0));
        assert_eq!(manifest.schema, SCHEMA);
        assert_eq!(
            manifest.published_at.as_deref(),
            Some("2026-11-01T08:00:00Z")
        );
        assert_eq!(manifest.notes, "### 新增\n- 一个版本");
        let asset = &manifest.assets["macos-aarch64"];
        assert_eq!(asset.size, 42);
        assert_eq!(asset.sha256, DIGEST.trim_start_matches("sha256:"));
        assert!(asset.urls[0].ends_with("-macos-aarch64.app.zip"));
        assert!(asset.urls[0].starts_with("https://"));
        assert_eq!(
            manifest.notes_url.as_deref(),
            Some("https://github.com/yunerduo/shell-rs/releases/tag/v0.3.0")
        );
    }

    #[test]
    fn tag_prefixes_do_not_change_order() {
        assert!(version_of_tag("v0.1.19") > version_of_tag("0.1.9"));
        assert_eq!(version_of_tag("V0.1.19"), version_of_tag("0.1.19"));
        assert!(version_of_tag("1.0.0-rc.1").unwrap() < Version::new(1, 0, 0));
        assert_eq!(version_of_tag("nightly"), None);
    }

    #[test]
    fn the_newest_version_wins_whatever_the_tag_order() {
        let list = format!(
            "[{}, {}, {}]",
            release("v0.1.9", "false"),
            release("v0.1.19", "false"),
            release("v0.2.0-beta.1", "true")
        );
        let beta = open(&list, Channel::Beta, "macos-aarch64").unwrap();
        // A beta of 0.2.0 is newer than 0.1.19, however the tags sort as text.
        assert_eq!(beta.version, Version::parse("0.2.0-beta.1").unwrap());
        let stable = open(&list, Channel::Stable, "macos-aarch64").unwrap();
        assert_eq!(stable.version, Version::new(0, 1, 19));
    }

    #[test]
    fn an_empty_or_draft_only_list_is_not_an_update() {
        assert_eq!(
            open("[]", Channel::Stable, "macos-aarch64"),
            Err(UpdateError::NotFound)
        );
        let draft = release("v0.3.0", "false").replace(r#""draft":false"#, r#""draft":true"#);
        assert_eq!(
            open(&format!("[{draft}]"), Channel::Beta, "macos-aarch64"),
            Err(UpdateError::NotFound)
        );
    }

    #[test]
    fn a_package_without_a_digest_is_refused() {
        let json = format!(
            r#"{{"tag_name":"v0.3.0","assets":[{}]}}"#,
            artifact("ShellAs-0.3.0-macos-aarch64.app.zip", None)
        );
        assert_eq!(
            open(&json, Channel::Stable, "macos-aarch64"),
            Err(UpdateError::Unverified(
                "ShellAs-0.3.0-macos-aarch64.app.zip".into()
            ))
        );
        let wrong = format!(
            r#"{{"tag_name":"v0.3.0","assets":[{}]}}"#,
            artifact("ShellAs-0.3.0-macos-aarch64.app.zip", Some("md5:abc"))
        );
        assert!(matches!(
            open(&wrong, Channel::Stable, "macos-aarch64"),
            Err(UpdateError::Unverified(_))
        ));
    }

    #[test]
    fn another_platforms_package_does_not_have_to_be_usable() {
        let json = format!(
            r#"{{"tag_name":"v0.3.0","assets":[{}, {}]}}"#,
            artifact("ShellAs-0.3.0-linux-x86_64.AppImage", None),
            artifact("ShellAs-0.3.0-windows-x86_64-setup.exe", Some(DIGEST))
        );
        let manifest = open(&json, Channel::Stable, "windows-x86_64").unwrap();
        assert!(!manifest.assets.contains_key("linux-x86_64"));
        assert!(manifest.assets.contains_key("windows-x86_64"));
        assert!(manifest.installers["windows-x86_64"].ends_with("-setup.exe"));
    }

    #[test]
    fn a_package_not_served_over_https_is_refused() {
        let json = format!(
            r#"{{"tag_name":"v0.3.0","assets":[{}]}}"#,
            artifact("ShellAs-0.3.0-macos-aarch64.app.zip", Some(DIGEST))
                .replace("https://github.com", "http://github.com")
        );
        assert_eq!(
            open(&json, Channel::Stable, "macos-aarch64"),
            Err(UpdateError::Insecure)
        );
    }

    #[test]
    fn a_tag_that_names_no_version_is_a_bad_manifest() {
        let json = format!(
            r#"{{"tag_name":"nightly","assets":[{}]}}"#,
            artifact("ShellAs-0.3.0-macos-aarch64.app.zip", Some(DIGEST))
        );
        assert!(matches!(
            open(&json, Channel::Stable, "macos-aarch64"),
            Err(UpdateError::BadManifest(_))
        ));
        assert!(matches!(
            open("<html>", Channel::Stable, "macos-aarch64"),
            Err(UpdateError::BadManifest(_))
        ));
    }
}
