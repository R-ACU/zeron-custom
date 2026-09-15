//! Hover previews for image file chips in the transcript.
//!
//! A `Read`/`Write`/`Edit` chip whose file badge names an image opens a small
//! floating card after a short dwell. The bytes come from the SAME path the
//! transcript's user-bubble thumbnails use — [`crate::attachments`]'s global
//! `(deviceId, path)` cache over the `ReadAttachmentChunk` RPC — so a file
//! owned by another device in synced mode resolves through its owner and a
//! second hover is instant. The UI never touches `std::fs`.
//!
//! The card is mounted by the transcript's ROOT element through
//! `deferred(anchored().position(..))`: a deferred draw carries no content
//! mask, so the card escapes the virtualized list's clip, and it contributes
//! no layout, so row heights (and therefore the list's measurements) cannot
//! move. The badge itself only publishes its window rect through a paint
//! canvas, the same no-notify-from-paint discipline as `user_heights`.

use std::{
    cell::Cell,
    collections::HashMap,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, Bounds, Image, IntoElement as _, ParentElement as _, Pixels, Point, SharedString,
    Size, Styled as _, StyledImage as _, Task, div, img, point, px,
};

use crate::{motion, theme::Theme};

/// Dwell before a hovered image chip opens its card.
pub const HOVER_DELAY: Duration = Duration::from_millis(250);

/// Largest logical box the preview image is fitted into.
pub const MAX_PREVIEW_W: f32 = 360.0;
/// See [`MAX_PREVIEW_W`].
pub const MAX_PREVIEW_H: f32 = 240.0;

/// Card chrome: padding inside the border, border width, caption strip.
const CARD_PAD: f32 = 6.0;
const CARD_BORDER: f32 = 1.0;
const CAPTION_GAP: f32 = 6.0;
const CAPTION_HEIGHT: f32 = 16.0;
/// The card never gets narrower than this: a portrait image is only ~120px
/// wide at the 240px height cap, and a card that narrow clipped the caption
/// down to a fragment of the file name.
const MIN_CONTENT_W: f32 = 268.0;
/// Gap between the chip and the card, and the margin the card keeps from the
/// window edges.
const ANCHOR_GAP: f32 = 8.0;
const WINDOW_MARGIN: f32 = 8.0;
const IMAGE_RADIUS: f32 = 6.0;

/// Decoded-preview metadata retained across hovers (bounded, LRU).
const CACHE_CAPACITY: usize = 32;

/// Fade-in for the card. `with_animation` snaps to the end state under
/// reduced motion, and [`preview_card`] skips the wrapper outright.
pub const PREVIEW_IN: motion::MotionSpec = motion::MotionSpec::new(120, motion::EASE);

// ---------------------------------------------------------------------------
// Pure helpers (extension, fitting, placement, caption)
// ---------------------------------------------------------------------------

/// Extensions that get a hover preview. Deliberately the raster/vector subset
/// gpui can paint AND the engine's read-back jail serves; `tif`/`tiff` are
/// left out because no browser-grade thumbnail is expected of them.
pub fn is_previewable_image(path: &str) -> bool {
    let name = path
        .rsplit(['/', '\\'])
        .find(|component| !component.is_empty())
        .unwrap_or(path);
    name.rsplit_once('.').is_some_and(|(stem, extension)| {
        !stem.is_empty()
            && matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg"
            )
    })
}

/// Scale `w x h` into `max_w x max_h` keeping the aspect ratio. Never
/// upscales: a 64px icon stays a 64px icon rather than becoming a blur.
pub fn fit_within(w: f32, h: f32, max_w: f32, max_h: f32) -> (f32, f32) {
    if !(w.is_finite() && h.is_finite()) || w <= 0.0 || h <= 0.0 {
        return (max_w, max_h);
    }
    let scale = (max_w / w).min(max_h / h).min(1.0);
    ((w * scale).max(1.0), (h * scale).max(1.0))
}

