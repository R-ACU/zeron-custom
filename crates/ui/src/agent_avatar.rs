//! The little agent mascots on the Automations page and in its wizard.
//!
//! Geometry comes from [`crate::agent_avatar_data`], generated out of the
//! extracted Grok-bot avatar assets by `scripts/gen-avatar-data.py` (25 shapes
//! and an 11-colour palette; `globe` is left out). Nothing is read at runtime.
//!
//! A figure is painted, not composed from divs: the body is an SVG path from
//! the data, filled in the avatar's colour, then the optional dark `parts`, then
//! the two eyes as rotated rounded rectangles. Motion is a manual tween off one
//! clock ([`AvatarMotion`]) that the owning view keeps alive, never
//! `with_animation` (whose element-id clock restarts on every rebuild).

use gpui::{
    AnyElement, Bounds, FillOptions, FillRule, Hsla, PathBuilder, PathStyle, Pixels, Point, canvas,
    div, point, prelude::*, px,
};

pub use crate::agent_avatar_data::{CENTER, PALETTE, SHAPES, VIEW_MIN, VIEW_SIZE};

/// Shared identity for the transcript row and the active-agent bubble.
pub fn spawn_seed(call: &zeron_proto::ToolCall) -> usize {
    let label = match call {
        zeron_proto::ToolCall::Unknown { name, .. } => name.as_str(),
        zeron_proto::ToolCall::Mcp { tool, .. } => tool.as_str(),
        _ => "Agent",
    };
    label
        .bytes()
        .fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619)) as usize
}

/// One eye: a rounded rectangle in viewBox units, rotated by `rot` degrees
/// around (`cx`, `cy`).
#[derive(Debug, Clone, Copy)]
pub struct EyeRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub r: f32,
    pub rot: f32,
    pub cx: f32,
    pub cy: f32,
}

/// A dark inner detail drawn between body and eyes.
#[derive(Debug, Clone, Copy)]
pub struct AvatarPart {
    pub d: &'static str,
    pub opacity: f32,
    pub even_odd: bool,
}

/// One silhouette plus its solved eye placement.
#[derive(Debug, Clone, Copy)]
pub struct AvatarShape {
    pub key: &'static str,
    pub label: &'static str,
    /// Optical size normalisation, pivoting on [`CENTER`].
    pub scale: f32,
    pub even_odd: bool,
    pub d: &'static str,
    pub eyes: [EyeRect; 2],
    pub parts: &'static [AvatarPart],
}

/// One palette entry; `rgb` is `0xRRGGBB`.
#[derive(Debug, Clone, Copy)]
pub struct AvatarColor {
    pub id: &'static str,
    pub label: &'static str,
    pub rgb: u32,
}

impl AvatarColor {
    pub fn hsla(&self) -> Hsla {
        gpui::rgb(self.rgb).into()
    }
    /// The stored wire value, `#rrggbb`.
    pub fn hex(&self) -> String {
        format!("#{:06x}", self.rgb)
    }
}

/// The eyes' ink, as in the source assets.
const EYE_INK: u32 = 0x0a_0a_0a;
/// Idle float travel, in viewBox units.
const FLOAT_UNITS: f32 = 15.0;
/// Idle float period, in seconds.
const FLOAT_SECONDS: f32 = 3.4;
/// Breathe period, in seconds.
const BREATHE_SECONDS: f32 = 3.8;
/// Breathe amplitude: 1.06 / 0.94.
const BREATHE_AMPLITUDE: f32 = 0.06;
/// One blink every ~5 s.
const BLINK_SECONDS: f32 = 5.0;
/// How long an eye stays shut.
const BLINK_CLOSED_SECONDS: f32 = 0.12;
/// How far shut a blink closes the eyes (scaleY).
const BLINK_SCALE: f32 = 0.1;
/// How long the shuffle pop runs.
pub const POP_SECONDS: f32 = 0.35;
/// How far the eyes travel towards a gaze target, in viewBox units.
pub const GAZE_UNITS: f32 = 14.0;
/// Distance (window px) at which the gaze is fully deflected.
pub const GAZE_REACH: f32 = 260.0;

/// Which idle loop a figure runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleMotion {
    Float,
    Breathe,
}

