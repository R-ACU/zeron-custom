//! The notice strip docked on the top edge of the composer card.
//!
//! A tiny generic system: the shell collects zero or more [`ComposerNotice`]s
//! for the harness the composer would run right now, and renders the first one
//! whose [`ComposerNotice::key`] is not in the dismissed set. Keys are the whole
//! dismissal memory — they carry the window's reset time, so dismissing a
//! notice hides it until the next threshold or the next window, and nothing is
//! persisted across app runs.
//!
//! Everything in this module above the render function is pure so the threshold
//! arithmetic, the wording and the keys are unit-testable without a window.

use chrono::{DateTime, Local, Utc};
use gpui::{App, Hsla, SharedString, Styled as _, Window, div, prelude::*, px};
use zeron_proto::HarnessId;

use crate::theme::Theme;

/// Subscription usage is announced when it first reaches one of these percents.
/// Each one fires once per window (the reset time is part of the notice key).
pub const USAGE_THRESHOLDS: [u8; 3] = [60, 75, 90];

/// Highest announced threshold the fraction has reached, if any. Pure.
pub fn crossed_threshold(fraction: f32) -> Option<u8> {
    if !fraction.is_finite() {
        return None;
    }
    let percent = fraction * 100.0;
    USAGE_THRESHOLDS
        .iter()
        .copied()
        .rev()
        .find(|t| percent + 1e-4 >= f32::from(*t))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    Info,
    Warning,
    Critical,
}

impl NoticeSeverity {
    /// Tint source. Warning follows the same theme token the Accounts usage
    /// meters use (`settings::accounts::usage_color`).
    pub fn color(self, theme: &Theme) -> Hsla {
        match self {
            NoticeSeverity::Info => theme.accent,
            NoticeSeverity::Warning => theme.warning,
            NoticeSeverity::Critical => theme.danger,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            NoticeSeverity::Info => crate::icons::INFO_CIRCLE,
            NoticeSeverity::Warning | NoticeSeverity::Critical => crate::icons::DANGER_TRIANGLE,
        }
    }
}

/// One subscription usage window that has crossed an announced threshold.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageNotice {
    pub harness: HarnessId,
    /// Window name as the engine reported it ("Session", "Week").
    pub window_label: String,
    pub threshold: u8,
    pub fraction: f32,
    pub resets_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComposerNotice {
    Usage(UsageNotice),
    /// The selected harness reports no linked account, so no usage can be
    /// shown at all. Info only, and keyed per harness so it appears once.
    NoAccount { harness: HarnessId },
}

/// Display name of a harness, for notice wording. Pure, so the strip needs no
/// loaded harness catalog to say who it is talking about.
pub fn harness_label(harness: HarnessId) -> &'static str {
    match harness {
        HarnessId::ClaudeCode => "Claude Code",
        HarnessId::Codex => "Codex",
        HarnessId::Cursor => "Cursor",
        HarnessId::Devin => "Devin",
        HarnessId::Grok => "Grok",
        HarnessId::Hermes => "Hermes",
        HarnessId::Pi => "Pi",
        HarnessId::Kimi => "Kimi",
        HarnessId::Cline => "Cline",
        HarnessId::Opencode => "opencode",
        HarnessId::Mock => "Mock",
    }
}

impl ComposerNotice {
    pub fn severity(&self) -> NoticeSeverity {
        match self {
            ComposerNotice::Usage(usage) if usage.threshold >= 90 => NoticeSeverity::Critical,
            ComposerNotice::Usage(_) => NoticeSeverity::Warning,
            ComposerNotice::NoAccount { .. } => NoticeSeverity::Info,
        }
    }

    pub fn text(&self) -> String {
        match self {
            ComposerNotice::Usage(usage) => {
                let mut text = format!(
                    "{} {} usage at {} % of your limit",
                    harness_label(usage.harness),
                    usage.window_label.to_lowercase(),
                    usage.threshold
                );
                if let Some(resets_at) = usage.resets_at {
                    text.push_str(&format!(
                        ", resets at {}",
                        resets_at.with_timezone(&Local).format("%H:%M")
                    ));
                }
                text
            }
            ComposerNotice::NoAccount { harness } => format!(
                "Sign in to {} in Settings > Accounts to see usage",
                harness_label(*harness)
            ),
        }
    }

    /// Dismissal identity. Stable while the notice means the same thing, and
    /// different as soon as a higher threshold or a new window applies.
    pub fn key(&self) -> String {
        match self {
            ComposerNotice::Usage(usage) => format!(
                "usage:{:?}:{}:{}:{}",
                usage.harness,
                usage.window_label,
                usage.threshold,
                usage
                    .resets_at
                    .map(|r| r.timestamp_millis().to_string())
                    .unwrap_or_else(|| "open".into())
            ),
            ComposerNotice::NoAccount { harness } => format!("no-account:{harness:?}"),
        }
    }
}

