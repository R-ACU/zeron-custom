//! Per-million-token list prices for the catalogs this crate ships.
//!
//! Two kinds of number live here, and they are NOT interchangeable:
//!
//! - **Catalog prices** ([`PriceSource::Catalog`]) come off the provider's own
//!   model catalog at discovery time — today only opencode, whose `/provider`
//!   response carries a `cost` object per model (models.dev data). Those are
//!   what the provider actually bills, so they are shown without a hint.
//! - **List prices** ([`PriceSource::ListPrice`]) are the static tables below.
//!   Claude Code, Codex and Kimi are normally driven through a SUBSCRIPTION
//!   (Claude Max, ChatGPT Plus/Pro, a Kimi Code plan) and then nothing is
//!   billed per token at all. The numbers are the published API rates and are
//!   informational only — the picker marks them "list price" and the composer
//!   shows no session cost chip for those harnesses.
//!
//! A model whose price is not published gets `None`. `None` means UNKNOWN, and
//! is never rendered as "Free".

use zeron_proto::{ModelPricing, PriceSource};

/// Anthropic list prices, USD per 1M tokens, taken from
/// <https://platform.claude.com/docs/en/about-claude/pricing> (fetched
/// 2026-09-15). The cached column is the "Cache hits and refreshes" rate
/// (0.1x base input, except Fable 5.1 / Mythos 5.1 at 0.025x); cache WRITES
/// are a separate, higher rate that this model deliberately does not carry.
///
/// Claude Code on a Claude subscription is not billed per token — these are
/// the Claude API list prices and are shown for orientation only.
const CLAUDE_PRICES: &[(&str, f64, f64, f64)] = &[
    // (model id, input, cached input (cache read), output)
    ("claude-fable-5-1", 10.0, 0.25, 50.0),
    ("claude-fable-5", 10.0, 1.0, 50.0),
    ("claude-opus-5", 5.0, 0.5, 25.0),
    ("claude-opus-4-8", 5.0, 0.5, 25.0),
    ("claude-opus-4-7", 5.0, 0.5, 25.0),
    ("claude-sonnet-5", 2.0, 0.2, 10.0),
    ("claude-haiku-4-5", 1.0, 0.1, 5.0),
];

/// OpenAI list prices, USD per 1M tokens, standard tier, taken from
/// <https://developers.openai.com/api/docs/pricing> (fetched 2026-09-15; the
/// old `platform.openai.com/docs/pricing` 301s there). The cached column is
/// the page's "Cached input"; its separate "Cache writes" column is not
/// modelled.
///
/// Deliberately absent, and therefore priced `None`:
/// - `gpt-5.3-codex-spark` — the id does not appear on the pricing page at all.
/// - `gpt-daybreak-blue-latest` — documented as a moving ALIAS ("aliases will
///   be updated to point to the latest models, with pricing adjusted to match
///   each underlying model"), so any number pinned here would silently rot.
///
/// Codex on a ChatGPT plan is not billed per token; see the module note.
const CODEX_PRICES: &[(&str, f64, f64, f64)] = &[
    ("gpt-6-astra", 10.0, 1.0, 50.0),
    ("gpt-5.6-sol", 4.0, 0.4, 20.0),
    ("gpt-5.6-terra", 2.0, 0.2, 12.0),
    ("gpt-5.6-luna", 0.2, 0.02, 1.2),
    // The page splits gpt-5.5 / gpt-5.4 by context length; the sub-272K row is
    // the one that matches how the Codex app server runs them.
    ("gpt-5.5", 5.0, 0.5, 30.0),
    ("gpt-5.4", 2.5, 0.25, 15.0),
    ("gpt-5.4-mini", 0.75, 0.075, 4.5),
];