impl IdleMotion {
    pub const ALL: [IdleMotion; 2] = [Self::Float, Self::Breathe];
}

/// The one clock behind every visible avatar of a view: a wall-clock origin
/// plus the current gaze target in window coordinates. The owning view renews
/// [`crate::motion::pulse_lease`] while avatars are on screen, so the clock
/// parks as soon as they are gone.
pub struct AvatarMotion {
    start: std::time::Instant,
    /// Where the eyes look: the pointer, or a focused field's caret.
    pub gaze: Option<Point<Pixels>>,
}

impl Default for AvatarMotion {
    fn default() -> Self {
        Self {
            start: std::time::Instant::now(),
            gaze: None,
        }
    }
}

impl AvatarMotion {
    /// Seconds since the clock started.
    pub fn seconds(&self) -> f32 {
        self.start.elapsed().as_secs_f32()
    }
}

/// A shape by key.
pub fn shape(key: &str) -> Option<&'static AvatarShape> {
    SHAPES.iter().find(|s| s.key == key)
}

/// The palette entry whose value is `hex` (`#rrggbb`, case-insensitive).
pub fn palette_color(hex: &str) -> Option<&'static AvatarColor> {
    let wanted = hex.trim().trim_start_matches('#').to_ascii_lowercase();
    PALETTE.iter().find(|c| format!("{:06x}", c.rgb) == wanted)
}

/// Parse a stored `#rrggbb` into a colour, falling back to the first palette
/// entry when the string is not a hex triplet.
pub fn hex_color(hex: &str) -> Hsla {
    let digits = hex.trim().trim_start_matches('#');
    u32::from_str_radix(digits, 16)
        .ok()
        .filter(|_| digits.len() == 6)
        .map(|rgb| gpui::rgb(rgb).into())
        .unwrap_or_else(|| PALETTE[0].hsla())
}

/// A pseudo-random shape, colour and idle loop for a brand new agent. Seeded
/// by the caller so the pick is reproducible in tests; `globe` can never come
/// back because it is not in the table.
pub fn random_pick(seed: u64) -> (&'static AvatarShape, &'static AvatarColor, IdleMotion) {
    // SplitMix64, so three draws off one seed do not correlate.
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) as usize
    };
    let shape = &SHAPES[next() % SHAPES.len()];
    let color = &PALETTE[next() % PALETTE.len()];
    let motion = IdleMotion::ALL[next() % IdleMotion::ALL.len()];
    (shape, color, motion)
}

/// A seed off the wall clock, for "shuffle" and for a freshly opened wizard.
pub fn clock_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x1234_5678)
}

// ---------------------------------------------------------------- path parsing

/// One parsed path segment, in viewBox units and absolute coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    /// Quadratic: control, end.
    Quad(f32, f32, f32, f32),
    /// Cubic: control a, control b, end.
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

/// Parse the subset of SVG path syntax the avatar data uses: absolute `M`,
/// `L`, `C`, `Q` and `Z`, with repeated coordinate sets after one command
/// letter. Anything else ends the parse, so a bad path draws nothing rather
/// than garbage.
pub fn parse_path(d: &str) -> Vec<Seg> {
    let bytes = d.as_bytes();
    let mut at = 0usize;
    let mut out = Vec::new();
    let mut command = 0u8;
    let skip = |at: &mut usize| {
        while *at < bytes.len() && matches!(bytes[*at], b' ' | b',' | b'\t' | b'\n' | b'\r') {
            *at += 1;
        }
    };
    let number = |at: &mut usize| -> Option<f32> {
        skip(at);
        let start = *at;
        if *at < bytes.len() && matches!(bytes[*at], b'-' | b'+') {
            *at += 1;
        }
        while *at < bytes.len() && (bytes[*at].is_ascii_digit() || bytes[*at] == b'.') {
            *at += 1;
        }
        // Exponent form does not occur in the data, but costs nothing to allow.
        if *at < bytes.len() && matches!(bytes[*at], b'e' | b'E') {
            *at += 1;
            if *at < bytes.len() && matches!(bytes[*at], b'-' | b'+') {
                *at += 1;
            }
            while *at < bytes.len() && bytes[*at].is_ascii_digit() {
                *at += 1;
            }
        }
        (start != *at).then(|| d[start..*at].parse().ok()).flatten()
    };
    loop {
        skip(&mut at);
        if at >= bytes.len() {
            break;
        }
        if bytes[at].is_ascii_alphabetic() {
            command = bytes[at];
            at += 1;
        }
        match command {
            b'Z' | b'z' => out.push(Seg::Close),
            b'M' | b'L' => {
                let Some(x) = number(&mut at) else { break };
                let Some(y) = number(&mut at) else { break };
                out.push(if command == b'M' {
                    Seg::Move(x, y)
                } else {
                    Seg::Line(x, y)
                });
                // A repeated M implicitly continues as L.
                if command == b'M' {
                    command = b'L';
                }
            }
            b'Q' => {
                let mut v = [0.0f32; 4];
                for slot in &mut v {
                    let Some(n) = number(&mut at) else {
                        return out;
                    };
                    *slot = n;
                }
                out.push(Seg::Quad(v[0], v[1], v[2], v[3]));
            }
            b'C' => {
                let mut v = [0.0f32; 6];
                for slot in &mut v {
                    let Some(n) = number(&mut at) else {
                        return out;
                    };
                    *slot = n;
                }
                out.push(Seg::Cubic(v[0], v[1], v[2], v[3], v[4], v[5]));
            }
            _ => break,
        }
    }
    out
}

