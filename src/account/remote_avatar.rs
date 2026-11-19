//! The account server's own avatar. The profile names it by a relative URL;
//! the picture is downloaded once and cached beside the account, keyed by
//! that URL. The server may hand back PNG, JPEG or WebP, so the bytes are
//! decoded and re-encoded as PNG rather than trusted by extension.
//!
//! Nothing here runs on the UI thread: `fetch` blocks on the network.

use std::io::Cursor;
use std::path::PathBuf;

use image::{DynamicImage, ImageFormat};
use sha2::{Digest as _, Sha256};

use super::describe::account_error;
use super::service::SharedAccountService;
use crate::app::server_avatar_dir;

fn hash(url: &str) -> String {
    let digest = Sha256::digest(url.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn cached_path(url: &str) -> PathBuf {
    server_avatar_dir().join(format!("{}.png", hash(url)))
}

pub fn cached(url: &str) -> Option<PathBuf> {
    let path = cached_path(url);
    path.is_file().then_some(path)
}

pub fn fetch(service: SharedAccountService, url: String) -> Result<PathBuf, String> {
    let target = cached_path(&url);
    let bytes = service
        .fetch_avatar(&url)
        .map_err(|error| account_error(&error).to_string())?;
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
    use super::cached_path;

    #[test]
    fn the_same_url_maps_to_the_same_cache_file() {
        assert_eq!(
            cached_path("/api/v1/avatars/one.png"),
            cached_path("/api/v1/avatars/one.png")
        );
        assert_ne!(
            cached_path("/api/v1/avatars/one.png"),
            cached_path("/api/v1/avatars/two.png")
        );
    }
}
