//! Per-million prices for models routed through OpenRouter.
//!
//! Two catalogs in this crate hand the picker models with no price on them:
//!
//! - **pi** advertises its models over ACP as `{provider}/{id}` (the `pi-acp`
//!   adapter composes both the value and the name that way), and the ACP wire
//!   carries no pricing at all. Nearly every pi setup routes through
//!   OpenRouter, so `openrouter/deepseek/deepseek-chat-v3-0324` is exactly an
//!   OpenRouter catalog id with one prefix in front of it.
//! - **opencode** serves models.dev costs on most rows, but some
//!   OpenRouter-routed rows arrive with no `cost` object.
//!
//! `GET https://openrouter.ai/api/v1/models` is public (no key) and lists
//! `data[].pricing.{prompt, completion, input_cache_read}` as USD **per
//! token**, as decimal strings. Multiplying by 1e6 gives the per-million
//! numbers [`ModelPricing`] holds; the source is [`PriceSource::Catalog`]
//! because these are the rates OpenRouter itself bills.
//!
//! Listing models must never wait on the network: [`prices`] answers from
//! memory or from the on-disk cache and kicks a background refresh when that
//! cache is missing or older than [`MAX_AGE`]. Only a completely cold start
//! (nothing in memory, nothing on disk) waits at all, and then for at most
//! [`LISTING_BUDGET`] — after which the listing goes out unpriced and the
//! NEXT listing picks the prices up. Callers must therefore apply prices on
//! every listing rather than baking them into a model cache.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use zeron_proto::{Model, ModelPricing, PriceSource};

/// The public catalog endpoint. No API key, no auth header.
pub const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";

/// The route prefix an OpenRouter-routed model id carries in this app.
pub const ROUTE_PREFIX: &str = "openrouter/";

/// How old the disk cache may get before a refresh is kicked.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// The longest a model listing may wait for a COLD fetch (nothing cached in
/// memory or on disk). A warm listing never waits at all.
pub const LISTING_BUDGET: Duration = Duration::from_secs(2);

/// How long the background fetch itself may take before it is abandoned.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

/// Per-million prices keyed by OpenRouter model id (lowercased).
#[derive(Debug, Default, Clone)]
pub struct OpenRouterPrices {
    by_id: HashMap<String, ModelPricing>,
}

impl OpenRouterPrices {
    /// Parse a `GET /api/v1/models` body. A malformed body yields an EMPTY
    /// table, never an error: missing prices hide a row, they never break
    /// model listing.
    pub fn parse(body: &str) -> Self {
        let catalog: Catalog = match serde_json::from_str(body) {
            Ok(catalog) => catalog,
            Err(error) => {
                tracing::debug!("openrouter catalog parse failed: {error}");
                return Self::default();
            }
        };
        let mut by_id = HashMap::new();
        for entry in catalog.data {
            let (Some(id), Some(pricing)) = (entry.id, entry.pricing) else {
                continue;
            };
            let Some(pricing) = per_million(
                pricing.prompt.as_ref(),
                pricing.completion.as_ref(),
                pricing.input_cache_read.as_ref(),
            ) else {
                continue;
            };
            by_id.insert(id.trim().to_ascii_lowercase(), pricing);
        }
        Self { by_id }
    }