/// Logical size of the image area for a preview with (or without) known
/// pixel dimensions.
pub fn content_size(meta: Option<PreviewMeta>) -> (f32, f32) {
    match meta {
        Some(meta) => fit_within(
            meta.width as f32,
            meta.height as f32,
            MAX_PREVIEW_W,
            MAX_PREVIEW_H,
        ),
        None => (MAX_PREVIEW_W, MAX_PREVIEW_H),
    }
}

/// Outer (border-box) size of the card around an image area.
pub fn card_size(content: (f32, f32)) -> (f32, f32) {
    let chrome = 2.0 * (CARD_PAD + CARD_BORDER);
    (
        content.0.max(MIN_CONTENT_W) + chrome,
        content.1 + CAPTION_GAP + CAPTION_HEIGHT + chrome,
    )
}

/// Where the card lands relative to its chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    /// Top-left of the card in WINDOW coordinates.
    pub origin: Point<Pixels>,
    /// Whether the card sits above the chip (it only goes below when there is
    /// no room above).
    pub above: bool,
}

/// Place the card over the chip when it fits, otherwise under it, clamped
/// into the window with a small margin. Pure — the anchored element applies
/// the result verbatim.
pub fn place_card(
    anchor: Bounds<Pixels>,
    card: Size<Pixels>,
    window: Size<Pixels>,
) -> Placement {
    let card_w = f32::from(card.width);
    let card_h = f32::from(card.height);
    let win_w = f32::from(window.width);
    let win_h = f32::from(window.height);
    let anchor_top = f32::from(anchor.origin.y);
    let anchor_bottom = anchor_top + f32::from(anchor.size.height);

    // Left-aligned with the chip, pulled back in when that would overflow.
    let mut x = f32::from(anchor.origin.x);
    let max_x = win_w - WINDOW_MARGIN - card_w;
    if x > max_x {
        x = max_x;
    }
    if x < WINDOW_MARGIN {
        x = WINDOW_MARGIN;
    }

    let above_y = anchor_top - ANCHOR_GAP - card_h;
    let above = above_y >= WINDOW_MARGIN;
    let mut y = if above {
        above_y
    } else {
        anchor_bottom + ANCHOR_GAP
    };
    let max_y = win_h - WINDOW_MARGIN - card_h;
    if y > max_y {
        y = max_y;
    }
    if y < WINDOW_MARGIN {
        y = WINDOW_MARGIN;
    }

    Placement {
        origin: point(px(x), px(y)),
        above,
    }
}

/// Pixel dimensions plus encoded byte length of a previewed image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreviewMeta {
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
}

/// Read an image's natural size from its ENCODED bytes — headers only, no
/// full decode (gpui decodes lazily at paint anyway).
pub fn measure(image: &Image) -> Option<PreviewMeta> {
    let bytes = image.bytes.len();
    let (width, height) = if matches!(image.format, gpui::ImageFormat::Svg) {
        let tree = usvg::Tree::from_data(&image.bytes, &crate::image_media::svg_options()).ok()?;
        (
            tree.size().width().round() as u32,
            tree.size().height().round() as u32,
        )
    } else {
        image::ImageReader::new(std::io::Cursor::new(image.bytes.as_slice()))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()?
    };
    (width > 0 && height > 0).then_some(PreviewMeta {
        width,
        height,
        bytes,
    })
}

