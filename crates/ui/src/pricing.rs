//! Money formatting and the session cost estimate.
//!
//! Two surfaces share this module: the COST row under the model picker's
//! effort slider (per-million list prices) and the composer footer's session
//! chip (what this chat's reported tokens would cost). Everything here is pure
//! so both can be unit-tested without a window.

use zeron_proto::{HarnessId, ModelPricing, PriceSource, UsageTotals};

/// Heading of the picker's cost section.
pub const COST_LABEL: &str = "Cost";

/// Muted hint beside the heading when the numbers are a static list price
/// rather than what the provider's own catalog reports. Claude Code, Codex and
/// Kimi normally run on a subscription and are not billed per token at all.
pub const LIST_PRICE_HINT: &str = "list price";

/// The unit every price in the row is quoted in.
pub const PER_MILLION_SUFFIX: &str = "/ 1M";

/// Column headings, in render order.
pub const INPUT_LABEL: &str = "Input";
pub const CACHED_INPUT_LABEL: &str = "Cached input";
pub const OUTPUT_LABEL: &str = "Output";

/// Rendered instead of the three columns when the provider serves the model
/// at no charge.
pub const FREE_LABEL: &str = "Free";

/// Tooltip on the footer's session cost chip.
pub const SESSION_ESTIMATE_TOOLTIP: &str =
    "Session estimate from reported tokens at list price; cached input is counted as normal input.";

/// Whether a chat on this harness is billed per token, and therefore whether
/// the composer shows a running cost estimate for it.
///
/// Only opencode today: it drives the user's own API keys (or OpenCode Zen)
/// and its catalog carries real prices. Claude Code, Codex and Kimi are
/// normally driven from a SUBSCRIPTION, where a dollar figure would be pure
/// fiction, so they stay off even though their models carry list prices. The
/// remaining ACP agents have no prices at all.
///
/// Widening this is a one-line change: add the harness here once its runs are
/// actually billed per token (and its models carry pricing).
pub fn shows_session_cost(harness: HarnessId) -> bool {
    match harness {
        // Both run on the user's own provider keys (OpenRouter and friends),
        // so every reported token costs real money.
        HarnessId::Opencode | HarnessId::Pi => true,
        HarnessId::ClaudeCode
        | HarnessId::Codex
        | HarnessId::Kimi
        | HarnessId::Cursor
        | HarnessId::Devin
        | HarnessId::Grok
        | HarnessId::Hermes
        | HarnessId::Mock => false,
    }
}

/// Whether the model picker shows the COST row for this harness at all.
///
/// Same question as [`shows_session_cost`], asked one step earlier: does the
/// user pay per token here? Only then is a price list something they act on.
///
/// - **opencode** and **pi** drive the user's OWN keys (opencode through its
///   provider config or OpenCode Zen, pi through `~/.pi` — in practice an
///   OpenRouter key). Every token is billed to them, so the row is shown.
/// - **Claude Code**, **Codex** and **Kimi** are subscription products (Claude
///   Max, a ChatGPT plan, a Kimi Code plan). Their models carry list prices
///   for orientation, but a price list in the picker reads as a bill the user
///   will never get.
/// - **Cursor**, **Devin**, **Grok** and **Hermes** are likewise subscription
///   products, and carry no prices at all today.
///
/// The pricing DATA stays on the model either way — hiding the row is a UI
/// decision, and widening this is a one-line change.
pub fn shows_model_cost(harness: HarnessId) -> bool {
    match harness {
        HarnessId::Opencode | HarnessId::Pi => true,
        HarnessId::ClaudeCode
        | HarnessId::Codex
        | HarnessId::Kimi
        | HarnessId::Cursor
        | HarnessId::Devin
        | HarnessId::Grok
        | HarnessId::Hermes
        | HarnessId::Mock => false,
    }
}