    /// Look a model up. Accepts the id with or without the `openrouter/`
    /// route prefix; a `:variant` suffix (`:free`, `:nitro`, `:floor`) falls
    /// back to the base id when the exact variant is not listed.
    pub fn get(&self, model_id: &str) -> Option<ModelPricing> {
        let id = strip_route(model_id).unwrap_or(model_id).trim();
        if id.is_empty() {
            return None;
        }
        let lower = id.to_ascii_lowercase();
        if let Some(pricing) = self.by_id.get(&lower) {
            return Some(pricing.clone());
        }
        let base = lower.split_once(':')?.0;
        self.by_id.get(base).cloned()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// Fill in `pricing` on every `openrouter/`-routed model that has none.
///
/// Never overwrites a price the harness's own catalog already reported (the
/// provider's own number wins over OpenRouter's list), and never touches a
/// model routed anywhere else.
pub fn apply(models: &mut [Model], prices: &OpenRouterPrices) {
    if prices.is_empty() {
        return;
    }
    for model in models {
        if model.pricing.is_some() {
            continue;
        }
        let Some(rest) = strip_route(&model.id) else {
            continue;
        };
        if let Some(pricing) = prices.get(rest) {
            model.pricing = Some(pricing);
        }
    }
}

/// The part of `id` after a leading, case-insensitive `openrouter/`.
pub fn strip_route(id: &str) -> Option<&str> {
    let head = id.get(..ROUTE_PREFIX.len())?;
    head.eq_ignore_ascii_case(ROUTE_PREFIX)
        .then(|| &id[ROUTE_PREFIX.len()..])
}

/// USD per token (decimal strings, as OpenRouter serves them) -> USD per 1M.
///
/// `prompt` and `completion` are required; a row without both is unknown, not
/// free. Both at zero is the genuinely free tier ([`PriceSource::Free`]).
/// `input_cache_read` at zero reads as "not published" rather than "free
/// cache reads": most rows carry it only when it differs from the prompt
/// rate, and a `$0` cached column would misstate the ones that do not.
pub fn per_million(
    prompt: Option<&serde_json::Value>,
    completion: Option<&serde_json::Value>,
    input_cache_read: Option<&serde_json::Value>,
) -> Option<ModelPricing> {
    let input = as_rate(prompt?)?;
    let output = as_rate(completion?)?;
    if input == 0.0 && output == 0.0 {
        return Some(ModelPricing::free());
    }
    let cached = input_cache_read
        .and_then(as_rate)
        .filter(|rate| *rate > 0.0)
        .map(per_1m);
    Some(ModelPricing::usd(
        per_1m(input),
        cached,
        per_1m(output),
        PriceSource::Catalog,
    ))
}

fn per_1m(per_token: f64) -> f64 {
    per_token * 1_000_000.0
}

/// A price field: a decimal STRING on the live wire, tolerated as a number.
/// Anything negative or non-finite is treated as absent.
fn as_rate(value: &serde_json::Value) -> Option<f64> {
    let rate = match value {
        serde_json::Value::String(text) => text.trim().parse::<f64>().ok()?,
        serde_json::Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    (rate.is_finite() && rate >= 0.0).then_some(rate)
}

#[derive(serde::Deserialize)]
struct Catalog {
    #[serde(default)]
    data: Vec<Entry>,
}

#[derive(serde::Deserialize)]
struct Entry {
    id: Option<String>,
    #[serde(default)]
    pricing: Option<Pricing>,
}

#[derive(serde::Deserialize)]
struct Pricing {
    prompt: Option<serde_json::Value>,
    completion: Option<serde_json::Value>,
    #[serde(default)]
    input_cache_read: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Shared snapshot + background refresh
// ---------------------------------------------------------------------------

/// `{data_dir}/cache/openrouter-models.json` — the verbatim response body, so
/// an offline start reparses exactly what the last online start saw. Its
/// mtime is the fetch time; no sidecar metadata.
pub fn cache_path() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("cache").join("openrouter-models.json"))
}

struct Shared {
    state: Mutex<State>,
    updates: tokio::sync::watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    prices: Arc<OpenRouterPrices>,
    fetched_at: Option<SystemTime>,
    /// The disk cache has been consulted once this process.
    disk_read: bool,
    in_flight: bool,
    revision: u64,
}

fn shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(|| Shared {
        state: Mutex::new(State::default()),
        updates: tokio::sync::watch::channel(0).0,
    })
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, State> {
    shared.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// The best prices known right now, refreshing in the background when stale.
///
/// Warm path: returns immediately. Cold path (nothing in memory, nothing on
/// disk): waits up to [`LISTING_BUDGET`] for the fetch that was just kicked,
/// then returns whatever there is — possibly an empty table, which simply
/// leaves the rows unpriced until the next listing.
pub async fn prices() -> Arc<OpenRouterPrices> {
    let shared = shared();
    // Subscribe BEFORE kicking the fetch, so a fast response cannot land
    // between the spawn and the wait.
    let mut updates = shared.updates.subscribe();
    let (snapshot, kick) = {
        let mut state = lock(shared);
        if !state.disk_read {
            state.disk_read = true;
            if let Some((body, mtime)) = read_cache() {
                let parsed = OpenRouterPrices::parse(&body);
                if !parsed.is_empty() {
                    state.prices = Arc::new(parsed);
                    state.fetched_at = Some(mtime);
                }
            }
        }
        let stale = state
            .fetched_at
            .and_then(|at| at.elapsed().ok())
            .is_none_or(|age| age >= MAX_AGE);
        let kick = stale && !state.in_flight;
        if kick {
            state.in_flight = true;
        }
        (state.prices.clone(), kick)
    };
    if kick {
        tokio::spawn(refresh());
    }
    if !snapshot.is_empty() {
        return snapshot;
    }
    // Cold start only.
    let _ = tokio::time::timeout(LISTING_BUDGET, updates.changed()).await;
    lock(shared).prices.clone()
}

/// One refresh: fetch, cache to disk, publish. Clears `in_flight` and bumps
/// the revision on EVERY path, so a waiting listing is never left hanging.
async fn refresh() {
    let fetched = fetch().await;
    let shared = shared();
    let mut state = lock(shared);
    state.in_flight = false;
    match fetched {
        Some(body) => {
            let parsed = OpenRouterPrices::parse(&body);
            if parsed.is_empty() {
                tracing::debug!("openrouter catalog carried no usable prices");
            } else {
                tracing::debug!("openrouter catalog: {} priced models", parsed.len());
                write_cache(&body);
                state.prices = Arc::new(parsed);
                state.fetched_at = Some(SystemTime::now());
            }
        }
        None => {
            // Offline: whatever the disk cache gave us stays in force.
        }
    }
    state.revision += 1;
    let revision = state.revision;
    drop(state);
    let _ = shared.updates.send(revision);
}

async fn fetch() -> Option<String> {
    let client = reqwest::Client::builder().timeout(FETCH_TIMEOUT).build();
    let client = match client {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!("openrouter client build failed: {error}");
            return None;
        }
    };
    match client.get(MODELS_URL).send().await {
        Ok(response) if response.status().is_success() => match response.text().await {
            Ok(body) => Some(body),
            Err(error) => {
                tracing::debug!("openrouter catalog body failed: {error}");
                None
            }
        },
        Ok(response) => {
            tracing::debug!("openrouter catalog returned {}", response.status());
            None
        }
        Err(error) => {
            tracing::debug!("openrouter catalog unreachable: {error}");
            None
        }
    }
}

fn read_cache() -> Option<(String, SystemTime)> {
    let path = cache_path()?;
    let body = std::fs::read_to_string(&path).ok()?;
    let mtime = std::fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    Some((body, mtime))
}

fn write_cache(body: &str) {
    let Some(path) = cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(error) = std::fs::write(&path, body) {
        tracing::debug!("openrouter cache write failed: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed verbatim from a live `GET https://openrouter.ai/api/v1/models`
    /// response (2026-09-15): the DeepSeek row pi routes to, a row with a
    /// published cache-read rate, a `:free` row, and a row with no pricing.
    const FIXTURE: &str = r#"{
      "data": [
        {
          "id": "deepseek/deepseek-chat-v3-0324",
          "name": "DeepSeek: DeepSeek V3 0324",
          "pricing": { "prompt": "0.00000025", "completion": "0.000001" }
        },
        {
          "id": "~deepseek/deepseek-pro-latest",
          "name": "DeepSeek: DeepSeek Pro Latest",
          "pricing": {
            "prompt": "0.00000066",
            "completion": "0.00000198",
            "input_cache_read": "0.000000022"
          }
        },
        {
          "id": "inclusionai/ling-3.0-flash-vl:free",
          "name": "inclusionAI: Ling 3.0 Flash VL (free)",
          "pricing": { "prompt": "0", "completion": "0", "input_cache_read": "0" }
        },
        {
          "id": "openrouter/auto-beta",
          "name": "Auto Router (Beta)"
        }
      ]
    }"#;

    #[test]
    fn openrouter_catalog_parses_the_live_shape() {
        let prices = OpenRouterPrices::parse(FIXTURE);
        // The unpriced router row is skipped; the other three are kept.
        assert_eq!(prices.len(), 3);
        assert!(prices.get("openrouter/auto-beta").is_none());
    }

    #[test]
    fn openrouter_per_token_strings_become_per_million() {
        let prices = OpenRouterPrices::parse(FIXTURE);
        let deepseek = prices.get("deepseek/deepseek-chat-v3-0324").unwrap();
        // 0.00000025 * 1e6 = 0.25, 0.000001 * 1e6 = 1.0
        assert!((deepseek.input_per_million - 0.25).abs() < 1e-9);
        assert!((deepseek.output_per_million - 1.0).abs() < 1e-9);
        assert_eq!(deepseek.source, PriceSource::Catalog);
        // No `input_cache_read` key at all -> unknown, never zero.
        assert_eq!(deepseek.cached_input_per_million, None);

        let pro = prices.get("~deepseek/deepseek-pro-latest").unwrap();
        assert!((pro.input_per_million - 0.66).abs() < 1e-9);
        assert!((pro.output_per_million - 1.98).abs() < 1e-9);
        assert!((pro.cached_input_per_million.unwrap() - 0.022).abs() < 1e-9);
    }

    #[test]
    fn openrouter_zero_both_ways_is_free_and_zero_cache_is_unknown() {
        let prices = OpenRouterPrices::parse(FIXTURE);
        let free = prices.get("inclusionai/ling-3.0-flash-vl:free").unwrap();
        assert!(free.is_free());
        // A published cache rate of 0 next to real prompt/completion rates is
        // "not published", not "free cache reads".
        let priced = per_million(
            Some(&serde_json::json!("0.000002")),
            Some(&serde_json::json!("0.00001")),
            Some(&serde_json::json!("0")),
        )
        .unwrap();
        assert_eq!(priced.cached_input_per_million, None);
        // Numbers are tolerated where the wire sends strings.
        let numeric = per_million(
            Some(&serde_json::json!(0.000002)),
            Some(&serde_json::json!(0.00001)),
            None,
        )
        .unwrap();
        assert!((numeric.input_per_million - 2.0).abs() < 1e-9);
        // A row with only one side is unknown, not free.
        assert!(per_million(Some(&serde_json::json!("0.000002")), None, None).is_none());
    }

    #[test]
    fn openrouter_ids_match_through_the_route_prefix_and_variants() {
        let prices = OpenRouterPrices::parse(FIXTURE);
        // pi's id shape: the OpenRouter id behind one route segment.
        assert!(
            prices
                .get("openrouter/deepseek/deepseek-chat-v3-0324")
                .is_some()
        );
        // Case-insensitive on both halves.
        assert!(
            prices
                .get("OpenRouter/DeepSeek/DeepSeek-Chat-V3-0324")
                .is_some()
        );
        // A variant suffix falls back to the base row…
        assert!(prices.get("deepseek/deepseek-chat-v3-0324:nitro").is_some());
        // …but an exact `:free` row wins over its own base lookup.
        assert!(
            prices
                .get("inclusionai/ling-3.0-flash-vl:free")
                .is_some_and(|p| p.is_free())
        );
        // Anything not routed through openrouter is left alone.
        assert_eq!(strip_route("anthropic/claude-opus-5"), None);
        assert_eq!(
            strip_route("openrouter/deepseek/deepseek-chat-v3-0324"),
            Some("deepseek/deepseek-chat-v3-0324")
        );
    }

    #[test]
    fn openrouter_pricing_only_fills_unpriced_routed_models() {
        let prices = OpenRouterPrices::parse(FIXTURE);
        let model = |id: &str, pricing: Option<ModelPricing>| Model {
            id: id.into(),
            label: id.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
            pricing,
        };
        let own = ModelPricing::usd(9.0, None, 9.0, PriceSource::Catalog);
        let mut models = vec![
            model("openrouter/deepseek/deepseek-chat-v3-0324", None),
            // Already priced by its own catalog: untouched.
            model(
                "openrouter/~deepseek/deepseek-pro-latest",
                Some(own.clone()),
            ),
            // Not routed through openrouter: untouched.
            model("anthropic/claude-opus-5", None),
            // Routed, but not in the catalog: stays unknown.
            model("openrouter/acme/does-not-exist", None),
        ];
        apply(&mut models, &prices);
        assert!((models[0].pricing.as_ref().unwrap().input_per_million - 0.25).abs() < 1e-9);
        assert_eq!(models[1].pricing.as_ref(), Some(&own));
        assert_eq!(models[2].pricing, None);
        assert_eq!(models[3].pricing, None);
        // An empty table is a no-op, not a wipe.
        let mut untouched = vec![model("openrouter/deepseek/deepseek-chat-v3-0324", None)];
        apply(&mut untouched, &OpenRouterPrices::default());
        assert_eq!(untouched[0].pricing, None);
    }

    /// Live check against the real endpoint. Ignored by default (network).
    /// `cargo test -p zeron-harness -- openrouter_catalog_is_reachable
    /// --ignored --nocapture`
    #[tokio::test]
    #[ignore = "hits the network"]
    async fn openrouter_catalog_is_reachable() {
        let body = fetch().await.expect("openrouter catalog fetch");
        let prices = OpenRouterPrices::parse(&body);
        println!("openrouter priced models: {}", prices.len());
        let deepseek = prices
            .get("openrouter/deepseek/deepseek-chat-v3-0324")
            .expect("deepseek/deepseek-chat-v3-0324 priced");
        println!(
            "deepseek/deepseek-chat-v3-0324: in {} cached {:?} out {}",
            deepseek.input_per_million,
            deepseek.cached_input_per_million,
            deepseek.output_per_million
        );
        assert!(deepseek.input_per_million > 0.0);
        assert!(deepseek.output_per_million > 0.0);
    }
}
