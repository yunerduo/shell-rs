//! What a message carries beside its words: pictures pasted or dropped into
//! the composer, and text files read from disk. The bytes are kept with the
//! session, so a restored conversation still shows its pictures.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use gpui_kit::{Image, ImageFormat};

/// The most attachments one message may carry.
pub const MAX_ATTACHMENTS: usize = 8;
/// The largest picture that is sent, after any resize.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// The longest edge a picture is resized to. Vision models accept more, but
/// anything bigger costs tokens without adding readable detail.
const MAX_IMAGE_EDGE: u32 = 1568;
/// A file larger than this is not read at all.
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
/// How much of a text file is sent; the rest is left out and marked so.
const MAX_TEXT_CHARS: usize = 40_000;
/// Guards against reading a header that would decode into an enormous image.
const MAX_DECODED_EDGE: u32 = 20_000;

/// Why a file could not become an attachment.
#[derive(Debug)]
pub enum AttachmentError {
    UnsupportedFormat,
    TooLarge { limit: u64 },
    Binary,
    Read(std::io::Error),
    LimitReached,
}

impl From<std::io::Error> for AttachmentError {
    fn from(error: std::io::Error) -> Self {
        Self::Read(error)
    }
}

/// One thing the user put into the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub id: String,
    pub name: String,
    pub kind: AttachmentKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentKind {
    Image(Arc<Image>),
    Text { text: String, truncated: bool },
}

/// What the journal keeps: where the bytes are, not the bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRef {
    pub id: String,
    pub name: String,
    /// `image` or `text`.
    pub kind: String,
    pub size: u64,
    #[serde(default)]
    pub truncated: bool,
    pub file: String,
}

impl Attachment {
    /// A picture, resized and re-encoded when the wire format would not
    /// take it as it is.
    pub fn from_image(name: impl Into<String>, image: Image) -> Result<Self, AttachmentError> {
        let image = normalize_image(image)?;
        Ok(Self {
            id: new_id(),
            name: name.into(),
            kind: AttachmentKind::Image(Arc::new(image)),
        })
    }

    /// Read one file. Pictures become pictures; everything else is read as
    /// text. A file that is neither is refused.
    pub fn from_path(path: &Path) -> Result<Self, AttachmentError> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(AttachmentError::Read(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a file",
            )));
        }
        if metadata.len() > MAX_FILE_BYTES {
            return Err(AttachmentError::TooLarge {
                limit: MAX_FILE_BYTES,
            });
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let bytes = fs::read(path)?;
        match image_format_for(path) {
            Some(format) => Self::from_image(name, Image::from_bytes(format, bytes)),
            None => Self::text(name, &bytes),
        }
    }

    /// A text file, decoded as UTF-8 or GBK and shortened when it is long.
    pub fn text(name: impl Into<String>, bytes: &[u8]) -> Result<Self, AttachmentError> {
        if bytes.iter().take(8192).any(|byte| *byte == 0) {
            return Err(AttachmentError::Binary);
        }
        let text = decode_text(bytes).ok_or(AttachmentError::Binary)?;
        let truncated = text.chars().count() > MAX_TEXT_CHARS;
        let text = if truncated {
            text.chars().take(MAX_TEXT_CHARS).collect()
        } else {
            text
        };
        Ok(Self {
            id: new_id(),
            name: name.into(),
            kind: AttachmentKind::Text { text, truncated },
        })
    }

    pub fn is_image(&self) -> bool {
        matches!(self.kind, AttachmentKind::Image(_))
    }

    pub fn image_source(&self) -> Option<gpui_kit::ImageSource> {
        match &self.kind {
            AttachmentKind::Image(image) => Some(gpui_kit::ImageSource::Image(image.clone())),
            AttachmentKind::Text { .. } => None,
        }
    }

    /// The picture's bytes and media type, base64 encoded for the wire.
    pub fn image_payload(&self) -> Option<(&'static str, String)> {
        match &self.kind {
            AttachmentKind::Image(image) => Some((
                image.format.mime_type(),
                base64::engine::general_purpose::STANDARD.encode(&image.bytes),
            )),
            AttachmentKind::Text { .. } => None,
        }
    }

    pub fn text_content(&self) -> Option<(&str, bool)> {
        match &self.kind {
            AttachmentKind::Text { text, truncated } => Some((text.as_str(), *truncated)),
            AttachmentKind::Image(_) => None,
        }
    }

    pub fn size(&self) -> u64 {
        match &self.kind {
            AttachmentKind::Image(image) => image.bytes.len() as u64,
            AttachmentKind::Text { text, .. } => text.len() as u64,
        }
    }

    /// The name the bytes are stored under inside the session's directory.
    pub fn file_name(&self) -> String {
        match &self.kind {
            AttachmentKind::Image(image) => {
                format!("{}.{}", self.id, image.format.extension())
            }
            AttachmentKind::Text { .. } => format!("{}.txt", self.id),
        }
    }

    pub fn reference(&self) -> AttachmentRef {
        AttachmentRef {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: match self.kind {
                AttachmentKind::Image(_) => "image".to_string(),
                AttachmentKind::Text { .. } => "text".to_string(),
            },
            size: self.size(),
            truncated: matches!(
                self.kind,
                AttachmentKind::Text {
                    truncated: true,
                    ..
                }
            ),
            file: self.file_name(),
        }
    }

    /// Write the bytes below the session's attachment directory.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let path = dir.join(self.file_name());
        match &self.kind {
            AttachmentKind::Image(image) => fs::write(path, &image.bytes),
            AttachmentKind::Text { text, .. } => fs::write(path, text.as_bytes()),
        }
    }

    /// Put an attachment back from the journal. A file that has gone since
    /// is left out; the message itself is still there.
    pub fn restore(dir: &Path, reference: &AttachmentRef) -> Option<Self> {
        let path = dir.join(&reference.file);
        let bytes = fs::read(&path).ok()?;
        let id = reference.id.clone();
        let name = reference.name.clone();
        match reference.kind.as_str() {
            "image" => {
                let format = image_format_for(&path).or_else(|| sniff_image_format(&bytes))?;
                Some(Self {
                    id,
                    name,
                    kind: AttachmentKind::Image(Arc::new(Image::from_bytes(format, bytes))),
                })
            }
            _ => {
                let text = decode_text(&bytes)?;
                Some(Self {
                    id,
                    name,
                    kind: AttachmentKind::Text {
                        text,
                        truncated: reference.truncated,
                    },
                })
            }
        }
    }
}

