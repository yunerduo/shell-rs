//! The mailbox avatar service Gravatar. A mailbox is addressed by the MD5
//! of the trimmed, lowercased address; the picture itself is public. The
//! result is cached beside the account so a restart shows it without a
//! request, and a miss or a failure just falls back to the initial.
//!
//! Nothing here runs on the UI thread: `fetch` blocks on the network.

use std::io::Cursor;
use std::path::PathBuf;
use std::time::Duration;

use image::{DynamicImage, ImageFormat};
use md5::{Digest as _, Md5};

use crate::app::gravatar_dir;

/// How long a missing picture is allowed to hold up the account.
const TIMEOUT: Duration = Duration::from_secs(3);

/// The service's picture for `email`, or `None` for an empty address.
fn url(email: &str) -> Option<String> {
    let hash = hash(email)?;
    Some(format!(
        "https://www.gravatar.com/avatar/{hash}?s=256&d=404&r=g"
    ))
}

/// Gravatar's mailbox key: the MD5 of the trimmed, lowercased address.
fn hash(email: &str) -> Option<String> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return None;
    }
    let digest = Md5::digest(email.as_bytes());
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Where the picture for `email` is cached, whether or not it is there yet.
fn cached_path(email: &str) -> Option<PathBuf> {
    let hash = hash(email)?;
    Some(gravatar_dir().join(format!("gravatar-{hash}.png")))
}

/// The cached picture for `email`, if one arrived before.
pub fn cached(email: &str) -> Option<PathBuf> {
    cached_path(email).filter(|path| path.is_file())
}

/// Fetch and cache the picture for `email`. A 404 means the mailbox has no
/// picture; any other failure leaves the cache untouched. The picture is
/// decoded and re-encoded as PNG so the cache never holds an error page and
/// the file's contents match its extension.
pub fn fetch(email: &str) -> Result<PathBuf, String> {
    let url = url(email).ok_or_else(|| "empty email".to_string())?;
    let target = cached_path(email).ok_or_else(|| "empty email".to_string())?;
    if target.is_file() {
        return Ok(target);
    }
    let response = reqwest::blocking::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("shellas/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| error.to_string())?
        .get(url)
        .send()
        .map_err(|error| error.to_string())?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Err("no picture".to_string());
    }
    if !response.status().is_success() {
        return Err(format!("avatar service replied {}", response.status()));
    }
    let bytes = response.bytes().map_err(|error| error.to_string())?;
    let picture =
        image::load_from_memory(&bytes).map_err(|error| format!("not a picture: {error}"))?;
    let mut png = Vec::new();
    DynamicImage::ImageRgba8(picture.to_rgba8())
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    let dir = target
        .parent()
        .ok_or_else(|| "no cache folder".to_string())?;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let temporary = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    std::fs::write(&temporary, &png).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, &target).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        error.to_string()
    })?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::hash;

    #[test]
    fn gravatar_keys_a_mailbox_by_its_lowercase_md5() {
        // The example published by Gravatar's own documentation.
        assert_eq!(
            hash("  MyEmailAddress@example.com ").as_deref(),
            Some("0bc83cb571cd1c50ba6f3e8a78ef1346")
        );
    }

    #[test]
    fn an_empty_address_has_no_picture() {
        assert_eq!(hash("   "), None);
    }
}