/// Compact byte size for the caption ("812 B", "245 KB", "1.2 MB").
pub fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;
    if bytes < KB {
        format!("{bytes} B")
    } else if bytes < MB {
        format!("{} KB", bytes.div_ceil(KB))
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// One-line caption: file name, then the pixel dimensions and size when the
/// bytes are already in hand.
pub fn caption(name: &str, meta: Option<PreviewMeta>) -> String {
    match meta {
        Some(meta) => format!(
            "{name} · {} × {} · {}",
            meta.width,
            meta.height,
            format_size(meta.bytes)
        ),
        None => name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Bounded metadata cache
// ---------------------------------------------------------------------------

/// Path-keyed LRU over [`PreviewMeta`], so re-hovering a chip costs no header
/// parse. The image BYTES are already cached by [`crate::attachments`]; this
/// only bounds the derived measurements.
#[derive(Default)]
pub struct PreviewCache {
    map: HashMap<String, (PreviewMeta, u64)>,
    tick: u64,
}

impl PreviewCache {
    pub fn get(&mut self, path: &str) -> Option<PreviewMeta> {
        self.tick += 1;
        let tick = self.tick;
        let entry = self.map.get_mut(path)?;
        entry.1 = tick;
        Some(entry.0)
    }

    pub fn put(&mut self, path: String, meta: PreviewMeta) {
        self.tick += 1;
        let tick = self.tick;
        self.map.insert(path, (meta, tick));
        while self.map.len() > CACHE_CAPACITY {
            let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.map.remove(&oldest);
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn contains(&self, path: &str) -> bool {
        self.map.contains_key(path)
    }
}

// ---------------------------------------------------------------------------
// Hover state machine
// ---------------------------------------------------------------------------

/// The chip currently under the pointer.
#[derive(Debug, Clone)]
pub struct HoverTarget {
    /// Element key of the chip's file badge (`"{row}#img{ix}"`).
    pub key: SharedString,
    /// Absolute path of the file, as the tool call reported it.
    pub path: String,
    /// Bare file name for the caption.
    pub name: String,
}

/// Dwell timer + anchor + metadata cache for the transcript's image previews.
///
/// The timer itself is owned here but ARMED by the transcript (only it has a
/// `Context`); a generation token invalidates a wake-up whose chip was left
/// before it fired — the same discipline as the composer's mention tooltip.
#[derive(Default)]
pub struct ImageHover {
    target: Option<HoverTarget>,
    open: bool,
    opened_at: Option<Instant>,
    timer: Option<Task<()>>,
    token: u64,
    anchor: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl ImageHover {
    /// Key of the chip under the pointer, open or still dwelling.
    pub fn target_key(&self) -> Option<&SharedString> {
        self.target.as_ref().map(|target| &target.key)
    }

    /// The chip whose card should paint this frame.
    pub fn open_target(&self) -> Option<&HoverTarget> {
        self.open.then(|| self.target.as_ref()).flatten()
    }

    /// The cell the hovered badge publishes its window rect into.
    pub fn anchor_cell(&self) -> Rc<Cell<Option<Bounds<Pixels>>>> {
        self.anchor.clone()
    }

    pub fn anchor_bounds(&self) -> Option<Bounds<Pixels>> {
        self.anchor.get()
    }

    pub fn opened_at(&self) -> Option<Instant> {
        self.opened_at
    }

    /// Enter a chip: park the previous card, arm a fresh generation. Returns
    /// the token the caller's wake-up must present to [`Self::reveal`].
    pub fn begin(&mut self, target: HoverTarget) -> u64 {
        self.target = Some(target);
        self.open = false;
        self.opened_at = None;
        self.anchor.set(None);
        self.timer = None;
        self.token = self.token.wrapping_add(1);
        self.token
    }

    /// Hand the dwell timer over for safekeeping (dropping it cancels it).
    pub fn arm(&mut self, timer: Task<()>) {
        self.timer = Some(timer);
    }

    /// The dwell elapsed: open the card if the pointer never left. `true` when
    /// something changed and the view must repaint.
    pub fn reveal(&mut self, token: u64) -> bool {
        if token != self.token || self.open || self.target.is_none() {
            return false;
        }
        self.open = true;
        self.opened_at = Some(Instant::now());
        self.timer = None;
        true
    }

    /// Pointer left `key`. A leave for a chip we already moved off is ignored,
    /// so an out-of-order enter/leave pair cannot close the new card.
    pub fn end(&mut self, key: &SharedString) -> bool {
        if self.target_key() != Some(key) {
            return false;
        }
        self.dismiss()
    }

    /// Close unconditionally (scroll, click, chat switch).
    pub fn dismiss(&mut self) -> bool {
        let was_live = self.target.is_some();
        self.target = None;
        self.open = false;
        self.opened_at = None;
        self.timer = None;
        self.anchor.set(None);
        self.token = self.token.wrapping_add(1);
        was_live
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// What the card shows for the hovered path this frame.
pub enum PreviewState {
    Loading,
    Ready {
        image: Arc<Image>,
        meta: Option<PreviewMeta>,
    },
    Failed,
}

/// A zero-size paint canvas publishing the badge's window rect. Writes only
/// into the cell — never notifies, so measurement cannot drive a repaint loop.
pub fn anchor_probe(cell: Rc<Cell<Option<Bounds<Pixels>>>>) -> AnyElement {
    gpui::canvas(
        move |bounds, _, _| cell.set(Some(bounds)),
        |_, _, _, _| (),
    )
    .absolute()
    .size_full()
    .into_any_element()
}

/// The floating card, mounted through `deferred(anchored(..))` at the window
/// position [`place_card`] chose. Never `occlude()`s: the chip underneath must
/// keep taking the click that opens the file.
pub fn preview_card(
    theme: &Theme,
    target: &HoverTarget,
    state: PreviewState,
    anchor: Bounds<Pixels>,
    viewport: Size<Pixels>,
    reduced_motion: bool,
) -> AnyElement {
    let (meta, body) = match state {
        PreviewState::Ready { image, meta } => {
            let (w, h) = content_size(meta);
            (
                meta,
                div()
                    .w(px(w))
                    .h(px(h))
                    .flex_none()
                    .overflow_hidden()
                    .rounded(px(IMAGE_RADIUS))
                    .child(
                        img(image)
                            .w(px(w))
                            .h(px(h))
                            .object_fit(gpui::ObjectFit::Contain),
                    ),
            )
        }
        PreviewState::Loading => (None, placeholder(theme, "Loading…")),
        PreviewState::Failed => (None, placeholder(theme, "Could not load image")),
    };
    let content = content_size(meta);
    let (card_w, card_h) = card_size(content);
    let placement = place_card(
        anchor,
        gpui::size(px(card_w), px(card_h)),
        viewport,
    );

    let card = crate::popover::popover_card(theme)
        .w(px(card_w))
        .h(px(card_h))
        .p(px(CARD_PAD))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(CAPTION_GAP))
        .child(body)
        .child(
            // Left-aligned, not centered: a centered line that overflows is
            // clipped at BOTH ends, so the file name loses its head as well
            // as its tail. Left-aligned it truncates once, with an ellipsis.
            div()
                .w_full()
                .h(px(CAPTION_HEIGHT))
                .flex()
                .items_center()
                .min_w_0()
                .truncate()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(caption(&target.name, meta))),
        );
    let card = crate::frost::frosted(crate::popover::CARD_RADIUS, crate::frost::MENU_BLUR, card);
    let layer = div().child(card);
    let layer = if reduced_motion {
        layer.into_any_element()
    } else {
        use gpui::AnimationExt as _;
        layer
            .with_animation(
                SharedString::from(format!("image-hover-{}", target.key)),
                PREVIEW_IN.animation(),
                |el, t| el.opacity(t),
            )
            .into_any_element()
    };

    gpui::deferred(
        gpui::anchored()
            .position(placement.origin)
            .anchor(gpui::Anchor::TopLeft)
            .snap_to_window_with_margin(px(WINDOW_MARGIN))
            .child(layer),
    )
    .priority(2)
    .into_any_element()
}

fn placeholder(theme: &Theme, label: &'static str) -> gpui::Div {
    div()
        .w(px(MAX_PREVIEW_W))
        .h(px(MAX_PREVIEW_H))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(IMAGE_RADIUS))
        .bg(crate::theme::ink(0.05))
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_muted)
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{bounds, size};

    #[test]
    fn extension_detection_accepts_only_previewable_images() {
        for path in [
            r"D:\AI-OS\projects\vps\x-bot\data\imgtest\winglee\HSMUwsNakAEamKV.jpg",
            "/work/shot.PNG",
            "a.jpeg",
            "a.gif",
            "a.webp",
            "a.bmp",
            "logo.svg",
        ] {
            assert!(is_previewable_image(path), "rejected {path}");
        }
        for path in [
            "crates/ui/src/transcript.rs",
            "README.md",
            "archive.tar.gz",
            "noextension",
            ".gitignore",
            "image.tiff",
            "",
        ] {
            assert!(!is_previewable_image(path), "accepted {path}");
        }
    }

    #[test]
    fn fitting_keeps_the_aspect_ratio_and_never_upscales() {
        // Wide: width-bound.
        let (w, h) = fit_within(1920.0, 1080.0, MAX_PREVIEW_W, MAX_PREVIEW_H);
        assert!((w - 360.0).abs() < 0.01, "{w}");
        assert!((h - 202.5).abs() < 0.01, "{h}");
        // Tall: height-bound.
        let (w, h) = fit_within(1000.0, 2000.0, MAX_PREVIEW_W, MAX_PREVIEW_H);
        assert!((h - 240.0).abs() < 0.01, "{h}");
        assert!((w - 120.0).abs() < 0.01, "{w}");
        // Aspect ratio preserved within a pixel.
        let ratio = 1920.0 / 1080.0;
        let (w, h) = fit_within(1920.0, 1080.0, MAX_PREVIEW_W, MAX_PREVIEW_H);
        assert!((w / h - ratio).abs() < 0.01);
        // Smaller than the box: untouched.
        assert_eq!(fit_within(64.0, 48.0, MAX_PREVIEW_W, MAX_PREVIEW_H), (64.0, 48.0));
        // Degenerate input falls back to the full box.
        assert_eq!(
            fit_within(0.0, 10.0, MAX_PREVIEW_W, MAX_PREVIEW_H),
            (MAX_PREVIEW_W, MAX_PREVIEW_H)
        );
        // An unknown size reserves the full target box.
        assert_eq!(content_size(None), (MAX_PREVIEW_W, MAX_PREVIEW_H));
    }

    #[test]
    fn card_size_adds_chrome_and_a_caption_strip() {
        let (w, h) = card_size((MAX_PREVIEW_W, MAX_PREVIEW_H));
        assert_eq!(w, MAX_PREVIEW_W + 2.0 * (CARD_PAD + CARD_BORDER));
        assert_eq!(
            h,
            MAX_PREVIEW_H + CAPTION_GAP + CAPTION_HEIGHT + 2.0 * (CARD_PAD + CARD_BORDER)
        );
        // A narrow image still yields a readable caption strip.
        let (narrow, _) = card_size((40.0, 240.0));
        assert_eq!(narrow, MIN_CONTENT_W + 2.0 * (CARD_PAD + CARD_BORDER));
    }

    #[test]
    fn anchor_placement_prefers_above_and_clamps_to_the_window() {
        let window = size(px(1200.0), px(800.0));
        let card = size(px(200.0), px(160.0));

        // Room above: the card sits over the chip, left-aligned with it.
        let chip = bounds(point(px(300.0), px(500.0)), size(px(120.0), px(22.0)));
        let placed = place_card(chip, card, window);
        assert!(placed.above);
        assert_eq!(placed.origin.x, px(300.0));
        assert_eq!(placed.origin.y, px(500.0 - ANCHOR_GAP - 160.0));

        // No room above: it flips below the chip.
        let chip = bounds(point(px(300.0), px(40.0)), size(px(120.0), px(22.0)));
        let placed = place_card(chip, card, window);
        assert!(!placed.above);
        assert_eq!(placed.origin.y, px(40.0 + 22.0 + ANCHOR_GAP));

        // Right edge: pulled back inside the margin.
        let chip = bounds(point(px(1150.0), px(500.0)), size(px(40.0), px(22.0)));
        let placed = place_card(chip, card, window);
        assert_eq!(placed.origin.x, px(1200.0 - WINDOW_MARGIN - 200.0));

        // Left edge: never crosses the margin.
        let chip = bounds(point(px(-30.0), px(500.0)), size(px(40.0), px(22.0)));
        assert_eq!(place_card(chip, card, window).origin.x, px(WINDOW_MARGIN));

        // A card taller than the window still starts inside it.
        let tall = size(px(200.0), px(900.0));
        let chip = bounds(point(px(300.0), px(400.0)), size(px(120.0), px(22.0)));
        let placed = place_card(chip, tall, window);
        assert!(!placed.above);
        assert_eq!(placed.origin.y, px(WINDOW_MARGIN));

        // A wider-than-window card pins to the left margin.
        let wide = size(px(1400.0), px(160.0));
        assert_eq!(place_card(chip, wide, window).origin.x, px(WINDOW_MARGIN));
    }

    #[test]
    fn caption_states_the_name_and_the_pixels() {
        let meta = PreviewMeta {
            width: 1200,
            height: 800,
            bytes: 245_000,
        };
        assert_eq!(
            caption("HSMUwsNakAEamKV.jpg", Some(meta)),
            "HSMUwsNakAEamKV.jpg · 1200 × 800 · 240 KB"
        );
        assert_eq!(caption("a.png", None), "a.png");
        assert_eq!(format_size(812), "812 B");
        assert_eq!(format_size(1024), "1 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
    }

    fn meta(n: u32) -> PreviewMeta {
        PreviewMeta {
            width: n,
            height: n,
            bytes: n as usize,
        }
    }

    #[test]
    fn cache_is_bounded_and_evicts_least_recently_used() {
        let mut cache = PreviewCache::default();
        for n in 0..CACHE_CAPACITY as u32 {
            cache.put(format!("p{n}"), meta(n));
        }
        assert_eq!(cache.len(), CACHE_CAPACITY);
        // Touch the oldest so the NEXT one is evicted instead.
        assert_eq!(cache.get("p0"), Some(meta(0)));
        cache.put("overflow".into(), meta(999));
        assert_eq!(cache.len(), CACHE_CAPACITY);
        assert!(cache.contains("p0"), "the touched entry was evicted");
        assert!(!cache.contains("p1"), "the stale entry survived");
        assert_eq!(cache.get("overflow"), Some(meta(999)));
        assert_eq!(cache.get("p1"), None);
    }

    #[test]
    fn hover_opens_only_for_the_chip_that_is_still_hovered() {
        let mut hover = ImageHover::default();
        let a = HoverTarget {
            key: "row#img0".into(),
            path: "/w/a.png".into(),
            name: "a.png".into(),
        };
        let b = HoverTarget {
            key: "row#img1".into(),
            path: "/w/b.png".into(),
            name: "b.png".into(),
        };
        let stale = hover.begin(a.clone());
        let fresh = hover.begin(b.clone());
        // The first chip's wake-up is dead once the pointer moved on.
        assert!(!hover.reveal(stale));
        assert!(hover.open_target().is_none());
        assert!(hover.reveal(fresh));
        assert_eq!(hover.open_target().map(|t| t.key.clone()), Some(b.key));
        assert!(hover.opened_at().is_some());
        // A leave for the chip we already left changes nothing.
        assert!(!hover.end(&a.key));
        assert!(hover.open_target().is_some());
        assert!(hover.end(&"row#img1".into()));
        assert!(hover.open_target().is_none());
        assert!(!hover.dismiss());
    }

    #[test]
    fn measured_dimensions_come_from_the_encoded_bytes() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(120, 48)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let bytes = png.into_inner();
        let len = bytes.len();
        let image = Image::from_bytes(gpui::ImageFormat::Png, bytes);
        assert_eq!(
            measure(&image),
            Some(PreviewMeta {
                width: 120,
                height: 48,
                bytes: len,
            })
        );
        assert!(measure(&Image::from_bytes(gpui::ImageFormat::Png, b"nope".to_vec())).is_none());
    }
}