// ------------------------------------------------------------------ gaze maths

/// How far the eyes shift towards `target`, in viewBox units: full deflection
/// at [`GAZE_REACH`] window px and beyond, proportionally less closer in.
pub fn gaze_offset(center: Point<f32>, target: Point<f32>) -> (f32, f32) {
    let (dx, dy) = (target.x - center.x, target.y - center.y);
    let distance = dx.hypot(dy);
    if distance < 0.001 {
        return (0.0, 0.0);
    }
    let reach = (distance / GAZE_REACH).min(1.0) * GAZE_UNITS;
    (dx / distance * reach, dy / distance * reach)
}

/// Where the caret roughly sits in a text field, the way the mockup estimates
/// it: the field's left edge plus 40 px of padding and 7 px per character,
/// never past the field's right edge.
pub fn caret_target(field: Bounds<Pixels>, text_len: usize) -> Point<Pixels> {
    let width = f32::from(field.size.width);
    let run = (40.0 + text_len as f32 * 7.0).min(width);
    point(field.origin.x + px(run), field.origin.y + px(18.0))
}

/// The blink factor (eye scaleY) at `seconds` on the clock: 1.0 while open,
/// [`BLINK_SCALE`] during the blink's [`BLINK_CLOSED_SECONDS`].
fn blink_scale(seconds: f32) -> f32 {
    let phase = seconds.rem_euclid(BLINK_SECONDS);
    if phase < BLINK_CLOSED_SECONDS {
        // Shut and open again inside the window, so it reads as a blink.
        let half = BLINK_CLOSED_SECONDS / 2.0;
        let t = if phase < half {
            phase / half
        } else {
            1.0 - (phase - half) / half
        };
        1.0 - (1.0 - BLINK_SCALE) * t
    } else {
        1.0
    }
}

/// The shuffle pop: 0.85 up through 1.05 and back to 1.0, on an ease-out-back
/// shape. `t` is the progress through [`POP_SECONDS`]; outside 0..1 the figure
/// sits at its resting scale.
pub fn pop_scale(t: f32) -> f32 {
    if !(0.0..1.0).contains(&t) {
        return 1.0;
    }
    // Ease out, so the overshoot lands early and the settle is unhurried.
    let eased = 1.0 - (1.0 - t).powi(3);
    if eased < 0.5 {
        lerp_f32(0.85, 1.05, eased / 0.5)
    } else {
        lerp_f32(1.05, 1.0, (eased - 0.5) / 0.5)
    }
}

/// One blink driven by a hover progress: the eye dips shut as the hover fade
/// crosses its middle and is open again by the time the fade settles.
pub fn blink_from_hover(t: f32) -> f32 {
    const AT: f32 = 0.4;
    const WIDTH: f32 = 0.22;
    let distance = (t - AT).abs();
    if distance >= WIDTH {
        return 1.0;
    }
    1.0 - (1.0 - BLINK_SCALE) * (1.0 - distance / WIDTH)
}