/// Moonshot list prices, USD per 1M tokens, taken from
/// <https://platform.kimi.ai/docs/pricing> (fetched 2026-09-15; the old
/// `platform.moonshot.ai/docs/pricing` 301s there). That page prices in USD
/// and states its cheap column as "Input Price (Cache Hit)", which is the
/// cached-input rate here; the expensive "Cache Miss" column is plain input.
///
/// The Kimi Code CLI's catalog ids are prefixed `kimi-code/`. `kimi-for-coding`
/// is labelled "K2.8 Preview" in the catalog and K2.8 is not on the pricing
/// page, so it stays `None`.
const KIMI_PRICES: &[(&str, f64, f64, f64)] = &[
    ("kimi-code/k3", 3.0, 0.3, 15.0),
    ("kimi-code/k3-256k", 3.0, 0.3, 15.0),
    // Catalog label "K2.7 Code Highspeed" -> the page's kimi-k2.7-code-highspeed.
    ("kimi-code/kimi-for-coding-highspeed", 1.9, 0.38, 8.0),
];

fn lookup(table: &[(&str, f64, f64, f64)], id: &str) -> Option<ModelPricing> {
    // Catalog ids sometimes carry a suffix the price table does not know: the
    // 1M context-window variant (`claude-opus-5[1m]`) and dated snapshots
    // (`claude-opus-4-7-20260101`). Exact match first, then longest prefix, so
    // `claude-opus-4-7` never shadows a future `claude-opus-4-7-5`.
    if let Some(row) = table.iter().find(|(known, ..)| *known == id) {
        return Some(row_pricing(row));
    }
    table
        .iter()
        .filter(|(known, ..)| {
            id.strip_prefix(known)
                .is_some_and(|rest| rest.starts_with('[') || rest.starts_with('-'))
        })
        .max_by_key(|(known, ..)| known.len())
        .map(row_pricing)
}

fn row_pricing(row: &(&str, f64, f64, f64)) -> ModelPricing {
    let (_, input, cached, output) = *row;
    ModelPricing::usd(input, Some(cached), output, PriceSource::ListPrice)
}

/// Anthropic list price for a Claude Code catalog id, if published.
pub fn claude_pricing(model_id: &str) -> Option<ModelPricing> {
    lookup(CLAUDE_PRICES, model_id)
}

/// OpenAI list price for a Codex catalog id, if published.
pub fn codex_pricing(model_id: &str) -> Option<ModelPricing> {
    lookup(CODEX_PRICES, model_id)
}

/// Moonshot list price for a Kimi catalog id, if published.
pub fn kimi_pricing(model_id: &str) -> Option<ModelPricing> {
    lookup(KIMI_PRICES, model_id)
}

/// The `cost` object opencode serves per model (models.dev data).
///
/// Two shapes are in the wild and both are accepted: the live `opencode serve`
/// `/provider` response nests the cache rates (`cost.cache.read` /
/// `cost.cache.write`, verified against opencode locally), while the SDK's
/// generated types and models.dev's `api.json` use the flat `cache_read` /
/// `cache_write`. Everything is optional — `gpt-4o-2024-05-13` in api.json
/// carries neither cache field, and 420 of its ~7.8k models omit `cost`
/// entirely.
#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct OpencodeCost {
    #[serde(default)]
    pub input: Option<f64>,
    #[serde(default)]
    pub output: Option<f64>,
    #[serde(default)]
    pub cache_read: Option<f64>,
    #[serde(default)]
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub cache: Option<OpencodeCacheCost>,
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct OpencodeCacheCost {
    #[serde(default)]
    pub read: Option<f64>,
    #[serde(default)]
    pub write: Option<f64>,
}

impl OpencodeCost {
    /// The cache-READ rate from whichever of the two shapes is present.
    fn read(&self) -> Option<f64> {
        self.cache_read.or_else(|| self.cache.as_ref()?.read)
    }
}

