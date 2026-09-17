//! Short display names for provider-routed model catalogs.
//!
//! A curated catalog names a model in two or three words ("Opus 5",
//! "GPT-5.6 Sol"). A PROVIDER-ROUTED one spells out the whole route: the
//! `pi-acp` adapter composes every row as `{provider}/{name}`, and OpenRouter's
//! own names already carry a `"{Vendor}: "` prefix, so one DeepSeek row reads
//!
//! ```text
//! id:    openrouter/deepseek/deepseek-chat-v3-0324
//! label: openrouter/DeepSeek: DeepSeek V3 0324
//! ```
//!
//! Rendered verbatim that overflows the picker row AND the composer pill, and
//! the two-thirds of it that repeat (the route, the vendor) are exactly the
//! parts a small mark can carry instead. [`display_model`] splits the label
//! into the three pieces the UI places separately:
//!
//! - `name` — what the row and the pill show ("DeepSeek V3 0324").
//! - `vendor` — the muted tagline beside it ("DeepSeek").
//! - `route` — which provider serves it, drawn as a mark (the OpenRouter mark
//!   for openrouter routes, the harness's own otherwise).
//!
//! Pure and id-aware: the route is only believed when the LABEL and the ID
//! agree on it, and a `"{Vendor}: "` prefix is only stripped when it is really
//! a vendor (it matches the id's namespace, it repeats the start of the name,
//! or it is a vendor word we know). Nothing here touches search — the picker
//! ranks the original label and id, so typing "openrouter" still finds these
//! rows.

use zeron_proto::HarnessId;

/// The provider a model is routed through, taken off the front of its id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The route segment exactly as it appeared (`openrouter`, `anthropic`).
    pub id: String,
    /// Display spelling for the tagline (`OpenRouter`, `Anthropic`).
    pub name: String,
}

impl Route {
    pub fn icon(&self) -> Option<&'static str> {
        use crate::icons::*;
        Some(match self.id.as_str() {
            "openrouter" => BRAND_OPENROUTER,
            "anthropic" => CLAUDE_MARK,
            "openai" | "openai-codex" => OPENAI_MARK,
            "google" | "google-gemini-cli" => BRAND_GOOGLE_GEMINI,
            "deepseek" => BRAND_DEEPSEEK,
            "groq" => BRAND_GROQ,
            "xai" => GROK_MARK,
            "moonshot" | "moonshotai" => KIMI_MARK,
            "mistral" => BRAND_MISTRAL,
            "fireworks" | "fireworks-ai" => BRAND_FIREWORKS,
            "together" | "togetherai" => BRAND_TOGETHER,
            _ => return None,
        })
    }

    /// Whether this route is OpenRouter — the one route with its own mark.
    pub fn is_openrouter(&self) -> bool {
        self.id.eq_ignore_ascii_case("openrouter")
    }
}

/// A model label split into the pieces the picker places separately.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ModelDisplay {
    pub name: String,
    pub vendor: Option<String>,
    pub route: Option<Route>,
}

impl ModelDisplay {
    /// The muted line beside (compact rows) or under (favorites rows) the
    /// name: who made the model, and which route serves it when that is not
    /// the same party.
    ///
    /// `route_marked` says whether the row already draws the route's own mark
    /// (the picker does for openrouter rows). Then the "via" clause is ink
    /// spent twice, and in a 304px popover it is the half that truncates, so
    /// it is dropped and only the vendor remains.
    pub fn tagline(&self, route_marked: bool) -> Option<String> {
        match (&self.vendor, &self.route) {
            (Some(vendor), Some(route)) if !route_marked && !same_party(vendor, &route.id) => {
                Some(format!("{vendor} · via {}", route.name))
            }
            (Some(vendor), _) => Some(vendor.clone()),
            (None, Some(route)) if !route_marked => Some(format!("via {}", route.name)),
            (None, _) => None,
        }
    }
}

/// Split `label` into name / vendor / route for the model `id` on `harness`.
///
/// Curated catalogs (Claude Code, Codex, and the mock that borrows Claude's)
/// are left exactly as they are: their labels are hand-written, and a stray
/// slash or colon in one must never be mistaken for a route or a vendor.
pub fn display_model(harness: HarnessId, id: &str, label: &str) -> ModelDisplay {
    let label = label.trim();
    if !routed_catalog(harness) {
        return ModelDisplay {
            name: label.to_owned(),
            ..Default::default()
        };
    }
    let route_id = if harness == HarnessId::Cline {
        id.split_once("::").map(|(provider, model)| format!("{provider}/{model}"))
    } else { None };
    let id = route_id.as_deref().unwrap_or(id);
    let (mut route, rest) = split_route(id, label);
    if route.is_none() && harness == HarnessId::Opencode {
        if let Some((provider, _)) = id.split_once('/') {
            route = Some(Route { id: provider.to_owned(), name: route_name(provider) });
        }
    }
    let (vendor, name) = split_vendor(rest, route.as_ref().map_or(id, |_| after_route(id)));
    ModelDisplay {
        // A label that is nothing BUT a route and a vendor keeps its original
        // text rather than rendering as an empty row.
        name: if name.is_empty() {
            label.to_owned()
        } else {
            name.to_owned()
        },
        vendor,
        route,
    }
}