fn lerp_f32(from: f32, to: f32, t: f32) -> f32 {
    from + (to - from) * t
}

/// An OS cursor position (physical screen pixels) in the window's own logical
/// coordinates — the space gpui mouse events use. `origin` is the window's
/// logical position on the virtual desktop ([`gpui::Window::bounds`]).
///
/// gpui only reports mouse moves that land inside the window, so an avatar
/// that should keep watching the pointer past the window edge has to ask the
/// OS itself; the result is deliberately not clamped to the window.
pub fn cursor_to_window(physical: Point<f32>, origin: Point<Pixels>, scale: f32) -> Point<Pixels> {
    let scale = if scale.abs() < 0.001 { 1.0 } else { scale };
    point(
        px(physical.x / scale) - origin.x,
        px(physical.y / scale) - origin.y,
    )
}

/// Where the OS says the pointer is, in this window's logical coordinates.
/// Windows only; elsewhere the caller falls back to gpui's last in-window
/// mouse position.
pub fn os_cursor(window: &gpui::Window) -> Option<Point<Pixels>> {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
        let mut cursor = POINT::default();
        // SAFETY: a plain out-parameter read; failure leaves `cursor` at 0,0
        // and is reported through the returned status.
        if unsafe { GetCursorPos(&mut cursor) }.is_err() {
            return None;
        }
        return Some(cursor_to_window(
            point(cursor.x as f32, cursor.y as f32),
            window.bounds().origin,
            window.scale_factor(),
        ));
    }
    #[cfg(not(windows))]
    {
        let _ = window;
        None
    }
}

// -------------------------------------------------------------------- painting

/// A figure ready to be placed in a layout.
pub struct Avatar {
    /// Shape key; an unknown key paints nothing.
    pub shape: &'static str,
    pub color: Hsla,
    /// Box size in px; the figure fills it.
    pub size: f32,
    pub motion: IdleMotion,
    /// Seconds on the view's [`AvatarMotion`]; `None` paints a still figure.
    pub time: Option<f32>,
    /// Per-figure phase offset in seconds, so a list does not breathe in sync.
    pub phase: f32,
    /// Gaze target in window coordinates.
    pub gaze: Option<Point<Pixels>>,
    /// One-off scale about the figure's centre (the shuffle pop).
    pub pop: f32,
    /// Override for the eye's scaleY; `None` blinks on the shared clock.
    pub blink: Option<f32>,
}

impl Avatar {
    pub fn new(shape: &'static str, color: Hsla, size: f32) -> Self {
        Self {
            shape,
            color,
            size,
            motion: IdleMotion::Float,
            time: None,
            phase: 0.0,
            gaze: None,
            pop: 1.0,
            blink: None,
        }
    }
    pub fn pop(mut self, pop: f32) -> Self {
        self.pop = pop;
        self
    }
    pub fn blink(mut self, blink: Option<f32>) -> Self {
        self.blink = blink;
        self
    }
    pub fn motion(mut self, motion: IdleMotion) -> Self {
        self.motion = motion;
        self
    }
    pub fn time(mut self, time: Option<f32>) -> Self {
        self.time = time;
        self
    }
    pub fn phase(mut self, phase: f32) -> Self {
        self.phase = phase;
        self
    }
    pub fn gaze(mut self, gaze: Option<Point<Pixels>>) -> Self {
        self.gaze = gaze;
        self
    }

    pub fn render(self) -> AnyElement {
        let Some(shape) = shape(self.shape) else {
            return div().flex_none().size(px(self.size)).into_any_element();
        };
        let size = self.size;
        let color = self.color;
        let motion = self.motion;
        let gaze = self.gaze;
        let pop = self.pop;
        let blink = self.blink;
        let seconds = self.time.map(|t| t + self.phase);
        div()
            .flex_none()
            .size(px(size))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        paint_avatar(
                            Painted {
                                shape,
                                color,
                                seconds,
                                motion,
                                gaze,
                                pop,
                                blink,
                            },
                            bounds,
                            window,
                        );
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }
}

/// The affine map from viewBox units to window px for one avatar box.
#[derive(Clone, Copy)]
struct Frame {
    origin: Point<Pixels>,
    scale: f32,
    /// Extra scale about the figure's bottom centre (breathing).
    body_scale: f32,
    /// Extra scale about the figure's centre (the shuffle pop).
    pop: f32,
    /// Shape normalisation scale about [`CENTER`].
    shape_scale: f32,
    /// Idle float, in viewBox units.
    dy: f32,
}