/// A small valid picture and its media type, for the model test in the
/// settings. Nothing looks at it: it only has to be a picture the service
/// will accept or refuse.
pub fn probe_picture() -> Option<(&'static str, String)> {
    let image = image::RgbaImage::from_pixel(64, 64, image::Rgba([32, 96, 160, 255]));
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgba8(image)
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .ok()?;
    Some((
        "image/png",
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ))
}

/// The user's words with the text attachments appended, in a shape a model
/// reads as files rather than as part of the sentence.
pub fn request_text(text: &str, attachments: &[Attachment]) -> String {
    let mut out = text.to_string();
    for attachment in attachments {
        let Some((body, truncated)) = attachment.text_content() else {
            continue;
        };
        out.push_str("\n\n<file name=\"");
        out.push_str(&attachment.name.replace('"', "'"));
        out.push('"');
        if truncated {
            out.push_str(" truncated=\"true\"");
        }
        out.push_str(">\n");
        out.push_str(body);
        out.push_str("\n</file>");
    }
    out
}

/// Where one session's attachments live.
pub fn session_dir(data_dir: &Path, session: &str) -> PathBuf {
    data_dir.join("assistant").join("attachments").join(session)
}

/// A size a person can read, in the composer and in errors.
pub fn size_label(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let bytes = bytes as f64;
    if bytes < KB {
        format!("{bytes:.0} B")
    } else if bytes < KB * KB {
        format!("{:.0} KB", bytes / KB)
    } else {
        format!("{:.1} MB", bytes / (KB * KB))
    }
}

fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

fn normalize_image(image: Image) -> Result<Image, AttachmentError> {
    let format = to_image_format(image.format).ok_or(AttachmentError::UnsupportedFormat)?;
    let reader = image::ImageReader::with_format(Cursor::new(&image.bytes), format);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| AttachmentError::UnsupportedFormat)?;
    if width > MAX_DECODED_EDGE || height > MAX_DECODED_EDGE {
        return Err(AttachmentError::TooLarge {
            limit: MAX_IMAGE_BYTES as u64,
        });
    }
    let web = matches!(
        image.format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Webp | ImageFormat::Gif
    );
    if web && width <= MAX_IMAGE_EDGE && height <= MAX_IMAGE_EDGE {
        if image.bytes.len() > MAX_IMAGE_BYTES {
            return Err(AttachmentError::TooLarge {
                limit: MAX_IMAGE_BYTES as u64,
            });
        }
        return Ok(image);
    }
    let mut decoded = image::load_from_memory_with_format(&image.bytes, format)
        .map_err(|_| AttachmentError::UnsupportedFormat)?;
    if width > MAX_IMAGE_EDGE || height > MAX_IMAGE_EDGE {
        decoded = decoded.resize(
            MAX_IMAGE_EDGE,
            MAX_IMAGE_EDGE,
            image::imageops::FilterType::Lanczos3,
        );
    }
    let mut bytes = Vec::new();
    decoded
        .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
        .map_err(|_| AttachmentError::UnsupportedFormat)?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(AttachmentError::TooLarge {
            limit: MAX_IMAGE_BYTES as u64,
        });
    }
    Ok(Image::from_bytes(ImageFormat::Png, bytes))
}