/// Build the notice list for one harness out of its usage windows. `None` for
/// `windows` means "no linked account / nothing probed".
pub fn notices_for(
    harness: HarnessId,
    windows: Option<&[zeron_proto::AgentUsageWindow]>,
) -> Vec<ComposerNotice> {
    let Some(windows) = windows else {
        // These harnesses route through provider keys or their own login and
        // expose no subscription-usage account in Zeron Settings.
        if matches!(harness, HarnessId::Pi | HarnessId::Opencode | HarnessId::Cline) {
            return Vec::new();
        }
        return vec![ComposerNotice::NoAccount { harness }];
    };
    let mut notices: Vec<ComposerNotice> = windows
        .iter()
        .filter_map(|window| {
            let threshold = crossed_threshold(window.used_fraction)?;
            Some(ComposerNotice::Usage(UsageNotice {
                harness,
                window_label: window.label.clone(),
                threshold,
                fraction: window.used_fraction,
                resets_at: window.resets_at,
            }))
        })
        .collect();
    // Loudest first: the window closest to its ceiling is the one to show.
    notices.sort_by(|a, b| severity_rank(b).cmp(&severity_rank(a)));
    notices
}

/// What the strip should be built from, given one harness's entry in the
/// shell's usage map and whether a probe has landed yet. Pure.
///
/// - `None` — stay silent (nothing probed yet).
/// - `Some(None)` — the harness has NO signed-in account: the `NoAccount` notice.
/// - `Some(Some(windows))` — a signed-in account's windows. An EMPTY slice is a
///   signed-in account whose CLI exposes no usage (Kimi), which must never read
///   as "not signed in".
pub fn usage_source<'a>(
    entry: Option<&'a [zeron_proto::AgentUsageWindow]>,
    probed: bool,
) -> Option<Option<&'a [zeron_proto::AgentUsageWindow]>> {
    match entry {
        Some(windows) => Some(Some(windows)),
        None if probed => Some(None),
        None => None,
    }
}

fn severity_rank(notice: &ComposerNotice) -> u8 {
    match notice {
        ComposerNotice::Usage(usage) => usage.threshold,
        ComposerNotice::NoAccount { .. } => 0,
    }
}

/// Height of the strip; the shell's bottom-stack canvas measures the composer
/// stack, so reserving transcript clearance needs no extra bookkeeping.
pub const NOTICE_HEIGHT: f32 = 30.0;
/// Distance from the composer column's edge: the card's own inset
/// (`Theme::SPACE_LG`) plus 24px, so the strip's edges sit visibly inside the
/// card's rounded top corners and nothing of it can read as sticking out.
const SIDE_INSET: f32 = crate::theme::Theme::SPACE_LG + 24.0;