/// Whether this harness's catalog comes from a provider registry (routed ids)
/// rather than a curated table in this repo.
fn routed_catalog(harness: HarnessId) -> bool {
    !matches!(
        harness,
        HarnessId::ClaudeCode | HarnessId::Codex | HarnessId::Mock
    )
}

/// The id with any leading `{segment}/` removed.
fn after_route(id: &str) -> &str {
    id.split_once('/').map_or(id, |(_, rest)| rest)
}

/// Peel a `"{provider}/"` prefix off the label — but only when the ID carries
/// the same segment. `pi-acp` composes both from the same provider name, so
/// agreement is proof; a label that merely contains a slash is left alone.
fn split_route<'a>(id: &str, label: &'a str) -> (Option<Route>, &'a str) {
    let Some((segment, rest)) = label.split_once('/') else {
        return (None, label);
    };
    let segment = segment.trim();
    let rest = rest.trim();
    if segment.is_empty() || rest.is_empty() {
        return (None, label);
    }
    let id_segment = id.split_once('/').map(|(head, _)| head).unwrap_or_default();
    if !id_segment.eq_ignore_ascii_case(segment) {
        return (None, label);
    }
    (
        Some(Route {
            id: segment.to_owned(),
            name: route_name(segment),
        }),
        rest,
    )
}

/// Peel a `"{Vendor}: "` prefix off the name. `id_rest` is the id BEHIND the
/// route (`deepseek/deepseek-chat-v3-0324`), whose first segment is the
/// registry's own vendor namespace — the most reliable evidence there is.
fn split_vendor<'a>(name: &'a str, id_rest: &str) -> (Option<String>, &'a str) {
    let Some((vendor, rest)) = name.split_once(": ") else {
        return (None, name);
    };
    let vendor = vendor.trim();
    let rest = rest.trim();
    if vendor.is_empty() || rest.is_empty() || vendor.contains('/') || vendor.split(' ').count() > 3
    {
        return (None, name);
    }
    let namespace = id_rest.split('/').next().unwrap_or_default();
    let is_vendor = same_party(vendor, namespace)
        || starts_with_word(rest, vendor)
        || KNOWN_VENDORS.contains(&squash(vendor).as_str());
    if is_vendor {
        (Some(vendor.to_owned()), rest)
    } else {
        (None, name)
    }
}

/// Whether `name` opens with `prefix` as a whole word ("DeepSeek" in
/// "DeepSeek V3 0324", but not "Codex" in "Codexa").
fn starts_with_word(name: &str, prefix: &str) -> bool {
    let (head, tail) = match name.split_at_checked(prefix.len()) {
        Some(split) => split,
        None => return false,
    };
    head.eq_ignore_ascii_case(prefix)
        && tail
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '.')
}

/// Two names for the same party, compared past spelling: `Moonshot AI` /
/// `moonshotai`, `Inference.net` / `inference-net`, `xAI` / `xai`.
fn same_party(a: &str, b: &str) -> bool {
    let (a, b) = (squash(a), squash(b));
    !a.is_empty() && a == b
}

