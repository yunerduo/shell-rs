//! 裁剪头像: the picture the user picked, framed by a circle, then squared
//! to what the circle holds and handed to [`AccountStore::set_avatar`].
//!
//! The file is decoded and shrunk on the background executor; only a
//! downscaled copy is kept, so a huge photo costs a few megabytes. Dragging
//! and the zoom slider move a plain image inside a round, clipping frame —
//! nothing here writes to the account until the dialog's 保存.

use std::path::{Path, PathBuf};

use gpui_kit::component::{
    ActiveTheme as _, WindowExt as _, h_flex,
    slider::{Slider, SliderState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use image::{ImageFormat, RgbaImage, imageops::FilterType};
use uuid::Uuid;

use super::store::AccountStore;
use crate::i18n::t;
use crate::shared::commit_footer;

/// Side of the round frame, and of the square it shows.
const FRAME: f32 = 256.;
/// The decoded picture is kept no bigger than this on its longest edge.
const MAX_EDGE: u32 = 1024;
/// The saved avatar is this square, plenty for a 48px avatar on a 2x screen.
const OUTPUT: u32 = 512;
/// Pictures beyond this many pixels are refused instead of decoded.
const MAX_PIXELS: u64 = 64_000_000;
/// The zoom slider's limits, in times the short edge of the picture.
const MIN_ZOOM: f32 = 1.;
const MAX_ZOOM: f32 = 3.;

/// Why a picture could not be prepared, in words the UI picks.
enum DecodeError {
    TooLarge,
    Unreadable,
}

struct Decoded {
    image: RgbaImage,
    /// The downscaled picture as a PNG, for `img` to draw.
    preview: PathBuf,
}

/// 裁剪头像: a round frame over the picture, a zoom slider, and 保存.
struct AvatarCropForm {
    account: Entity<AccountStore>,
    image: Option<RgbaImage>,
    preview: Option<PathBuf>,
    /// The picture's translation inside the frame, in points. Positive moves
    /// the picture right and down.
    offset: (f32, f32),
    zoom: Entity<SliderState>,
    panning: bool,
    last_pointer: Option<Point<Pixels>>,
    error: Option<SharedString>,
    _zoom_changes: Subscription,
    _decode: Task<()>,
}

impl AvatarCropForm {
    fn new(account: Entity<AccountStore>, source: PathBuf, cx: &mut Context<Self>) -> Self {
        let zoom = cx.new(|_| {
            SliderState::new()
                .min(MIN_ZOOM)
                .max(MAX_ZOOM)
                .step(0.1)
                .default_value(MIN_ZOOM)
        });
        // A zoom keeps the picture's centre: the offset is clamped to what
        // the new size allows, so the frame never shows past an edge.
        let zoom_changes = cx.observe(&zoom, |this, _, cx| {
            this.clamp_offset(cx);
            cx.notify();
        });
        let decode = cx.spawn(async move |this, cx| {
            let decoded = cx.background_spawn(async move { decode(&source) }).await;
            let _ = this.update(cx, |form, cx| form.apply(decoded, cx));
        });
        Self {
            account,
            image: None,
            preview: None,
            offset: (0., 0.),
            zoom,
            panning: false,
            last_pointer: None,
            error: None,
            _zoom_changes: zoom_changes,
            _decode: decode,
        }
    }

    fn apply(&mut self, decoded: Result<Decoded, DecodeError>, cx: &mut Context<Self>) {
        match decoded {
            Ok(decoded) => {
                self.image = Some(decoded.image);
                self.preview = Some(decoded.preview);
                self.error = None;
            }
            Err(DecodeError::TooLarge) => {
                self.error = Some(t!("account.avatar.crop_too_big"));
            }
            Err(DecodeError::Unreadable) => {
                self.error = Some(t!("account.avatar.crop_unreadable"));
            }
        }
        cx.notify();
    }

    fn zoom_value(&self, cx: &App) -> f32 {
        self.zoom.read(cx).value().end()
    }

    fn clamp_offset(&mut self, cx: &App) {
        let Some(image) = self.image.as_ref() else {
            return;
        };
        self.offset = clamp_offset(
            self.offset,
            image.width(),
            image.height(),
            self.zoom_value(cx),
        );
    }

    fn begin_pan(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.panning = true;
        self.last_pointer = Some(position);
        cx.notify();
    }

    fn drag(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if !self.panning {
            return;
        }
        let (Some(last), Some(image)) = (self.last_pointer, self.image.as_ref()) else {
            return;
        };
        let delta = (
            (position.x - last.x).as_f32(),
            (position.y - last.y).as_f32(),
        );
        self.last_pointer = Some(position);
        let zoom = self.zoom_value(cx);
        self.offset = clamp_offset(
            (self.offset.0 + delta.0, self.offset.1 + delta.1),
            image.width(),
            image.height(),
            zoom,
        );
        cx.notify();
    }

    fn end_pan(&mut self, cx: &mut Context<Self>) {
        if !self.panning && self.last_pointer.is_none() {
            return;
        }
        self.panning = false;
        self.last_pointer = None;
        cx.notify();
    }

    /// Square the picture to what the frame holds, save it as PNG, and let
    /// the account copy it in. The dialog closes only once that worked.
    fn commit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(image) = self.image.as_ref() else {
            self.error = Some(t!("account.avatar.crop_loading"));
            cx.notify();
            return false;
        };
        let zoom = self.zoom_value(cx);
        let (x, y, side) = crop_rect(image.width(), image.height(), zoom, self.offset);
        let cropped = image::imageops::crop_imm(image, x, y, side, side).to_image();
        let square = image::imageops::resize(&cropped, OUTPUT, OUTPUT, FilterType::Lanczos3);
        let temporary = std::env::temp_dir().join(format!("shellas-avatar-{}.png", Uuid::new_v4()));
        if let Err(error) = square.save_with_format(&temporary, ImageFormat::Png) {
            self.error = Some(t!("account.avatar.failed", error = error.to_string()));
            cx.notify();
            return false;
        }
        let saved = self
            .account
            .update(cx, |account, cx| account.set_avatar(&temporary, cx));
        let _ = std::fs::remove_file(&temporary);
        match saved {
            Ok(()) => {
                window.close_dialog(cx);
                true
            }
            Err(message) => {
                self.error = Some(t!("account.avatar.failed", error = message));
                cx.notify();
                false
            }
        }
    }
}

impl Drop for AvatarCropForm {
    fn drop(&mut self) {
        if let Some(preview) = self.preview.take() {
            let _ = std::fs::remove_file(preview);
        }
    }
}

impl Render for AvatarCropForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let zoom = self.zoom_value(cx);
        let entity = cx.entity();
        let frame = match (&self.image, &self.preview) {
            (Some(image), Some(preview)) => {
                let (left, top, width, height) =
                    image_box(image.width(), image.height(), zoom, self.offset);
                div()
                    .id("account-avatar-crop-frame")
                    .test_support()
                    .relative()
                    .flex_none()
                    .size(px(FRAME))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(theme.muted)
                    .border_2()
                    .border_color(theme.border)
                    .child(
                        img(preview.clone())
                            .absolute()
                            .left(px(left))
                            .top(px(top))
                            .w(px(width))
                            .h(px(height)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.begin_pan(event.position, cx)
                        }),
                    )
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |_, _, window, _| {
                                let this = entity.clone();
                                window.on_mouse_event(move |event: &MouseMoveEvent, _, _, cx| {
                                    let this = this.clone();
                                    if event.dragging() {
                                        this.update(cx, |form, cx| form.drag(event.position, cx));
                                    } else {
                                        this.update(cx, |form, cx| form.end_pan(cx));
                                    }
                                });
                                let this = entity.clone();
                                window.on_mouse_event(move |event: &MouseUpEvent, _, _, cx| {
                                    if event.button != MouseButton::Left {
                                        return;
                                    }
                                    let this = this.clone();
                                    this.update(cx, |form, cx| form.end_pan(cx));
                                });
                            },
                        )
                        .absolute()
                        .size_full(),
                    )
            }
            _ => div()
                .id("account-avatar-crop-frame")
                .test_support()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .size(px(FRAME))
                .rounded_full()
                .bg(theme.muted)
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(t!("account.avatar.crop_loading")),
        };

        v_flex()
            .gap_3()
            .w_full()
            .child(
                div()
                    .min_w_0()
                    .whitespace_normal()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(t!("account.avatar.crop_hint")),
            )
            .child(h_flex().w_full().justify_center().child(frame))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_none()
                            .text_sm()
                            .child(t!("account.avatar.crop_zoom")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Slider::new(&self.zoom).horizontal()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w_10()
                            .text_sm()
                            .child(format!("{zoom:.1}×")),
                    ),
            )
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    div()
                        .id("account-avatar-crop-error")
                        .test_support()
                        .min_w_0()
                        .whitespace_normal()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(error),
                )
            })
    }
}