/// The strip itself: icon, message, dismiss button. `on_dismiss` receives the
/// notice key.
pub fn render(
    notice: &ComposerNotice,
    theme: &Theme,
    on_dismiss: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
) -> gpui::Stateful<gpui::Div> {
    let severity = notice.severity();
    let color = severity.color(theme);
    div()
        .id("composer-notice")
        .flex_none()
        // The card itself is inset by the composer container's horizontal
        // padding (`Theme::SPACE_LG`); the strip adds another 24px on each
        // side, so it is always clearly narrower than the card.
        .mx(px(SIDE_INSET))
        // Sits ON the card like a tab: round top corners, flat bottom resting
        // on the card's straight top edge. No overlap — tucking it under the
        // card drags its tint through the card's backdrop blur.
        .h(px(NOTICE_HEIGHT))
        .rounded_t(px(12.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(7.0))
        .bg(color.opacity(0.16))
        .border_1()
        .border_color(color.opacity(0.28))
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(color)
        // Never let the strip hand a click to the titlebar drag region or the
        // card underneath.
        .occlude()
        .child(
            crate::icons::icon(severity.icon())
                .size(px(13.0))
                .flex_none()
                .text_color(color),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(SharedString::from(notice.text())),
        )
        .child(
            div()
                .id("composer-notice-dismiss")
                .flex_none()
                .size(px(18.0))
                .rounded(px(5.0))
                .flex()
                .items_center()
                .justify_center()
                .hover(|s| s.bg(color.opacity(0.18)))
                .cursor_pointer()
                .child(
                    crate::icons::icon(crate::icons::CLOSE)
                        .size(px(11.0))
                        .text_color(color),
                )
                .on_mouse_down(gpui::MouseButton::Left, on_dismiss),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routed_agents_do_not_advertise_nonexistent_usage_login() {
        for harness in [HarnessId::Pi, HarnessId::Opencode, HarnessId::Cline] {
            assert!(notices_for(harness, None).is_empty());
        }
        assert!(!notices_for(HarnessId::Codex, None).is_empty());
    }


    fn window(label: &str, fraction: f32) -> zeron_proto::AgentUsageWindow {
        zeron_proto::AgentUsageWindow {
            label: label.into(),
            used_fraction: fraction,
            resets_at: None,
        }
    }

    #[test]
    fn notice_thresholds_fire_at_60_75_and_90() {
        assert_eq!(crossed_threshold(0.0), None);
        assert_eq!(crossed_threshold(0.59), None);
        assert_eq!(crossed_threshold(0.60), Some(60));
        assert_eq!(crossed_threshold(0.74), Some(60));
        assert_eq!(crossed_threshold(0.75), Some(75));
        assert_eq!(crossed_threshold(0.89), Some(75));
        assert_eq!(crossed_threshold(0.90), Some(90));
        assert_eq!(crossed_threshold(1.4), Some(90));
        assert_eq!(crossed_threshold(f32::NAN), None);
    }

    #[test]
    fn notice_severity_escalates_only_at_the_top_threshold() {
        let at = |f| {
            notices_for(HarnessId::ClaudeCode, Some(&[window("Session", f)]))
                .remove(0)
                .severity()
        };
        assert_eq!(at(0.60), NoticeSeverity::Warning);
        assert_eq!(at(0.75), NoticeSeverity::Warning);
        assert_eq!(at(0.95), NoticeSeverity::Critical);
    }

    #[test]
    fn notice_key_changes_per_threshold_and_per_window_reset() {
        let key = |f, resets_at| {
            ComposerNotice::Usage(UsageNotice {
                harness: HarnessId::ClaudeCode,
                window_label: "Session".into(),
                threshold: crossed_threshold(f).unwrap(),
                fraction: f,
                resets_at,
            })
            .key()
        };
        let reset = DateTime::from_timestamp(1_700_000_000, 0);
        // Same window, same threshold: one dismissal holds.
        assert_eq!(key(0.61, reset), key(0.70, reset));
        // Next threshold speaks up again.
        assert_ne!(key(0.70, reset), key(0.80, reset));
        // So does the next window, at the same threshold.
        assert_ne!(
            key(0.80, reset),
            key(0.80, DateTime::from_timestamp(1_700_018_000, 0))
        );
    }

    #[test]
    fn notice_text_names_the_window_and_the_reset_time() {
        let text = ComposerNotice::Usage(UsageNotice {
            harness: HarnessId::ClaudeCode,
            window_label: "Session".into(),
            threshold: 75,
            fraction: 0.76,
            resets_at: None,
        })
        .text();
        assert_eq!(text, "Claude Code session usage at 75 % of your limit");
        assert!(!text.contains("resets"));
    }

    #[test]
    fn notice_without_an_account_is_info_and_fires_once_per_harness() {
        let notices = notices_for(HarnessId::ClaudeCode, None);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].severity(), NoticeSeverity::Info);
        assert_eq!(notices[0].key(), "no-account:ClaudeCode");
        assert!(notices[0].text().contains("Settings > Accounts"));
    }

    #[test]
    fn notice_no_account_only_without_a_signed_in_account() {
        // Nothing probed yet: silence.
        assert_eq!(usage_source(None, false), None);
        // Probed, no account for this harness: the sign-in hint.
        assert_eq!(usage_source(None, true), Some(None));
        assert_eq!(
            notices_for(HarnessId::Kimi, usage_source(None, true).unwrap()),
            vec![ComposerNotice::NoAccount {
                harness: HarnessId::Kimi
            }]
        );
        // Signed in, but the CLI exposes no usage windows: no notice at all —
        // never the contradictory "Sign in to Kimi" hint.
        let signed_in_without_usage: &[zeron_proto::AgentUsageWindow] = &[];
        assert_eq!(
            usage_source(Some(signed_in_without_usage), true),
            Some(Some(signed_in_without_usage))
        );
        assert!(
            notices_for(
                HarnessId::Kimi,
                usage_source(Some(signed_in_without_usage), true).unwrap()
            )
            .is_empty()
        );
    }

    #[test]
    fn notice_list_is_empty_below_the_first_threshold_and_loudest_first() {
        assert!(
            notices_for(
                HarnessId::ClaudeCode,
                Some(&[window("Session", 0.1), window("Week", 0.42)])
            )
            .is_empty()
        );
        let notices = notices_for(
            HarnessId::ClaudeCode,
            Some(&[window("Session", 0.61), window("Week", 0.93)]),
        );
        assert_eq!(notices.len(), 2);
        assert!(notices[0].text().contains("week"));
        assert_eq!(notices[0].severity(), NoticeSeverity::Critical);
    }
}
