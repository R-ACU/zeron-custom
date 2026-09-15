//! The composer picker's EFFORT slider: the horizontal replacement for the
//! old reasoning ladder rows (user request, modeled on ChatGPT's model sheet):
//! a rounded track filled with a provider-colored gradient up to a large white
//! thumb, the ladder's level names as small ticks underneath, and a field of
//! drifting sparkles inside the fill at the top rung.
//!
//! The data model is untouched: the steps ARE the `ReasoningLevel` ladder the
//! harness advertises for the selected model (`Model::reasoning_levels`,
//! falling back to the harness descriptor's), so the number of steps and their
//! names vary per model and a ladder-less model (Claude Haiku) gets no slider
//! at all.
//!
//! Geometry is DETERMINISTIC (a fixed track width passed in by the caller, who
//! knows the popover's width) so the first painted frame is already correct;
//! the measured track bounds are only needed to map a pointer x onto a step.
//! Everything below the element builder is pure and unit-tested.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{AnyElement, Bounds, Context, Hsla, Pixels, SharedString, div, prelude::*, px};
use zeron_proto::{HarnessId, ReasoningLevel};

use crate::motion;
use crate::theme::Theme;

/// Track height (the rounded rail the fill rides in).
pub const TRACK_HEIGHT: f32 = 11.0;
/// Thumb diameter, deliberately larger than the track (ChatGPT-style).
pub const THUMB_SIZE: f32 = 22.0;
/// The control's name in the UI. "Effort", never "Reasoning" (user request):
/// the ladder is what the run spends, not what it thinks.
pub const EFFORT_LABEL: &str = "Effort";

/// What a ladder-less model (Claude Haiku) reads instead of a slider.
pub const NO_EFFORT_HINT: &str = "No effort control for this model";

/// Sparkle count at the top rung.
pub const SPARKLE_COUNT: usize = 14;
/// Sparkle alpha band (dimmest .. brightest).
pub const SPARKLE_MIN_ALPHA: f32 = 0.35;
pub const SPARKLE_MAX_ALPHA: f32 = 0.9;
/// Twinkle cycles per drift cycle: the dots blink faster than they travel.
const SPARKLE_TWINKLE_CYCLES: f32 = 3.0;
/// How far a dot travels per drift cycle, as a fraction of the filled track
/// (a 6s cycle over a ~230px fill works out to roughly 6px per second).
const SPARKLE_DRIFT_SPAN: f32 = 0.16;

// ---------------------------------------------------------------------------
// Pure: steps
// ---------------------------------------------------------------------------

/// Fill fraction (0..1) for step `ix` of `steps`. A single-step ladder reads
/// as full, because there is nothing to compare it against.
pub fn step_fraction(ix: usize, steps: usize) -> f32 {
    if steps <= 1 {
        return 1.0;
    }
    (ix.min(steps - 1) as f32) / (steps - 1) as f32
}

/// The step a pointer at `x` (relative to the track's left edge, in px) lands
/// on: the NEAREST rung, never the one it happens to be past. Out-of-range
/// values clamp to the ends so a drag that leaves the track still tracks.
pub fn snap_step(x: f32, width: f32, steps: usize) -> usize {
    if steps <= 1 || width <= 0.0 {
        return 0;
    }
    let fraction = (x / width).clamp(0.0, 1.0);
    (fraction * (steps - 1) as f32).round() as usize
}

// ---------------------------------------------------------------------------
// Pure: labels
// ---------------------------------------------------------------------------

/// The abbreviated tick label for a level. Long ladders (Claude's seven-rung
/// one) would otherwise overlap under a 250px track.
pub fn short_level_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Min",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Med",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-Hi",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Code",
        ReasoningLevel::Ultrathink => "Think",
    }
}

/// Above this many rungs the ticks switch to [`short_level_label`].
const SHORT_LABEL_THRESHOLD: usize = 4;

/// Tick captions for a ladder: the full level names while they fit, the
/// abbreviations once the ladder grows past four rungs. The CURRENT level is
/// always spelled out in full above the track, so nothing is lost.
pub fn tick_labels(levels: &[ReasoningLevel]) -> Vec<&'static str> {
    levels
        .iter()
        .map(|level| {
            if levels.len() > SHORT_LABEL_THRESHOLD {
                short_level_label(*level)
            } else {
                crate::pickers::reasoning_label(*level)
            }
        })
        .collect()
}

/// Split a model label into its tier word and the rest ("Opus 5" → "Opus" +
/// " 5", "GPT-5.5 Codex" → "GPT-5.5" + " Codex"). The tier word wears the
/// provider color on the card, exactly like ChatGPT's sheet.
pub fn split_tier(label: &str) -> (&str, &str) {
    match label.find(' ') {
        Some(at) => (&label[..at], &label[at..]),
        None => (label, ""),
    }
}