/// Open 裁剪头像 for `source`. Only a signed-in account can take a picture.
pub fn open_avatar_crop_dialog(
    account: Entity<AccountStore>,
    source: PathBuf,
    window: &mut Window,
    cx: &mut App,
) {
    if !account.read(cx).is_signed_in() {
        return;
    }
    let form = cx.new(|cx| AvatarCropForm::new(account, source, cx));
    window.open_dialog(cx, {
        let form = form.clone();
        move |dialog, _, _| {
            dialog
                .title(t!("account.avatar.crop_title"))
                .overlay_closable(true)
                .child(form.clone())
                .footer(commit_footer(
                    "account-avatar-crop-commit",
                    t!("account.avatar.crop_save"),
                ))
                .on_ok({
                    let form = form.clone();
                    move |_, window, cx| form.update(cx, |form, cx| form.commit(window, cx))
                })
        }
    });
}

/// The scale at which the picture's short edge fills the frame, at 1×.
fn preview_scale(width: u32, height: u32) -> f32 {
    FRAME / width.min(height).max(1) as f32
}

/// How much the picture may be moved at `zoom`, in points: half of what
/// overflows the frame on each axis.
fn max_offset(width: u32, height: u32, zoom: f32) -> (f32, f32) {
    let scale = preview_scale(width, height) * zoom;
    (
        (width as f32 * scale - FRAME).max(0.) / 2.,
        (height as f32 * scale - FRAME).max(0.) / 2.,
    )
}