/// Lowercase, letters and digits only.
fn squash(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Display spellings for the provider ids these catalogs route through (pi's
/// `providers/data/*.json` names, opencode's provider ids). Anything unlisted
/// is title-cased from its own segment.
const ROUTE_NAMES: &[(&str, &str)] = &[
    ("openrouter", "OpenRouter"),
    ("opencode", "OpenCode"),
    ("openai", "OpenAI"),
    ("openai-codex", "OpenAI Codex"),
    ("azure-openai-responses", "Azure OpenAI"),
    ("anthropic", "Anthropic"),
    ("amazon-bedrock", "Amazon Bedrock"),
    ("google", "Google"),
    ("google-vertex", "Google Vertex"),
    ("github-copilot", "GitHub Copilot"),
    ("xai", "xAI"),
    ("zai", "Z.AI"),
    ("deepseek", "DeepSeek"),
    ("mistral", "Mistral"),
    ("moonshotai", "Moonshot AI"),
    ("moonshotai-cn", "Moonshot AI"),
    ("kimi-coding", "Kimi"),
    ("minimax", "MiniMax"),
    ("minimax-cn", "MiniMax"),
    ("nvidia", "NVIDIA"),
    ("huggingface", "Hugging Face"),
    ("cloudflare-ai-gateway", "Cloudflare AI Gateway"),
    ("cloudflare-workers-ai", "Cloudflare Workers AI"),
];

/// Vendor words the registries use that neither repeat the model name nor the
/// id namespace. Short on purpose: the two structural tests above carry the
/// catalog, and this only rescues the stragglers.
const KNOWN_VENDORS: &[&str] = &[
    "openai",
    "anthropic",
    "google",
    "meta",
    "metallama",
    "mistral",
    "mistralai",
    "deepseek",
    "qwen",
    "alibaba",
    "xai",
    "zai",
    "moonshotai",
    "moonshot",
    "minimax",
    "nousresearch",
    "perplexity",
    "cohere",
    "amazon",
    "microsoft",
    "nvidia",
    "baidu",
    "bytedance",
    "ai21",
    "inflection",
    "openrouter",
];

fn route_name(segment: &str) -> String {
    let squashed = squash(segment);
    if let Some((_, name)) = ROUTE_NAMES
        .iter()
        .find(|(id, _)| squash(id) == squashed && !squashed.is_empty())
    {
        return (*name).to_owned();
    }
    segment
        .split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(title_case)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Capitalize a word, leaving one that already carries capitals alone
/// (`xAI`, `GPT`).
fn title_case(word: &str) -> String {
    if word.chars().any(char::is_uppercase) {
        return word.to_owned();
    }
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_marks_follow_billing_route_not_model_vendor() {
        let cline = display_model(HarnessId::Cline, "openrouter::anthropic/claude-test", "openrouter/Claude Test");
        assert_eq!(cline.name, "Claude Test");
        assert_eq!(cline.route.unwrap().icon(), Some(crate::icons::BRAND_OPENROUTER));
        let opencode = display_model(HarnessId::Opencode, "openrouter/anthropic/claude-test", "Claude Test");
        assert_eq!(opencode.route.unwrap().icon(), Some(crate::icons::BRAND_OPENROUTER));
    }


    /// Verbatim from a live pi catalog: `pi-acp` builds every row as
    /// `modelId = "{provider}/{id}"` and `name = "{provider}/{name}"`, where
    /// the inner id/name pair is OpenRouter's own
    /// (`pi-ai/dist/providers/data/openrouter.json`).
    fn pi(id: &str, label: &str) -> ModelDisplay {
        display_model(HarnessId::Pi, id, label)
    }

    #[test]
    fn model_display_shortens_pi_openrouter_rows() {
        let shown = pi(
            "openrouter/deepseek/deepseek-chat-v3-0324",
            "openrouter/DeepSeek: DeepSeek V3 0324",
        );
        assert_eq!(shown.name, "DeepSeek V3 0324");
        assert_eq!(shown.vendor.as_deref(), Some("DeepSeek"));
        let route = shown.route.as_ref().unwrap();
        assert_eq!(route.name, "OpenRouter");
        assert!(route.is_openrouter());
        assert_eq!(
            shown.tagline(false).as_deref(),
            Some("DeepSeek · via OpenRouter")
        );
        // A row that already draws the OpenRouter mark says it once, not
        // twice - the "via" half is what truncates in a 304px popover.
        assert_eq!(shown.tagline(true).as_deref(), Some("DeepSeek"));

        let fable = pi(
            "openrouter/anthropic/claude-fable-5.1",
            "openrouter/Anthropic: Claude Fable 5.1",
        );
        assert_eq!(fable.name, "Claude Fable 5.1");
        assert_eq!(fable.vendor.as_deref(), Some("Anthropic"));
        assert_eq!(
            fable.tagline(false).as_deref(),
            Some("Anthropic · via OpenRouter")
        );
    }

    #[test]
    fn model_display_keeps_names_that_carry_no_vendor() {
        // OpenRouter ships some rows with no "Vendor: " prefix at all.
        let opus = pi("openrouter/anthropic/claude-opus-5", "openrouter/Claude Opus 5");
        assert_eq!(opus.name, "Claude Opus 5");
        assert_eq!(opus.vendor, None);
        assert_eq!(opus.tagline(false).as_deref(), Some("via OpenRouter"));
        // With the mark drawn there is nothing left to say.
        assert_eq!(opus.tagline(true), None);

        // …and some whose "vendor" is really part of the product name.
        let router = pi("openrouter/openrouter/auto-beta", "openrouter/Auto Router (Beta)");
        assert_eq!(router.name, "Auto Router (Beta)");
        assert_eq!(router.vendor, None);
    }

    #[test]
    fn model_display_reads_vendors_off_the_id_namespace() {
        // "Inference.net" neither repeats the name nor is a word we listed —
        // the id's own namespace is what proves it is a vendor.
        let shown = pi(
            "openrouter/inference-net/schematron-v2-turbo",
            "openrouter/Inference.net: Schematron V2 Turbo",
        );
        assert_eq!(shown.name, "Schematron V2 Turbo");
        assert_eq!(shown.vendor.as_deref(), Some("Inference.net"));

        // A colon that is part of the model name survives untouched.
        let colon = pi("openrouter/acme/k2:thinking", "openrouter/K2: Thinking Edition");
        assert_eq!(colon.name, "K2: Thinking Edition");
        assert_eq!(colon.vendor, None);
    }

    #[test]
    fn model_display_names_non_openrouter_routes() {
        let direct = pi(
            "anthropic/claude-fable-5.1",
            "anthropic/Anthropic: Claude Fable 5.1",
        );
        assert_eq!(direct.name, "Claude Fable 5.1");
        let route = direct.route.as_ref().unwrap();
        assert!(!route.is_openrouter());
        assert_eq!(route.name, "Anthropic");
        // Vendor and route are the same party: no "via" clause.
        assert_eq!(direct.tagline(false).as_deref(), Some("Anthropic"));

        // Unlisted provider ids get a readable spelling of their own segment.
        let other = pi("some-provider/acme/m1", "some-provider/Acme: M1");
        assert_eq!(other.route.as_ref().unwrap().name, "Some Provider");
        assert_eq!(other.name, "M1");
        assert_eq!(other.vendor.as_deref(), Some("Acme"));
        // An unnamespaced id proves nothing, so an unknown prefix stays in
        // the name rather than being guessed away.
        let unknown = pi("some-provider/m1", "some-provider/Vendor: M1");
        assert_eq!(unknown.name, "Vendor: M1");
        assert_eq!(unknown.vendor, None);

        // xAI keeps its own casing rather than becoming "Xai".
        let grok = pi("xai/grok-4.5", "xai/xAI: Grok 4.5");
        assert_eq!(grok.route.as_ref().unwrap().name, "xAI");
        assert_eq!(grok.name, "Grok 4.5");
        assert_eq!(grok.tagline(false).as_deref(), Some("xAI"));
    }

    #[test]
    fn model_display_only_believes_a_route_the_id_confirms() {
        // Label says a route, id does not: left whole.
        let bogus = pi("deepseek-chat-v3-0324", "openrouter/DeepSeek: DeepSeek V3 0324");
        assert_eq!(bogus.route, None);
        assert_eq!(bogus.name, "openrouter/DeepSeek: DeepSeek V3 0324");

        // OpenCode keeps its plain label, but its id still proves the route.
        let opencode = display_model(
            HarnessId::Opencode,
            "openrouter/deepseek/deepseek-chat-v3-0324",
            "DeepSeek V3 0324",
        );
        assert_eq!(opencode.name, "DeepSeek V3 0324");
        assert_eq!(opencode.route.as_ref().unwrap().id, "openrouter");
        assert_eq!(opencode.tagline(false).as_deref(), Some("via OpenRouter"));
    }

    #[test]
    fn model_display_leaves_curated_catalogs_alone() {
        for (harness, id, label) in [
            (HarnessId::ClaudeCode, "claude-opus-5", "Opus 5"),
            (HarnessId::Codex, "gpt-5.6-sol", "GPT-5.6 Sol"),
            (HarnessId::Mock, "claude-opus-5", "Opus 5"),
        ] {
            let shown = display_model(harness, id, label);
            assert_eq!(shown.name, label);
            assert_eq!(shown.vendor, None);
            assert_eq!(shown.route, None);
        }
        // Cline's catalog is routed (301 vendor-prefixed ids) but its labels
        // are already clean, so the vendor moves to the grey subline.
        let cline = display_model(HarnessId::Cline, "anthropic/claude-sonnet-5", "Claude Sonnet 5");
        assert_eq!(cline.name, "Claude Sonnet 5");
        assert_eq!(cline.route, None);
        // Kimi's ids are namespaced but its labels are curated and terse.
        let kimi = display_model(HarnessId::Kimi, "kimi-code/k3-256k", "K3-256k");
        assert_eq!(kimi.name, "K3-256k");
        assert_eq!(kimi.route, None);
    }
}