// ---------------------------------------------------------------------------
// Pure: provider colors
// ---------------------------------------------------------------------------

/// The gradient a harness fills its track with: Claude's warm coral, the
/// OpenAI/Codex purple, and the theme accent for every other agent (Cursor,
/// Devin, Grok, Hermes, Pi, Kimi, opencode), none of which has a mark color
/// of its own in this UI, so the accent keeps the card coherent.
pub fn provider_gradient(harness: Option<HarnessId>, theme: &Theme) -> (Hsla, Hsla) {
    match harness {
        // The mock harness scripts Claude-flavoured runs and wears the Claude
        // mark everywhere else; keep it on the same color here.
        Some(HarnessId::ClaudeCode) | Some(HarnessId::Mock) => {
            (gpui::rgb(0x8F4A36).into(), gpui::rgb(0xD97757).into())
        }
        Some(HarnessId::Codex) => (gpui::rgb(0x5B21B6).into(), gpui::rgb(0xA855F7).into()),
        _ => (
            motion::mix(theme.accent, gpui::black(), 0.45),
            theme.accent,
        ),
    }
}

/// The tier word's color on the card: the gradient's bright end.
pub fn provider_ink(harness: Option<HarnessId>, theme: &Theme) -> Hsla {
    provider_gradient(harness, theme).1
}

// ---------------------------------------------------------------------------
// Pure: sparkles
// ---------------------------------------------------------------------------

/// One dot of the top-rung sparkle field. Positions are fractions of the
/// FILLED track (x) and of the track's inner height (y).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sparkle {
    pub x: f32,
    pub y: f32,
    /// Dot diameter in px (2..3).
    pub size: f32,
    /// Twinkle phase offset (0..1) so the dots never blink in unison.
    pub phase: f32,
}

/// A deterministic sparkle field (fixed seed, so captures and tests are
/// stable): a small LCG spread over the filled track.
pub fn sparkle_field(count: usize) -> Vec<Sparkle> {
    let mut state: u32 = 0x5EED_1234;
    let mut next = || {
        // Numerical Recipes LCG: tiny, deterministic, good enough for dust.
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((state >> 8) & 0xFF_FFFF) as f32 / 16_777_216.0
    };
    (0..count)
        .map(|_| Sparkle {
            x: next(),
            y: next(),
            size: 2.0 + next(),
            phase: next(),
        })
        .collect()
}

/// A dot's alpha at drift phase `t` (0..1): a sine twinkle mapped onto
/// [`SPARKLE_MIN_ALPHA`]..[`SPARKLE_MAX_ALPHA`].
pub fn sparkle_alpha(phase: f32, t: f32) -> f32 {
    let wave = (std::f32::consts::TAU * (SPARKLE_TWINKLE_CYCLES * t + phase)).sin();
    SPARKLE_MIN_ALPHA + (SPARKLE_MAX_ALPHA - SPARKLE_MIN_ALPHA) * (0.5 + 0.5 * wave)
}

/// A dot's x fraction at drift phase `t`: a slow leftward-to-rightward crawl
/// that wraps inside the fill rather than piling up at one edge.
pub fn sparkle_x(base: f32, t: f32) -> f32 {
    (base + SPARKLE_DRIFT_SPAN * t).rem_euclid(1.0)
}

// ---------------------------------------------------------------------------
// Element
// ---------------------------------------------------------------------------

/// Drag payload for a held slider thumb (same shape as the picker's scrollbar
/// drag: gpui keeps dispatching `on_drag_move` to the owning view even after
/// the pointer has left the track).
#[derive(Clone)]
pub struct EffortDrag;

pub struct EffortDragGhost;

impl gpui::Render for EffortDragGhost {
    fn render(&mut self, _: &mut gpui::Window, _: &mut Context<Self>) -> impl IntoElement {
        // An invisible ghost: the thumb itself is the drag feedback.
        div().w(px(0.0)).h(px(0.0))
    }
}