fn clamp_offset(offset: (f32, f32), width: u32, height: u32, zoom: f32) -> (f32, f32) {
    let (max_x, max_y) = max_offset(width, height, zoom);
    (offset.0.clamp(-max_x, max_x), offset.1.clamp(-max_y, max_y))
}

/// Where the picture sits inside the frame, in points: `(left, top, width,
/// height)`.
fn image_box(width: u32, height: u32, zoom: f32, offset: (f32, f32)) -> (f32, f32, f32, f32) {
    let scale = preview_scale(width, height) * zoom;
    let display = (width as f32 * scale, height as f32 * scale);
    (
        FRAME / 2. - display.0 / 2. + offset.0,
        FRAME / 2. - display.1 / 2. + offset.1,
        display.0,
        display.1,
    )
}

/// The square of the picture the frame shows, in its own pixels:
/// `(x, y, side)`. [`image_box`] draws from the same numbers, so what is
/// framed is what is saved.
fn crop_rect(width: u32, height: u32, zoom: f32, offset: (f32, f32)) -> (u32, u32, u32) {
    let short = width.min(height).max(1) as f32;
    let side = (short / zoom.max(MIN_ZOOM)).round().max(1.).min(short) as u32;
    let scale = preview_scale(width, height) * zoom.max(MIN_ZOOM);
    let x = (width as f32 / 2. - side as f32 / 2. - offset.0 / scale).round();
    let y = (height as f32 / 2. - side as f32 / 2. - offset.1 / scale).round();
    let x = x.clamp(0., width.saturating_sub(side) as f32) as u32;
    let y = y.clamp(0., height.saturating_sub(side) as f32) as u32;
    (x, y, side)
}

fn decode(source: &Path) -> Result<Decoded, DecodeError> {
    let picture = image::open(source).map_err(|_| DecodeError::Unreadable)?;
    let (width, height) = (picture.width(), picture.height());
    if width == 0 || height == 0 {
        return Err(DecodeError::Unreadable);
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(DecodeError::TooLarge);
    }
    let picture = if width.max(height) > MAX_EDGE {
        picture.resize(MAX_EDGE, MAX_EDGE, FilterType::Lanczos3)
    } else {
        picture
    };
    let image = picture.to_rgba8();
    let preview =
        std::env::temp_dir().join(format!("shellas-avatar-preview-{}.png", Uuid::new_v4()));
    image
        .save_with_format(&preview, ImageFormat::Png)
        .map_err(|_| DecodeError::Unreadable)?;
    Ok(Decoded { image, preview })
}

#[cfg(test)]
mod tests {
    use super::{crop_rect, image_box};

    #[test]
    fn a_landscape_picture_is_cropped_from_the_middle() {
        // 400×200 in a 256 frame at 1×: the 200-tall middle, x 100..300.
        assert_eq!(crop_rect(400, 200, 1., (0., 0.)), (100, 0, 200));
    }

    #[test]
    fn dragging_right_shows_more_of_the_left_edge() {
        let (x, _, side) = crop_rect(400, 200, 1., (50., 0.));
        assert_eq!(side, 200);
        assert!(x < 100);
    }

    #[test]
    fn the_frame_and_the_saved_square_agree() {
        // At 2× the short edge halves, so half as much is shown...
        let (_, _, side) = crop_rect(400, 200, 2., (0., 0.));
        assert_eq!(side, 100);
        // ...and the picture is drawn twice the short-edge size.
        let (_, _, width, height) = image_box(400, 200, 2., (0., 0.));
        assert_eq!((width, height), (1024., 512.));
    }

    #[test]
    fn a_pan_past_the_edge_is_clamped() {
        // Far right: the frame sits against the picture's left edge.
        assert_eq!(crop_rect(400, 200, 1., (10_000., 0.)).0, 0);
        assert_eq!(crop_rect(400, 200, 1., (-10_000., 0.)).0, 200);
    }
}