impl Frame {
    /// Map a point in viewBox units to window px.
    fn map(&self, x: f32, y: f32) -> Point<Pixels> {
        // Shape normalisation pivots on the centre of the figure.
        let mut x = CENTER + (x - CENTER) * self.shape_scale;
        let mut y = CENTER + (y - CENTER) * self.shape_scale;
        // Breathing pivots on 50% 100% - the figure's feet.
        x = CENTER + (x - CENTER) * self.body_scale;
        y = FIGURE_BOTTOM + (y - FIGURE_BOTTOM) * self.body_scale;
        // The pop pivots on the middle, so the figure swells in place.
        x = CENTER + (x - CENTER) * self.pop;
        y = CENTER + (y - CENTER) * self.pop;
        y += self.dy;
        point(
            self.origin.x + px((x - VIEW_MIN) * self.scale),
            self.origin.y + px((y - VIEW_MIN) * self.scale),
        )
    }
}

/// The figure's baseline in viewBox units (bodies run 0..228.541).
const FIGURE_BOTTOM: f32 = 228.541;

/// Everything one painted figure needs, so the paint closure stays readable.
#[derive(Clone, Copy)]
struct Painted {
    shape: &'static AvatarShape,
    color: Hsla,
    seconds: Option<f32>,
    motion: IdleMotion,
    gaze: Option<Point<Pixels>>,
    pop: f32,
    blink: Option<f32>,
}

fn paint_avatar(figure: Painted, bounds: Bounds<Pixels>, window: &mut gpui::Window) {
    let Painted {
        shape,
        color,
        seconds,
        motion,
        gaze,
        pop,
        blink: blink_override,
    } = figure;
    let box_size = f32::from(bounds.size.width).min(f32::from(bounds.size.height));
    if box_size <= 0.0 {
        return;
    }
    let (dy, body_scale) = match (seconds, motion) {
        (None, _) => (0.0, 1.0),
        (Some(t), IdleMotion::Float) => (
            -FLOAT_UNITS * (t / FLOAT_SECONDS * std::f32::consts::TAU).sin(),
            1.0,
        ),
        (Some(t), IdleMotion::Breathe) => (
            0.0,
            1.0 + BREATHE_AMPLITUDE * (t / BREATHE_SECONDS * std::f32::consts::TAU).sin(),
        ),
    };
    let frame = Frame {
        origin: bounds.origin,
        scale: box_size / VIEW_SIZE,
        body_scale,
        shape_scale: shape.scale,
        pop,
        dy,
    };
    if let Some(path) = build_path(&parse_path(shape.d), shape.even_odd, &frame) {
        window.paint_path(path, color);
    }
    let ink: Hsla = gpui::rgb(EYE_INK).into();
    for part in shape.parts {
        if let Some(path) = build_path(&parse_path(part.d), part.even_odd, &frame) {
            window.paint_path(path, ink.opacity(part.opacity));
        }
    }
    // The eyes follow the pointer / caret, and blink on the shared clock.
    let (gx, gy) = match gaze {
        Some(target) => {
            let center = bounds.center();
            gaze_offset(
                point(f32::from(center.x), f32::from(center.y)),
                point(f32::from(target.x), f32::from(target.y)),
            )
        }
        None => (0.0, 0.0),
    };
    let blink = blink_override.unwrap_or_else(|| seconds.map(blink_scale).unwrap_or(1.0));
    for eye in &shape.eyes {
        if let Some(path) = build_path(&eye_path(eye, gx, gy, blink), false, &frame) {
            window.paint_path(path, ink);
        }
    }
}