/// A per-million price as it reads in the cost row: `$1.4`, `$0.26`, `$50`.
///
/// Up to three decimals, trailing zeros trimmed. Two decimals covers every
/// price in the tables; the third only ever shows for sub-cent rates such as
/// OpenAI's `$0.075` cached input, where rounding to two would misstate the
/// price by seven percent.
pub fn format_price(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        return "-".into();
    }
    let mut text = format!("{value:.3}");
    if text.contains('.') {
        text = text.trim_end_matches('0').trim_end_matches('.').to_owned();
    }
    format!("${text}")
}

/// Estimated USD for the tokens reported so far.
///
/// The `Usage` event reports only totals, with no split between cached and
/// uncached input, so EVERY input token is charged at the uncached rate — the
/// cached rate in [`ModelPricing`] is deliberately unused here. That
/// overstates a cache-heavy session, which the chip's tooltip says out loud.
pub fn estimate_usd(totals: UsageTotals, pricing: &ModelPricing) -> Option<f64> {
    if pricing.source == PriceSource::Free {
        return Some(0.0);
    }
    let input = pricing.input_per_million;
    let output = pricing.output_per_million;
    if !input.is_finite() || !output.is_finite() || input < 0.0 || output < 0.0 {
        return None;
    }
    let per_token = |tokens: u64, per_million: f64| tokens as f64 * per_million / 1_000_000.0;
    Some(per_token(totals.input_tokens, input) + per_token(totals.output_tokens, output))
}

/// The footer chip's text: two decimals, with a floor so a session that has
/// cost a fraction of a cent does not read as free.
pub fn format_estimate(usd: f64) -> String {
    if !usd.is_finite() || usd < 0.0 {
        return "-".into();
    }
    if usd > 0.0 && usd < 0.005 {
        return "<$0.01".into();
    }
    format!("${usd:.2}")
}