/// Everything the slider paints. The caller owns the data model and hands over
/// resolved values only.
pub struct EffortSlider {
    /// Element-id prefix, also the tween key, so two mounted sliders never
    /// share a glide.
    pub id: &'static str,
    pub levels: Vec<ReasoningLevel>,
    /// Index of the current level within `levels`.
    pub current: usize,
    pub harness: Option<HarnessId>,
    /// The track's painted width in px (the caller knows the card's width).
    pub width: f32,
    /// Drift phase (0..1) for the sparkle field, or `None` when the current
    /// level is not the top rung or motion is reduced.
    pub sparkle_t: Option<f32>,
    /// Bounds sink for pointer mapping, written during prepaint.
    pub bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl EffortSlider {
    /// Paint the track, fill, thumb and ticks. Mouse/keyboard wiring stays
    /// with the caller (it owns the picker's state); this returns the styled
    /// body to attach listeners to.
    pub fn render<V: 'static>(self, theme: &Theme, cx: &mut Context<V>) -> AnyElement {
        let steps = self.levels.len();
        let target = step_fraction(self.current, steps);
        let reduced = motion::reduced_motion(cx);
        let key = format!("{}-fill", self.id);
        let fraction = motion::value_tween(&key, target, &motion::EFFORT_SLIDE, reduced);
        if motion::value_tween_active(&key) {
            // Keep frames coming until the glide lands (the shared 30fps
            // clock, never a per-frame animation element).
            motion::pulse_lease(cx.entity_id(), cx);
        }
        let (from, to) = provider_gradient(self.harness, theme);
        let travel = (self.width - THUMB_SIZE).max(0.0);
        let center = THUMB_SIZE / 2.0 + fraction * travel;

        let mut fill = div()
            .absolute()
            .left_0()
            .top_0()
            .bottom_0()
            .w(px(center))
            .rounded(px(TRACK_HEIGHT / 2.0))
            .overflow_hidden()
            .bg(gpui::linear_gradient(
                90.0,
                gpui::linear_color_stop(from, 0.0),
                gpui::linear_color_stop(to, 1.0),
            ));
        if let Some(t) = self.sparkle_t {
            for (ix, dot) in sparkle_field(SPARKLE_COUNT).into_iter().enumerate() {
                let x = sparkle_x(dot.x, t) * (center - dot.size).max(0.0);
                let y = dot.y * (TRACK_HEIGHT - dot.size).max(0.0);
                fill = fill.child(
                    div()
                        .id(("effort-sparkle", ix))
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(dot.size))
                        .h(px(dot.size))
                        .rounded_full()
                        .bg(gpui::white().opacity(sparkle_alpha(dot.phase, t))),
                );
            }
        }

        let measured = self.bounds.clone();
        let track = div()
            .absolute()
            .left_0()
            .top(px((THUMB_SIZE - TRACK_HEIGHT) / 2.0))
            .w(px(self.width))
            .h(px(TRACK_HEIGHT))
            .rounded(px(TRACK_HEIGHT / 2.0))
            .bg(crate::theme::ink(0.07))
            .border_1()
            .border_color(crate::theme::hairline(0.10))
            .child(fill)
            .child(
                gpui::canvas(
                    move |bounds, _, _| measured.set(Some(bounds)),
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            );

        let thumb = div()
            .absolute()
            .top_0()
            .left(px(center - THUMB_SIZE / 2.0))
            .w(px(THUMB_SIZE))
            .h(px(THUMB_SIZE))
            .rounded_full()
            .bg(gpui::white())
            .shadow(vec![gpui::BoxShadow {
                color: gpui::hsla(0.0, 0.0, 0.0, 0.35),
                offset: gpui::point(px(0.0), px(1.0)),
                blur_radius: px(4.0),
                spread_radius: px(0.0),
                inset: false,
            }]);

        // The rungs live between the thumb's two extreme centers, so the tick
        // row is inset by half a thumb on each side; otherwise the end
        // captions drift away from the positions they name.
        let ticks = div()
            .w(px(self.width))
            .px(px(THUMB_SIZE / 2.0 - 4.0))
            .flex()
            .flex_row()
            .items_start()
            .justify_between()
            .pt(px(6.0))
            .children(tick_labels(&self.levels).into_iter().enumerate().map(
                |(ix, label)| {
                    div()
                        .flex_none()
                        .text_size(crate::typography::ui_rems(9.5))
                        .font_weight(if ix == self.current {
                            gpui::FontWeight::SEMIBOLD
                        } else {
                            gpui::FontWeight::NORMAL
                        })
                        .text_color(if ix == self.current {
                            theme.text.opacity(0.9)
                        } else {
                            theme.text_muted.opacity(0.55)
                        })
                        .child(SharedString::from(label))
                },
            ));

        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .w(px(self.width))
                    .h(px(THUMB_SIZE))
                    .child(track)
                    .child(thumb),
            )
            .child(ticks)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_is_called_effort_everywhere() {
        assert_eq!(EFFORT_LABEL, "Effort");
        assert_eq!(NO_EFFORT_HINT, "No effort control for this model");
        assert!(
            !NO_EFFORT_HINT.to_lowercase().contains("reasoning"),
            "the UI never says Reasoning"
        );
    }

    #[test]
    fn step_fractions_span_the_track() {
        assert_eq!(step_fraction(0, 5), 0.0);
        assert_eq!(step_fraction(4, 5), 1.0);
        assert!((step_fraction(2, 5) - 0.5).abs() < 1e-6);
        // Single-rung ladders read as full rather than empty.
        assert_eq!(step_fraction(0, 1), 1.0);
        // Out-of-range indices clamp.
        assert_eq!(step_fraction(9, 3), 1.0);
    }