fn build_path(segs: &[Seg], even_odd: bool, frame: &Frame) -> Option<gpui::Path<Pixels>> {
    if segs.is_empty() {
        return None;
    }
    let mut builder = PathBuilder::fill().with_style(PathStyle::Fill(
        FillOptions::default().with_fill_rule(if even_odd {
            FillRule::EvenOdd
        } else {
            FillRule::NonZero
        }),
    ));
    let mut open = false;
    for seg in segs {
        match *seg {
            Seg::Move(x, y) => {
                if open {
                    builder.close();
                }
                builder.move_to(frame.map(x, y));
                open = true;
            }
            Seg::Line(x, y) => builder.line_to(frame.map(x, y)),
            Seg::Quad(cx, cy, x, y) => builder.curve_to(frame.map(x, y), frame.map(cx, cy)),
            Seg::Cubic(ax, ay, bx, by, x, y) => {
                builder.cubic_bezier_to(frame.map(x, y), frame.map(ax, ay), frame.map(bx, by))
            }
            Seg::Close => {
                builder.close();
                open = false;
            }
        }
    }
    if open {
        builder.close();
    }
    builder.build().ok()
}

/// Bezier circle constant: a quarter arc as one cubic.
const KAPPA: f32 = 0.552_284_75;

/// One eye as a rounded rectangle, shifted by the gaze, squashed by a blink
/// and rotated around its own pivot. gpui quads cannot rotate, so the eye is
/// a path like the body.
fn eye_path(eye: &EyeRect, gx: f32, gy: f32, blink: f32) -> Vec<Seg> {
    let height = (eye.h * blink).max(0.01);
    let top = eye.y + (eye.h - height) / 2.0;
    let r = eye.r.min(eye.w / 2.0).min(height / 2.0);
    let (x0, y0) = (eye.x, top);
    let (x1, y1) = (eye.x + eye.w, top + height);
    let (sin, cos) = eye.rot.to_radians().sin_cos();
    let place = |x: f32, y: f32| -> (f32, f32) {
        let (dx, dy) = (x - eye.cx, y - eye.cy);
        (
            eye.cx + dx * cos - dy * sin + gx,
            eye.cy + dx * sin + dy * cos + gy,
        )
    };
    let mut out = Vec::with_capacity(10);
    let k = r * KAPPA;
    let move_to = |x: f32, y: f32, out: &mut Vec<Seg>| {
        let (px_, py) = place(x, y);
        out.push(Seg::Move(px_, py));
    };
    move_to(x0 + r, y0, &mut out);
    let line = |x: f32, y: f32, out: &mut Vec<Seg>| {
        let (a, b) = place(x, y);
        out.push(Seg::Line(a, b));
    };
    let arc = |c1: (f32, f32), c2: (f32, f32), to: (f32, f32), out: &mut Vec<Seg>| {
        let (ax, ay) = place(c1.0, c1.1);
        let (bx, by) = place(c2.0, c2.1);
        let (tx, ty) = place(to.0, to.1);
        out.push(Seg::Cubic(ax, ay, bx, by, tx, ty));
    };
    line(x1 - r, y0, &mut out);
    arc((x1 - r + k, y0), (x1, y0 + r - k), (x1, y0 + r), &mut out);
    line(x1, y1 - r, &mut out);
    arc((x1, y1 - r + k), (x1 - r + k, y1), (x1 - r, y1), &mut out);
    line(x0 + r, y1, &mut out);
    arc((x0 + r - k, y1), (x0, y1 - r + k), (x0, y1 - r), &mut out);
    line(x0, y0 + r, &mut out);
    arc((x0, y0 + r - k), (x0 + r - k, y0), (x0 + r, y0), &mut out);
    out.push(Seg::Close);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_holds_every_shape_but_the_globe() {
        assert_eq!(SHAPES.len(), 25);
        assert!(SHAPES.iter().all(|s| s.key != "globe"));
        assert_eq!(PALETTE.len(), 11);
        assert!(shape("blob").is_some());
        assert!(shape("globe").is_none());
    }

    #[test]
    fn every_shape_parses_into_a_closed_path() {
        for shape in SHAPES {
            let segs = parse_path(shape.d);
            assert!(segs.len() > 3, "{} parsed to {segs:?}", shape.key);
            assert!(
                matches!(segs.first(), Some(Seg::Move(..))),
                "{} does not start with a move",
                shape.key
            );
            assert_eq!(
                segs.last(),
                Some(&Seg::Close),
                "{} is not closed",
                shape.key
            );
            for part in shape.parts {
                assert!(parse_path(part.d).len() > 3);
            }
        }
    }

    #[test]
    fn the_parser_reads_the_commands_the_data_uses() {
        assert_eq!(
            parse_path("M1 2L3 4Z"),
            vec![Seg::Move(1.0, 2.0), Seg::Line(3.0, 4.0), Seg::Close]
        );
        // Repeated coordinate sets after one letter, comma separators,
        // negatives run together with the previous number.
        assert_eq!(
            parse_path("M0,0 C1 2 3 4 5 6 7 8 9 10 11 12"),
            vec![
                Seg::Move(0.0, 0.0),
                Seg::Cubic(1.0, 2.0, 3.0, 4.0, 5.0, 6.0),
                Seg::Cubic(7.0, 8.0, 9.0, 10.0, 11.0, 12.0),
            ]
        );
        assert_eq!(
            parse_path("M1.5 -2.5Q3 4 5 6"),
            vec![Seg::Move(1.5, -2.5), Seg::Quad(3.0, 4.0, 5.0, 6.0)]
        );
        // A second coordinate pair after M continues as a line.
        assert_eq!(
            parse_path("M0 0 10 10"),
            vec![Seg::Move(0.0, 0.0), Seg::Line(10.0, 10.0)]
        );
        // Unsupported commands stop the parse instead of misreading it.
        assert_eq!(parse_path("M0 0A1 1 0 0 1 2 2"), vec![Seg::Move(0.0, 0.0)]);
        assert!(parse_path("").is_empty());
        assert!(parse_path("nonsense").is_empty());
    }

    #[test]
    fn the_gaze_is_clamped_to_fourteen_units() {
        let center = point(100.0, 100.0);
        // Straight right, far away: full deflection on x only.
        let (dx, dy) = gaze_offset(center, point(1000.0, 100.0));
        assert!((dx - GAZE_UNITS).abs() < 0.001, "{dx}");
        assert!(dy.abs() < 0.001);
        // Half the reach deflects half as far.
        let (dx, _) = gaze_offset(center, point(100.0 + GAZE_REACH / 2.0, 100.0));
        assert!((dx - GAZE_UNITS / 2.0).abs() < 0.01, "{dx}");
        // Never longer than the cap, whatever the direction.
        for (x, y) in [(900.0, 900.0), (-900.0, 20.0), (50.0, -800.0)] {
            let (dx, dy) = gaze_offset(center, point(x, y));
            assert!(dx.hypot(dy) <= GAZE_UNITS + 0.001);
        }
        // On top of the eyes: no division by zero, no drift.
        assert_eq!(gaze_offset(center, center), (0.0, 0.0));
    }

    #[test]
    fn the_caret_target_stays_inside_the_field() {
        let field = Bounds {
            origin: point(px(100.0), px(50.0)),
            size: gpui::size(px(200.0), px(36.0)),
        };
        let empty = caret_target(field, 0);
        assert_eq!(empty.x, px(140.0));
        assert_eq!(empty.y, px(68.0));
        assert_eq!(caret_target(field, 10).x, px(210.0));
        // A long value never runs past the right edge.
        assert_eq!(caret_target(field, 500).x, px(300.0));
    }

    #[test]
    fn a_random_pick_is_reproducible_and_never_the_globe() {
        for seed in 0..500u64 {
            let (shape, color, _) = random_pick(seed);
            assert_ne!(shape.key, "globe");
            assert!(SHAPES.iter().any(|s| s.key == shape.key));
            assert!(PALETTE.iter().any(|c| c.id == color.id));
        }
        let first = random_pick(7);
        assert_eq!(first.0.key, random_pick(7).0.key);
        // Distinct seeds do not all collapse onto one shape.
        let spread: std::collections::HashSet<&str> =
            (0..60u64).map(|s| random_pick(s).0.key).collect();
        assert!(spread.len() > 8, "{spread:?}");
    }

    #[test]
    fn colours_round_trip_through_their_hex() {
        for color in PALETTE {
            assert_eq!(palette_color(&color.hex()).map(|c| c.id), Some(color.id));
            assert_eq!(hex_color(&color.hex()), color.hsla());
        }
        assert!(palette_color("#123456").is_none());
        // A broken value falls back instead of painting nothing.
        assert_eq!(hex_color("nope"), PALETTE[0].hsla());
        assert_eq!(hex_color("#FF7A17"), hex_color("#ff7a17"));
    }

    #[test]
    fn a_blink_shuts_the_eyes_briefly_and_reopens_them() {
        assert_eq!(blink_scale(1.0), 1.0);
        assert!(blink_scale(BLINK_CLOSED_SECONDS / 2.0) < 0.2);
        assert_eq!(blink_scale(BLINK_SECONDS - 0.5), 1.0);
        // Once per period, on every period.
        assert!(blink_scale(BLINK_SECONDS * 3.0 + 0.06) < 0.5);
    }

    #[test]
    fn the_shuffle_pop_overshoots_once_and_settles() {
        assert_eq!(pop_scale(-0.1), 1.0);
        assert_eq!(pop_scale(1.0), 1.0);
        assert!((pop_scale(0.0) - 0.85).abs() < 0.001);
        // It passes through the overshoot and comes back down.
        let peak = (0..100)
            .map(|i| pop_scale(i as f32 / 100.0))
            .fold(0.0f32, f32::max);
        assert!((peak - 1.05).abs() < 0.01, "{peak}");
        assert!(pop_scale(0.98) > 0.99 && pop_scale(0.98) <= 1.05);
    }

    #[test]
    fn a_hover_blinks_the_tile_once() {
        assert_eq!(blink_from_hover(0.0), 1.0);
        assert_eq!(blink_from_hover(1.0), 1.0);
        assert!(blink_from_hover(0.4) < 0.2);
        // Only one dip, not a flutter.
        let shut: Vec<f32> = (0..=100)
            .map(|i| i as f32 / 100.0)
            .filter(|t| blink_from_hover(*t) < 0.5)
            .collect();
        assert!(!shut.is_empty());
        assert!(shut.last().unwrap() - shut.first().unwrap() < 0.35);
    }

    #[test]
    fn the_os_cursor_converts_into_window_logical_coordinates() {
        let origin = point(px(100.0), px(50.0));
        // 1x display: physical screen px minus the window's logical origin.
        assert_eq!(
            cursor_to_window(point(150.0, 90.0), origin, 1.0),
            point(px(50.0), px(40.0))
        );
        // 2x display: physical px are twice the logical ones.
        assert_eq!(
            cursor_to_window(point(300.0, 200.0), origin, 2.0),
            point(px(50.0), px(50.0))
        );
        // Outside the window the result goes negative - never clamped, so the
        // eyes keep turning once the pointer has left.
        let outside = cursor_to_window(point(0.0, 0.0), origin, 1.0);
        assert_eq!(outside, point(px(-100.0), px(-50.0)));
        // A nonsense scale factor must not produce infinities.
        assert!(f32::from(cursor_to_window(point(10.0, 10.0), origin, 0.0).x).is_finite());
    }

    #[test]
    fn an_eye_is_a_closed_rounded_rectangle() {
        let eye = SHAPES[0].eyes[0];
        let open = eye_path(&eye, 0.0, 0.0, 1.0);
        assert!(matches!(open.first(), Some(Seg::Move(..))));
        assert_eq!(open.last(), Some(&Seg::Close));
        // The gaze shifts every point by the same vector.
        let moved = eye_path(&eye, 5.0, -3.0, 1.0);
        match (open.first(), moved.first()) {
            (Some(Seg::Move(ax, ay)), Some(Seg::Move(bx, by))) => {
                assert!((bx - ax - 5.0).abs() < 0.001);
                assert!((by - ay + 3.0).abs() < 0.001);
            }
            _ => panic!("expected a move"),
        }
        // A blink keeps the eye centred on its own middle.
        let shut = eye_path(&eye, 0.0, 0.0, BLINK_SCALE);
        assert_eq!(shut.len(), open.len());
    }
}

/// The saved automation face, using the same legacy fallback as its editor.
pub fn automation_face(identity: &zeron_proto::ChatAutomation, size: f32) -> AnyElement {
    let seed = identity.id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3));
    let fallback = random_pick(seed);
    let saved = identity.avatar.as_ref();
    let key = saved.and_then(|a| shape(&a.shape)).map(|s| s.key).unwrap_or(fallback.0.key);
    let color = saved.map(|a| hex_color(&a.color)).unwrap_or_else(|| fallback.1.hsla());
    Avatar::new(key, color, size).render()
}