fn to_image_format(format: ImageFormat) -> Option<image::ImageFormat> {
    match format {
        ImageFormat::Png => Some(image::ImageFormat::Png),
        ImageFormat::Jpeg => Some(image::ImageFormat::Jpeg),
        ImageFormat::Webp => Some(image::ImageFormat::WebP),
        ImageFormat::Gif => Some(image::ImageFormat::Gif),
        ImageFormat::Bmp => Some(image::ImageFormat::Bmp),
        ImageFormat::Tiff => Some(image::ImageFormat::Tiff),
        ImageFormat::Ico => Some(image::ImageFormat::Ico),
        ImageFormat::Svg | ImageFormat::Pnm => None,
    }
}

fn image_format_for(path: &Path) -> Option<ImageFormat> {
    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match extension.as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::Webp),
        "gif" => Some(ImageFormat::Gif),
        "bmp" => Some(ImageFormat::Bmp),
        "tif" | "tiff" => Some(ImageFormat::Tiff),
        "ico" => Some(ImageFormat::Ico),
        _ => None,
    }
}

fn sniff_image_format(bytes: &[u8]) -> Option<ImageFormat> {
    let format = image::guess_format(bytes).ok()?;
    match format {
        image::ImageFormat::Png => Some(ImageFormat::Png),
        image::ImageFormat::Jpeg => Some(ImageFormat::Jpeg),
        image::ImageFormat::WebP => Some(ImageFormat::Webp),
        image::ImageFormat::Gif => Some(ImageFormat::Gif),
        image::ImageFormat::Bmp => Some(ImageFormat::Bmp),
        image::ImageFormat::Tiff => Some(ImageFormat::Tiff),
        image::ImageFormat::Ico => Some(ImageFormat::Ico),
        _ => None,
    }
}

fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Some(text.to_string());
    }
    let (text, _, had_errors) = encoding_rs::GBK.decode(bytes);
    if had_errors {
        None
    } else {
        Some(text.into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "shellas-attachment-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([9, 9, 9, 255]));
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode");
        bytes
    }

    #[test]
    fn a_png_keeps_its_bytes() {
        let bytes = png(4, 4);
        let attachment =
            Attachment::from_image("a.png", Image::from_bytes(ImageFormat::Png, bytes.clone()))
                .expect("attachment");
        let (mime, _) = attachment.image_payload().expect("payload");
        assert_eq!(mime, "image/png");
        match attachment.kind {
            AttachmentKind::Image(image) => assert_eq!(image.bytes, bytes),
            _ => panic!("expected an image"),
        }
    }

    #[test]
    fn a_bitmap_is_re_encoded_as_png() {
        let source = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([1, 2, 3, 255]),
        ));
        let mut bytes = Vec::new();
        source
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Bmp)
            .expect("encode bmp");
        let attachment =
            Attachment::from_image("a.bmp", Image::from_bytes(ImageFormat::Bmp, bytes))
                .expect("attachment");
        match attachment.kind {
            AttachmentKind::Image(image) => assert_eq!(image.format, ImageFormat::Png),
            _ => panic!("expected an image"),
        }
    }

    #[test]
    fn a_long_text_file_is_cut_and_marked() {
        let bytes = "x".repeat(MAX_TEXT_CHARS + 10);
        let attachment = Attachment::text("long.log", bytes.as_bytes()).expect("attachment");
        let (text, truncated) = attachment.text_content().expect("text");
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
        assert!(truncated);
    }

    #[test]
    fn a_binary_file_is_refused() {
        let error = Attachment::text("core.bin", b"abc\0def").expect_err("refused");
        assert!(matches!(error, AttachmentError::Binary));
    }

    #[test]
    fn text_attachments_are_appended_as_files() {
        let attachment = Attachment::text("build.log", b"line one").expect("attachment");
        let text = request_text("look at this", &[attachment]);
        assert!(text.starts_with("look at this\n\n<file name=\"build.log\">"));
        assert!(text.contains("line one"));
        assert!(text.ends_with("</file>"));
    }

    #[test]
    fn a_saved_attachment_comes_back() {
        let dir = temp_dir("round-trip");
        let attachment =
            Attachment::from_image("a.png", Image::from_bytes(ImageFormat::Png, png(3, 3)))
                .expect("attachment");
        attachment.save(&dir).expect("save");
        let restored = Attachment::restore(&dir, &attachment.reference()).expect("restore");
        assert_eq!(restored.name, "a.png");
        assert!(restored.is_image());
        let _ = fs::remove_dir_all(&dir);
    }
}