    #[test]
    fn snap_picks_the_nearest_rung_and_clamps_outside_the_track() {
        // Four rungs over 300px: boundaries at 50, 150, 250.
        assert_eq!(snap_step(0.0, 300.0, 4), 0);
        assert_eq!(snap_step(49.0, 300.0, 4), 0);
        assert_eq!(snap_step(51.0, 300.0, 4), 1);
        assert_eq!(snap_step(149.0, 300.0, 4), 1);
        assert_eq!(snap_step(151.0, 300.0, 4), 2);
        assert_eq!(snap_step(300.0, 300.0, 4), 3);
        // A drag that left the track still tracks its nearest end.
        assert_eq!(snap_step(-80.0, 300.0, 4), 0);
        assert_eq!(snap_step(900.0, 300.0, 4), 3);
        // Degenerate inputs never panic or divide by zero.
        assert_eq!(snap_step(10.0, 0.0, 4), 0);
        assert_eq!(snap_step(10.0, 300.0, 1), 0);
        assert_eq!(snap_step(10.0, 300.0, 0), 0);
    }

    #[test]
    fn snap_round_trips_every_rung_of_a_claude_ladder() {
        let steps = 7;
        let width = 250.0;
        for ix in 0..steps {
            let center = step_fraction(ix, steps) * width;
            assert_eq!(snap_step(center, width, steps), ix, "rung {ix}");
        }
    }

    #[test]
    fn tick_labels_shorten_only_for_long_ladders() {
        use ReasoningLevel::*;
        assert_eq!(
            tick_labels(&[Low, Medium, High, Max]),
            vec!["Low", "Medium", "High", "Max"]
        );
        assert_eq!(
            tick_labels(&[Low, Medium, High, XHigh, Max, Ultracode, Ultrathink]),
            vec!["Low", "Med", "High", "X-Hi", "Max", "Code", "Think"]
        );
        assert!(tick_labels(&[]).is_empty());
    }

    #[test]
    fn tier_word_splits_off_the_model_label() {
        assert_eq!(split_tier("Opus 5"), ("Opus", " 5"));
        assert_eq!(split_tier("Haiku 4.5"), ("Haiku", " 4.5"));
        assert_eq!(split_tier("GPT-5.5 Codex"), ("GPT-5.5", " Codex"));
        assert_eq!(split_tier("Sonnet"), ("Sonnet", ""));
    }

    #[test]
    fn sparkle_field_is_deterministic_and_inside_the_track() {
        let a = sparkle_field(SPARKLE_COUNT);
        let b = sparkle_field(SPARKLE_COUNT);
        assert_eq!(a, b, "same seed, same field");
        assert_eq!(a.len(), SPARKLE_COUNT);
        assert!(
            (12..=18).contains(&SPARKLE_COUNT),
            "the field stays in the 12..18 band"
        );
        for dot in &a {
            assert!((0.0..1.0).contains(&dot.x), "x {}", dot.x);
            assert!((0.0..1.0).contains(&dot.y), "y {}", dot.y);
            assert!((2.0..3.0).contains(&dot.size), "size {}", dot.size);
            assert!((0.0..1.0).contains(&dot.phase), "phase {}", dot.phase);
        }
        // Not all dots stacked on one spot.
        assert!(a.iter().any(|d| (d.x - a[0].x).abs() > 0.1));
    }

    #[test]
    fn sparkle_alpha_stays_inside_its_band_and_oscillates() {
        let mut min = f32::MAX;
        let mut max = f32::MIN;
        for step in 0..240 {
            let t = step as f32 / 240.0;
            let a = sparkle_alpha(0.0, t);
            assert!(
                (SPARKLE_MIN_ALPHA - 1e-5..=SPARKLE_MAX_ALPHA + 1e-5).contains(&a),
                "alpha {a} out of band"
            );
            min = min.min(a);
            max = max.max(a);
        }
        assert!((min - SPARKLE_MIN_ALPHA).abs() < 1e-3, "reaches the floor");
        assert!((max - SPARKLE_MAX_ALPHA).abs() < 1e-3, "reaches the ceiling");
        // The phase offset de-synchronizes the dots.
        assert!((sparkle_alpha(0.0, 0.0) - sparkle_alpha(0.25, 0.0)).abs() > 0.1);
    }

    #[test]
    fn sparkles_drift_and_wrap_inside_the_fill() {
        assert!((sparkle_x(0.5, 0.0) - 0.5).abs() < 1e-6);
        let drifted = sparkle_x(0.5, 1.0);
        assert!(drifted > 0.5, "drifts forward: {drifted}");
        // Wraps rather than running off the end.
        let wrapped = sparkle_x(0.98, 1.0);
        assert!((0.0..1.0).contains(&wrapped), "wrapped {wrapped}");
    }
}