/// Fold one CUMULATIVE usage report from the engine into what the UI holds.
///
/// The engine sends running totals, not deltas, so the only thing that can go
/// wrong is going backwards — a stale frame arriving after a fresher one, or a
/// resubscribe replaying an older snapshot. Keeping the per-field maximum makes
/// the chip monotonic within a session without ever double-counting a token.
pub fn merge_totals(previous: Option<UsageTotals>, incoming: UsageTotals) -> UsageTotals {
    let previous = previous.unwrap_or_default();
    UsageTotals {
        input_tokens: previous.input_tokens.max(incoming.input_tokens),
        output_tokens: previous.output_tokens.max(incoming.output_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog(input: f64, cached: Option<f64>, output: f64) -> ModelPricing {
        ModelPricing::usd(input, cached, output, PriceSource::Catalog)
    }

    #[test]
    fn prices_trim_to_the_reference_look() {
        // The three columns of the reference screenshot.
        assert_eq!(format_price(1.4), "$1.4");
        assert_eq!(format_price(0.26), "$0.26");
        assert_eq!(format_price(4.4), "$4.4");
        // Whole dollars carry no decimals at all.
        assert_eq!(format_price(50.0), "$50");
        assert_eq!(format_price(5.0), "$5");
        assert_eq!(format_price(0.0), "$0");
        // Real table values.
        assert_eq!(format_price(0.25), "$0.25");
        assert_eq!(format_price(12.5), "$12.5");
        assert_eq!(format_price(0.075), "$0.075");
        assert_eq!(format_price(0.02), "$0.02");
        // Nonsense never renders as money.
        assert_eq!(format_price(-1.0), "-");
        assert_eq!(format_price(f64::NAN), "-");
    }

    #[test]
    fn estimate_charges_all_input_at_the_uncached_rate() {
        let pricing = catalog(3.0, Some(0.3), 15.0);
        let totals = UsageTotals {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
        };
        // 1M input at $3 + 100k output at $15/1M = 3.0 + 1.5
        let usd = estimate_usd(totals, &pricing).unwrap();
        assert!((usd - 4.5).abs() < 1e-9, "{usd}");
        // The cached rate must not creep in: halving it changes nothing.
        let cheaper = catalog(3.0, Some(0.15), 15.0);
        assert_eq!(
            estimate_usd(totals, &cheaper),
            estimate_usd(totals, &pricing)
        );
    }

    #[test]
    fn estimate_handles_free_and_empty_sessions() {
        let free = ModelPricing::free();
        assert_eq!(estimate_usd(UsageTotals::default(), &free), Some(0.0));
        let pricing = catalog(2.0, None, 10.0);
        assert_eq!(estimate_usd(UsageTotals::default(), &pricing), Some(0.0));
        let broken = catalog(f64::NAN, None, 10.0);
        assert_eq!(estimate_usd(UsageTotals::default(), &broken), None);
    }

    #[test]
    fn estimates_format_with_a_sub_cent_floor() {
        assert_eq!(format_estimate(0.1234), "$0.12");
        assert_eq!(format_estimate(0.0), "$0.00");
        assert_eq!(format_estimate(0.0001), "<$0.01");
        assert_eq!(format_estimate(12.0), "$12.00");
        assert_eq!(format_estimate(-1.0), "-");
    }

    #[test]
    fn totals_accumulate_monotonically() {
        let mut held = None;
        for (input, output, expect_in, expect_out) in [
            (1_000u64, 200u64, 1_000u64, 200u64),
            // A later, larger cumulative report replaces it wholesale…
            (4_500, 900, 4_500, 900),
            // …and a stale replay can never walk it back.
            (1_000, 200, 4_500, 900),
        ] {
            let merged = merge_totals(
                held,
                UsageTotals {
                    input_tokens: input,
                    output_tokens: output,
                },
            );
            assert_eq!(merged.input_tokens, expect_in);
            assert_eq!(merged.output_tokens, expect_out);
            held = Some(merged);
        }
        // Accumulation plus pricing is what the chip shows.
        let pricing = catalog(1.0, None, 5.0);
        let usd = estimate_usd(held.unwrap(), &pricing).unwrap();
        // 4500 * 1/1M + 900 * 5/1M = 0.0045 + 0.0045 = 0.009 -> rounds to a cent
        assert_eq!(format_estimate(usd), "$0.01");
        // A tenth of that is below the floor and reads as "under a cent".
        assert_eq!(format_estimate(usd / 10.0), "<$0.01");
    }

    #[test]
    fn only_opencode_is_billed_per_token_today() {
        assert!(shows_session_cost(HarnessId::Opencode));
        for subscription in [HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Kimi] {
            assert!(!shows_session_cost(subscription));
        }
        assert!(!shows_session_cost(HarnessId::Grok));
    }

    #[test]
    fn the_cost_row_follows_who_pays_per_token() {
        // The user's own keys: a price list is a bill they will get.
        assert!(shows_model_cost(HarnessId::Opencode));
        assert!(shows_model_cost(HarnessId::Pi));
        // Subscription products, even the ones carrying list prices.
        for subscription in [
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Kimi,
            HarnessId::Cursor,
            HarnessId::Devin,
            HarnessId::Grok,
            HarnessId::Hermes,
            HarnessId::Mock,
        ] {
            assert!(!shows_model_cost(subscription), "{subscription:?}");
        }
        // Every harness that shows a running session cost must also be
        // allowed to show the price list that estimate is built from.
        for harness in [
            HarnessId::Opencode,
            HarnessId::Pi,
            HarnessId::ClaudeCode,
            HarnessId::Codex,
            HarnessId::Kimi,
            HarnessId::Cursor,
            HarnessId::Devin,
            HarnessId::Grok,
            HarnessId::Hermes,
            HarnessId::Mock,
        ] {
            assert!(
                !shows_session_cost(harness) || shows_model_cost(harness),
                "{harness:?}"
            );
        }
    }
}