/// Map an opencode `cost` object onto a [`ModelPricing`].
///
/// All-zero costs are the tricky case. Live `/provider` output shows BOTH
/// meanings: OpenCode Zen's free tier reports 0/0 and really is free, while
/// the subscription-authenticated `openai` provider reports 0/0 for all 20 of
/// its models, which are certainly not free — opencode simply has no
/// per-token price for them. So zero reads as Free only when the model
/// announces itself as free (its id or name says so) or it comes from Zen's
/// own free catalog; otherwise it reads as UNKNOWN and no COST row is shown.
pub fn from_opencode_cost(
    provider_id: &str,
    model_id: &str,
    model_name: Option<&str>,
    cost: Option<&OpencodeCost>,
) -> Option<ModelPricing> {
    let cost = cost?;
    let input = cost.input?;
    let output = cost.output?;
    let cached = cost.read();
    if input <= 0.0 && output <= 0.0 && cached.unwrap_or(0.0) <= 0.0 {
        let names_free = [model_id, model_name.unwrap_or_default()]
            .iter()
            .any(|s| s.to_ascii_lowercase().contains("free"));
        return (names_free || provider_id == "opencode").then(ModelPricing::free);
    }
    if input < 0.0 || output < 0.0 {
        return None; // nonsense row; better no price than a negative one
    }
    Some(ModelPricing::usd(
        input,
        cached.filter(|c| *c >= 0.0),
        output,
        PriceSource::Catalog,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positive(p: &ModelPricing) -> bool {
        p.input_per_million > 0.0
            && p.output_per_million > 0.0
            && p.cached_input_per_million.is_none_or(|c| c > 0.0)
    }

    #[test]
    fn every_priced_claude_catalog_id_resolves() {
        let mut priced = 0;
        for model in crate::claude::catalog::static_models() {
            let Some(pricing) = model.pricing else {
                panic!("claude catalog id {} lost its price", model.id);
            };
            assert!(positive(&pricing), "{} has a non-positive price", model.id);
            assert_eq!(pricing.source, PriceSource::ListPrice);
            assert_eq!(pricing.currency, "USD");
            priced += 1;
        }
        assert_eq!(priced, 7, "every Claude catalog model is priced");
    }

    #[test]
    fn codex_catalog_prices_are_positive_and_unpublished_ids_stay_none() {
        for model in crate::codex::catalog::static_models() {
            match model.pricing {
                Some(pricing) => {
                    assert!(positive(&pricing), "{} has a non-positive price", model.id);
                    assert_eq!(pricing.source, PriceSource::ListPrice);
                }
                // Not published on OpenAI's pricing page; see CODEX_PRICES.
                None => assert!(
                    matches!(
                        model.id.as_str(),
                        "gpt-5.3-codex-spark" | "gpt-daybreak-blue-latest"
                    ),
                    "{} unexpectedly lost its price",
                    model.id
                ),
            }
        }
        assert!(codex_pricing("gpt-5.3-codex-spark").is_none());
        assert!(codex_pricing("gpt-daybreak-blue-latest").is_none());
    }

    #[test]
    fn kimi_prices_cover_k3_and_leave_the_preview_unpriced() {
        let k3 = kimi_pricing("kimi-code/k3").expect("k3 is published");
        assert_eq!(k3.input_per_million, 3.0);
        assert_eq!(k3.cached_input_per_million, Some(0.3));
        assert_eq!(k3.output_per_million, 15.0);
        assert!(positive(&k3));
        assert_eq!(
            kimi_pricing("kimi-code/k3-256k").map(|p| p.output_per_million),
            Some(15.0)
        );
        assert!(kimi_pricing("kimi-code/kimi-for-coding").is_none());
        assert!(kimi_pricing("grok-4.5").is_none());
    }

    #[test]
    fn suffixed_ids_fall_back_to_their_family() {
        // The 1M context-window variant and dated snapshots bill as the base.
        assert_eq!(
            claude_pricing("claude-opus-5[1m]").map(|p| p.input_per_million),
            Some(5.0)
        );
        assert_eq!(
            claude_pricing("claude-opus-4-7-20260101").map(|p| p.output_per_million),
            Some(25.0)
        );
        // A different family must not borrow a neighbour's price.
        assert!(claude_pricing("claude-opus-4-5").is_none());
        assert!(claude_pricing("claude-sonnet-4-5").is_none());
    }

    #[test]
    fn fable_51_keeps_its_cheaper_cache_read() {
        let fable_51 = claude_pricing("claude-fable-5-1").unwrap();
        let fable_5 = claude_pricing("claude-fable-5").unwrap();
        assert_eq!(fable_51.cached_input_per_million, Some(0.25));
        assert_eq!(fable_5.cached_input_per_million, Some(1.0));
    }

    /// Fixture copied verbatim from a live `GET /provider` response
    /// (opencode 0.2.x on this machine, 2026-09-15) plus the flat shape from
    /// models.dev's `api.json`.
    #[test]
    fn opencode_cost_deserializes_both_wire_shapes() {
        let nested: OpencodeCost =
            serde_json::from_str(r#"{"input":3,"output":15,"cache":{"read":0.3,"write":3.75}}"#)
                .unwrap();
        let pricing =
            from_opencode_cost("anthropic", "claude-sonnet-4-5", None, Some(&nested)).unwrap();
        assert_eq!(pricing.input_per_million, 3.0);
        assert_eq!(pricing.cached_input_per_million, Some(0.3));
        assert_eq!(pricing.output_per_million, 15.0);
        assert_eq!(pricing.source, PriceSource::Catalog);

        let flat: OpencodeCost =
            serde_json::from_str(r#"{"input":5,"output":25,"cache_read":0.5,"cache_write":6.25}"#)
                .unwrap();
        let pricing = from_opencode_cost("anthropic", "claude-opus-5", None, Some(&flat)).unwrap();
        assert_eq!(pricing.cached_input_per_million, Some(0.5));

        // Partial cost object: no cache fields at all (models.dev gpt-4o).
        let partial: OpencodeCost = serde_json::from_str(r#"{"input":5,"output":15}"#).unwrap();
        let pricing =
            from_opencode_cost("openai", "gpt-4o-2024-05-13", None, Some(&partial)).unwrap();
        assert_eq!(pricing.cached_input_per_million, None);
        assert_eq!(pricing.input_per_million, 5.0);

        // Unknown extra fields (tiers, context_over_200k, reasoning) are skipped.
        let extra: OpencodeCost = serde_json::from_str(
            r#"{"input":2,"output":6,"cache":{"read":0.3,"write":0},"tiers":[{"input":4}],
                "context_over_200k":{"input":4,"output":12},"reasoning":15}"#,
        )
        .unwrap();
        assert_eq!(extra.input.unwrap(), 2.0);
        assert_eq!(extra.read(), Some(0.3));
    }

    #[test]
    fn zero_cost_is_free_only_when_the_model_says_so() {
        let zero: OpencodeCost =
            serde_json::from_str(r#"{"input":0,"output":0,"cache":{"read":0,"write":0}}"#).unwrap();

        // OpenCode Zen's free tier: really free.
        let zen = from_opencode_cost("opencode", "big-pickle", Some("Big Pickle"), Some(&zero));
        assert!(zen.is_some_and(|p| p.is_free()));
        // Named free anywhere else: also free.
        let named = from_opencode_cost(
            "openrouter",
            "google/gemma-4-31b-it:free",
            Some("Gemma 4 31B"),
            Some(&zero),
        );
        assert!(named.is_some_and(|p| p.is_free()));
        // Subscription-authenticated OpenAI models report 0/0 and are NOT free.
        assert!(
            from_opencode_cost("openai", "gpt-5.6-sol", Some("GPT-5.6 Sol"), Some(&zero)).is_none()
        );
        // No cost object at all is unknown, not free.
        assert!(from_opencode_cost("openai", "gpt-5.6-sol", None, None).is_none());
    }
}
