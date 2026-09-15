//! Composer pickers (feature-inventory §1.7): RepoPicker (recents + search +
//! in-app folder browser + clone/create), BranchPicker (search + isolated-
//! worktree toggle), HarnessModelPicker (harness rail + model list, harness
//! locked once the chat exists), TraitsPicker (reasoning ladder + advertised
//! model options; trigger shows the non-default summary "High · 1M · Fast").
//!
//! All selections accumulate into a [`DraftConfig`] the composer threads into
//! the Run command and the `Mutate createChat` call on first send.
//!
//! Pure logic (repo ordering, folder-browser navigation, traits summary) lives
//! in free functions with unit tests; RPC results land in [`Loadable`] slots
//! rendered as skeletons / inline errors with Retry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable as _, KeyDownEvent, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};

use zeron_engine::registry::HarnessDescriptor;
use zeron_proto::{
    ChatConfig, FolderListing, HarnessId, Model, ReasoningLevel, RepoRef, SandboxLevel, Space,
};
use zeron_rpc::methods;

/// Display cap for the ref list (t3code shows pages of 100 with a status
/// footer; a flat cap + "Showing X of Y refs" reads the same without
/// pagination plumbing).
const MAX_REF_ROWS: usize = 300;

const FOOTER_CHIP_RADIUS: f32 = 6.0;

/// Width of the harness/model popover card (t3 ModelPickerContent, shrunk to
/// its tabbed layout).
const MODEL_POPOVER_WIDTH: f32 = 304.0;

/// The effort slider's painted track width: the popover minus its 1px border
/// and [`EFFORT_ROW_PAD`] on each side. Fixed rather than measured
/// so the first painted frame is already correct.
const EFFORT_TRACK_WIDTH: f32 = MODEL_POPOVER_WIDTH - 2.0 * (1.0 + EFFORT_ROW_PAD);

/// Horizontal padding of the effort row. 14px lines its EFFORT label up with
/// the tray's option headings (6px tray inset + 8px heading padding) and with
/// the model rows above (6px list inset + 8px row padding).
const EFFORT_ROW_PAD: f32 = 14.0;

/// Both sides of the composer handoff share one leading-aligned workspace
/// cluster. Available width belongs after the pair, never between its labels.
fn workspace_footer_row() -> gpui::Div {
    div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(4.0))
}

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::motion;
use crate::popover::{self, Loadable, MenuKey};
use crate::settings::composer::ComposerDefaults;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

/// Dev/testing knob: `ZERON_SLOW_CATALOG_MS=<ms>` delays every harness and
/// model catalog result app-side — the chip/tab/list loading states are
/// sub-second against a warm local daemon and unstageable otherwise
/// (headless-rig captures; same family as `ZERON_OPEN_PICKER`).
fn slow_catalog_delay() -> Option<std::time::Duration> {
    std::env::var("ZERON_SLOW_CATALOG_MS")
        .ok()
        .and_then(|ms| ms.parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
}

// ---------------------------------------------------------------------------
// Catalog invalidation (Settings → Agents toggles)
// ---------------------------------------------------------------------------

/// Marker global: [`bump_harness_catalog`] pokes it whenever a Settings →
/// Agents toggle changes some device's enabled set, and every [`Pickers`]
/// observes it to force-refresh its cached harness catalog — without this the
/// composer served the boot-time list until restart (user report).
#[derive(Default)]
pub struct HarnessCatalogChanged;

impl gpui::Global for HarnessCatalogChanged {}

/// Notify all composers that some device's harness catalog changed. The
/// global carries no data — `default_global` pushes the observer effect, and
/// the observers re-fetch from the engine (the source of truth).
pub fn bump_harness_catalog(cx: &mut App) {
    cx.default_global::<HarnessCatalogChanged>();
}

// ---------------------------------------------------------------------------
// Draft config (what the pickers accumulate)
// ---------------------------------------------------------------------------

/// Everything a new chat is configured with before the first send. The folder
/// and device come from the selected SPACE — the draft only carries the git
/// extras (ref + checkout kind) and the run config.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DraftConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    /// The picked ref (base branch in NewWorktree mode; a worktree's branch
    /// when reusing one). `None` = the repo's current branch.
    pub branch: Option<String>,
    /// Where the new session runs (the t3code env-mode).
    pub checkout: CheckoutKind,
}

/// Where a new session runs (t3code's env-mode: `local | worktree`). "Current
/// worktree" is NOT a third mode — it's `Local` when the picked ref is already
/// materialized as a worktree (the session reuses that checkout's path).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CheckoutKind {
    /// The space's own folder — or the picked ref's existing worktree.
    #[default]
    Local,
    /// A fresh isolated worktree created off the picked base ref on send.
    NewWorktree,
}

/// The resolved on-send checkout action (composer consumes this — see
/// [`Pickers::checkout_plan`]).
#[derive(Debug, Clone, PartialEq)]
pub enum CheckoutPlan {
    /// Run in the space folder as-is. `branch` is the checkout's branch (the
    /// picked or current ref), carried onto `createChat` so the session names
    /// it from the first frame; `None` = refs never loaded.
    CurrentCheckout { branch: Option<String> },
    /// Reuse the picked ref's existing worktree (a cwd override; no git).
    ReuseWorktree { path: String, branch: String },
    /// `CreateWorktree` off `base` on send (zeron mints a `zeron/<name>`
    /// branch). `base: None` = refs never loaded — send falls back to the
    /// space folder rather than failing.
    NewWorktree { base: Option<String> },
}

/// The fully-resolved run configuration the composer sends: concrete harness,
/// model and reasoning (never a "default" passthrough once the catalog is
/// loaded), plus the explicit non-default option picks.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedRunConfig {
    pub harness: Option<HarnessId>,
    pub model: Option<String>,
    pub reasoning: Option<ReasoningLevel>,
    pub model_options: serde_json::Map<String, serde_json::Value>,
}

impl ResolvedRunConfig {
    /// The `ChatConfig` recorded on `Mutate createChat` (needs a known harness).
    pub fn chat_config(&self) -> Option<ChatConfig> {
        Some(ChatConfig {
            harness: self.harness?,
            model: self.model.clone(),
            reasoning: self.reasoning,
            model_options: self.model_options.clone(),
            sandbox: SandboxLevel::WorkspaceWrite,
        })
    }
}

// ---------------------------------------------------------------------------
// Pure: default resolution (no "Default" placeholders — a concrete pick always)
// ---------------------------------------------------------------------------

/// The harness's default model: the first catalog row (both curated catalogs
/// lead with the flagship — zeron's `pickDefaultModel` Opus preference maps to
/// the same row here).
pub fn default_model(models: &[Model]) -> Option<&Model> {
    models.first()
}

/// A model's default reasoning: X-High when the ladder offers it (zeron
/// `DEFAULT_REASONING = "xhigh"`), else High, else the ladder's first entry.
/// `None` only for ladder-less models (e.g. Haiku's thinking toggle instead).
pub fn default_reasoning(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    // The recommended default is High (user-corrected — not X-High globally);
    // fall to Medium then the ladder's first entry for shorter ladders.
    if ladder.contains(&ReasoningLevel::High) {
        return Some(ReasoningLevel::High);
    }
    if ladder.contains(&ReasoningLevel::Medium) {
        return Some(ReasoningLevel::Medium);
    }
    ladder.first().copied()
}

/// The level a model starts on when it has never been used: the catalog
/// default for its ladder, falling back to the ladder's middle rung when the
/// catalog names none. `None` only for ladder-less models.
pub fn default_effort(ladder: &[ReasoningLevel]) -> Option<ReasoningLevel> {
    default_reasoning(ladder).or_else(|| ladder.get(ladder.len() / 2).copied())
}

/// The effort a model should show: its own remembered pick when the ladder
/// still offers it, else the model's default. Pure: the per-model memory map
/// lives in the sticky composer defaults.
pub fn resolve_effort(
    remembered: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match remembered {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_effort(ladder),
    }
}

/// Clamp a picked/remembered level to what the model actually offers: keep it
/// when the ladder lists it, else fall to the model's default (never a stale
/// or foreign level — zeron use-run-config.ts's derived-model discipline).
pub fn clamp_reasoning(
    level: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
) -> Option<ReasoningLevel> {
    match level {
        Some(level) if ladder.contains(&level) => Some(level),
        _ => default_reasoning(ladder),
    }
}

// ---------------------------------------------------------------------------
// Pure: labels + traits summary
// ---------------------------------------------------------------------------

pub fn reasoning_label(level: ReasoningLevel) -> &'static str {
    match level {
        ReasoningLevel::Minimal => "Minimal",
        ReasoningLevel::Low => "Low",
        ReasoningLevel::Medium => "Medium",
        ReasoningLevel::High => "High",
        ReasoningLevel::XHigh => "X-High",
        ReasoningLevel::Max => "Max",
        ReasoningLevel::Ultra => "Ultra",
        ReasoningLevel::Ultracode => "Ultracode",
        ReasoningLevel::Ultrathink => "Ultrathink",
    }
}

/// The TraitsPicker trigger summary: the effective reasoning level plus every
/// model option's effective choice — the explicit pick when one is saved and
/// still offered, else the option's default — joined with " · " ("High · 1M ·
/// Fast", Cursor's "Agent · Balance"). Defaults are spelled out rather than
/// hidden so the run's configuration reads without opening the popover; `None`
/// only when the model has nothing to describe (no ladder, no options).
pub fn traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let choice_id = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .filter(|id| option.choices.iter().any(|c| c.id == *id))
                .unwrap_or(&option.default_choice);
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}

/// The composer pill's summary: the effective EFFORT level plus every model
/// option that departs from its default ("Low", "High · 1M · Fast"). Unlike
/// [`traits_summary`], which spells out every option's effective choice for
/// the picker, defaults stay silent here. A pill reading "Haiku 4.5 · Low ·
/// Off" spent its second half on a thinking toggle sitting at its default
/// (user report). `None` when there is nothing to say.
pub fn pill_traits_summary(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    selections: &serde_json::Map<String, serde_json::Value>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(level) = reasoning {
        parts.push(reasoning_label(level).to_string());
    }
    if let Some(model) = model {
        for option in &model.options {
            let Some(choice_id) = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .filter(|id| *id != option.default_choice)
                .filter(|id| option.choices.iter().any(|c| c.id == *id))
            else {
                continue;
            };
            if let Some(choice) = option.choices.iter().find(|c| c.id == choice_id) {
                parts.push(choice.label.clone());
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Keep only the picks `model` still offers. Remembered picks outlive the
/// model they were made on, and harnesses apply some options blindly (Claude
/// appends `[1m]` to any model id when `contextWindow` is "1m").
pub fn offered_options(
    model: &Model,
    mut selections: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    selections.retain(|id, choice| {
        model.options.iter().any(|option| {
            option.id == *id
                && choice
                    .as_str()
                    .is_some_and(|choice| option.choices.iter().any(|c| c.id == choice))
        })
    });
    selections
}

/// Whether any trait departs from its default — the trigger brightens only
/// then, so a customized run still stands out now that the summary always
/// names the effective choices.
pub fn traits_customized(
    model: Option<&Model>,
    reasoning: Option<ReasoningLevel>,
    ladder: &[ReasoningLevel],
    selections: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    if reasoning != default_reasoning(ladder) {
        return true;
    }
    model.is_some_and(|model| {
        model.options.iter().any(|option| {
            selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .is_some_and(|id| {
                    id != option.default_choice && option.choices.iter().any(|c| c.id == id)
                })
        })
    })
}

// ---------------------------------------------------------------------------
// Pure: folder-browser navigation (used by the shell's add-space flow)
// ---------------------------------------------------------------------------

/// Parent of an absolute path; `None` at the filesystem root.
pub fn parent_path(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None; // was "/" (or empty)
    }
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(at) => Some(trimmed[..at].to_string()),
        None => None,
    }
}

/// Join a listing path and an entry name.
pub fn child_path(base: &str, name: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Byte length of `name`'s prefix matching `query`, compared char-for-char
/// case-insensitively; `None` when `query` isn't a prefix of `name`. The
/// length indexes into `name` (not `query`) so the completion suffix keeps
/// the folder's real casing: `("Documents", "doc") → Some(3)` → `"uments"`.
pub fn completion_prefix_len(name: &str, query: &str) -> Option<usize> {
    let mut len = 0;
    let mut name_chars = name.chars();
    for qc in query.chars() {
        let nc = name_chars.next()?;
        if !nc.to_lowercase().eq(qc.to_lowercase()) {
            return None;
        }
        len += nc.len_utf8();
    }
    Some(len)
}

/// Resolve a typed path segment against folder `names` (slash-descend):
/// exact match first — case-SENSITIVE before case-insensitive, so `GitHub/`
/// picks a `GitHub` sibling over `github` — then a unique case-insensitive
/// prefix. Ambiguity resolves to `None`: the slash stays in the query.
pub fn segment_target(names: &[&str], query: &str) -> Option<usize> {
    if let Some(ix) = names.iter().position(|n| *n == query) {
        return Some(ix);
    }
    if let Some(ix) = names
        .iter()
        .position(|n| completion_prefix_len(n, query) == Some(n.len()))
    {
        return Some(ix);
    }
    let mut hits = names
        .iter()
        .enumerate()
        .filter(|(_, n)| completion_prefix_len(n, query).is_some());
    let (ix, _) = hits.next()?;
    hits.next().is_none().then_some(ix)
}

/// Interpret a palette query as a typed path jump: absolute (`/disk2/projects`)
/// or home-relative (`~`, `~/github`). Returns the absolute path to browse,
/// trailing slash trimmed. `home` is the device's resolved home — `None`
/// until the first listing lands, when `~` can't expand yet. A query like
/// `~foo` is a folder name, not a path.
pub fn typed_path_target(query: &str, home: Option<&str>) -> Option<String> {
    let query = query.trim();
    if let Some(rest) = query.strip_prefix('~') {
        let home = home?.trim_end_matches('/');
        if rest.is_empty() {
            return Some(home.to_string());
        }
        let rest = rest.strip_prefix('/')?.trim_end_matches('/');
        return Some(if rest.is_empty() {
            home.to_string()
        } else {
            format!("{home}/{rest}")
        });
    }
    if query.starts_with('/') {
        let trimmed = query.trim_end_matches('/');
        return Some(if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        });
    }
    None
}

/// Breadcrumb segments for a path: `(label, full path)`, root first.
pub fn breadcrumbs(path: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut acc = String::new();
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(segment);
        out.push((segment.to_string(), acc.clone()));
    }
    out
}

/// Directory rows of a listing (files never render in the browser).
pub fn browser_rows(listing: &FolderListing) -> Vec<&zeron_proto::FolderEntry> {
    listing.entries.iter().filter(|e| e.is_dir).collect()
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

/// Sentinel for "no keyboard-highlighted row" (`active`): matches no index,
/// and `usize::MAX as isize == -1` — `menu_step` treats it like `None`, so
/// the first Down lands on row 0.
const NO_ACTIVE_ROW: usize = usize::MAX;

/// Which pane the harness/model picker's icon rail is showing (t3code
/// ModelPickerContent `selectedInstanceId | "favorites"`). `Harness` means
/// "the effective harness's list" — the rail has no browse-without-commit
/// state; clicking a brand icon picks that harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ModelRail {
    Favorites,
    #[default]
    Harness,
}

/// Cache key for the flattened model-row list: any input that changes the
/// list's CONTENT (not its highlight/selection, which render per-row).
#[derive(Clone, PartialEq, Eq)]
struct ModelRowsKey {
    query: String,
    rail: ModelRail,
    effective: Option<HarnessId>,
    locked: bool,
    catalog_rev: u64,
}

/// One row of the model list: the model plus the harness it belongs to —
/// search results and the favorites view mix harnesses, and every row's
/// subline names its harness (t3code ModelListRow `showProvider`).
#[derive(Debug, Clone)]
struct ModelRowData {
    harness: HarnessId,
    harness_name: SharedString,
    model: Model,
}

/// The picker's flattened model rows plus the position of the "OTHER"
/// divider inside them. Keyboard nav, the mod-N badges and Enter all index
/// the ROWS; the divider is a render-only item the list re-inserts, so it
/// can never be highlighted or activated (see [`row_for_list_ix`]).
#[derive(Debug, Default)]
struct ModelRows {
    rows: Vec<ModelRowData>,
    /// Index in `rows` the divider is drawn ABOVE, i.e. the number of
    /// flagship rows. `None` on a filtered list, the favorites tab, or when
    /// one of the two groups would be empty.
    divider: Option<usize>,
}

impl std::ops::Deref for ModelRows {
    type Target = Vec<ModelRowData>;

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl ModelRows {
    /// Item count of the `uniform_list` backing these rows (the divider
    /// occupies one item of its own).
    fn list_len(&self) -> usize {
        self.rows.len() + usize::from(self.divider.is_some())
    }
}

/// Small-caps label of the divider parting the flagship models from the rest.
const OTHER_GROUP_LABEL: &str = "Other";

/// How many leading catalog rows count as flagship for a harness with no
/// curated flagship ids. Catalogs are served newest-first, so this is "the
/// newest two" (plus the `default` alias row, which every such catalog
/// points at its own pick).
const FALLBACK_FLAGSHIP_ROWS: usize = 2;

/// The model ids a harness leads its list with, as id PREFIXES so the `[1m]`
/// context-window variants and dated point releases come along. Defined in
/// ONE place (user request: "the best models first"); everything else falls
/// under the OTHER divider in catalog order.
///
/// Claude Code leads with the three current flagships, Codex with the
/// top-generation GPT family; a harness with no curated set uses its
/// `default` row plus [`FALLBACK_FLAGSHIP_ROWS`] newest catalog rows.
fn flagship_models(harness: HarnessId) -> &'static [&'static str] {
    match harness {
        // The mock harness scripts Claude-flavoured runs and wears the
        // Claude catalog everywhere else.
        HarnessId::ClaudeCode | HarnessId::Mock => {
            &["claude-fable-5-1", "claude-opus-5", "claude-sonnet-5"]
        }
        HarnessId::Codex => &["gpt-6"],
        _ => &["default"],
    }
}

/// Whether `id` is one of the harness's flagship models.
fn is_flagship_model(harness: HarnessId, id: &str) -> bool {
    flagship_models(harness)
        .iter()
        .any(|prefix| id.starts_with(prefix))
}

/// Leading catalog rows a harness takes on TOP of [`flagship_models`]
/// (catalogs are served newest-first). Zero where the ids above are curated
/// and complete.
fn flagship_lead_rows(harness: HarnessId) -> usize {
    match harness {
        HarnessId::ClaudeCode | HarnessId::Mock | HarnessId::Codex => 0,
        _ => FALLBACK_FLAGSHIP_ROWS,
    }
}

/// Split a harness's catalog rows into (flagships, the rest), each in its
/// original order.
fn split_flagships<'a>(
    harness: HarnessId,
    models: &[&'a Model],
) -> (Vec<&'a Model>, Vec<&'a Model>) {
    let lead = flagship_lead_rows(harness);
    let mut taken = 0usize;
    models.iter().copied().partition(|model| {
        if is_flagship_model(harness, &model.id) {
            return true;
        }
        if taken < lead {
            taken += 1;
            return true;
        }
        false
    })
}

/// The row a list item maps to, or `None` for the divider item itself.
fn row_for_list_ix(list_ix: usize, divider: Option<usize>) -> Option<usize> {
    match divider {
        Some(at) if list_ix == at => None,
        Some(at) if list_ix > at => Some(list_ix - 1),
        _ => Some(list_ix),
    }
}

/// The list item a row sits at (the inverse of [`row_for_list_ix`]).
fn list_ix_for_row(row_ix: usize, divider: Option<usize>) -> usize {
    match divider {
        Some(at) if row_ix >= at => row_ix + 1,
        _ => row_ix,
    }
}

/// Which picker popover is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    Branch,
    /// The checkout-kind dropdown in the composer footer (Current
    /// checkout/worktree | New worktree).
    Checkout,
    /// The combined agent/model/traits popover: harness tabs across the top,
    /// the tab's model list beneath the search, and the pinned traits tray
    /// (reasoning ladder + model options) at the bottom — one trigger, one
    /// card (the separate Traits popover folded in here).
    HarnessModel,
    /// New-session canvas only: which project the session mints into. A pick
    /// re-keys everything project-derived (refs, harness/model catalogs) via
    /// the state observer.
    Space,
    /// New-session canvas only: the device project-less sessions run on (a
    /// project pick implies its own host and overrides this).
    Device,
}

pub(crate) struct ReturnComposerFocus;

impl gpui::EventEmitter<ReturnComposerFocus> for Pickers {}

pub struct Pickers {
    state: Entity<AppState>,
    config: DraftConfig,
    /// Sticky last-used picks (zeron `zeron.composer.defaults:v1`): seeds the
    /// new-chat chips and is rewritten on every new-chat pick.
    defaults: ComposerDefaults,
    /// Where [`Self::defaults`] persists (`{data_dir}/composer-defaults.json`);
    /// `None` before bootstrap stamps the state (writes are skipped).
    data_dir: Option<PathBuf>,
    /// Selection the draft picks belong to — switching chats drops them so a
    /// pick made in one chat never leaks into another.
    draft_owner: Option<String>,
    /// Space the branch draft/cache belong to (see the state observer).
    space_owner: Option<String>,
    device_owner: Option<String>,
    target_generation: u64,
    open: popover::Popup<PickerKind>,
    /// The harness/model picker's rail selection (favorites vs the effective
    /// harness's list). Re-primed on every open.
    model_rail: ModelRail,
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    models: HashMap<HarnessId, Loadable<Vec<Model>>>,
    refs: Loadable<Vec<RepoRef>>,
    /// Space id the `refs` slot belongs to (invalidated on space change).
    refs_space: Option<String>,
    /// Highlighted row in the open list (keyboard nav).
    active: usize,
    /// Models-list scroll — keyboard nav keeps the highlighted row in view.
    /// A `UniformListScrollHandle`: the model list virtualizes (7k-model
    /// catalogs must scroll smoothly), and this is its handle; the plain
    /// base handle inside serves the floating scrollbar's metrics.
    model_scroll: gpui::UniformListScrollHandle,
    /// Flattened rows the list/keyboard/⌘N all walk, cached per
    /// [`ModelRowsKey`]: a 7k-model catalog rebuilt+ranked on every
    /// keystroke, arrow press AND render was the picker's open/scroll lag.
    model_rows_cache: std::cell::RefCell<Option<(ModelRowsKey, std::sync::Arc<ModelRows>)>>,
    /// Bumped on every catalog/favorites mutation; invalidates the cache.
    catalog_rev: u64,
    /// Hover/drag state of the floating model-list scrollbar.
    model_bar: popover::MenuScrollbarState,
    /// Painted bounds of the effort slider's track, written during prepaint:
    /// the only thing a pointer x needs to be mapped onto a rung.
    effort_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    /// Shared search / URL / name input, reused across popovers.
    search: Entity<ComposerInput>,
    /// One-shot mute for the next Edited event's highlight reset — armed by
    /// [`Self::toggle`]'s programmatic clear (see the subscription).
    search_reset_muted: bool,
    focus: FocusHandle,
    /// `ZERON_OPEN_PICKER` boot: keep claiming focus until it sticks, so
    /// keyboard nav drives the data-side-opened popover (headless rigs have
    /// no synthetic pointer, but synthetic keys do arrive).
    boot_focus_pending: bool,
    load_task: Option<Task<()>>,
    /// Own slot: the refs load runs concurrently with the eager
    /// harness/model loads — sharing `load_task` would abort one mid-flight.
    refs_task: Option<Task<()>>,
    /// In-flight mid-session `SwitchRef` (the ref being switched to).
    switching: Option<String>,
    switch_task: Option<Task<()>>,
    /// Last mid-session switch failure (shown in the ref popover).
    switch_error: Option<String>,
    mutate_task: Option<Task<()>>,
    _search_events: Subscription,
    _state_observe: Subscription,
    _catalog_observe: Subscription,
}

impl Pickers {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            ComposerInput::new("Search…", cx).with_accessibility_role(gpui::Role::SearchInput)
        });
        let search_events = cx.subscribe(&search, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Edited => {
                // Typing in a filter resets the highlight to the top of the
                // fresh results. `set_text` emits Edited on programmatic
                // clears too, and this subscription runs AFTER `toggle`
                // returns — an unmuted reset clobbers the just-anchored
                // selected row back to 0, leaving the top row wearing a
                // second highlight next to the selection (user report;
                // `toggle` arms the mute right before its clear).
                if !std::mem::take(&mut this.search_reset_muted) {
                    if matches!(
                        this.open_kind(),
                        Some(PickerKind::Branch | PickerKind::Space | PickerKind::Device)
                    ) {
                        this.active = 0;
                    }
                    if this.open_kind() == Some(PickerKind::HarnessModel) {
                        this.active = 0;
                        this.model_scroll_base().set_offset(gpui::Point::default());
                    }
                }
                cx.notify();
            }
            ComposerInputEvent::Submitted | ComposerInputEvent::ModifiedSubmitted => {
                this.on_search_submit(cx)
            }
            // Pasted images/files don't apply to a search box.
            ComposerInputEvent::PastedImages(_)
            | ComposerInputEvent::PastedPaths(_)
            | ComposerInputEvent::CursorMoved
            | ComposerInputEvent::ViewportChanged
            | ComposerInputEvent::MentionNavigate(_)
            | ComposerInputEvent::MentionAccept
            | ComposerInputEvent::MentionDismiss => {}
        });
        // Chat selection / config changes must re-render the chips (child views
        // only re-render on their own notify). A selection change also drops
        // the draft picks — they belonged to the previous chat/new-chat canvas.
        let state_observe = cx.observe(&state, |this: &mut Self, state, cx| {
            let selected = state.read(cx).selected_chat.clone();
            if selected != this.draft_owner {
                this.draft_owner = selected;
                this.config.harness = None;
                this.config.model = None;
                this.config.reasoning = None;
                this.switch_error = None;
            }
            // A space switch invalidates the branch draft + cache — the folder
            // (and possibly the device) changed under them.
            let space = state.read(cx).selected_space.clone();
            let device = state.read(cx).effective_device_id();
            if space != this.space_owner || device != this.device_owner {
                this.space_owner = space;
                this.device_owner = device;
                this.target_generation = this.target_generation.wrapping_add(1);
                this.refs_task = None;
                this.load_task = None;
                this.config.branch = None;
                this.config.checkout = CheckoutKind::default();
                this.refs = Loadable::Idle;
                this.refs_space = None;
                // Catalogs are per-DEVICE (fetched from the space's host):
                // a space switch may land on another device, so refetch.
                this.harnesses = Loadable::Idle;
                this.models.clear();
                this.catalog_rev += 1;
            }
            cx.notify();
        });
        // A Settings → Agents toggle changed some device's enabled set:
        // force-refresh the cached catalog so the rail/chips follow without a
        // restart (stale rows stay visible while the reload runs).
        let catalog_observe = cx.observe_global::<HarnessCatalogChanged>(|this: &mut Self, cx| {
            this.ensure_harnesses(true, cx);
            cx.notify();
        });
        // Dev/testing knob: `ZERON_OPEN_PICKER=model|traits|repo|branch` boots
        // with that popover open — synthetic input can't reach the app on
        // headless compositors, so captures need a data-side path.
        let boot_open = match std::env::var("ZERON_OPEN_PICKER").ok().as_deref() {
            Some("model") => Some(PickerKind::HarnessModel),
            Some("traits") => Some(PickerKind::HarnessModel),
            Some("branch") => Some(PickerKind::Branch),
            Some("checkout") => Some(PickerKind::Checkout),
            Some("project") => Some(PickerKind::Space),
            Some("device") => Some(PickerKind::Device),
            _ => None,
        };
        let mut open = popover::Popup::default();
        if let Some(kind) = boot_open {
            open.open(kind);
        }
        // Sticky last-used picks: loaded synchronously so the very first frame
        // shows the remembered harness/model/reasoning, never a placeholder.
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        // Restore explicit opt-outs as well as project picks before the first frame.
        state.update(cx, |s, _| s.restore_composer_target(&defaults));
        let draft_owner = state.read(cx).selected_chat.clone();
        let space_owner = state.read(cx).selected_space.clone();
        let device_owner = state.read(cx).effective_device_id();
        Self {
            state,
            space_owner,
            device_owner,
            target_generation: 0,
            config: DraftConfig::default(),
            defaults,
            data_dir,
            draft_owner,
            open,
            model_rail: ModelRail::default(),
            harnesses: Loadable::Idle,
            models: HashMap::new(),
            refs: Loadable::Idle,
            refs_space: None,
            active: 0,
            model_scroll: gpui::UniformListScrollHandle::new(),
            model_rows_cache: std::cell::RefCell::new(None),
            catalog_rev: 0,
            model_bar: popover::MenuScrollbarState::default(),
            effort_bounds: std::rc::Rc::new(std::cell::Cell::new(None)),
            search,
            search_reset_muted: false,
            focus: cx.focus_handle(),
            boot_focus_pending: boot_open.is_some(),
            load_task: None,
            refs_task: None,
            switching: None,
            switch_task: None,
            switch_error: None,
            mutate_task: None,
            _search_events: search_events,
            _state_observe: state_observe,
            _catalog_observe: catalog_observe,
        }
    }

    /// Persist the sticky defaults (best-effort; picks are rare and tiny).
    fn save_defaults(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-defaults save failed");
        }
    }

    pub fn draft(&self) -> &DraftConfig {
        &self.config
    }

    /// Harness is locked once the chat exists (feature-inventory §1.7).
    fn harness_locked(&self, cx: &App) -> bool {
        self.state.read(cx).selected_chat.is_some()
    }

    fn engine(&self, cx: &App) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    /// The selected target device when it differs from the connected
    /// engine's own — harness/model catalogs come from the device that RUNS
    /// the agents (the CLIs live there; the viewer may have neither claude
    /// nor codex installed — user report: "can't load codex models/traits
    /// anywhere" from a Mac without codex).
    fn space_target(&self, cx: &App) -> Option<String> {
        let state = self.state.read(cx);
        let device = state.effective_device_id()?;
        (state.local_device_id.as_deref() != Some(device.as_str())).then_some(device)
    }

    /// Effective harness: picked, or the chat's config, or the first listed.
    fn effective_harness(&self, cx: &App) -> Option<HarnessId> {
        if let Some(harness) = self.config.harness {
            return Some(harness);
        }
        if let Some(config) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            return Some(config.harness);
        }
        // New-chat canvas: the remembered last-used harness (sticky defaults),
        // when the loaded catalog still offers it (the device may have
        // disabled it in Settings → Agents since).
        if let Some(harness) = self.defaults.harness {
            let offered = match self.harnesses.ready() {
                Some(list) => offered_harnesses(list).iter().any(|d| d.id == harness),
                None => true, // catalog not loaded yet — trust the memory
            };
            if offered {
                return Some(harness);
            }
        }
        // Fall back to the first OFFERED harness: the registry lists the mock
        // harness first, and resolving chips against it would boot the
        // new-chat canvas onto "Mock" instead of Claude Code + its default
        // model (it stays available under `ZERON_HARNESS=mock`).
        self.harnesses
            .ready()
            .and_then(|list| offered_harnesses(list).first().map(|d| d.id))
    }

    /// Effective model id: the draft pick, the selected chat's config, or (on
    /// the new-chat canvas) the remembered last-used model for the harness.
    fn effective_model_id<'a>(&'a self, cx: &'a App) -> Option<&'a str> {
        if let Some(id) = self.config.model.as_deref() {
            return Some(id);
        }
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat.config.as_ref().and_then(|c| c.model.as_deref());
        }
        let harness = self.effective_harness(cx)?;
        self.defaults.model_for(harness).map(|m| m.id.as_str())
    }

    /// The effort remembered FOR THE SELECTED MODEL (the `effortByModel` map),
    /// falling back to the legacy global pick so an existing install keeps its
    /// level on the model it was last used on.
    fn remembered_effort(&self, cx: &App) -> Option<ReasoningLevel> {
        let harness = self.effective_harness(cx)?;
        let model = self
            .selected_model(cx)
            .map(|m| m.id.as_str())
            .or_else(|| self.effective_model_id(cx))?;
        self.defaults.effort_for(harness, model)
    }

    /// Effective effort, always concrete once the model is known: the draft
    /// pick / chat config, else the level remembered for THIS model, else the
    /// model's default, clamped to the selected model's ladder.
    fn effective_reasoning(&self, cx: &App) -> Option<ReasoningLevel> {
        let explicit = self.config.reasoning.or_else(|| {
            match self.state.read(cx).selected_chat_row() {
                Some(chat) => chat.config.as_ref().and_then(|c| c.reasoning),
                // New chat: whatever this model was last run at.
                None => self.remembered_effort(cx),
            }
        });
        if self.selected_model(cx).is_none() {
            // Catalog not loaded yet: show the explicit value as-is (nothing
            // to clamp against); it resolves to a concrete level on load.
            return explicit;
        }
        resolve_effort(explicit, &self.trait_ladder(cx))
    }

    /// The selected model — concrete from the moment the list loads: the
    /// effective id when the list still offers it, else the harness default
    /// (first row). Never `None` with a non-empty catalog.
    fn selected_model<'a>(&'a self, cx: &'a App) -> Option<&'a Model> {
        let harness = self.effective_harness(cx)?;
        let models = self.models.get(&harness)?.ready()?;
        match self.effective_model_id(cx) {
            Some(id) => models
                .iter()
                .find(|m| m.id == id)
                .or_else(|| default_model(models)),
            None => default_model(models),
        }
    }

    /// The explicit (non-default) option picks: the chat's persisted
    /// selections for existing chats, the remembered picks for the model the
    /// new-chat canvas resolves to (same id [`Self::resolved`] sends).
    fn explicit_options(&self, cx: &App) -> serde_json::Map<String, serde_json::Value> {
        if let Some(chat) = self.state.read(cx).selected_chat_row() {
            return chat
                .config
                .as_ref()
                .map(|c| c.model_options.clone())
                .unwrap_or_default();
        }
        let Some(harness) = self.effective_harness(cx) else {
            return Default::default();
        };
        match self.selected_model(cx) {
            Some(model) => offered_options(
                model,
                self.defaults
                    .model_options_for(harness, &model.id)
                    .cloned()
                    .unwrap_or_default(),
            ),
            // Catalog not loaded (or failed): the picks were validated for
            // this exact model when made, so they are safe to send as-is.
            None => self
                .effective_model_id(cx)
                .and_then(|id| self.defaults.model_options_for(harness, id))
                .cloned()
                .unwrap_or_default(),
        }
    }

    /// The catalog is loaded and offers nothing runnable — the no-agents
    /// state (every enabled harness is missing its CLI, or nothing is
    /// enabled). False while the catalog is still loading or failed
    /// (nothing to conclude yet; offline sends must not be blocked on it).
    pub fn no_agents_available(&self) -> bool {
        self.harnesses
            .ready()
            .is_some_and(|list| offered_harnesses(list).is_empty())
    }

    /// The fully-resolved config the composer threads into the Run request and
    /// `Mutate createChat`: concrete model + reasoning whenever the catalog is
    /// loaded (no "engine picks a default" passthrough).
    pub fn resolved(&self, cx: &App) -> ResolvedRunConfig {
        ResolvedRunConfig {
            harness: self.effective_harness(cx),
            model: self
                .selected_model(cx)
                .map(|m| m.id.clone())
                // Catalog not loaded (offline): still send the id we know.
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
            reasoning: self.effective_reasoning(cx),
            model_options: self.explicit_options(cx),
        }
    }

    /// Prices for the model a send would use right now — the composer's
    /// session cost chip reads this alongside [`Self::resolved`].
    pub fn resolved_pricing(&self, cx: &App) -> Option<zeron_proto::ModelPricing> {
        self.selected_model(cx)?.pricing.clone()
    }

    // ---- open/close ----

    /// The picker that's open AND interactive — `None` while one animates out.
    fn open_kind(&self) -> Option<PickerKind> {
        self.open.as_open().copied()
    }

    /// Whether any picker popover is open (shell-side: session-nav shortcuts
    /// go quiet underneath an open popover instead of yanking the session out
    /// from under it).
    pub fn is_open(&self) -> bool {
        self.open.as_open().is_some()
    }

    /// The picker to render: open or mid-exit.
    fn mounted_kind(&self) -> Option<PickerKind> {
        self.open.get().copied()
    }

    /// Begin the exit animation (shared by every close path).
    fn animate_close(&mut self, cx: &mut Context<Self>) {
        if self.is_open() {
            cx.emit(ReturnComposerFocus);
        }
        self.dismiss(cx);
    }

    /// Outside clicks and navigation keep focus at the clicked destination.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.model_bar = popover::MenuScrollbarState::default();
        if self.open.begin_close() {
            popover::reap_popup(cx, |pickers: &mut Self| &mut pickers.open);
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.animate_close(cx);
        cx.notify();
    }

    /// Capture knob (`ZERON_OPEN_DIALOG=model`): open the combined
    /// harness/model menu programmatically.
    /// A jump-slot press while the model menu is open. The shell's session
    /// bindings (Mod+1…9) win the dispatch race — gpui runs a matched
    /// binding before any key handler — so the shell forwards the slot here
    /// instead of going quiet and eating the very chips the rows advertise
    /// (macOS field report: "cmd shortcuts do nothing in the model
    /// selector"). Returns whether the menu was open and the slot consumed.
    pub fn jump_model_slot(&mut self, slot: usize, cx: &mut Context<Self>) -> bool {
        if self.open_kind() != Some(PickerKind::HarnessModel) {
            return false;
        }
        self.activate_model_index(slot, cx);
        cx.notify();
        true
    }

    pub fn open_model_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_kind() != Some(PickerKind::HarnessModel) {
            self.toggle(PickerKind::HarnessModel, window, cx);
        }
    }

    fn toggle(&mut self, kind: PickerKind, window: &mut Window, cx: &mut Context<Self>) {
        // A press that found this picker open closes it — the card's
        // `on_mouse_down_out` already began the close on that same press,
        // so by click time the popup reads as closed and a plain toggle
        // would reopen it. A press while a DIFFERENT picker is open doesn't
        // count (see note_trigger_press_matching): that click switches.
        let pressed_open = self.open.take_press_was_open();
        if self.open_kind() == Some(kind) || pressed_open {
            self.animate_close(cx);
            if pressed_open {
                cx.emit(ReturnComposerFocus);
            }
            cx.notify();
            return;
        }
        self.open.open(kind);
        // Clearing stale text emits Edited AFTER this function returns —
        // mute that one event so its reset can't clobber the highlight
        // anchored below (the no-op clear is also skipped for the same
        // reason).
        self.search_reset_muted = !self.search.read(cx).text().is_empty();
        self.search.update(cx, |input, cx| {
            input.set_placeholder("Search…", cx);
            if !input.text().is_empty() {
                input.set_text("", cx);
            }
        });
        // Prime the model picker's rail BEFORE anchoring the highlight (the
        // visible rows depend on it): the favorites view when stars exist —
        // t3 ModelPickerContent's initial selection — else the effective
        // harness. Locked chats stay on their own harness.
        if kind == PickerKind::HarnessModel {
            self.model_rail = if !self.harness_locked(cx) && !self.defaults.favorites.is_empty() {
                ModelRail::Favorites
            } else {
                ModelRail::Harness
            };
        }
        // The keyboard-nav highlight starts ON the selected row — row 0
        // otherwise reads as a second active row (user report).
        self.active = match kind {
            PickerKind::Checkout => match self.config.checkout {
                CheckoutKind::Local => 0,
                CheckoutKind::NewWorktree => 1,
            },
            PickerKind::Branch => self.selected_ref_index(cx),
            PickerKind::HarnessModel => self.selected_model_index(cx),
            PickerKind::Space => self.selected_space_index(cx),
            PickerKind::Device => self.selected_device_index(cx),
        };
        if kind == PickerKind::HarnessModel {
            self.model_scroll_base().set_offset(gpui::Point::default());
            self.scroll_model_row_into_view(self.active, cx);
        }
        // Searchable pickers focus the filter input (it sits inside the frame,
        // so the frame's key handler still sees arrows/Enter); the rest focus
        // the frame itself for pure keyboard nav.
        match kind {
            PickerKind::Branch => {
                self.switch_error = None; // stale mid-session failures don't linger
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search refs…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Space => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search projects…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::Device => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search devices…", cx);
                });
                window.focus(&handle, cx);
            }
            PickerKind::HarnessModel => {
                let handle = self.search.read(cx).focus_handle(cx);
                self.search.update(cx, |input, cx| {
                    input.set_placeholder("Search models…", cx);
                });
                window.focus(&handle, cx);
            }
            _ => window.focus(&self.focus, cx),
        }
        match kind {
            // Force: the checkout state moves under us (a send mints a
            // worktree+branch, terminals switch refs) — every open
            // revalidates, keeping stale rows visible until fresh ones land.
            PickerKind::Branch | PickerKind::Checkout => self.ensure_refs(true, cx),
            PickerKind::HarnessModel => {
                // Force: the enabled set moves under us (Settings → Agents,
                // possibly from another viewer) — every open revalidates,
                // keeping current rows visible until the fresh catalog lands.
                self.ensure_harnesses(true, cx);
                // Model discovery can recover after a slow/plugin-heavy ACP
                // cold start. Revalidate on every open instead of pinning a
                // timeout/fallback result until the application restarts.
                self.prefetch_models(true, cx);
            }
            // Projects and devices are already synced state — nothing to load.
            PickerKind::Space | PickerKind::Device => {}
        }
        cx.notify();
    }

    // ---- loads ----

    fn ensure_harnesses(&mut self, force: bool, cx: &mut Context<Self>) {
        // Non-forced (the render loop's eager kick) only loads from Idle: an
        // Error that could re-trigger a load would flip back to Loading
        // before the retry row ever painted (and spam the engine); Retry
        // resets to Idle. FORCED refreshes (a Settings → Agents toggle, a
        // picker open) reload through Ready/Error too — the enabled set just
        // changed under the cache, which otherwise served the boot-time
        // catalog until restart (user report). Stale-while-revalidate: loaded
        // rows stay on screen while the fresh catalog lands.
        let reload = match self.harnesses {
            Loadable::Idle => true,
            Loadable::Loading => false,
            Loadable::Ready(_) | Loadable::Error(_) => force,
        };
        if !reload {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.space_target(cx);
        let generation = self.target_generation;
        if !matches!(self.harnesses, Loadable::Ready(_)) {
            self.harnesses = Loadable::Loading;
            self.catalog_rev += 1;
        }
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            if let Some(target) = &target {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_HARNESSES, serde_json::Value::Object(params))
                .await;
            if let Some(delay) = slow_catalog_delay() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |pickers, cx| {
                if pickers.target_generation != generation {
                    return;
                }
                pickers.catalog_rev += 1;
                pickers.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                pickers.prefetch_models(false, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// Kick a model load for the effective harness AND every offered one, in
    /// parallel — by the time the user opens the picker (or switches rail
    /// tabs) the lists are already there, instead of a per-selection
    /// "Loading models…" round-trip. Each `ensure_models` call is guarded by
    /// its slot state, so re-running this every catalog load/render is free.
    fn prefetch_models(&mut self, force: bool, cx: &mut Context<Self>) {
        let mut targets: Vec<HarnessId> = match self.harnesses.ready() {
            Some(list) => offered_harnesses(list).iter().map(|d| d.id).collect(),
            None => Vec::new(),
        };
        // The committed chat's harness may be outside the offered set (e.g.
        // disabled after the chat was created) — its models still matter.
        if let Some(effective) = self.effective_harness(cx)
            && !targets.contains(&effective)
        {
            targets.push(effective);
        }
        for harness in targets {
            self.ensure_models(harness, force, cx);
        }
    }

    fn ensure_models(&mut self, harness: HarnessId, force: bool, cx: &mut Context<Self>) {
        // Normal prefetches load absent/Idle slots once. Picker-open refreshes
        // also retry Ready/Error slots, while an in-flight load is always
        // reused. Ready rows stay visible until the replacement lands.
        let reload = match self.models.get(&harness) {
            None | Some(Loadable::Idle) => true,
            Some(Loadable::Loading) => false,
            Some(Loadable::Ready(_)) | Some(Loadable::Error(_)) => force,
        };
        if !reload {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let target = self.space_target(cx);
        let generation = self.target_generation;
        if !matches!(self.models.get(&harness), Some(Loadable::Ready(_))) {
            self.models.insert(harness, Loadable::Loading);
            self.catalog_rev += 1;
        }
        cx.spawn(async move |this, cx| {
            let mut params = serde_json::json!({ "harness": harness });
            if let (Some(target), Some(object)) = (&target, params.as_object_mut()) {
                object.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(target.clone()),
                );
            }
            // A plugin-heavy OpenCode cold start can fail once while caches,
            // MCP servers, or plugin runtimes are still warming. Keep this
            // single Loading slot alive for two retries so recovery requires
            // no picker close/reopen and cannot launch duplicate probes.
            let mut attempt = 1_u64;
            let result = loop {
                let result = engine
                    .client()
                    .call(methods::LIST_MODELS, params.clone())
                    .await;
                if result.is_ok() || harness != HarnessId::Opencode || attempt >= 3 {
                    break result;
                }
                if let Err(error) = &result {
                    tracing::warn!(
                        %error,
                        attempt,
                        "OpenCode model discovery failed; retrying automatically"
                    );
                }
                if this.update(cx, |_, _| {}).is_err() {
                    return;
                }
                cx.background_executor()
                    .timer(Duration::from_secs(attempt * 2))
                    .await;
                attempt += 1;
            };
            if let Some(delay) = slow_catalog_delay() {
                cx.background_executor().timer(delay).await;
            }
            this.update(cx, |pickers, cx| {
                if pickers.target_generation != generation {
                    return;
                }
                let loaded = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Model>>(value) {
                        // Display hygiene for catalogs from older engines
                        // (`default` alias rows, orphan `[1m]` variants,
                        // version-less alias labels).
                        Ok(models) => Loadable::Ready(normalize_model_rows(harness, models)),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                if let Loadable::Ready(models) = &loaded {
                    let fresh = pickers
                        .defaults
                        .remember_labels(models.iter().map(|m| (m.id.as_str(), m.label.as_str())));
                    if fresh {
                        pickers.save_defaults();
                    }
                }
                pickers.models.insert(harness, loaded);
                pickers.catalog_rev += 1;
                // A list that landed while its popover is open re-anchors the
                // keyboard highlight onto the selected row (it sat at 0 while
                // loading).
                if pickers.open_kind() == Some(PickerKind::HarnessModel)
                    && pickers.effective_harness(cx) == Some(harness)
                {
                    pickers.active = pickers.selected_model_index(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// ListRefs for the selected SPACE's folder — targeted at the space's
    /// device (relay-forwarded when remote), keyed/invalidated by space id.
    /// Rows carry checkout state (`current`, `worktreePath`) so the picker can
    /// tag refs and the checkout-kind selector can offer worktree reuse.
    fn ensure_refs(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        if !space.git_detected {
            return;
        }
        let fresh = self.refs_space.as_deref() == Some(space.id.as_str());
        if fresh && matches!(self.refs, Loadable::Loading) {
            return; // a load is already in flight
        }
        // Non-forced (the footer's eager kick, re-run every render) only loads
        // from Idle: an Error must WAIT for an explicit retry/reopen (force),
        // or re-render would flip Error back to Loading before the retry row
        // ever paints — an eternal skeleton plus an RPC storm (user report:
        // "the ref dropdown never loads anything").
        if !force && fresh && !matches!(self.refs, Loadable::Idle) {
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        // Stale-while-revalidate: a forced refresh of an already-loaded space
        // keeps the current rows on screen while the reload runs — a send that
        // just minted a worktree (or a terminal-side branch) appears on the
        // popover's next open without the list ever flashing to a skeleton.
        if !(force && fresh && matches!(self.refs, Loadable::Ready(_))) {
            self.refs = Loadable::Loading;
        }
        self.refs_space = Some(space.id.clone());
        self.refs_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::LIST_REFS, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.refs = match result {
                    Ok(value) => match serde_json::from_value::<Vec<RepoRef>>(value) {
                        Ok(refs) => Loadable::Ready(refs),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                // Rows landed under an open, un-searched popover: re-home the
                // nav highlight to the selected row.
                if pickers.open_kind() == Some(PickerKind::Branch)
                    && pickers.search.read(cx).text().is_empty()
                {
                    pickers.active = pickers.selected_ref_index(cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- selections ----

    fn pick_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        // Refs are fixed at creation: an existing session can never move
        // (wing's rule — the footer renders read-only labels there, so this
        // is a belt-and-braces guard).
        if self.state.read(cx).selected_chat_row().is_some() {
            return;
        }
        if row.worktree_path.is_some() {
            // Reuse the ref's existing worktree ("Current worktree") — the
            // t3code `reuseExistingWorktree` path.
            self.config.branch = Some(row.name.clone());
            self.config.checkout = CheckoutKind::Local;
        } else if self.config.checkout == CheckoutKind::NewWorktree || row.current {
            // Base pick for a new worktree, or the already-current ref.
            self.config.branch = Some(row.name.clone());
        } else {
            // Local mode + a plain non-current ref: CHECK OUT the space
            // folder (full t3code `switchRef` — picking `main` means "put my
            // local checkout on main", it must never flip the mode).
            self.switch_draft_ref(row, cx);
            return;
        }
        self.animate_close(cx);
        cx.notify();
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder
    /// (relay-forwarded for remote spaces). Success records the pick and
    /// refreshes tags; failure keeps the popover open with git's message.
    fn switch_draft_ref(&mut self, row: RepoRef, cx: &mut Context<Self>) {
        if self.switching.is_some() {
            return; // one switch at a time
        }
        let Some(space) = self.state.read(cx).selected_space_row().cloned() else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let local = self.state.read(cx).local_device_id.clone();
        self.switch_error = None;
        self.switching = Some(row.name.clone());
        let ref_name = row.name.clone();
        self.switch_task = Some(cx.spawn(async move |this, cx| {
            let mut params = serde_json::Map::new();
            params.insert(
                "repoPath".into(),
                serde_json::Value::String(space.path.clone()),
            );
            params.insert(
                "refName".into(),
                serde_json::Value::String(ref_name.clone()),
            );
            if local.as_deref() != Some(space.device_id.as_str()) {
                params.insert(
                    "targetDeviceId".into(),
                    serde_json::Value::String(space.device_id.clone()),
                );
            }
            let result = engine
                .client()
                .call(methods::SWITCH_REF, serde_json::Value::Object(params))
                .await;
            this.update(cx, |pickers, cx| {
                pickers.switching = None;
                match result {
                    Ok(_) => {
                        pickers.config.branch = Some(ref_name);
                        pickers.animate_close(cx);
                        pickers.ensure_refs(true, cx);
                    }
                    Err(err) => pickers.switch_error = Some(err.to_string()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn pick_checkout(&mut self, kind: CheckoutKind, cx: &mut Context<Self>) {
        if kind == CheckoutKind::Local
            && self.config.checkout == CheckoutKind::NewWorktree
            && self.selected_ref_worktree().is_none()
            && self.selected_ref().is_some_and(|r| !r.current)
        {
            // Back to "Current checkout" with a non-current plain ref picked:
            // drop the pick (we don't checkout the main folder) — the current
            // branch takes over.
            self.config.branch = None;
        }
        self.config.checkout = kind;
        self.animate_close(cx);
        cx.notify();
    }

    fn pick_harness(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        if self.harness_locked(cx) {
            return;
        }
        if self.config.harness != Some(harness) {
            // The remembered model for this harness takes over via the
            // defaults fallback; a foreign pick must not linger.
            self.config.model = None;
            self.config.reasoning = None;
        }
        self.config.harness = Some(harness);
        self.defaults.harness = Some(harness);
        self.save_defaults();
        self.model_scroll_base().set_offset(gpui::Point::default());
        self.ensure_models(harness, false, cx);
        // Re-anchor the keyboard highlight onto the new harness's selected row.
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    fn pick_model(&mut self, model_id: String, cx: &mut Context<Self>) {
        // The card stays open on a pick (user request): model and traits
        // share one popover now, and adjusting the tray right after choosing
        // a model is the expected flow. Esc, click-out, or the chip close it.
        // Effort sticks to the MODEL: the level the new model was last run at
        // (or its catalog default), never the one the previous model carried.
        let restored = self.effective_harness(cx).and_then(|harness| {
            let ladder = self.model_ladder(Some(harness), &model_id);
            if ladder.is_empty() {
                return None;
            }
            resolve_effort(self.defaults.effort_for(harness, &model_id), &ladder)
        });
        if self.state.read(cx).selected_chat.is_some() {
            // Existing chat: persist to the chat row (Mutate setChatConfig) —
            // survives restarts and syncs; next runs in this chat use it.
            self.update_chat_config(cx, move |config| {
                config.model = Some(model_id);
                if let Some(level) = restored {
                    config.reasoning = Some(level);
                }
            });
        } else {
            // New chat: draft pick + sticky last-used memory for this harness.
            self.config.model = Some(model_id.clone());
            // Drop the previous model's effort; this model's own level
            // (remembered or default) takes over.
            self.config.reasoning = restored;
            if let Some(harness) = self.effective_harness(cx) {
                let label = self
                    .models
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .and_then(|models| models.iter().find(|m| m.id == model_id))
                    .map(|m| m.label.clone())
                    .unwrap_or_else(|| model_id.clone());
                self.defaults.remember_model(harness, model_id, label);
                self.save_defaults();
            }
        }
        cx.notify();
    }

    fn pick_reasoning(&mut self, level: ReasoningLevel, cx: &mut Context<Self>) {
        // The pick belongs to the model it was made on, so only that entry is
        // written, so every other model keeps its own effort (user request).
        let target = self.effective_harness(cx).zip(
            self.selected_model(cx)
                .map(|m| m.id.clone())
                .or_else(|| self.effective_model_id(cx).map(str::to_string)),
        );
        if let Some((harness, model)) = target {
            self.defaults.remember_effort(harness, &model, level);
            self.save_defaults();
        }
        // Always a concrete selection (no toggle-back-to-default). The chat
        // config still carries the level so the run request is unchanged.
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| config.reasoning = Some(level));
        } else {
            self.config.reasoning = Some(level);
        }
        cx.notify();
    }

    /// Step the effort slider by `delta` rungs (keyboard ←/→ on the open
    /// picker). No-op for ladder-less models.
    fn step_reasoning(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let ladder = self.trait_ladder(cx);
        if ladder.is_empty() {
            return false;
        }
        let current = self
            .effective_reasoning(cx)
            .and_then(|level| ladder.iter().position(|l| *l == level))
            .unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, ladder.len() as isize - 1) as usize;
        if next as isize == current {
            return true; // consumed: the slider is already at that end
        }
        self.pick_reasoning(ladder[next], cx);
        true
    }

    /// Snap the slider to the rung under `x` (track-relative px) and pick it.
    fn pick_reasoning_at(&mut self, x: f32, width: f32, cx: &mut Context<Self>) {
        let ladder = self.trait_ladder(cx);
        if ladder.is_empty() {
            return;
        }
        let ix = crate::reasoning_slider::snap_step(x, width, ladder.len());
        let Some(level) = ladder.get(ix).copied() else {
            return;
        };
        if self.effective_reasoning(cx) == Some(level) {
            return; // already there: no write, no repaint churn while dragging
        }
        self.pick_reasoning(level, cx);
    }

    fn pick_option(
        &mut self,
        option_id: String,
        choice_id: String,
        default: bool,
        cx: &mut Context<Self>,
    ) {
        if self.state.read(cx).selected_chat.is_some() {
            self.update_chat_config(cx, move |config| {
                if default {
                    config.model_options.remove(&option_id);
                } else {
                    config
                        .model_options
                        .insert(option_id, serde_json::Value::String(choice_id));
                }
            });
        } else if let Some(harness) = self.effective_harness(cx)
            && let Some(model) = self
                .selected_model(cx)
                .filter(|m| m.options.iter().any(|o| o.id == option_id))
                .map(|m| m.id.clone())
        {
            // New chat: the pick is the sticky memory for the catalog model
            // that offered it — never stored unvalidated.
            let options = self.defaults.model_options_mut(harness, &model);
            if default {
                options.remove(&option_id);
            } else {
                options.insert(option_id, serde_json::Value::String(choice_id));
            }
            self.save_defaults();
        }
        cx.notify();
    }

    /// Apply `change` to the selected chat's effective config and persist it:
    /// optimistic row stamp (chips update on click) + `Mutate setChatConfig`
    /// (LWW workspace write — restarts and other devices see it). The written
    /// row always carries the CONCRETE resolved model/reasoning, with the
    /// reasoning re-clamped to the (possibly just-changed) model's ladder.
    fn update_chat_config(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut ChatConfig)) {
        let Some(chat_id) = self.state.read(cx).selected_chat.clone() else {
            return;
        };
        let resolved = self.resolved(cx);
        let Some(mut config) = resolved.chat_config() else {
            return; // harness unknown (catalog + chat row both missing) — nothing safe to write
        };
        // Preserve fields the pickers don't own.
        if let Some(existing) = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.config.as_ref())
        {
            config.sandbox = existing.sandbox;
        }
        change(&mut config);
        // Reasoning must stay concrete for whatever model the row now names —
        // same ladder resolution as [`Self::trait_ladder`] (model levels, else
        // the harness's advertised ladder).
        if let Some(models) = self.models.get(&config.harness).and_then(|l| l.ready()) {
            let mut ladder = config
                .model
                .as_deref()
                .and_then(|id| models.iter().find(|m| m.id == id))
                .map(|m| m.reasoning_levels.clone())
                .unwrap_or_default();
            if ladder.is_empty()
                && let Some(descriptor) = self
                    .harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == config.harness))
            {
                ladder = descriptor.reasoning_levels.clone();
            }
            if !ladder.is_empty() {
                config.reasoning = clamp_reasoning(config.reasoning, &ladder);
            }
            // Options likewise: a model switch must not carry picks the new
            // model doesn't offer (e.g. a 1M context onto Haiku).
            if let Some(model) = config
                .model
                .as_deref()
                .and_then(|id| models.iter().find(|m| m.id == id))
            {
                config.model_options =
                    offered_options(model, std::mem::take(&mut config.model_options));
            }
        }
        self.state.update(cx, |state, cx| {
            state.apply_chat_config(&chat_id, config.clone());
            cx.notify();
        });
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.mutate_task = Some(cx.spawn(async move |_, _| {
            let params = serde_json::json!({
                "op": "setChatConfig",
                "chatId": chat_id,
                "config": config,
            });
            if let Err(err) = engine.client().call(methods::MUTATE, params).await {
                tracing::warn!(error = %err, "setChatConfig mutate failed");
            }
        }));
    }

    // ---- keyboard ----

    /// The traits popover's reasoning ladder (model levels, falling back to
    /// the harness's advertised ladder) — shared by render and keyboard nav.
    fn trait_ladder(&self, cx: &App) -> Vec<ReasoningLevel> {
        let Some(model) = self.selected_model(cx) else {
            return Vec::new();
        };
        let harness = self.effective_harness(cx);
        self.model_ladder(harness, &model.id)
    }

    /// The ladder for ONE model id under `harness`, resolved like
    /// [`Self::trait_ladder`] (model levels, else the harness's advertised
    /// ones), for a model that is not the selected one yet (the effort a
    /// just-picked model restores to).
    fn model_ladder(&self, harness: Option<HarnessId>, model_id: &str) -> Vec<ReasoningLevel> {
        let Some(harness) = harness else {
            return Vec::new();
        };
        let levels = self
            .models
            .get(&harness)
            .and_then(|l| l.ready())
            .and_then(|models| models.iter().find(|m| m.id == model_id))
            .map(|m| m.reasoning_levels.clone())
            .unwrap_or_default();
        if !levels.is_empty() {
            return levels;
        }
        self.harnesses
            .ready()
            .and_then(|list| list.iter().find(|d| d.id == harness))
            .map(|d| d.reasoning_levels.clone())
            .unwrap_or_default()
    }

    /// The harness descriptors the picker rail offers, with the committed
    /// harness force-included even when it's outside the offered set (a
    /// dev session's mock harness, or one disabled after the chat existed).
    fn rail_descriptors(&self, cx: &App) -> Vec<HarnessDescriptor> {
        let Some(list) = self.harnesses.ready() else {
            return Vec::new();
        };
        let mut descriptors = offered_harnesses(list);
        if let Some(effective) = self.effective_harness(cx)
            && !descriptors.iter().any(|d| d.id == effective)
            && let Some(descriptor) = list.iter().find(|d| d.id == effective)
        {
            descriptors.insert(0, descriptor.clone());
        }
        descriptors
    }

    /// The model rows the picker currently shows, flat and in render order —
    /// keyboard nav, ⌘N jumps, Enter and the render walk THE SAME list.
    ///
    /// A live search spans every ready harness (t3: the sidebar hides and
    /// the query ignores it); otherwise the rail selection decides —
    /// favorites across harnesses, or the effective harness's list with its
    /// starred rows floated to the top (t3 `groupFavorites`). A locked chat
    /// restricts every view to its own harness.
    /// Cached [`Self::visible_model_rows`]: selection/highlight changes and
    /// re-renders share one flattened list until an input actually changes.
    fn model_rows(&self, cx: &App) -> std::sync::Arc<ModelRows> {
        let key = ModelRowsKey {
            query: self.search.read(cx).text().trim().to_string(),
            rail: self.model_rail,
            effective: self.effective_harness(cx),
            locked: self.harness_locked(cx),
            catalog_rev: self.catalog_rev,
        };
        if let Some((cached_key, rows)) = self.model_rows_cache.borrow().as_ref()
            && *cached_key == key
        {
            return rows.clone();
        }
        let rows = std::sync::Arc::new(self.visible_model_rows(cx));
        *self.model_rows_cache.borrow_mut() = Some((key, rows.clone()));
        rows
    }

    fn visible_model_rows(&self, cx: &App) -> ModelRows {
        let effective = self.effective_harness(cx);
        let mut descriptors = self.rail_descriptors(cx);
        if self.harness_locked(cx) {
            descriptors.retain(|d| Some(d.id) == effective);
        }
        // Favorite lookups are per-row; the Vec scan made the flatten
        // O(models × favorites).
        let favorites: std::collections::HashSet<(HarnessId, &str)> = self
            .defaults
            .favorites
            .iter()
            .map(|f| (f.harness, f.model.as_str()))
            .collect();
        let query = self.search.read(cx).text().trim().to_string();
        scoped_model_rows(
            &query,
            self.model_rail,
            effective,
            &descriptors,
            |harness| {
                self.models
                    .get(&harness)
                    .and_then(|l| l.ready())
                    .map(|models| models.as_slice())
            },
            |harness, model| favorites.contains(&(harness, model)),
        )
    }

    /// The row the keyboard-nav highlight starts on: the resolved selected
    /// model's index in the VISIBLE rows (the favorites/search views may not
    /// contain it — then 0), 0 while the list is loading.
    fn selected_model_index(&self, cx: &App) -> usize {
        let selected = self.selected_model(cx).map(|m| m.id.clone());
        let effective = self.effective_harness(cx);
        self.model_rows(cx)
            .iter()
            .position(|row| {
                Some(row.harness) == effective && selected.as_deref() == Some(row.model.id.as_str())
            })
            .unwrap_or(0)
    }

    /// The picker's visible row count (keyboard nav bounds). The OTHER
    /// divider is NOT one of them: keyboard nav walks models only.
    fn model_rows_len(&self, cx: &App) -> usize {
        self.model_rows(cx).len()
    }

    /// Scroll the highlighted ROW into view, translating its index past the
    /// divider item the list inserts.
    fn scroll_model_row_into_view(&self, row_ix: usize, cx: &App) {
        let divider = self.model_rows(cx).divider;
        self.model_scroll.scroll_to_item(
            list_ix_for_row(row_ix, divider),
            gpui::ScrollStrategy::Nearest,
        );
    }

    /// Enter on the harness/model popover: pick the highlighted model.
    fn activate_model_row(&mut self, cx: &mut Context<Self>) {
        self.activate_model_index(self.active, cx);
    }

    /// Pick the visible row at `ix` — a foreign-harness row (favorites /
    /// search) switches the harness first, exactly like clicking its rail
    /// icon and then the model.
    fn activate_model_index(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(row) = self.model_rows(cx).get(ix).cloned() else {
            return;
        };
        if self.effective_harness(cx) != Some(row.harness) {
            if self.harness_locked(cx) {
                return;
            }
            self.pick_harness(row.harness, cx);
        }
        self.pick_model(row.model.id, cx);
    }

    /// Star/unstar a model and persist it with the sticky defaults.
    fn toggle_model_favorite(&mut self, harness: HarnessId, model: &str, cx: &mut Context<Self>) {
        self.defaults.toggle_favorite(harness, model);
        self.save_defaults();
        self.catalog_rev += 1;
        // Starring REORDERS the list (stars float to the top / leave the
        // favorites view) — re-home the keyboard highlight onto the SELECTED
        // row so exactly one row reads highlighted afterwards. Following the
        // starred row instead left its cursor wash next to the selected
        // row's ring: "two highlighted rows" (user report, twice).
        self.active = self.selected_model_index(cx);
        cx.notify();
    }

    fn filtered_ref_rows(&self, cx: &App) -> Vec<RepoRef> {
        let Some(refs) = self.refs.ready() else {
            return Vec::new();
        };
        let names: Vec<String> = refs.iter().map(|r| r.name.clone()).collect();
        let query = self.search.read(cx).text().to_string();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| refs[ix].clone())
            .collect()
    }

    // ---- checkout resolution (the t3code env-mode semantics) ----

    /// Index of the highlighted-by-default row in the (filtered) ref list:
    /// the session's branch on an existing chat, the draft pick on a new one,
    /// else the current branch. Capped to the displayed window.
    fn selected_ref_index(&self, cx: &App) -> usize {
        let rows = self.filtered_ref_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone())
            .or_else(|| self.config.branch.clone());
        let index = match selected {
            Some(name) => rows.iter().position(|r| r.name == name).unwrap_or(0),
            None => rows.iter().position(|r| r.current).unwrap_or(0),
        };
        index.min(MAX_REF_ROWS.saturating_sub(1))
    }

    /// The picked ref's row, else the repo's current branch's row.
    fn selected_ref(&self) -> Option<&RepoRef> {
        let refs = self.refs.ready()?;
        match self.config.branch.as_deref() {
            Some(name) => refs.iter().find(|r| r.name == name),
            None => refs.iter().find(|r| r.current),
        }
    }

    /// The picked (or current) ref's name.
    fn effective_ref_name(&self) -> Option<String> {
        self.config
            .branch
            .clone()
            .or_else(|| self.selected_ref().map(|r| r.name.clone()))
    }

    /// The existing worktree the picked ref is materialized in, if any.
    fn selected_ref_worktree(&self) -> Option<String> {
        self.selected_ref().and_then(|r| r.worktree_path.clone())
    }

    /// The resolved on-send checkout action for a new session.
    pub fn checkout_plan(&self) -> CheckoutPlan {
        match self.config.checkout {
            CheckoutKind::NewWorktree => CheckoutPlan::NewWorktree {
                base: self.effective_ref_name(),
            },
            CheckoutKind::Local => match self.selected_ref_worktree() {
                Some(path) => CheckoutPlan::ReuseWorktree {
                    path,
                    branch: self.effective_ref_name().unwrap_or_default(),
                },
                None => CheckoutPlan::CurrentCheckout {
                    branch: self.effective_ref_name(),
                },
            },
        }
    }

    /// Label of the checkout-kind trigger (t3code `resolveEnvModeLabel` /
    /// `resolveCurrentWorkspaceLabel`).
    fn checkout_label(&self) -> &'static str {
        match self.config.checkout {
            CheckoutKind::NewWorktree => "New worktree",
            CheckoutKind::Local => {
                if self.selected_ref_worktree().is_some() {
                    "Current worktree"
                } else {
                    "Current checkout"
                }
            }
        }
    }

    /// Label of the ref trigger: `From <ref>` only when a NEW worktree will be
    /// created off it (t3code `getBranchTriggerLabel`); the bare name otherwise.
    fn ref_label(&self) -> SharedString {
        match (self.config.checkout, self.effective_ref_name()) {
            (_, None) => SharedString::from("Select ref"),
            (CheckoutKind::NewWorktree, Some(name)) => SharedString::from(format!("From {name}")),
            (CheckoutKind::Local, Some(name)) => SharedString::from(name),
        }
    }

    // ---- the space picker (new-session canvas) ----

    /// The picker's project rows: scoped to the canvas's device — the device
    /// switcher narrows the list, projects on other devices don't show
    /// (pick the device first, then its project). Unscoped only while the
    /// device is still unknown (pre-probe boot).
    fn scoped_space_rows(&self, cx: &App) -> Vec<Space> {
        let state = self.state.read(cx);
        let device = state.effective_device_id();
        state
            .spaces_sorted()
            .into_iter()
            .filter(|s| match device.as_deref() {
                Some(d) => s.device_id == d,
                None => true,
            })
            .cloned()
            .collect()
    }

    /// [`Self::scoped_space_rows`] matching the search query, ranked
    /// (`popover::filter_indices`).
    fn filtered_space_rows(&self, cx: &App) -> Vec<Space> {
        let query = self.search.read(cx).text().to_string();
        let spaces = self.scoped_space_rows(cx);
        let names: Vec<String> = spaces
            .iter()
            .map(|s| s.display_name().to_string())
            .collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| spaces[ix].clone())
            .collect()
    }

    /// Current project row on an unsearched open, or the final opt-out row.
    /// An implicit empty selection has no highlight until the user navigates.
    fn selected_space_index(&self, cx: &App) -> usize {
        if self.state.read(cx).no_project {
            return self.scoped_space_rows(cx).len();
        }
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        selected
            .as_deref()
            .and_then(|id| self.scoped_space_rows(cx).iter().position(|s| s.id == id))
            .unwrap_or(NO_ACTIVE_ROW)
    }

    /// Re-home the canvas onto another project. The state observer does the
    /// heavy lifting: branch draft, ref cache, and the per-device
    /// harness/model catalogs all invalidate on the project change.
    fn pick_space(&mut self, space_id: String, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            s.auto_selected = true;
            s.select_space(Some(space_id), cx);
        });
        self.remember_target(cx);
        self.close(cx);
    }

    fn pick_no_project(&mut self, cx: &mut Context<Self>) {
        self.state.update(cx, |s, cx| {
            // A late opening chats frame must not auto-open an old session
            // after the user has explicitly chosen the new-session target.
            s.auto_selected = true;
            s.select_space(None, cx);
        });
        self.remember_target(cx);
        self.close(cx);
    }

    fn pick_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        self.state
            .update(cx, |s, cx| s.select_device(device_id, cx));
        self.remember_target(cx);
        self.close(cx);
    }

    /// Persist the device/project picks — the "last selected" defaults the
    /// next boot's canvas restores.
    fn remember_target(&mut self, cx: &App) {
        {
            let state = self.state.read(cx);
            self.defaults.device = state
                .selected_device
                .clone()
                .or_else(|| state.local_device_id.clone());
            self.defaults.project = state.selected_space.clone();
            self.defaults.no_project = state.no_project;
        }
        if let Some(dir) = &self.data_dir {
            if let Err(err) = self.defaults.save(dir) {
                tracing::warn!(error = %err, "composer-defaults save failed");
            }
        }
    }

    /// Devices in picker order: this device first, then by name.
    fn device_rows(&self, cx: &App) -> Vec<zeron_proto::Device> {
        let state = self.state.read(cx);
        let local = state.local_device_id.clone();
        let mut devices: Vec<zeron_proto::Device> = state.devices.clone();
        devices.sort_by_key(|d| {
            (
                local.as_deref() != Some(d.id.as_str()),
                d.name.to_lowercase(),
                d.id.clone(),
            )
        });
        devices
    }

    /// [`Self::device_rows`] filtered by the search box (same ranked
    /// substring match as the project rows).
    fn filtered_device_rows(&self, cx: &App) -> Vec<zeron_proto::Device> {
        let query = self.search.read(cx).text().to_string();
        let rows = self.device_rows(cx);
        let names: Vec<String> = rows.iter().map(|d| d.name.clone()).collect();
        popover::filter_indices(&query, &names)
            .into_iter()
            .map(|ix| rows[ix].clone())
            .collect()
    }

    fn selected_device_index(&self, cx: &App) -> usize {
        let effective = self.state.read(cx).effective_device_id();
        self.device_rows(cx)
            .iter()
            .position(|d| Some(d.id.as_str()) == effective.as_deref())
            .unwrap_or(0)
    }

    /// The device popover: search + one row per device (name, muted "offline"
    /// tag, check on the canvas's effective device).
    fn render_device_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let now = chrono::Utc::now();
        let rows = self.filtered_device_rows(cx);
        let (effective, local, online): (Option<String>, Option<String>, Vec<bool>) = {
            let state = self.state.read(cx);
            (
                state.effective_device_id(),
                state.local_device_id.clone(),
                rows.iter()
                    .map(|d| state.device_online(&d.id, now))
                    .collect(),
            )
        };
        let active = self.active;
        let body: AnyElement =
            if rows.is_empty() {
                div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No devices match."))
                    .into_any_element()
            } else {
                div()
                    .id("device-list")
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .max_h(px(224.0))
                    .overflow_y_scroll()
                    .children(rows.into_iter().zip(online).enumerate().map(
                        |(ix, (device, online))| {
                            let is_local = local.as_deref() == Some(device.id.as_str());
                            let label: SharedString = device.name.clone().into();
                            let is_selected = effective.as_deref() == Some(device.id.as_str());
                            let pick_id = device.id.clone();
                            popover::menu_row_nav(
                                &theme,
                                is_selected,
                                ix == active,
                                format!("device-row-{ix}"),
                            )
                            .id(("device-row", ix))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.pick_device(pick_id.clone(), cx);
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(label))
                            // The local device wears a muted right-aligned "You"
                            // instead of a "(this device)" suffix in the name.
                            .when(is_local, |el| {
                                el.child(
                                    div()
                                        .flex_none()
                                        .text_size(crate::typography::ui_rems(10.0))
                                        .text_color(theme.text_muted.opacity(0.45))
                                        .child(SharedString::from("You")),
                                )
                            })
                            // Disconnected glyph, not the word (user request).
                            .when(!online, |el| {
                                el.child(
                                    crate::icons::icon(crate::icons::WIFI_OFF)
                                        .size(px(12.0))
                                        .flex_none()
                                        .text_color(theme.warning.opacity(0.8)),
                                )
                            })
                        },
                    ))
                    .into_any_element()
            };
        div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body)
            .into_any_element()
    }

    /// The project popover: search + one row per project on the picked device
    /// (check on the current pick), then "New project…" and the opt-out rows. Rows
    /// are device-scoped, so no per-row `@ device` tag — the device chip next
    /// door names the host.
    fn render_space_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let rows = self.filtered_space_rows(cx);
        let selected = self
            .state
            .read(cx)
            .selected_space_row()
            .map(|s| s.id.clone());
        let active = self.active;
        let no_project_index = rows.len();
        let body: AnyElement = if rows.is_empty() {
            // Distinguish "the filter ate everything" from "this device has
            // no projects yet" — the scoped list makes the latter common.
            let empty: &str = if self.search.read(cx).text().is_empty() {
                "No projects on this device."
            } else {
                "No projects match."
            };
            div()
                .p(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from(empty.to_string()))
                .into_any_element()
        } else {
            div()
                .id("space-list")
                .flex()
                .flex_col()
                .gap(px(2.0))
                .max_h(px(224.0))
                .overflow_y_scroll()
                .children(rows.into_iter().enumerate().map(|(ix, space)| {
                    let label: SharedString = space.display_name().to_string().into();
                    let is_selected = selected.as_deref() == Some(space.id.as_str());
                    let pick_id = space.id.clone();
                    popover::menu_row_nav(
                        &theme,
                        is_selected,
                        ix == active,
                        format!("space-row-{ix}"),
                    )
                    .id(("space-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.pick_space(pick_id.clone(), cx);
                    }))
                    .child(div().flex_1().min_w_0().truncate().child(label))
                }))
                .into_any_element()
        };
        let no_project = popover::menu_row_nav(
            &theme,
            self.state.read(cx).no_project,
            active == no_project_index,
            "project-none".to_string(),
        )
        .id("project-none")
        .on_click(cx.listener(|this, _, _, cx| this.pick_no_project(cx)))
        .child(
            crate::icons::icon(crate::icons::CLOSE)
                .size(px(12.0))
                .flex_none()
                .text_color(theme.text_muted.opacity(0.7)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child("Don't work in a project"),
        );
        // Action row under a hairline: mint a project.
        let new_project = popover::menu_row_nav(&theme, false, false, "project-new".to_string())
            .id("project-new")
            .on_click(cx.listener(|this, _, window, cx| {
                this.dismiss(cx);
                window.dispatch_action(Box::new(crate::shell::AddSpacePalette), cx);
            }))
            .child(
                crate::icons::icon(crate::icons::PLUS)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from("New project…")),
            );
        div()
            .flex()
            .flex_col()
            // Same 2px rhythm as the list's own row gap — the action rows
            // sat flush while list rows breathed (user report).
            .gap(px(2.0))
            .child(self.search_box(&theme))
            .child(body)
            .child(
                // Full-bleed through the card's 4px inset — a divider
                // stopping short of the edges read as a mistake.
                div()
                    .my(px(2.0))
                    .mx(px(-4.0))
                    .h(px(1.0))
                    .flex_none()
                    .bg(theme.border.opacity(0.6)),
            )
            .child(new_project)
            .child(no_project)
            .into_any_element()
    }

    fn on_search_submit(&mut self, cx: &mut Context<Self>) {
        if self.open_kind() == Some(PickerKind::Branch)
            && let Some(row) = self.filtered_ref_rows(cx).into_iter().nth(self.active)
        {
            self.pick_ref(row, cx);
        }
        if self.open_kind() == Some(PickerKind::Space) {
            let rows = self.filtered_space_rows(cx);
            if let Some(space) = rows.get(self.active) {
                self.pick_space(space.id.clone(), cx);
            } else if self.active == rows.len() {
                self.pick_no_project(cx);
            }
        }
        if self.open_kind() == Some(PickerKind::Device)
            && let Some(device) = self.filtered_device_rows(cx).into_iter().nth(self.active)
        {
            self.pick_device(device.id, cx);
        }
        // The model search box submits the highlighted row (Enter reaches
        // here via the input's Submitted event while it holds focus).
        if self.open_kind() == Some(PickerKind::HarnessModel) {
            self.activate_model_row(cx);
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &Window, cx: &mut Context<Self>) {
        // The frame stays mounted (and possibly focused) through the exit
        // animation — keys must not drive a dying popover.
        if !self.open.is_open() {
            return;
        }
        // ⌘1…⌘9 jump-picks the Nth visible model row (t3 modelPickerKeys;
        // the chips on the rows advertise these).
        if self.open_kind() == Some(PickerKind::HarnessModel)
            && event.keystroke.modifiers.platform
            && let Ok(n) = event.keystroke.key.parse::<usize>()
            && (1..=9).contains(&n)
        {
            self.activate_model_index(n - 1, cx);
            cx.notify();
            return;
        }
        let search_focused = self.search.read(cx).focus_handle(cx).is_focused(window);
        // ←/→ step the effort slider (the ladder rows it replaced were walked
        // with ↑/↓, which still belong to the model list). Only while the
        // search box holds no text: there the arrows are caret motion.
        if self.open_kind() == Some(PickerKind::HarnessModel)
            && !event.keystroke.modifiers.platform
            && !event.keystroke.modifiers.control
            && (!search_focused || self.search.read(cx).text().is_empty())
        {
            let delta = match event.keystroke.key.as_str() {
                "left" => Some(-1),
                "right" => Some(1),
                _ => None,
            };
            if let Some(delta) = delta
                && self.step_reasoning(delta, cx)
            {
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            MenuKey::Escape => {
                self.animate_close(cx);
                cx.notify();
                cx.stop_propagation();
            }
            MenuKey::Up | MenuKey::Down => {
                let delta = if key == MenuKey::Up { -1 } else { 1 };
                let count = match self.open_kind() {
                    Some(PickerKind::Branch) => self.filtered_ref_rows(cx).len().min(MAX_REF_ROWS),
                    Some(PickerKind::Checkout) => 2,
                    // Keyboard nav walks the MODEL list only; the traits
                    // chips below (reasoning ladder, model options) are
                    // mouse-only.
                    Some(PickerKind::HarnessModel) => self.model_rows_len(cx),
                    Some(PickerKind::Space) => self.filtered_space_rows(cx).len() + 1,
                    Some(PickerKind::Device) => self.filtered_device_rows(cx).len(),
                    None => 0,
                };
                let current = (self.active != NO_ACTIVE_ROW).then_some(self.active);
                self.active = popover::menu_step(current, count, delta).unwrap_or(0);
                // Keep the highlighted MODEL row in view (the rows are the
                // scroll container's direct children, so indices map 1:1);
                // the traits chips below live in the pinned tray and never
                // need scrolling into view.
                if self.open_kind() == Some(PickerKind::HarnessModel)
                    && self.active < self.model_rows_len(cx)
                {
                    self.scroll_model_row_into_view(self.active, cx);
                }
                cx.notify();
            }
            MenuKey::Enter if !search_focused => {
                if self.open_kind() == Some(PickerKind::HarnessModel) {
                    self.activate_model_row(cx);
                } else if self.open_kind() == Some(PickerKind::Checkout) {
                    let kind = if self.active == 0 {
                        CheckoutKind::Local
                    } else {
                        CheckoutKind::NewWorktree
                    };
                    self.pick_checkout(kind, cx);
                } else {
                    self.on_search_submit(cx);
                }
            }
            _ => {}
        }
    }

    // ---- render ----

    #[allow(clippy::too_many_arguments)]
    fn trigger_chip(
        &self,
        kind: PickerKind,
        label: SharedString,
        set: bool,
        chip_icon: Option<(&'static str, Option<gpui::Hsla>)>,
        // The chip never collapses while identity resolves (user report):
        // `icon_loading` swaps the brand slot for the pixel-glyph loader
        // (harness unknown), `label_loading` swaps the text for a ghost bar
        // (model unknown).
        icon_loading: bool,
        label_loading: bool,
        suffix: Option<(SharedString, Option<gpui::Hsla>)>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id: &'static str = match kind {
            PickerKind::Branch => "picker-branch",
            PickerKind::Checkout => "picker-checkout",
            PickerKind::HarnessModel => "picker-model",
            PickerKind::Space => "picker-space",
            PickerKind::Device => "picker-device",
        };
        let open = self.open_kind() == Some(kind);
        // Ghost pill (zeron composer/styles.tsx `pill`): `h-8 rounded-lg px-2.5
        // gap-1.5 text-[12px] font-medium text-muted-foreground`, icons size-4,
        // hover/open wash — no border, no caret; the actions row stays quiet.
        div()
            .id(id)
            .h(px(32.0))
            .max_w(px(248.0))
            // Shrinkable under row pressure — four footer chips share one
            // line; without min_w_0 they overflowed and painted overlapped.
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            // zeron composer/styles.tsx `pill`: `transition-colors` — the wash
            // and text brighten fade over 150ms.
            .text_color(motion::hover_blend(
                id,
                if set {
                    theme.text.opacity(0.9)
                } else {
                    theme.text_muted
                },
                theme.text,
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .when(icon_loading, |el| {
                el.child(div().flex_none().child(crate::loaders::mini_glyph_spinner(
                    "picker-chip-loader",
                    2.0,
                    theme.glyph,
                    cx.entity_id(),
                    cx,
                )))
            })
            .when_some(
                (!icon_loading).then_some(chip_icon).flatten(),
                |el, (path, tint)| {
                    el.child(
                        crate::icons::icon(path)
                            .size(px(16.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    )
                },
            )
            .when(label_loading, |el| {
                el.child(popover::skeleton_bar(56.0, cx.entity_id(), cx))
            })
            .when(!label_loading, |el| {
                el.child(div().min_w_0().truncate().child(label))
            })
            // The effort half of the combined model+effort chip (and the space
            // chip's "@ device" tag): muted, no icon — one button, two tones.
            // `tint` overrides the muted tone (the offline warning). Under row
            // pressure the suffix yields FIRST (large shrink factor) so the
            // model name — the run's identity — truncates last.
            .when_some(suffix, |el, (suffix, tint)| {
                el.child(
                    div()
                        .flex_shrink(1000.0)
                        .min_w_0()
                        .truncate()
                        .text_color(tint.unwrap_or(theme.text_muted.opacity(0.7)))
                        .child(suffix),
                )
            })
    }

    /// A footer-row trigger (t3code ghost `Button size="xs"`): leading icon,
    /// truncating label, trailing chevron — smaller and quieter than the
    /// in-pill chips.
    fn footer_chip(
        &self,
        kind: PickerKind,
        id: &'static str,
        icon_path: &'static str,
        label: SharedString,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let open = self.open_kind() == Some(kind);
        div()
            .id(id)
            .h(px(20.0))
            .max_w(px(280.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(FOOTER_CHIP_RADIUS))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(motion::hover_blend(
                id,
                theme.text_muted.opacity(0.7),
                theme.text.opacity(0.8),
            ))
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _, _, _| {
                    this.open.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.toggle(kind, window, cx)))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(div().min_w_0().truncate().child(label))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.5)),
            )
    }

    /// A read-only footer label (locked sessions — t3code's
    /// `resolveLockedWorkspaceLabel` span).
    fn footer_label(icon_path: &'static str, label: SharedString, theme: &Theme) -> gpui::Div {
        div()
            .h(px(20.0))
            // Four of these share one row now (device, project, checkout,
            // ref): cap each early and let them SHRINK (`min_w_0`) — without
            // it the clusters overflowed into each other and the labels
            // painted overlapped (user report).
            .max_w(px(160.0))
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(
                crate::icons::icon(icon_path)
                    .size(px(12.0))
                    .text_color(theme.text_muted.opacity(0.6)),
            )
            .child(div().min_w_0().truncate().child(label))
    }

    /// New-session destination controls. Machine and project form the
    /// original chip-only cluster floating above the composer's trailing edge.
    pub fn render_new_thread_target_selectors(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Space) => {
                let content = self.render_space_popover(cx);
                Some((PickerKind::Space, self.popover_frame(280.0, content, cx)))
            }
            Some(PickerKind::Device) => {
                let content = self.render_device_popover(cx);
                Some((PickerKind::Device, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };
        let (device_label, project_label, offline) = {
            let state = self.state.read(cx);
            let device_id = state.effective_device_id();
            let device_label: SharedString = device_id
                .as_deref()
                .and_then(|id| state.device_name(id))
                .map(str::to_string)
                .unwrap_or_else(|| "This device".to_string())
                .into();
            let offline = device_id
                .as_deref()
                .is_some_and(|id| !state.device_online(id, chrono::Utc::now()));
            let project_label: SharedString = state
                .selected_space_row()
                .map(|s| s.display_name().to_string())
                .unwrap_or_else(|| "No project".to_string())
                .into();
            (device_label, project_label, offline)
        };
        let device_chip = self
            .footer_chip(
                PickerKind::Device,
                "picker-device",
                crate::icons::MONITOR,
                device_label,
                &theme,
                cx,
            )
            .when(offline, |el| el.text_color(theme.warning.opacity(0.8)));
        let project_chip = self.footer_chip(
            PickerKind::Space,
            "picker-project",
            crate::icons::FOLDER,
            project_label,
            &theme,
            cx,
        );
        div()
            .flex_none()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .child(attach_overlay(
                device_chip,
                &mut overlay,
                PickerKind::Device,
                "device-popover",
                closing,
            ))
            .child(attach_overlay_end(
                project_chip,
                &mut overlay,
                PickerKind::Space,
                "project-popover",
                closing,
            ))
            .into_any_element()
    }

    /// New-session Git controls. Checkout mode and branch form the original
    /// chip-only cluster floating below the composer's leading edge.
    pub fn render_new_thread_git_selectors(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let git = self
            .state
            .read(cx)
            .selected_space_row()
            .is_some_and(|space| space.git_detected);
        if !git {
            return None;
        }
        self.ensure_refs(false, cx);
        let theme = Theme::of(cx).clone();
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            _ => None,
        };
        let kind_icon = match (self.config.checkout, self.selected_ref_worktree().is_some()) {
            (CheckoutKind::Local, false) => crate::icons::FOLDER,
            _ => crate::icons::FOLDER_WITH_FILES,
        };
        let checkout_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        let branch_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            self.ref_label(),
            &theme,
            cx,
        );
        Some(
            workspace_footer_row()
                .child(attach_overlay_below(
                    checkout_chip,
                    &mut overlay,
                    PickerKind::Checkout,
                    "checkout-popover",
                    closing,
                ))
                .child(attach_overlay_below(
                    branch_chip,
                    &mut overlay,
                    PickerKind::Branch,
                    "branch-popover",
                    closing,
                ))
                .into_any_element(),
        )
    }

    /// The composer footer row: checkout-kind + ref, LEFT-aligned, only when
    /// the picked (or session's) project has git. New sessions use the floating
    /// chip clusters; sessions name their target in the titlebar.
    pub fn render_footer(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // A selected chat whose workspace row hasn't synced yet (the moment
        // right after send mints it) still renders the DRAFT footer — the
        // values are identical, so the toolbar never blinks through a
        // half-empty locked state.
        let (space, session, change_request) = {
            let state = self.state.read(cx);
            let space = state.selected_space_row().cloned();
            let session = state
                .selected_chat
                .as_ref()
                .and_then(|_| state.selected_chat_row().cloned());
            let change_request = session
                .as_ref()
                .and_then(|chat| state.change_request_for_chat(chat).cloned());
            (space, session, change_request)
        };
        let row = || {
            // The composer owns the row's animated reveal and negative bottom
            // margin. Keeping that geometry outside this reusable content
            // lets the new-thread route handoff collapse the footer without
            // clipping its controls or changing its steady-state spacing.
            // `w_full` is load-bearing: without it the canvas layout sizes
            // the row to CONTENT, and the left cluster's flex_1 (basis 0)
            // collapsed to zero width — both clusters painted from the same
            // origin, chips overlapping (user report).
            workspace_footer_row().px(px(10.0))
        };

        if let Some(chat) = &session {
            // Sessions never move: read-only checkout-kind + ref labels,
            // LEFT-aligned, only when the session's project has git. The
            // target (project @ device) lives in the titlebar now.
            let Some(space) = space.as_ref().filter(|s| s.git_detected) else {
                return None;
            };
            let is_worktree = chat.cwd.as_deref().is_some_and(|cwd| cwd != space.path);
            let (icon_path, label) = if is_worktree {
                (crate::icons::FOLDER_WITH_FILES, "Worktree")
            } else {
                (crate::icons::FOLDER, "Local checkout")
            };
            // Keep the same reading order and leading edge as the draft.
            let left = div()
                .flex()
                .flex_row()
                .items_center()
                .min_w_0()
                .child(Self::footer_label(
                    icon_path,
                    SharedString::from(label),
                    &theme,
                ));
            let right = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .min_w_0()
                .child(Self::footer_label(
                    crate::icons::GIT_BRANCH,
                    chat.branch
                        .clone()
                        .map(SharedString::from)
                        .unwrap_or_else(|| SharedString::from("No ref")),
                    &theme,
                ));
            // Checkout + branch stay together. PR and usage form the trailing
            // status group, independently of the branch label's length.
            return Some(
                row()
                    .pr_0()
                    .child(left)
                    .child(right)
                    .child(div().flex_1().min_w_0())
                    .when_some(change_request, |el, summary| {
                        el.child(div().flex_none().child(
                            crate::change_requests::pull_request_badge(
                                "composer-pull-request".into(),
                                summary,
                                crate::change_requests::ChangeRequestBadgeSurface::Composer,
                                &theme,
                            ),
                        ))
                    })
                    .into_any_element(),
            );
        }

        // New-session draft: checkout + ref only, LEFT-aligned (device +
        // project live in the row above the pill now).
        let git = space.as_ref().is_some_and(|s| s.git_detected);
        if !git {
            return None;
        }
        // Refs feed the draft labels — eager + idempotent.
        self.ensure_refs(false, cx);
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            Some(PickerKind::Branch) => {
                let content = self.render_branch_popover(cx);
                Some((PickerKind::Branch, self.popover_frame(320.0, content, cx)))
            }
            Some(PickerKind::Checkout) => {
                let content = self.render_checkout_popover(cx);
                Some((PickerKind::Checkout, self.popover_frame(224.0, content, cx)))
            }
            // Space/Device popovers mount in the floating row above the pill.
            _ => None,
        };

        let ref_label = self.ref_label();
        let ref_chip = self.footer_chip(
            PickerKind::Branch,
            "picker-branch",
            crate::icons::GIT_BRANCH,
            ref_label,
            &theme,
            cx,
        );
        let kind_icon = match (self.config.checkout, self.selected_ref_worktree().is_some()) {
            (CheckoutKind::Local, false) => crate::icons::FOLDER,
            _ => crate::icons::FOLDER_WITH_FILES,
        };
        let kind_chip = self.footer_chip(
            PickerKind::Checkout,
            "picker-checkout",
            kind_icon,
            SharedString::from(self.checkout_label()),
            &theme,
            cx,
        );
        // Match the floating draft's adjacent checkout/ref pair, including
        // while the newly created session is waiting for its workspace row.
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay(
                kind_chip,
                &mut overlay,
                PickerKind::Checkout,
                "checkout-popover",
                closing,
            ));
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .child(attach_overlay(
                ref_chip,
                &mut overlay,
                PickerKind::Branch,
                "branch-popover",
                closing,
            ));
        Some(row().child(left).child(right).into_any_element())
    }

    fn popover_frame(&self, width: f32, content: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card(&theme)
            .w(px(width))
            // zeron caps its tallest picker at min(640px, 75vh).
            .max_h(px(640.0))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.is_open() && !this.focus.contains_focused(window, cx) {
                        window.focus(&this.focus, cx);
                    }
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.dismiss(cx);
                if this.focus.contains_focused(window, cx) {
                    window.blur();
                }
            }))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    /// [`Self::popover_frame`] without the p-1 inset — the harness/model
    /// picker's rail + list panes bleed to the card edge (zeron
    /// harness-model-picker.tsx `className="w-80 p-0"`).
    fn popover_frame_flush(
        &self,
        width: f32,
        content: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        popover::popover_card_flush(&theme)
            .w(px(width))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.is_open() && !this.focus.contains_focused(window, cx) {
                        window.focus(&this.focus, cx);
                    }
                }),
            )
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.dismiss(cx);
                if this.focus.contains_focused(window, cx) {
                    window.blur();
                }
            }))
            .flex()
            .flex_col()
            .child(content)
            .into_any_element()
    }

    fn search_box(&self, theme: &Theme) -> AnyElement {
        popover::search_input_frame(theme, self.search.clone().into_any_element())
            .into_any_element()
    }

    fn retry_row(
        &self,
        id: &'static str,
        message: &str,
        kind: PickerKind,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        popover::error_row(theme, message)
            .child(
                div()
                    .id(id)
                    .px(px(Theme::SPACE_SM))
                    .py(px(3.0))
                    .rounded(px(Theme::CONTROL_RADIUS))
                    .border_1()
                    .border_color(theme.border)
                    .text_color(theme.text)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.element_hover))
                    .on_click(cx.listener(move |this, _, _, cx| match kind {
                        PickerKind::Branch | PickerKind::Checkout => this.ensure_refs(true, cx),
                        PickerKind::HarnessModel => {
                            this.harnesses = Loadable::Idle;
                            this.models.clear();
                            this.catalog_rev += 1;
                            this.ensure_harnesses(false, cx);
                        }
                        // Projects/devices load nothing; no retry surface exists.
                        PickerKind::Space | PickerKind::Device => {}
                    }))
                    .child(SharedString::from("Retry")),
            )
            .into_any_element()
    }

    /// The virtualized list's plain scroll handle (bounds/offset for the
    /// floating scrollbar; `UniformList` tracks it internally).
    fn model_scroll_base(&self) -> gpui::ScrollHandle {
        self.model_scroll.0.borrow().base_handle.clone()
    }

    fn on_model_list_hover(
        &mut self,
        hovered: &bool,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.model_bar.set_list_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_model_scrollbar_hover(
        &mut self,
        hovered: &bool,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.model_bar.set_bar_hovered(*hovered) {
            cx.notify();
        }
    }

    fn on_model_scrollbar_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let scroll = self.model_scroll_base();
        if !self.model_bar.begin_press(&scroll, event.position.y) {
            return;
        }
        window.focus(&self.focus, cx);
        cx.stop_propagation();
        cx.notify();
    }

    fn on_model_scrollbar_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<popover::MenuScrollbarDrag>,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        let scroll = self.model_scroll_base();
        if self.model_bar.drag_to(&scroll, event.event.position.y) {
            cx.notify();
        }
    }

    fn on_model_scrollbar_mouse_up(
        &mut self,
        _event: &gpui::MouseUpEvent,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.model_bar.end_press();
        cx.notify();
    }

    fn render_model_scrollbar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let metrics = self.model_bar.metrics(&self.model_scroll_base())?;
        Some(
            self.model_bar
                .render_rail(theme, metrics)?
                .id("model-scrollbar")
                .on_hover(cx.listener(Self::on_model_scrollbar_hover))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_down),
                )
                .on_drag(popover::MenuScrollbarDrag, |_, _, _, cx| {
                    cx.stop_propagation();
                    cx.new(|_| popover::MenuScrollbarDragGhost)
                })
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_up),
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(Self::on_model_scrollbar_mouse_up),
                )
                .into_any_element(),
        )
    }

    /// The ref picker (t3code BranchToolbarBranchSelector): search on top,
    /// rows with right-aligned muted `current`/`worktree` tags, and a
    /// "Showing X of Y refs" footer when the list is capped.
    fn render_branch_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        if self.state.read(cx).selected_space_row().is_none() {
            return div()
                .p(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_faint)
                .child(SharedString::from("No project selected"))
                .into_any_element();
        }
        let rows = self.filtered_ref_rows(cx);
        let total = rows.len();
        let shown = total.min(MAX_REF_ROWS);
        // Existing session: the highlighted row is the SESSION's branch and a
        // pick switches the checkout (see `pick_ref`); a new chat highlights
        // the draft pick.
        let session_branch = self
            .state
            .read(cx)
            .selected_chat_row()
            .and_then(|c| c.branch.clone());
        let switching = self.switching.clone();
        let body: AnyElement =
            match &self.refs {
                Loadable::Loading | Loadable::Idle => {
                    popover::skeleton_rows("branch-skeleton", &theme, 4, cx.entity_id(), cx)
                }
                Loadable::Error(message) => {
                    let message = message.clone();
                    self.retry_row("branch-retry", &message, PickerKind::Branch, &theme, cx)
                }
                Loadable::Ready(_) if rows.is_empty() => div()
                    .p(px(Theme::SPACE_SM))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_faint)
                    .child(SharedString::from("No refs found."))
                    .into_any_element(),
                Loadable::Ready(_) => {
                    let active = self.active;
                    let selected = session_branch.or_else(|| self.config.branch.clone());
                    div()
                        .id("branch-list")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .max_h(px(224.0))
                        .overflow_y_scroll()
                        .children(rows.into_iter().take(MAX_REF_ROWS).enumerate().map(
                            |(ix, row)| {
                                let label: SharedString = row.name.clone().into();
                                let is_selected = selected.as_deref() == Some(row.name.as_str());
                                // Right-aligned muted tag (t3code `text-[10px]
                                // text-muted-foreground/45`): current beats worktree.
                                let tag: Option<&'static str> = if row.current {
                                    Some("current")
                                } else if row.worktree_path.is_some() {
                                    Some("worktree")
                                } else {
                                    None
                                };
                                let is_switching = switching.as_deref() == Some(row.name.as_str());
                                popover::menu_row_nav(
                                    &theme,
                                    is_selected,
                                    ix == active,
                                    format!("branch-row-{ix}"),
                                )
                                .id(("branch-row", ix))
                                .when(switching.is_some(), |el| el.opacity(0.55))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_ref(row.clone(), cx);
                                }))
                                .child(div().flex_1().min_w_0().truncate().child(label))
                                .when(is_switching, |el| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(10.0))
                                            .text_color(theme.text_muted.opacity(0.6))
                                            .child(SharedString::from("switching…")),
                                    )
                                })
                                .when_some(tag, |el, tag| {
                                    el.child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(10.0))
                                            .text_color(theme.text_muted.opacity(0.45))
                                            .child(SharedString::from(tag)),
                                    )
                                })
                            },
                        ))
                        .into_any_element()
                }
            };
        let mut popover = div()
            .flex()
            .flex_col()
            .child(self.search_box(&theme))
            .child(body);
        // Mid-session switch failure (dirty tree, ref checked out elsewhere):
        // git's own message, under a hairline.
        if let Some(error) = &self.switch_error {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.danger.opacity(0.9))
                        .child(SharedString::from(error.clone())),
                ),
            );
        }
        if total > shown {
            popover = popover.child(
                popover::menu_section().child(
                    div()
                        .px(px(Theme::SPACE_SM))
                        .py(px(4.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .text_color(theme.text_faint)
                        .child(SharedString::from(format!(
                            "Showing {shown} of {total} refs"
                        ))),
                ),
            );
        }
        popover.into_any_element()
    }

    /// The checkout-kind dropdown (t3code BranchToolbarEnvModeSelector): two
    /// rows — "Current checkout"/"Current worktree" (local) and "New worktree".
    fn render_checkout_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let has_worktree = self.selected_ref_worktree().is_some();
        let local_label: &'static str = if has_worktree {
            "Current worktree"
        } else {
            "Current checkout"
        };
        let local_icon = if has_worktree {
            crate::icons::FOLDER_WITH_FILES
        } else {
            crate::icons::FOLDER
        };
        let options: [(CheckoutKind, &'static str, &'static str); 2] = [
            (CheckoutKind::Local, local_label, local_icon),
            (
                CheckoutKind::NewWorktree,
                "New worktree",
                crate::icons::FOLDER_WITH_FILES,
            ),
        ];
        let active = self.active;
        let current = self.config.checkout;
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(
                options
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (kind, label, icon_path))| {
                        let is_selected = current == kind;
                        popover::menu_row_nav(
                            &theme,
                            is_selected,
                            ix == active,
                            format!("checkout-row-{ix}"),
                        )
                        .id(("checkout-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.pick_checkout(kind, cx);
                        }))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(label)),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The combined harness + model switcher (zeron harness-model-picker.tsx):
    /// a vertical harness rail of square brand-icon tabs on the left, the
    /// viewed harness's models on the right. On an existing chat the other
    /// tabs stay visible but disabled — the lock reads as a rule.
    /// The harness/model picker (t3code ModelPickerContent): an icons-only
    /// harness rail on the left (favorites star on top), a search box over
    /// the model list on the right. Rows are two lines — model name over the
    /// harness icon + name (t3 `showProvider`, replacing the description) —
    /// with a ⌘N jump chip and a star toggle trailing. Searching hides the
    /// rail and spans every harness.
    fn render_harness_model_popover(&mut self, cx: &mut Context<Self>) -> AnyElement {
        // Compact tabbed layout (user request, modeled on the referenced
        // picker): the model LIST gets a fixed band of roughly seven compact
        // rows; the pinned traits tray below sizes to its sections.
        const LIST_HEIGHT: f32 = 216.0;

        let theme = Theme::of(cx).clone();

        // Catalog-level loading/error take over the whole card — the tabs ARE
        // the catalog, so there is nothing stable to draw above the skeleton.
        match &self.harnesses {
            Loadable::Loading | Loadable::Idle => {
                return div()
                    .h(px(LIST_HEIGHT))
                    .p(px(8.0))
                    .child(popover::skeleton_menu_rows(
                        "harness-skeleton",
                        &theme,
                        5,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Error(message) => {
                let message = message.clone();
                return div()
                    .h(px(LIST_HEIGHT))
                    .p(px(8.0))
                    .child(self.retry_row(
                        "harness-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    ))
                    .into_any_element();
            }
            Loadable::Ready(_) => {}
        }

        let locked = self.harness_locked(cx);
        let effective = self.effective_harness(cx);
        let model_scroll = self.model_scroll.clone();
        let query = self.search.read(cx).text().trim().to_string();
        let searching = !query.is_empty();
        let favorites_view = self.model_rail == ModelRail::Favorites;
        let descriptors = self.rail_descriptors(cx);
        // No-agents empty state: the catalog loaded but offers nothing
        // runnable (every enabled harness is missing its CLI, or nothing is
        // enabled) and there's no committed chat harness to force-include —
        // guidance instead of an empty tab row.
        if descriptors.is_empty() {
            return div()
                .p(px(16.0))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(8.0))
                .child(
                    crate::icons::icon(crate::icons::TERMINAL)
                        .size(px(20.0))
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child(SharedString::from("No agents available")),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .text_center()
                        .child(SharedString::from(
                            "Enable an installed agent in Settings → Agents, \
                             or install an agent CLI.",
                        )),
                )
                .into_any_element();
        }
        let rows = self.model_rows(cx);

        // ── tabs: the favorites star, then one brand icon per harness —
        //    ACROSS THE TOP (user request; was a left rail). The
        //    viewed tab wears a 2px accent bar sitting on the row's bottom
        //    hairline. Tabs never hide: a live search only filters the
        //    viewed tab's list, so switching tabs re-scopes the same query.
        let mut tabs = div()
            .flex_none()
            .h(px(40.0))
            .px(px(6.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.08))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(2.0));
        tabs = tabs.child(
            div()
                .id("model-tab-favorites")
                .relative()
                .w(px(32.0))
                .h(px(32.0))
                .rounded(px(8.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .when(!favorites_view, |el| {
                    el.hover(|s| s.bg(crate::theme::ink(0.06)))
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.model_rail = ModelRail::Favorites;
                    // Anchor on the selected row when it's starred, else
                    // the top — never a stray second highlight.
                    this.active = this.selected_model_index(cx);
                    this.model_scroll_base().set_offset(gpui::Point::default());
                    this.scroll_model_row_into_view(this.active, cx);
                    cx.notify();
                }))
                .child(
                    crate::icons::icon(crate::icons::STAR_BOLD)
                        .size(px(15.0))
                        .text_color(if favorites_view {
                            theme.text
                        } else {
                            theme.text_muted.opacity(0.75)
                        }),
                )
                .when(favorites_view, |el| el.child(tab_indicator(theme.accent))),
        );
        for (ix, descriptor) in descriptors.iter().enumerate() {
            let harness = descriptor.id;
            let is_viewed = !favorites_view && effective == Some(harness);
            let is_disabled = locked && effective != Some(harness);
            let (icon_path, tint) = harness_brand_icon(harness);
            tabs =
                tabs.child(
                    div()
                        .id(("harness-tab", ix))
                        .relative()
                        .w(px(32.0))
                        .h(px(32.0))
                        .rounded(px(8.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(is_disabled, |el| el.opacity(0.35))
                        .when(!is_disabled, |el| el.cursor_pointer())
                        .when(!is_disabled && !is_viewed, |el| {
                            el.hover(|s| s.bg(crate::theme::ink(0.06)))
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.model_rail = ModelRail::Harness;
                            this.pick_harness(harness, cx);
                            cx.notify();
                        }))
                        .child(crate::icons::icon(icon_path).size(px(16.0)).text_color(
                            tint.unwrap_or(if is_viewed {
                                theme.text
                            } else {
                                theme.text_muted
                            }),
                        ))
                        .when(is_viewed, |el| el.child(tab_indicator(theme.accent))),
                );
        }

        // ── search row: icon + borderless input over a full-bleed hairline.
        //    The placeholder names the scope — the query never leaves the
        //    viewed tab (user request; the old global search hid the rail).
        let search_row = div()
            .flex_none()
            .h(px(40.0))
            .px(px(10.0))
            .border_b_1()
            .border_color(crate::theme::hairline(0.08))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(
                crate::icons::icon(crate::icons::MAGNIFER)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.7)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(crate::typography::ui_rems(13.0))
                    .child(self.search.clone()),
            );

        // ── model rows: a VIRTUALIZED uniform list — only the visible slice
        //    renders, so a 7k-model catalog scrolls as smoothly as seven
        //    (field report: the un-virtualized stack was the picker's lag).
        //    Keyboard nav scrolls via the UniformListScrollHandle.
        let effective_models = effective.and_then(|h| self.models.get(&h));
        let model_list: Option<AnyElement> = if !rows.is_empty() {
            let entity = cx.entity();
            let row_data = rows.clone();
            let divider = rows.divider;
            Some(
                gpui::uniform_list(
                    "model-menu-scroll",
                    rows.list_len(),
                    move |range, _window, app| {
                        entity.update(app, |this, cx| {
                            range
                                .map(|list_ix| match row_for_list_ix(list_ix, divider) {
                                    // The divider is a render-only item: no
                                    // id, no listeners, so keyboard nav and
                                    // the mod-N badges never see it.
                                    None => render_model_divider(cx),
                                    Some(ix) => row_data
                                        .get(ix)
                                        .map(|row| this.render_model_row(ix, row, cx))
                                        .unwrap_or_else(|| div().into_any_element()),
                                })
                                .collect::<Vec<AnyElement>>()
                        })
                    },
                )
                .size_full()
                .px(px(6.0))
                .track_scroll(&model_scroll)
                .into_any_element(),
            )
        } else {
            None
        };
        let list_children: Vec<AnyElement> = if !rows.is_empty() {
            Vec::new()
        } else if searching {
            vec![empty_list_note(&theme, "No models found")]
        } else if favorites_view {
            vec![empty_list_note(
                &theme,
                "No starred models yet — hit a row's star",
            )]
        } else {
            match effective_models {
                Some(Loadable::Error(message)) => {
                    let message = message.clone();
                    vec![self.retry_row(
                        "model-retry",
                        &message,
                        PickerKind::HarnessModel,
                        &theme,
                        cx,
                    )]
                }
                _ => vec![popover::skeleton_menu_rows(
                    "model-skeleton",
                    &theme,
                    5,
                    cx.entity_id(),
                    cx,
                )],
            }
        };

        // Small-caps group header naming what the list is showing: the
        // viewed harness (its tab is the grouping) or the starred mix.
        let group_label = if favorites_view {
            SharedString::from("Favorites")
        } else {
            descriptors
                .iter()
                .find(|d| Some(d.id) == effective)
                .map(|d| SharedString::from(d.name.clone()))
                .unwrap_or_else(|| SharedString::from("Models"))
        };
        let group_header = div()
            .flex_none()
            .px(px(12.0))
            .pt(px(6.0))
            .text_size(crate::typography::ui_rems(10.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text_muted.opacity(0.6))
            .child(SharedString::from(popover::tracked_upper(&group_label)));

        let model_scrollbar = self.render_model_scrollbar(&theme, cx);
        let list_host = div()
            .id("model-list-scroll-host")
            .relative()
            .flex_none()
            .h(px(LIST_HEIGHT))
            .py(px(6.0))
            // A whisper of wash keeps the scrolling band readable between
            // the pinned chrome above and the traits tray below.
            .bg(crate::theme::ink(0.02))
            .on_hover(cx.listener(Self::on_model_list_hover))
            .child(match model_list {
                Some(list) => list,
                // Empty/loading/error notes: a plain static stack.
                None => div()
                    .id("model-menu-scroll")
                    .size_full()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .px(px(6.0))
                    .children(list_children)
                    .into_any_element(),
            })
            // Absolute child: the hit rail and thumb float above the
            // scroll content without consuming any list width.
            .children(model_scrollbar);

        // ── traits tray: the EFFORT card (model name + slider) plus the
        //    model's advertised options, PINNED under the list (the separate
        //    Traits popover folded in here, user request). Hidden entirely
        //    until a model resolves.
        let effort_card = self.render_effort_card(cx);
        let cost_row = self.render_cost_row(cx);
        let has_options = self
            .selected_model(cx)
            .is_some_and(|m| !m.options.is_empty());
        let tray: Option<AnyElement> = (effort_card.is_some() || cost_row.is_some() || has_options)
            .then(|| {
                let sections = has_options.then(|| self.render_traits_sections(cx));
                div()
                    .id("model-traits-tray")
                    .flex_none()
                    .border_t_1()
                    .border_color(crate::theme::hairline(0.08))
                    // Long option stacks scroll inside the tray rather than
                    // growing the card past the viewport.
                    .max_h(px(300.0))
                    .overflow_y_scroll()
                    .pb(px(6.0))
                    .children(effort_card)
                    .children(cost_row)
                    .child(div().px(px(6.0)).children(sections))
                    .into_any_element()
            });

        div()
            .flex()
            .flex_col()
            .child(tabs)
            .child(search_row)
            .child(group_header)
            .child(list_host)
            .children(tray)
            .into_any_element()
    }

    /// One model row for the virtualized list. `ix` is the row's GLOBAL index
    /// (⌘N chips, hover-cursor, and activation all key on it). The 2px
    /// inter-row gap is baked into each item's bottom padding so every item
    /// is the same height (uniform_list measures the first).
    fn render_model_row(
        &mut self,
        ix: usize,
        row: &ModelRowData,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let effective = self.effective_harness(cx);
        let is_selected = Some(row.harness) == effective
            && self.selected_model(cx).map(|m| m.id.as_str()) == Some(row.model.id.as_str());
        let is_active = ix == self.active;
        let is_fav = self.defaults.is_favorite(row.harness, &row.model.id);
        let (icon_path, tint) = harness_brand_icon(row.harness);
        let label: SharedString = row.model.label.clone().into();
        let harness_name = row.harness_name.clone();
        let harness = row.harness;
        let star_model = row.model.id.clone();
        // Provider attribution (field report: several connected opencode
        // providers advertise identically-named models — "GLM-5.2" exists
        // under 64 providers — and rows were indistinguishable). The driver
        // ships the provider display name in `description`; other harnesses'
        // taglines read fine in the same slot. Skip when it just repeats the
        // harness name.
        let attribution: Option<SharedString> = row
            .model
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty() && !d.eq_ignore_ascii_case(harness_name.as_ref()))
            .map(|d| SharedString::from(d.to_owned()));
        let compact = self.model_rail == ModelRail::Harness;
        let mut el = div()
            .id(("model-row", ix))
            .px(px(8.0))
            .py(px(if compact { 5.0 } else { 6.0 }))
            .rounded(px(6.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer();
        // ONE moving highlight (t3/Base-UI combobox): hovering moves the
        // keyboard cursor instead of painting its own wash, so hover + arrow
        // cursor can never wear two washes at once. Selection is the
        // distinct stronger treatment (wash + ring).
        if is_selected {
            el = el
                .bg(crate::theme::card_selected_bg())
                .shadow(crate::theme::card_selected_shadows());
        } else if is_active {
            el = el.bg(crate::theme::ink(0.05));
        }
        el = el.on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
            if *hovered && this.active != ix {
                this.active = ix;
                cx.notify();
            }
        }));
        // Compact single-line rows on a harness tab (user request): every
        // row there shares the tab's harness, so the identity subline is
        // dead weight — attribution rides inline instead (opencode ships
        // identically-named models under 64 providers; it must stay
        // visible). The favorites tab mixes harnesses and keeps the
        // two-line layout with the brand subline.
        let body: AnyElement = if compact {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .flex_none()
                        .max_w_full()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(label),
                )
                .when_some(attribution, |el, attribution| {
                    el.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(11.0))
                            .text_color(theme.text_muted.opacity(0.7))
                            .child(attribution),
                    )
                })
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(label),
                )
                .child(
                    // Harness identity subline (t3 `showProvider`), plus
                    // the model's own attribution when it carries one.
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            crate::icons::icon(icon_path)
                                .size(px(11.0))
                                .flex_none()
                                .text_color(tint.unwrap_or(theme.text_muted.opacity(0.7))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(crate::typography::ui_rems(11.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(harness_name),
                        )
                        .when_some(attribution, |el, attribution| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.45))
                                    .child(SharedString::from("·")),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(attribution),
                            )
                        }),
                )
                .into_any_element()
        };
        el = el
            .on_click(cx.listener(move |this, _, _, cx| {
                this.activate_model_index(ix, cx);
            }))
            .child(body);
        if ix < 9 {
            el = el.child(popover::kbd_hint(
                &theme,
                &crate::settings::badge_combo(&format!("mod-{}", ix + 1)),
            ));
        }
        el = el.child(
            div()
                .id(("model-star", ix))
                .flex_none()
                .w(px(22.0))
                .h(px(22.0))
                .rounded(px(6.0))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|s| s.bg(crate::theme::ink(0.08)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.toggle_model_favorite(harness, &star_model, cx);
                }))
                .child(
                    crate::icons::icon(if is_fav {
                        crate::icons::STAR_BOLD
                    } else {
                        crate::icons::STAR
                    })
                    .size(px(13.0))
                    .text_color(if is_fav {
                        theme.warning
                    } else {
                        theme.text_muted.opacity(0.45)
                    }),
                ),
        );
        div().pb(px(2.0)).child(el).into_any_element()
    }

    /// The EFFORT row pinned under the model list: a small-caps "EFFORT" label
    /// on the left, the current level name on the right, and the hairline
    /// slider beneath them with the ladder's rungs as ticks. No boxed card and
    /// no model name (the selected model is already ringed in the list above)
    /// - user feedback called that "too fat, thick and cluttered", so the row
    /// now carries the same weight as the CONTEXT WINDOW rows below it. Models
    /// without a ladder (Claude Haiku) get a one-line note instead.
    fn render_effort_card(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        // The row only appears once a model resolves; the model itself is NOT
        // repeated inside it (it is the ringed row in the list above).
        self.selected_model(cx)?;
        let harness = self.effective_harness(cx);
        let levels = self.trait_ladder(cx);
        let current = self.effective_reasoning(cx);
        let current_ix = current
            .and_then(|level| levels.iter().position(|l| *l == level))
            .unwrap_or(0);
        let body: AnyElement = if levels.is_empty() {
            div()
                .w_full()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(crate::reasoning_slider::NO_EFFORT_HINT))
                .into_any_element()
        } else {
            let width = EFFORT_TRACK_WIDTH;
            // Sparkles ride the top rung only - the special effect marking
            // the ladder's ceiling.
            let at_max = current_ix + 1 == levels.len();
            let sparkle_t = (at_max && !motion::reduced_motion(cx))
                .then(|| motion::pulse_delta(&motion::EFFORT_SPARKLE, cx.entity_id(), cx));
            let slider = crate::reasoning_slider::EffortSlider {
                id: "effort",
                levels: levels.clone(),
                current: current_ix,
                harness,
                width,
                sparkle_t,
                bounds: self.effort_bounds.clone(),
            }
            .render(&theme, cx);
            // One calm header line: the section label left, the level right in
            // NORMAL weight (a bold level name read as a second title).
            let caption = div()
                .w_full()
                .flex()
                .flex_row()
                .items_baseline()
                .justify_between()
                .pb(px(3.0))
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(popover::tracked_upper(
                            crate::reasoning_slider::EFFORT_LABEL,
                        ))),
                )
                .child(
                    div()
                        .text_size(crate::typography::ui_rems(11.0))
                        .font_weight(gpui::FontWeight::NORMAL)
                        .text_color(theme.text.opacity(0.85))
                        .child(SharedString::from(
                            current.map(reasoning_label).unwrap_or(""),
                        )),
                );
            div()
                .w_full()
                .flex()
                .flex_col()
                .child(caption)
                .child(
                    div()
                        .id("effort-slider")
                        .w(px(width))
                        .cursor_pointer()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(Self::on_effort_mouse_down),
                        )
                        .on_drag(crate::reasoning_slider::EffortDrag, |_, _, _, cx| {
                            cx.stop_propagation();
                            cx.new(|_| crate::reasoning_slider::EffortDragGhost)
                        })
                        .child(slider),
                )
                .into_any_element()
        };

        // A flat section, not a card: no wash, no rounded box, only the
        // tray's own hairline above and one below to part it from the
        // option rows.
        Some(
            div()
                .w_full()
                .px(px(EFFORT_ROW_PAD))
                .pt(px(6.0))
                .pb(px(7.0))
                .border_b_1()
                .border_color(crate::theme::hairline(0.06))
                .flex()
                .flex_col()
                .child(body)
                .into_any_element(),
        )
    }

    /// The COST row under the effort slider: per-million list prices for the
    /// selected model as three labelled columns (Input / Cached input /
    /// Output), matching the reference footer. A free model collapses to one
    /// muted word; a model with no published price hides the section entirely
    /// rather than showing zeros. `ListPrice` rows carry a muted "list price"
    /// hint beside the heading so subscription users read them as
    /// informational.
    fn render_cost_row(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let pricing = self.selected_model(cx)?.pricing.clone()?;
        let heading = |text: String, muted: f32, size: f32| {
            div()
                .text_size(crate::typography::ui_rems(size))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted.opacity(muted))
                .child(SharedString::from(text))
        };
        let body: AnyElement = if pricing.is_free() {
            div()
                .text_size(crate::typography::ui_rems(11.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(crate::pricing::FREE_LABEL))
                .into_any_element()
        } else {
            // Label above value, three equal columns: the popover is 304px
            // wide and "Cached input $0.26 / 1M" does not fit on one line.
            let column = |label: &'static str, value: Option<f64>| {
                let value = value.map(crate::pricing::format_price);
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(9.0))
                            .text_color(theme.text_muted.opacity(0.55))
                            .child(SharedString::from(label)),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(3.0))
                            .text_size(crate::typography::ui_rems(11.0))
                            .child(
                                div()
                                    .text_color(theme.text.opacity(0.85))
                                    .child(SharedString::from(value.unwrap_or_else(|| "-".into()))),
                            )
                            .child(
                                div()
                                    .text_size(crate::typography::ui_rems(9.0))
                                    .text_color(theme.text_muted.opacity(0.5))
                                    .child(SharedString::from(crate::pricing::PER_MILLION_SUFFIX)),
                            ),
                    )
            };
            div()
                .w_full()
                .flex()
                .flex_row()
                .gap(px(6.0))
                .child(column(
                    crate::pricing::INPUT_LABEL,
                    Some(pricing.input_per_million),
                ))
                .child(column(
                    crate::pricing::CACHED_INPUT_LABEL,
                    pricing.cached_input_per_million,
                ))
                .child(column(
                    crate::pricing::OUTPUT_LABEL,
                    Some(pricing.output_per_million),
                ))
                .into_any_element()
        };
        let caption = div()
            .w_full()
            .flex()
            .flex_row()
            .items_baseline()
            .justify_between()
            .pb(px(3.0))
            .child(heading(
                popover::tracked_upper(crate::pricing::COST_LABEL),
                0.6,
                10.0,
            ))
            .when(
                pricing.source == zeron_proto::PriceSource::ListPrice,
                |row| {
                    row.child(heading(
                        crate::pricing::LIST_PRICE_HINT.to_owned(),
                        0.5,
                        10.0,
                    ))
                },
            );
        Some(
            div()
                .w_full()
                .px(px(EFFORT_ROW_PAD))
                .pt(px(6.0))
                .pb(px(7.0))
                .border_b_1()
                .border_color(crate::theme::hairline(0.06))
                .flex()
                .flex_col()
                .child(caption)
                .child(body)
                .into_any_element(),
        )
    }

    /// A press anywhere on the slider row snaps to the nearest rung (and
    /// starts the drag - `on_drag_move` on the picker root continues it).
    fn on_effort_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus, cx);
        self.drag_effort_to(event.position.x, cx);
        cx.stop_propagation();
    }

    fn on_effort_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::reasoning_slider::EffortDrag>,
        _window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_effort_to(event.event.position.x, cx);
    }

    /// Map a window-space pointer x onto the track and pick that rung.
    fn drag_effort_to(&mut self, pointer_x: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(bounds) = self.effort_bounds.get() else {
            return; // never painted - nothing to map against
        };
        let width = f32::from(bounds.size.width);
        let x = f32::from(pointer_x - bounds.left());
        self.pick_reasoning_at(x, width, cx);
    }

    /// The advertised model options under the effort card (t3code
    /// TraitsPicker): each option as a headed section of menu ROWS - label, a
    /// "Default" badge on the section's default choice, and the trailing check
    /// on the selected row. Selecting keeps the menu open for multi-adjust.
    fn render_traits_sections(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let Some(model) = self.selected_model(cx).cloned() else {
            return popover::skeleton_menu_rows("traits-skeleton", &theme, 3, cx.entity_id(), cx);
        };

        let mut sections: Vec<AnyElement> = Vec::new();
        let selections = self.explicit_options(cx);
        for (opt_ix, option) in model.options.iter().enumerate() {
            if !sections.is_empty() {
                sections.push(popover::menu_separator().into_any_element());
            }
            let selected_choice = selections
                .get(&option.id)
                .and_then(|v| v.as_str())
                .unwrap_or(&option.default_choice)
                .to_string();
            let option_id = option.id.clone();
            let default_choice = option.default_choice.clone();
            sections.push(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(2.0)) // same rhythm as the Reasoning section above
                    .child(popover::menu_heading(&theme, &option.label))
                    .children(
                        option
                            .choices
                            .iter()
                            .enumerate()
                            .map(|(choice_ix, choice)| {
                                let is_active = selected_choice == choice.id;
                                let choice_id = choice.id.clone();
                                let option_id = option_id.clone();
                                let is_default = choice.id == default_choice;
                                let mut row = popover::menu_row(
                                    &theme,
                                    is_active,
                                    format!("trait-choice-{opt_ix}-{choice_ix}"),
                                )
                                .py(px(5.0))
                                .rounded(px(6.0))
                                .text_size(crate::typography::ui_rems(12.5))
                                .id(("trait-choice", opt_ix * 32 + choice_ix))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.pick_option(
                                        option_id.clone(),
                                        choice_id.clone(),
                                        is_default,
                                        cx,
                                    );
                                }))
                                .child(SharedString::from(choice.label.clone()));
                                row = row.child(div().flex_1());
                                if is_default {
                                    row = row.child(default_badge(&theme));
                                }
                                row
                            }),
                    )
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .flex_col()
            .pb(px(2.0))
            .children(sections)
            .into_any_element()
    }
}

/// The "Default" marker beside a section's default choice: a ghost badge —
/// bare muted text, no border or fill (user request; t3code draws an outline
/// pill here).
fn default_badge(theme: &Theme) -> gpui::Div {
    div()
        .flex_none()
        .text_size(crate::typography::ui_rems(10.0))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(theme.text_muted.opacity(0.6))
        .child(SharedString::from("Default"))
}

/// Brand mark + optional tint for a harness (the Claude mark keeps its brand
/// orange even on the monochrome surface; the mock harness scripts
/// Claude-flavoured runs, so it wears the Claude mark).
/// The 2px underline marking the viewed top tab: sits on the tab row's
/// bottom hairline (the tab is 32px tall inside a 40px row, so -4px lands
/// exactly on the border), rounded like a capsule.
fn tab_indicator(tint: gpui::Hsla) -> gpui::Div {
    div()
        .absolute()
        .bottom(px(-4.0))
        .left(px(6.0))
        .right(px(6.0))
        .h(px(2.0))
        .rounded(px(1.0))
        .bg(tint)
}

/// Flatten the picker's visible rows for one tab. The QUERY NEVER LEAVES THE
/// VIEWED TAB (user request; the old global search spanned every harness and
/// hid the rail): on a harness tab it ranks that harness's models only, on
/// the favorites tab it ranks the starred set. Without a query, a harness
/// tab lists its catalog stars-first and the favorites tab lists every star.
fn scoped_model_rows<'a>(
    query: &str,
    rail: ModelRail,
    effective: Option<HarnessId>,
    descriptors: &[HarnessDescriptor],
    models_for: impl Fn(HarnessId) -> Option<&'a [Model]>,
    is_favorite: impl Fn(HarnessId, &str) -> bool,
) -> ModelRows {
    let row = |descriptor: &HarnessDescriptor, model: &Model| ModelRowData {
        harness: descriptor.id,
        harness_name: SharedString::from(descriptor.name.clone()),
        model: model.clone(),
    };
    let in_scope = |descriptor: &HarnessDescriptor, model: &Model| match rail {
        ModelRail::Favorites => is_favorite(descriptor.id, &model.id),
        ModelRail::Harness => Some(descriptor.id) == effective,
    };
    if !query.is_empty() {
        // Rank: label prefix < label substring < description hit; stars,
        // then input order, break ties (t3 modelPickerSearch's field ladder
        // + favorite boost, collapsed to our ranks). The description stays
        // in the haystack — opencode's provider attribution ("anthropic")
        // must find its models even inside one tab.
        let mut ranked: Vec<(usize, usize, usize, ModelRowData)> = Vec::new();
        let mut input_ix = 0usize;
        for descriptor in descriptors {
            let Some(models) = models_for(descriptor.id) else {
                continue;
            };
            for model in models {
                if !in_scope(descriptor, model) {
                    continue;
                }
                let by_label = popover::match_rank(query, &model.label);
                let by_description = popover::match_rank(
                    query,
                    &format!(
                        "{} {}",
                        model.description.as_deref().unwrap_or(""),
                        model.label
                    ),
                )
                .map(|rank| rank + 2);
                if let Some(rank) = by_label.into_iter().chain(by_description).min() {
                    let starred = !is_favorite(descriptor.id, &model.id);
                    ranked.push((rank, starred as usize, input_ix, row(descriptor, model)));
                }
                input_ix += 1;
            }
        }
        ranked.sort_by_key(|(rank, unstarred, ix, _)| (*rank, *unstarred, *ix));
        // A filtered list is one ranked run: no flagship/OTHER split (user
        // request), because rank already decided what leads.
        return ModelRows {
            rows: ranked.into_iter().map(|(_, _, _, row)| row).collect(),
            divider: None,
        };
    }
    match rail {
        ModelRail::Favorites => {
            // The starred mix is the user's own order of merit; nothing to
            // re-rank and nothing to divide.
            let mut rows = Vec::new();
            for descriptor in descriptors {
                let Some(models) = models_for(descriptor.id) else {
                    continue;
                };
                for model in models {
                    if is_favorite(descriptor.id, &model.id) {
                        rows.push(row(descriptor, model));
                    }
                }
            }
            ModelRows {
                rows,
                divider: None,
            }
        }
        ModelRail::Harness => {
            let Some(descriptor) = descriptors.iter().find(|d| Some(d.id) == effective) else {
                return ModelRows::default();
            };
            let Some(models) = models_for(descriptor.id) else {
                return ModelRows::default();
            };
            // Stars float first (unchanged), then the harness's flagships,
            // then the OTHER divider, then the rest of the catalog in its
            // served order.
            let (starred, rest): (Vec<&Model>, Vec<&Model>) = models
                .iter()
                .partition(|m| is_favorite(descriptor.id, &m.id));
            let (flagship, other) = split_flagships(descriptor.id, &rest);
            // A divider needs something on BOTH sides of it.
            let divider =
                (!flagship.is_empty() && !other.is_empty()).then(|| starred.len() + flagship.len());
            let rows = starred
                .into_iter()
                .chain(flagship)
                .chain(other)
                .map(|model| row(descriptor, model))
                .collect();
            ModelRows { rows, divider }
        }
    }
}

/// The non-selectable row parting a harness's flagship models from the rest:
/// a small-caps "OTHER" caption trailed by a hairline. It lives inside the
/// same `uniform_list` (so it scrolls with the models) but carries no id and
/// no listener, and keyboard nav indexes the model rows only - so it can
/// neither be highlighted nor activated.
fn render_model_divider(cx: &mut Context<Pickers>) -> AnyElement {
    let theme = Theme::of(cx).clone();
    div()
        .w_full()
        .h_full()
        .px(px(8.0))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .text_size(crate::typography::ui_rems(10.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text_muted.opacity(0.55))
                .child(SharedString::from(popover::tracked_upper(
                    OTHER_GROUP_LABEL,
                ))),
        )
        .child(div().flex_1().h(px(1.0)).bg(crate::theme::hairline(0.10)))
        .into_any_element()
}

/// Centered muted note filling an empty model list ("No models found").
fn empty_list_note(theme: &Theme, copy: &str) -> AnyElement {
    div()
        .px(px(8.0))
        .py(px(24.0))
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text_muted.opacity(0.6))
        .text_center()
        .child(SharedString::from(copy.to_string()))
        .into_any_element()
}

/// Display-side model-list hygiene, mirroring the engine's discovery-side
/// fold (`models_from_session`) for catalogs served by OLDER engines (the
/// space's device may run any version): the `default` alias row drops when a
/// real row exists, an orphan `<model>[1m]` variant presents as its base id
/// with the Context Window trait pinned to 1M, and Claude rows adopt the
/// curated catalog's labels so the version number always shows ("Opus 5",
/// not the wire's terse "Opus" alias — user request). Idempotent over
/// already-clean lists. The send path recomposes the advertised id from the
/// base + trait (`pick_model_value`), so a folded pick still runs.
pub(crate) fn normalize_model_rows(harness: HarnessId, models: Vec<Model>) -> Vec<Model> {
    fn strip_1m(id: &str) -> Option<&str> {
        id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
    }
    fn norm(id: &str) -> String {
        id.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }
    let catalog = match harness {
        HarnessId::ClaudeCode => zeron_harness::claude::catalog::static_models(),
        _ => Vec::new(),
    };
    // Curated label for an id: exact normalized match, else — for bare
    // alphabetic aliases like `opus` — the first (flagship-ordered) family
    // row. Versioned foreign ids never fuzzy-match.
    let curated_label = |id: &str| -> Option<String> {
        let id_norm = norm(id);
        if let Some(row) = catalog.iter().find(|m| norm(&m.id) == id_norm) {
            return Some(row.label.clone());
        }
        (!id_norm.is_empty() && id_norm.chars().all(|c| c.is_ascii_alphabetic()))
            .then(|| catalog.iter().find(|m| norm(&m.id).contains(&id_norm)))
            .flatten()
            .map(|m| m.label.clone())
    };
    let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    let has_real = ids.iter().any(|id| !id.eq_ignore_ascii_case("default"));
    models
        .into_iter()
        .filter_map(|mut model| {
            if has_real && model.id.eq_ignore_ascii_case("default") {
                return None;
            }
            if let Some(base) = strip_1m(&model.id.clone()) {
                if ids.iter().any(|other| other == base) {
                    // The bare base is listed too — the engine already gave
                    // it the Context Window trait; the variant row is noise.
                    return None;
                }
                model.id = base.to_string();
                // "Opus (1M context)" → "Opus".
                if let Some(at) = model.label.rfind(" (")
                    && model.label.ends_with(')')
                {
                    model.label.truncate(at);
                    while model.label.ends_with(' ') {
                        model.label.pop();
                    }
                }
                if !model.options.iter().any(|o| o.id == "contextWindow") {
                    model.options.push(zeron_proto::ModelOption {
                        id: "contextWindow".into(),
                        label: "Context Window".into(),
                        choices: vec![
                            zeron_proto::ModelOptionChoice {
                                id: "200k".into(),
                                label: "200K".into(),
                            },
                            zeron_proto::ModelOptionChoice {
                                id: "1m".into(),
                                label: "1M".into(),
                            },
                        ],
                        default_choice: "1m".into(),
                    });
                }
            }
            if let Some(label) = curated_label(&model.id) {
                model.label = label;
            }
            Some(model)
        })
        .collect()
}

pub(crate) fn harness_brand_icon(harness: HarnessId) -> (&'static str, Option<gpui::Hsla>) {
    match harness {
        HarnessId::ClaudeCode | HarnessId::Mock => (
            crate::icons::CLAUDE_MARK,
            Some(crate::icons::claude_brand()),
        ),
        HarnessId::Codex => (crate::icons::OPENAI_MARK, None),
        HarnessId::Cursor => (crate::icons::CURSOR_MARK, None),
        // Cognition's mark (the Devin product icon), monochrome.
        HarnessId::Devin => (crate::icons::DEVIN_MARK, None),
        // Monochrome mark, tinted by the surface like OpenAI's.
        HarnessId::Grok => (crate::icons::GROK_MARK, None),
        // Nous Research's mark (the Hermes product icon), monochrome.
        HarnessId::Hermes => (crate::icons::HERMES_MARK, None),
        HarnessId::Pi => (crate::icons::PI_MARK, None),
        HarnessId::Kimi => (crate::icons::KIMI_MARK, None),
        // The pixel-"o" from opencode's wordmark (their favicon), monochrome.
        HarnessId::Opencode => (crate::icons::OPENCODE_MARK, None),
    }
}

/// `ZERON_HARNESS=mock` (the e2e/dev rig) opts the mock harness into the UI;
/// production launches never set it, so the mock never surfaces there.
fn mock_harness_enabled() -> bool {
    std::env::var("ZERON_HARNESS")
        .ok()
        .as_deref()
        .map(str::trim)
        == Some("mock")
}

/// Production pickers AND chip resolution hide the mock harness — the
/// registry always lists it, but it must never surface in real UI (neither in
/// the picker rail nor as the eager default the chips resolve against).
/// `ZERON_HARNESS=mock` shows it; otherwise it only remains when it's
/// literally all there is (a dev build with no real harness registered).
pub fn visible_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, mock_harness_enabled())
}

fn visible_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    if allow_mock {
        return list.to_vec();
    }
    let real: Vec<HarnessDescriptor> = list
        .iter()
        .filter(|d| d.id != HarnessId::Mock)
        .cloned()
        .collect();
    if real.is_empty() { list.to_vec() } else { real }
}

/// What the composer actually offers: [`visible_harnesses`] narrowed to the
/// catalog device's enabled set AND installed CLIs (Settings → Agents is
/// per-device state, so a space on another device follows THAT device's
/// toggles; a default-enabled agent whose CLI is missing would only
/// manufacture NotInstalled errors at send). The dev-rig mock opt-in
/// survives the filter. There is NO fallback: a catalog where nothing is
/// both enabled and installed offers nothing, and the composer surfaces the
/// no-agents empty state + blocks new sends — resurrecting descriptors that
/// can only fail with NotInstalled is the #128 bug.
pub fn offered_harnesses(list: &[HarnessDescriptor]) -> Vec<HarnessDescriptor> {
    offered_harnesses_impl(list, mock_harness_enabled())
}

fn offered_harnesses_impl(list: &[HarnessDescriptor], allow_mock: bool) -> Vec<HarnessDescriptor> {
    visible_harnesses_impl(list, allow_mock)
        .into_iter()
        .filter(|d| {
            d.installed
                && (zeron_engine::registry::descriptor_enabled(d)
                    || (allow_mock && d.id == HarnessId::Mock))
        })
        .collect()
}

/// Attach the (single) open popover above a selector trigger.
fn attach_overlay(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_above(id, element, closing));
    }
    chip
}

/// Attach the (single) open popover below a selector trigger.
fn attach_overlay_below(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip.child(popover::anchored_menu_below(id, element, closing));
    }
    chip
}

/// Attach the menu ABOVE and RIGHT-ALIGNED to the trigger (t3code
/// `align="end"` — right-edge controls like the model picker open leftward).
fn attach_overlay_end(
    chip: gpui::Stateful<gpui::Div>,
    overlay: &mut Option<(PickerKind, AnyElement)>,
    kind: PickerKind,
    id: &'static str,
    closing: Option<std::time::Instant>,
) -> gpui::Stateful<gpui::Div> {
    if overlay.as_ref().is_some_and(|(k, _)| *k == kind)
        && let Some((_, element)) = overlay.take()
    {
        return chip
            .relative()
            .child(popover::anchored_menu_above_end(id, element, closing));
    }
    chip
}

impl Render for Pickers {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        // A ZERON_OPEN_PICKER popover never went through `toggle`, so claim
        // its keyboard focus here (re-claim until it sticks — the shell's
        // first-paint fallback focuses the composer after our first render).
        if self.boot_focus_pending {
            match self.open_kind() {
                Some(PickerKind::Branch) => {
                    self.search.update(cx, |input, cx| {
                        input.set_placeholder("Search refs…", cx);
                    });
                    let handle = self.search.read(cx).focus_handle(cx);
                    if handle.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&handle, cx);
                    }
                }
                Some(_) => {
                    if self.focus.is_focused(window) {
                        self.boot_focus_pending = false;
                    } else {
                        window.focus(&self.focus, cx);
                    }
                }
                None => self.boot_focus_pending = false,
            }
        }

        if self.is_open() {
            let search = self.search.focus_handle(cx);
            let frame = self.focus.clone();
            window.defer(cx, move |window, cx| {
                // Loading, empty, and error states can omit the search box.
                // Keep Escape/arrow keys on the mounted menu in those states.
                if search.is_focused(window) && !frame.contains(&search, window) {
                    window.focus(&frame, cx);
                }
            });
        }

        // Eager-load the harness catalog + every offered harness's models so
        // the chip reads "Fable 5" (a concrete pick) before any popover
        // opens, and rail switches inside the picker are instant.
        self.ensure_harnesses(false, cx);
        self.prefetch_models(false, cx);
        // A popover opened data-side (ZERON_OPEN_PICKER) never went through
        // `toggle`, so kick its loads here (all ensure_* are idempotent).
        if matches!(
            self.open_kind(),
            Some(PickerKind::Branch) | Some(PickerKind::Checkout)
        ) && matches!(self.refs, Loadable::Idle)
        {
            self.ensure_refs(false, cx);
        }
        // Chip shows the model's display name alone (zeron `modelText`); the
        // harness reads from the brand mark beside it. Never "Default model":
        // before the catalog lands the remembered label (or the configured id)
        // names the pick; the loaded list then resolves it to a concrete row.
        // No-agents state: nothing runnable resolved (and the catalog is
        // loaded, so that's a conclusion, not a loading gap) — the chip says
        // so instead of wearing a brand mark for an agent that can't run.
        let no_agents = self.no_agents_available() && self.effective_harness(cx).is_none();
        let model_label: SharedString = if no_agents {
            SharedString::from("No agents available")
        } else {
            let loaded = self.selected_model(cx).map(|m| m.label.clone());
            let label = loaded.or_else(|| {
                let remembered = self
                    .effective_harness(cx)
                    .and_then(|h| self.defaults.model_for(h));
                match self.effective_model_id(cx) {
                    Some(id) => Some(
                        remembered
                            .filter(|m| m.id == id)
                            .map(|m| m.label.clone())
                            .or_else(|| self.defaults.label_for(id).map(str::to_string))
                            .unwrap_or_else(|| id.to_string()),
                    ),
                    None => remembered.map(|m| m.label.clone()),
                }
            });
            label.map(SharedString::from).unwrap_or_default()
        };
        let catalog_loading = matches!(self.harnesses, Loadable::Idle | Loadable::Loading);
        let models_loading = self.effective_harness(cx).is_some_and(|harness| {
            !matches!(
                self.models.get(&harness),
                Some(Loadable::Ready(_)) | Some(Loadable::Error(_))
            )
        });
        // Harness unknown while the catalog resolves: the pixel-glyph loader
        // instead of guessing a brand mark.
        let chip_icon_loading =
            self.effective_harness(cx).is_none() && !no_agents && catalog_loading;
        // Harness known but nothing names the model yet (fresh install, no
        // remembered pick): a ghost label instead of a bare icon.
        let chip_label_loading =
            !no_agents && model_label.is_empty() && (catalog_loading || models_loading);
        let harness_icon: (&'static str, Option<gpui::Hsla>) = match self.effective_harness(cx) {
            Some(harness) => harness_brand_icon(harness),
            None if no_agents => (crate::icons::TERMINAL, Some(theme.text_muted)),
            None => (
                crate::icons::CLAUDE_MARK,
                Some(crate::icons::claude_brand()),
            ),
        };
        let explicit_options = self.explicit_options(cx);
        let traits_set = pill_traits_summary(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &explicit_options,
        );
        let traits_active = traits_customized(
            self.selected_model(cx),
            self.effective_reasoning(cx),
            &self.trait_ladder(cx),
            &explicit_options,
        );
        // Render the open popover's body first (mutable borrow), then the
        // chips. Branch/Checkout render in the composer FOOTER row (see
        // `render_footer`), not here.
        let closing = self.open.closing_since();
        let mut overlay: Option<(PickerKind, AnyElement)> = match self.mounted_kind() {
            // Footer-row pickers — their popovers mount down there.
            Some(PickerKind::Branch)
            | Some(PickerKind::Checkout)
            | Some(PickerKind::Space)
            | Some(PickerKind::Device) => None,
            Some(PickerKind::HarnessModel) => {
                let content = self.render_harness_model_popover(cx);
                Some((
                    PickerKind::HarnessModel,
                    // Compact single-harness pane (t3 ModelPickerContent
                    // shrunk to its tabbed layout).
                    self.popover_frame_flush(MODEL_POPOVER_WIDTH, content, cx),
                ))
            }
            None => None,
        };

        // Left cluster: empty — the device/project pickers live in the
        // composer FOOTER row alongside checkout + ref.
        // Right cluster: agent+model and traits — the composer appends
        // attach + send after this element (zeron composer-actions.tsx
        // arrangement).
        let left = div()
            .flex()
            .flex_row()
            .items_center()
            .min_w_0()
            .gap(px(4.0));
        // ONE chip for the whole run identity (user request): brand icon +
        // model name, then the effort level and every option that DEPARTS
        // from its default, joined with " · " ("· Low", "· High · 1M · Fast").
        // Defaults-as-chosen used to be spelled out too, which put a
        // meaningless "· Off" (Haiku's thinking toggle at its default) on the
        // pill; the level plus the real departures reads clearer.
        let chip_suffix = traits_set.map(|summary| {
            (
                SharedString::from(format!("· {summary}")),
                traits_active.then(|| theme.text.opacity(0.85)),
            )
        });
        let model_chip = self.trigger_chip(
            PickerKind::HarnessModel,
            model_label,
            true,
            Some(harness_icon),
            chip_icon_loading,
            chip_label_loading,
            chip_suffix,
            &theme,
            cx,
        );
        let right = div()
            .flex()
            .flex_row()
            .items_center()
            // Shrinkable under row pressure, like the footer chips: the chip's
            // own `min_w_0().truncate()` label/suffix only engage when this
            // cluster is allowed to give up width — `flex_none` here let the
            // labels paint over the attach/send buttons at narrow widths
            // instead of truncating (user report).
            .min_w_0()
            .gap(px(4.0))
            // End-anchored: the menu's right edge sits flush with the chip's
            // right edge (user request), same as the footer's ref popover.
            .child(attach_overlay_end(
                model_chip,
                &mut overlay,
                PickerKind::HarnessModel,
                "model-popover",
                closing,
            ));
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::SPACE_SM))
            // GPUI dispatches this captured stream while the thumb is dragged,
            // including when the pointer has left the model popover.
            .on_drag_move(cx.listener(Self::on_model_scrollbar_drag_move))
            // Same for a held slider thumb: the pointer routinely leaves the
            // track mid-drag and the rung must keep following it.
            .on_drag_move(cx.listener(Self::on_effort_drag_move))
            .child(left)
            .child(right)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_proto::{FolderEntry, Model, ModelOption, ModelOptionChoice};

    #[gpui::test]
    fn workspace_footer_pair_keeps_its_leading_edge_and_gap(cx: &mut gpui::TestAppContext) {
        struct Fixture {
            width: f32,
            bounds: std::rc::Rc<std::cell::RefCell<Vec<gpui::Bounds<gpui::Pixels>>>>,
        }
        impl gpui::Render for Fixture {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let chip = |width| {
                    let measured = self.bounds.clone();
                    gpui::canvas(
                        move |bounds, _, _| measured.borrow_mut().push(bounds),
                        |_, _, _, _| {},
                    )
                    .w(px(width))
                    .h(px(20.0))
                    .flex_none()
                };
                div().w(px(self.width)).child(
                    workspace_footer_row()
                        .px(px(10.0))
                        .child(chip(120.0))
                        .child(chip(90.0))
                        .child(div().flex_1().min_w_0())
                        .child(chip(60.0)),
                )
            }
        }
        let bounds = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let handle = cx.add_window(|_, _| Fixture {
            width: 320.0,
            bounds: bounds.clone(),
        });
        let mut first_left = None;
        for width in [320.0, 680.0, 1000.0, 320.0] {
            handle
                .update(cx, |fixture, _, cx| {
                    fixture.width = width;
                    cx.notify();
                })
                .unwrap();
            bounds.borrow_mut().clear();
            cx.update_window(handle.into(), |_, window, cx| window.draw(cx).clear())
                .unwrap();
            let measured = bounds.borrow();
            let pair = &measured[measured.len() - 3..];
            assert_eq!(pair[0].left(), *first_left.get_or_insert(pair[0].left()));
            assert!((f32::from(pair[1].left() - pair[0].right()) - 4.0).abs() < 0.1);
            assert_eq!(pair[0].top(), pair[1].top());
            assert!((f32::from(pair[2].right() - pair[0].left()) - (width - 20.0)).abs() < 0.1);
        }
    }

    #[gpui::test]
    fn picker_completion_and_dismissal_have_distinct_focus_behavior(cx: &mut gpui::TestAppContext) {
        use std::cell::Cell;
        use std::rc::Rc;
        cx.update(|cx| cx.set_global(Theme::dark()));
        let handle = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Pickers::new(state, cx)
        });
        let returned = Rc::new(Cell::new(0));
        let observed = returned.clone();
        let _sub = cx.update(|cx| {
            cx.subscribe(
                &handle.entity(cx).unwrap(),
                move |_, _: &ReturnComposerFocus, _| observed.set(observed.get() + 1),
            )
        });
        handle
            .update(cx, |pickers, _, cx| {
                pickers.open.open(PickerKind::Checkout);
                pickers.pick_checkout(CheckoutKind::Local, cx);
            })
            .unwrap();
        assert_eq!(returned.get(), 1);
        handle
            .update(cx, |pickers, _, cx| {
                pickers.open.open(PickerKind::HarnessModel);
                pickers.pick_model("test-model".into(), cx);
                assert!(
                    pickers.is_open(),
                    "model options remain available after a selection"
                );
                pickers.dismiss(cx);
            })
            .unwrap();
        assert_eq!(
            returned.get(),
            1,
            "outside clicks must not request composer focus"
        );
        handle
            .update(cx, |pickers, window, cx| {
                pickers.open.open(PickerKind::Checkout);
                pickers.open.note_trigger_press();
                pickers.dismiss(cx); // Capture closes before the trigger's click.
                pickers.toggle(PickerKind::Checkout, window, cx);
            })
            .unwrap();
        assert_eq!(
            returned.get(),
            2,
            "closing via the trigger returns to the composer"
        );
    }

    #[gpui::test]
    fn projectless_picker_clears_checkout_and_supports_keyboard_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        let state = cx.new(|_| {
            let mut state = AppState::new();
            state.local_device_id = Some("local".into());
            state.apply_spaces(vec![Space {
                id: "repo".into(),
                device_id: "local".into(),
                path: "/repo".into(),
                name: None,
                git_detected: true,
                git_checked_at: None,
                checkout_id: None,
                created_at: chrono::Utc::now(),
            }]);
            state
        });
        let pickers = cx.new(|cx| Pickers::new(state.clone(), cx));
        pickers.update(cx, |pickers, cx| {
            pickers.config.branch = Some("old-branch".into());
            pickers.config.checkout = CheckoutKind::NewWorktree;
            pickers.open.open(PickerKind::Space);
            pickers.active = 1; // project row followed by the projectless row
            pickers.on_search_submit(cx);
        });
        cx.run_until_parked();
        pickers.update(cx, |pickers, cx| {
            assert!(pickers.config.branch.is_none());
            assert_eq!(pickers.config.checkout, CheckoutKind::default());
            assert!(pickers.defaults.no_project);
            assert!(pickers.state.read(cx).auto_selected);
            assert!(pickers.defaults.project.is_none());
            assert_eq!(pickers.selected_space_index(cx), 1);
        });
        state.update(cx, |state, cx| state.select_device("remote".into(), cx));
        cx.run_until_parked();
        pickers.update(cx, |pickers, cx| {
            assert_eq!(pickers.space_target(cx).as_deref(), Some("remote"));
            assert_eq!(pickers.selected_space_index(cx), 0); // empty device
            assert!(pickers.target_generation >= 2);
        });
    }

    fn bare_model(id: &str, label: &str) -> Model {
        Model {
            id: id.into(),
            label: label.into(),
            description: None,
            reasoning_levels: Vec::new(),
            options: Vec::new(),
            pricing: None,
        }
    }

    #[gpui::test]
    fn remembered_options_stay_valid_for_the_sent_model_without_a_catalog(
        cx: &mut gpui::TestAppContext,
    ) {
        let one_m = serde_json::Value::String("1m".into());
        let mut opus = bare_model("opus", "Opus");
        opus.options.push(ModelOption {
            id: "contextWindow".into(),
            label: "Context window".into(),
            choices: ["200k", "1m"]
                .map(|id| ModelOptionChoice {
                    id: id.into(),
                    label: id.into(),
                })
                .into(),
            default_choice: "200k".into(),
        });
        let state = cx.new(|_| AppState::new());
        let pickers = cx.new(|cx| Pickers::new(state, cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults.harness = Some(HarnessId::ClaudeCode);
            pickers.models.insert(
                HarnessId::ClaudeCode,
                Loadable::Ready(vec![opus.clone(), bare_model("haiku", "Haiku")]),
            );
            pickers.pick_model("opus".into(), cx);
            pickers.pick_option("contextWindow".into(), "1m".into(), false, cx);
            let resolved = pickers.resolved(cx);
            assert_eq!(resolved.model.as_deref(), Some("opus"));
            assert_eq!(resolved.model_options.get("contextWindow"), Some(&one_m));

            pickers.pick_model("haiku".into(), cx);
            assert!(pickers.resolved(cx).model_options.is_empty());

            // Restart (draft cleared) and send before the catalog is usable.
            for catalog in [
                Loadable::Idle,
                Loadable::Loading,
                Loadable::Error("down".into()),
            ] {
                pickers.config = DraftConfig::default();
                pickers.models.insert(HarnessId::ClaudeCode, catalog);
                let resolved = pickers.resolved(cx);
                assert_eq!(resolved.model.as_deref(), Some("haiku"));
                assert!(
                    resolved.model_options.is_empty(),
                    "Opus's 1M pick must not ride along with Haiku"
                );
            }

            // The Opus memory itself survives for when Opus is picked again.
            pickers.pick_model("opus".into(), cx);
            let resolved = pickers.resolved(cx);
            assert_eq!(resolved.model.as_deref(), Some("opus"));
            assert_eq!(resolved.model_options.get("contextWindow"), Some(&one_m));
        });
    }

    fn ladder_model(id: &str, label: &str, ladder: &[ReasoningLevel]) -> Model {
        let mut model = bare_model(id, label);
        model.reasoning_levels = ladder.to_vec();
        model
    }

    /// The effort level belongs to the MODEL it was chosen on: A to B to A
    /// restores A's level, and a model nobody has touched starts at its own
    /// catalog default (user request).
    #[gpui::test]
    fn effort_sticks_to_the_model_it_was_chosen_on(cx: &mut gpui::TestAppContext) {
        use ReasoningLevel::*;
        let opus = ladder_model("opus", "Opus 5", &[Low, Medium, High, Max]);
        let sonnet = ladder_model("sonnet", "Sonnet 5", &[Minimal, Low, Medium, High]);
        // No ladder at all (Haiku): nothing to remember, nothing to show.
        let haiku = bare_model("haiku", "Haiku 4.5");
        let state = cx.new(|_| AppState::new());
        let pickers = cx.new(|cx| Pickers::new(state, cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults.harness = Some(HarnessId::ClaudeCode);
            pickers.models.insert(
                HarnessId::ClaudeCode,
                Loadable::Ready(vec![opus.clone(), sonnet.clone(), haiku.clone()]),
            );

            // Never seen: the catalog default (High) for both ladders.
            pickers.pick_model("opus".into(), cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(High));
            pickers.pick_reasoning(Low, cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(Low));

            // A different model starts at ITS default, not at Opus's pick.
            pickers.pick_model("sonnet".into(), cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(High));
            pickers.pick_reasoning(Minimal, cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(Minimal));

            // Back and forth: each model keeps its own.
            pickers.pick_model("opus".into(), cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(Low));
            pickers.pick_model("sonnet".into(), cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(Minimal));

            // Only the picked model's entry is ever written.
            assert_eq!(
                pickers.defaults.effort_for(HarnessId::ClaudeCode, "opus"),
                Some(Low)
            );
            assert_eq!(
                pickers.defaults.effort_for(HarnessId::ClaudeCode, "sonnet"),
                Some(Minimal)
            );
            assert_eq!(
                pickers.defaults.effort_for(HarnessId::ClaudeCode, "haiku"),
                None
            );

            // A ladder-less model has no effort at all (and no slider).
            pickers.pick_model("haiku".into(), cx);
            assert_eq!(pickers.effective_reasoning(cx), None);
            assert!(pickers.trait_ladder(cx).is_empty());

            // The run request still carries the level (chat config parity).
            pickers.pick_model("opus".into(), cx);
            assert_eq!(pickers.resolved(cx).reasoning, Some(Low));
        });
    }

    /// Left/right walk the ladder and stop at its ends rather than wrapping.
    #[gpui::test]
    fn arrow_keys_step_the_effort_slider_and_clamp(cx: &mut gpui::TestAppContext) {
        use ReasoningLevel::*;
        let opus = ladder_model("opus", "Opus 5", &[Low, Medium, High, Max]);
        let state = cx.new(|_| AppState::new());
        let pickers = cx.new(|cx| Pickers::new(state, cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults.harness = Some(HarnessId::ClaudeCode);
            pickers
                .models
                .insert(HarnessId::ClaudeCode, Loadable::Ready(vec![opus.clone()]));
            pickers.pick_model("opus".into(), cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(High));
            assert!(pickers.step_reasoning(1, cx));
            assert_eq!(pickers.effective_reasoning(cx), Some(Max));
            // Already at the top rung: consumed, but nothing moves.
            assert!(pickers.step_reasoning(1, cx));
            assert_eq!(pickers.effective_reasoning(cx), Some(Max));
            assert!(pickers.step_reasoning(-1, cx));
            assert!(pickers.step_reasoning(-1, cx));
            assert!(pickers.step_reasoning(-1, cx));
            assert_eq!(pickers.effective_reasoning(cx), Some(Low));
            assert!(pickers.step_reasoning(-1, cx));
            assert_eq!(pickers.effective_reasoning(cx), Some(Low));
        });
    }

    /// A press on the track snaps to the nearest rung - the same code path the
    /// old ladder rows used, so persistence is unchanged.
    #[gpui::test]
    fn pressing_the_track_snaps_to_the_nearest_rung(cx: &mut gpui::TestAppContext) {
        use ReasoningLevel::*;
        let opus = ladder_model("opus", "Opus 5", &[Low, Medium, High, Max]);
        let state = cx.new(|_| AppState::new());
        let pickers = cx.new(|cx| Pickers::new(state, cx));
        pickers.update(cx, |pickers, cx| {
            pickers.defaults.harness = Some(HarnessId::ClaudeCode);
            pickers
                .models
                .insert(HarnessId::ClaudeCode, Loadable::Ready(vec![opus.clone()]));
            pickers.pick_model("opus".into(), cx);
            let width = EFFORT_TRACK_WIDTH;
            pickers.pick_reasoning_at(0.0, width, cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(Low));
            pickers.pick_reasoning_at(width * 0.4, width, cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(Medium));
            pickers.pick_reasoning_at(width * 2.0, width, cx);
            assert_eq!(pickers.effective_reasoning(cx), Some(Max));
            // And the pick landed in the per-model memory, not a global slot.
            assert_eq!(
                pickers.defaults.effort_for(HarnessId::ClaudeCode, "opus"),
                Some(Max)
            );
        });
    }

    fn descriptor(id: HarnessId, name: &str) -> HarnessDescriptor {
        HarnessDescriptor {
            id,
            name: name.into(),
            installed: true,
            enabled: Some(true),
            reasoning_levels: Vec::new(),
            steering_mode: zeron_proto::SteeringMode::StepBoundary,
            supports_steering: false,
        }
    }

    #[test]
    fn tab_search_never_leaves_the_viewed_harness() {
        let descriptors = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
            descriptor(HarnessId::Codex, "Codex"),
        ];
        let claude = vec![bare_model("fable-5", "Fable 5")];
        let codex = vec![bare_model("gpt-fable", "Fable (Codex)")];
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            match harness {
                HarnessId::ClaudeCode => Some(claude.as_slice()),
                HarnessId::Codex => Some(codex.as_slice()),
                _ => None,
            }
        };
        // Both catalogs match "fable", but the viewed tab is Claude — the
        // Codex hit must not appear.
        let rows = scoped_model_rows(
            "fable",
            ModelRail::Harness,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            |_, _| false,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, HarnessId::ClaudeCode);
        assert_eq!(rows[0].model.id, "fable-5");
    }

    #[test]
    fn favorites_tab_search_ranks_only_starred_rows() {
        let descriptors = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
            descriptor(HarnessId::Codex, "Codex"),
        ];
        let claude = vec![bare_model("fable-5", "Fable 5")];
        let codex = vec![bare_model("gpt-fable", "Fable (Codex)")];
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            match harness {
                HarnessId::ClaudeCode => Some(claude.as_slice()),
                HarnessId::Codex => Some(codex.as_slice()),
                _ => None,
            }
        };
        let starred =
            |harness: HarnessId, model: &str| harness == HarnessId::Codex && model == "gpt-fable";
        let rows = scoped_model_rows(
            "fable",
            ModelRail::Favorites,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].harness, HarnessId::Codex);

        // Empty query on the favorites tab: the starred set, nothing else.
        let rows = scoped_model_rows(
            "",
            ModelRail::Favorites,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model.id, "gpt-fable");
    }

    #[test]
    fn harness_tab_lists_stars_first_and_description_still_matches() {
        let descriptors = vec![descriptor(HarnessId::Opencode, "opencode")];
        let mut provider_a = bare_model("glm-5.2-a", "GLM-5.2");
        provider_a.description = Some("Anthropic".into());
        let mut provider_b = bare_model("glm-5.2-b", "GLM-5.2");
        provider_b.description = Some("Baseten".into());
        let models = vec![provider_a, provider_b];
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::Opencode).then_some(models.as_slice())
        };
        let starred = |harness: HarnessId, model: &str| {
            harness == HarnessId::Opencode && model == "glm-5.2-b"
        };
        // No query: catalog order with the star floated to the top.
        let rows = scoped_model_rows(
            "",
            ModelRail::Harness,
            Some(HarnessId::Opencode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows[0].model.id, "glm-5.2-b");
        assert_eq!(rows[1].model.id, "glm-5.2-a");
        // Provider attribution stays searchable inside the tab.
        let rows = scoped_model_rows(
            "baseten",
            ModelRail::Harness,
            Some(HarnessId::Opencode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model.id, "glm-5.2-b");
    }

    #[test]
    fn the_claude_tab_leads_with_its_flagships_then_an_other_group() {
        let descriptors = vec![descriptor(HarnessId::ClaudeCode, "Claude Code")];
        // Catalog order as the harness serves it (fable-5 sits between the
        // two flagship Fables, so a plain "top three" rule would be wrong).
        let models: Vec<Model> = [
            ("claude-fable-5-1", "Fable 5.1"),
            ("claude-fable-5", "Fable 5"),
            ("claude-opus-5", "Opus 5"),
            ("claude-opus-4-8", "Opus 4.8"),
            ("claude-sonnet-5", "Sonnet 5"),
            ("claude-haiku-4-5", "Haiku 4.5"),
        ]
        .into_iter()
        .map(|(id, label)| bare_model(id, label))
        .collect();
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::ClaudeCode).then_some(models.as_slice())
        };
        let rows = scoped_model_rows(
            "",
            ModelRail::Harness,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            |_, _| false,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.model.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude-fable-5-1",
                "claude-opus-5",
                "claude-sonnet-5",
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-haiku-4-5",
            ]
        );
        assert_eq!(
            rows.divider,
            Some(3),
            "OTHER opens under the three flagships"
        );
        assert_eq!(rows.list_len(), rows.len() + 1);
    }

    #[test]
    fn the_1m_context_variants_ride_with_their_flagship() {
        assert!(is_flagship_model(
            HarnessId::ClaudeCode,
            "claude-opus-5[1m]"
        ));
        assert!(is_flagship_model(HarnessId::Mock, "claude-sonnet-5"));
        // A previous generation is not a flagship, and the prefixes never
        // swallow the older point releases.
        assert!(!is_flagship_model(HarnessId::ClaudeCode, "claude-fable-5"));
        assert!(!is_flagship_model(HarnessId::ClaudeCode, "claude-opus-4-8"));
    }

    #[test]
    fn the_codex_tab_leads_with_the_newest_gpt_generation() {
        let descriptors = vec![descriptor(HarnessId::Codex, "Codex")];
        let models: Vec<Model> = [
            ("gpt-6-astra", "GPT-6-Astra"),
            ("gpt-5.6-sol", "GPT-5.6-Sol"),
            ("gpt-5.5", "GPT-5.5"),
        ]
        .into_iter()
        .map(|(id, label)| bare_model(id, label))
        .collect();
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::Codex).then_some(models.as_slice())
        };
        let rows = scoped_model_rows(
            "",
            ModelRail::Harness,
            Some(HarnessId::Codex),
            &descriptors,
            models_for,
            |_, _| false,
        );
        assert_eq!(rows[0].model.id, "gpt-6-astra");
        assert_eq!(rows.divider, Some(1));
        // The rest keeps the catalog's newest-first order.
        assert_eq!(rows[1].model.id, "gpt-5.6-sol");
        assert_eq!(rows[2].model.id, "gpt-5.5");
    }

    #[test]
    fn an_uncurated_harness_leads_with_default_plus_the_two_newest() {
        let descriptors = vec![descriptor(HarnessId::Opencode, "opencode")];
        let models: Vec<Model> = [
            ("glm-5.2", "GLM-5.2"),
            ("qwen-4", "Qwen 4"),
            ("kimi-3", "Kimi 3"),
            ("default", "Default"),
        ]
        .into_iter()
        .map(|(id, label)| bare_model(id, label))
        .collect();
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::Opencode).then_some(models.as_slice())
        };
        let rows = scoped_model_rows(
            "",
            ModelRail::Harness,
            Some(HarnessId::Opencode),
            &descriptors,
            models_for,
            |_, _| false,
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.model.id.as_str()).collect();
        assert_eq!(ids, vec!["glm-5.2", "qwen-4", "default", "kimi-3"]);
        assert_eq!(rows.divider, Some(3));
    }

    #[test]
    fn stars_stay_above_the_flagships_and_a_query_drops_the_divider() {
        let descriptors = vec![descriptor(HarnessId::ClaudeCode, "Claude Code")];
        let models: Vec<Model> = [
            ("claude-fable-5-1", "Fable 5.1"),
            ("claude-opus-5", "Opus 5"),
            ("claude-haiku-4-5", "Haiku 4.5"),
        ]
        .into_iter()
        .map(|(id, label)| bare_model(id, label))
        .collect();
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::ClaudeCode).then_some(models.as_slice())
        };
        let starred = |harness: HarnessId, model: &str| {
            harness == HarnessId::ClaudeCode && model == "claude-haiku-4-5"
        };
        let rows = scoped_model_rows(
            "",
            ModelRail::Harness,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            starred,
        );
        // Favorites behaviour is untouched: the star still floats to the top,
        // and the divider counts from below it.
        assert_eq!(rows[0].model.id, "claude-haiku-4-5");
        assert_eq!(rows.divider, None, "nothing left for OTHER");

        // A filtered list is one ranked run, no divider (every label carries
        // a version digit, so "5" keeps all three rows).
        let rows = scoped_model_rows(
            "5",
            ModelRail::Harness,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            starred,
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows.divider, None);
        assert_eq!(rows.list_len(), 3);
    }

    #[test]
    fn the_favorites_tab_never_grows_a_divider() {
        let descriptors = vec![descriptor(HarnessId::ClaudeCode, "Claude Code")];
        let models = vec![
            bare_model("claude-opus-5", "Opus 5"),
            bare_model("claude-haiku-4-5", "Haiku 4.5"),
        ];
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::ClaudeCode).then_some(models.as_slice())
        };
        let rows = scoped_model_rows(
            "",
            ModelRail::Favorites,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            |_, _| true,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows.divider, None);
    }

    #[test]
    fn keyboard_navigation_steps_over_the_divider_item() {
        // Three flagships, divider, three others: the list has seven items
        // but only six navigable rows.
        let divider = Some(3);
        assert_eq!(row_for_list_ix(0, divider), Some(0));
        assert_eq!(row_for_list_ix(2, divider), Some(2));
        assert_eq!(row_for_list_ix(3, divider), None, "the divider itself");
        assert_eq!(row_for_list_ix(4, divider), Some(3));
        assert_eq!(row_for_list_ix(6, divider), Some(5));
        // Every row round-trips, and no row ever maps onto the divider item.
        for row_ix in 0..6 {
            let list_ix = list_ix_for_row(row_ix, divider);
            assert_ne!(list_ix, 3, "row {row_ix} would land on the divider");
            assert_eq!(row_for_list_ix(list_ix, divider), Some(row_ix));
        }
        // Without a divider the two indices are the same thing.
        for ix in 0..6 {
            assert_eq!(row_for_list_ix(ix, None), Some(ix));
            assert_eq!(list_ix_for_row(ix, None), ix);
        }
    }

    #[test]
    fn the_mod_n_badges_follow_the_new_visual_order() {
        let descriptors = vec![descriptor(HarnessId::ClaudeCode, "Claude Code")];
        let models: Vec<Model> = [
            ("claude-fable-5", "Fable 5"),
            ("claude-opus-5", "Opus 5"),
            ("claude-haiku-4-5", "Haiku 4.5"),
        ]
        .into_iter()
        .map(|(id, label)| bare_model(id, label))
        .collect();
        let models_for = |harness: HarnessId| -> Option<&[Model]> {
            (harness == HarnessId::ClaudeCode).then_some(models.as_slice())
        };
        let rows = scoped_model_rows(
            "",
            ModelRail::Harness,
            Some(HarnessId::ClaudeCode),
            &descriptors,
            models_for,
            |_, _| false,
        );
        // The badge printed on row `ix` is mod-(ix+1) and `activate_model_index`
        // takes the SAME ix, so the flagship that now leads the list is what
        // mod-1 selects.
        assert_eq!(rows[0].model.id, "claude-opus-5");
        assert_eq!(rows.divider, Some(1));
        assert_eq!(rows[1].model.id, "claude-fable-5");
    }

    #[test]
    fn normalize_drops_default_alias_and_folds_orphan_1m_rows() {
        // The shape an OLDER engine serves: a `default` alias row plus
        // 1M-pinned variants with no bare base. A non-claude harness keeps
        // wire labels (no curated catalog to borrow from).
        let models = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("default", "Default (recommended)"),
                bare_model("titan[1m]", "Titan (1M context)"),
                bare_model("gpt-x-9[1m]", "GPT X-9"),
                bare_model("nano", "Nano"),
            ],
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["titan", "gpt-x-9", "nano"]
        );
        assert_eq!(models[0].label, "Titan");
        assert_eq!(models[1].label, "GPT X-9");
        // Folded rows pin the Context Window trait to 1M.
        assert!(
            models[0]
                .options
                .iter()
                .any(|o| o.id == "contextWindow" && o.default_choice == "1m")
        );
        assert!(models[2].options.is_empty());

        // A `default`-only list survives (nothing real to prefer).
        let only_default =
            normalize_model_rows(HarnessId::Codex, vec![bare_model("default", "Default")]);
        assert_eq!(only_default.len(), 1);

        // A base-plus-variant pair (already folded by a NEWER engine — the
        // variant never reaches us; belt-and-braces if it does): variant
        // drops, base is untouched.
        let paired = normalize_model_rows(
            HarnessId::Codex,
            vec![
                bare_model("titan-5", "Titan 5"),
                bare_model("titan-5[1m]", "Titan 5 (1M)"),
            ],
        );
        assert_eq!(paired.len(), 1);
        assert_eq!(paired[0].id, "titan-5");

        // Idempotent over a clean list.
        let clean = vec![bare_model("titan-5", "Titan 5")];
        assert_eq!(normalize_model_rows(HarnessId::Codex, clean.clone()), clean);
    }

    #[test]
    fn normalize_gives_claude_rows_their_versioned_catalog_labels() {
        // The real prod shape: alias values with terse names. Claude rows
        // adopt the curated labels so the version number always shows
        // (user request), exact ids included; foreign ids pass through.
        let models = normalize_model_rows(
            HarnessId::ClaudeCode,
            vec![
                bare_model("default", "Default (recommended)"),
                bare_model("opus[1m]", "Opus (1M context)"),
                bare_model("claude-fable-5[1m]", "Fable"),
                bare_model("sonnet", "Sonnet"),
                bare_model("haiku", "Haiku"),
                bare_model("claude-nova-1", "Nova 1"),
            ],
        );
        assert_eq!(
            models.iter().map(|m| m.label.as_str()).collect::<Vec<_>>(),
            vec!["Opus 5", "Fable 5", "Sonnet 5", "Haiku 4.5", "Nova 1"]
        );
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["opus", "claude-fable-5", "sonnet", "haiku", "claude-nova-1"]
        );
    }

    #[test]
    fn traits_summary_formats_non_defaults() {
        let model = Model {
            id: "opus".into(),
            label: "Opus".into(),
            description: None,
            reasoning_levels: vec![ReasoningLevel::Medium, ReasoningLevel::High],
            options: vec![
                ModelOption {
                    id: "context".into(),
                    label: "Context window".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "standard".into(),
                            label: "Standard".into(),
                        },
                        ModelOptionChoice {
                            id: "1m".into(),
                            label: "1M".into(),
                        },
                    ],
                    default_choice: "standard".into(),
                },
                ModelOption {
                    id: "speed".into(),
                    label: "Speed".into(),
                    choices: vec![
                        ModelOptionChoice {
                            id: "normal".into(),
                            label: "Normal".into(),
                        },
                        ModelOptionChoice {
                            id: "fast".into(),
                            label: "Fast".into(),
                        },
                    ],
                    default_choice: "normal".into(),
                },
            ],
            pricing: None,
        };
        let mut selections = serde_json::Map::new();
        selections.insert("context".into(), serde_json::Value::String("1m".into()));
        selections.insert("speed".into(), serde_json::Value::String("fast".into()));
        assert_eq!(
            traits_summary(Some(&model), Some(ReasoningLevel::High), &selections),
            Some("High · 1M · Fast".to_string())
        );
        // All defaults: the effective choices still read on the trigger.
        assert_eq!(
            traits_summary(Some(&model), None, &serde_json::Map::new()),
            Some("Standard · Normal".to_string())
        );
        // A saved choice the option no longer offers falls back to the default
        // label rather than vanishing or echoing a stale id.
        let mut stale = serde_json::Map::new();
        stale.insert(
            "speed".into(),
            serde_json::Value::String("ludicrous".into()),
        );
        assert_eq!(
            traits_summary(Some(&model), None, &stale),
            Some("Standard · Normal".to_string())
        );
        // Remembered picks drop what the model doesn't offer before sending.
        let mut remembered = selections.clone();
        remembered.insert(
            "speed".into(),
            serde_json::Value::String("ludicrous".into()),
        );
        remembered.insert("fastMode".into(), serde_json::Value::String("on".into()));
        let mut want = serde_json::Map::new();
        want.insert("context".into(), serde_json::Value::String("1m".into()));
        assert_eq!(offered_options(&model, remembered), want);
        // Reasoning shows without a model too.
        assert_eq!(
            traits_summary(
                None,
                Some(ReasoningLevel::Ultrathink),
                &serde_json::Map::new()
            ),
            Some("Ultrathink".to_string())
        );
        // Nothing to describe → "Traits" fallback upstream.
        assert_eq!(traits_summary(None, None, &serde_json::Map::new()), None);

        // Customized (bright trigger) only when something departs from its
        // default: default-choice selections and the default reasoning level
        // don't count; stale ids don't either.
        let ladder = model.reasoning_levels.clone();
        assert!(traits_customized(
            Some(&model),
            Some(ReasoningLevel::High),
            &ladder,
            &selections
        ));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &serde_json::Map::new()
        ));
        let mut defaults = serde_json::Map::new();
        defaults.insert("speed".into(), serde_json::Value::String("normal".into()));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &defaults
        ));
        assert!(!traits_customized(
            Some(&model),
            default_reasoning(&ladder),
            &ladder,
            &stale
        ));
        assert!(traits_customized(
            Some(&model),
            Some(ReasoningLevel::Medium),
            &ladder,
            &serde_json::Map::new()
        ));
    }

    #[test]
    fn folder_paths_and_breadcrumbs() {
        assert_eq!(parent_path("/home/w/dev"), Some("/home/w".to_string()));
        assert_eq!(parent_path("/home"), Some("/".to_string()));
        assert_eq!(parent_path("/home/"), Some("/".to_string()));
        assert_eq!(parent_path("/"), None);
        assert_eq!(parent_path(""), None);
        assert_eq!(child_path("/home", "w"), "/home/w");
        assert_eq!(child_path("/", "home"), "/home");
        let crumbs = breadcrumbs("/home/w/dev");
        let labels: Vec<&str> = crumbs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, ["/", "home", "w", "dev"]);
        assert_eq!(crumbs[2].1, "/home/w");
        assert_eq!(breadcrumbs("/").len(), 1);
    }

    #[test]
    fn completion_prefix_lengths() {
        // Case-insensitive; the length indexes into the NAME's bytes.
        assert_eq!(completion_prefix_len("Documents", "doc"), Some(3));
        assert_eq!(&"Documents"[3..], "uments");
        assert_eq!(completion_prefix_len("zeron", "zeron"), Some(5));
        assert_eq!(completion_prefix_len("zeron", ""), Some(0));
        assert_eq!(completion_prefix_len("zeron", "dev"), None);
        // Longer than the name → not a prefix.
        assert_eq!(completion_prefix_len("dev", "devel"), None);
        // Multibyte names slice on a char boundary.
        assert_eq!(completion_prefix_len("héllo", "hé"), Some(3));
        assert_eq!(&"héllo"[3..], "llo");
    }

    #[test]
    fn segment_target_resolution() {
        let names = ["github", "GitHub", "worktree"];
        // Exact casing beats the earlier case-insensitive sibling…
        assert_eq!(segment_target(&names, "GitHub"), Some(1));
        assert_eq!(segment_target(&names, "github"), Some(0));
        // …but with no exact-cased hit, case-insensitive exact still lands.
        assert_eq!(segment_target(&names, "WORKTREE"), Some(2));
        // Unique prefix descends; an ambiguous one keeps the slash honest.
        assert_eq!(segment_target(&names, "work"), Some(2));
        assert_eq!(segment_target(&names, "g"), None);
        assert_eq!(segment_target(&names, "x"), None);
    }

    #[test]
    fn typed_path_target_expands_absolute_and_home_paths() {
        let home = Some("/home/wing");
        assert_eq!(typed_path_target("/disk2/", home), Some("/disk2".into()));
        assert_eq!(
            typed_path_target("/disk2/projects", home),
            Some("/disk2/projects".into())
        );
        assert_eq!(typed_path_target("/", home), Some("/".into()));
        assert_eq!(typed_path_target("~", home), Some("/home/wing".into()));
        assert_eq!(typed_path_target("~/", home), Some("/home/wing".into()));
        assert_eq!(
            typed_path_target("~/github/", home),
            Some("/home/wing/github".into())
        );
        // `~x` is a folder name; relative queries are searches, not paths.
        assert_eq!(typed_path_target("~x", home), None);
        assert_eq!(typed_path_target("src", home), None);
        // `~` can't expand before the device's home is known.
        assert_eq!(typed_path_target("~/github", None), None);
        assert_eq!(typed_path_target("/disk2", None), Some("/disk2".into()));
    }

    #[test]
    fn browser_navigation_reducer() {
        let listing = FolderListing {
            path: "/home/w".into(),
            entries: vec![
                FolderEntry {
                    name: "notes.txt".into(),
                    is_dir: false,
                    is_repo: false,
                },
                FolderEntry {
                    name: "dev".into(),
                    is_dir: true,
                    is_repo: false,
                },
                FolderEntry {
                    name: "zeron".into(),
                    is_dir: true,
                    is_repo: true,
                },
            ],
            truncated: false,
        };
        // Files never show as rows.
        assert_eq!(browser_rows(&listing).len(), 2);
        assert_eq!(browser_rows(&listing)[1].name, "zeron");
    }

    #[test]
    fn resolved_chat_config_requires_harness() {
        let mut resolved = ResolvedRunConfig::default();
        assert!(resolved.chat_config().is_none());
        resolved.harness = Some(HarnessId::ClaudeCode);
        resolved.model = Some("opus".into());
        resolved.reasoning = Some(ReasoningLevel::High);
        let config = resolved.chat_config().expect("harness set");
        assert_eq!(config.harness, HarnessId::ClaudeCode);
        assert_eq!(config.model.as_deref(), Some("opus"));
        assert_eq!(config.sandbox, SandboxLevel::WorkspaceWrite);
    }

    #[test]
    fn default_model_is_first_catalog_row() {
        let models = vec![
            Model {
                id: "flagship".into(),
                label: "Flagship".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
                pricing: None,
            },
            Model {
                id: "fast".into(),
                label: "Fast".into(),
                description: None,
                reasoning_levels: vec![],
                options: vec![],
                pricing: None,
            },
        ];
        assert_eq!(default_model(&models).map(|m| &*m.id), Some("flagship"));
        assert!(default_model(&[]).is_none());
    }

    #[test]
    fn default_effort_falls_back_to_the_ladders_middle_rung() {
        use ReasoningLevel::*;
        // The catalog default wins wherever it names one.
        assert_eq!(default_effort(&[Low, Medium, High, Max]), Some(High));
        // Ladder-less models have no effort.
        assert_eq!(default_effort(&[]), None);
        // A remembered level is kept only while the ladder offers it.
        assert_eq!(resolve_effort(Some(Low), &[Low, Medium, High]), Some(Low));
        assert_eq!(resolve_effort(Some(Max), &[Low, Medium, High]), Some(High));
        assert_eq!(resolve_effort(None, &[Low, Medium, High]), Some(High));
        assert_eq!(resolve_effort(Some(Low), &[]), None);
    }

    #[test]
    fn pill_summary_names_the_effort_and_hides_default_options() {
        let mut model = bare_model("haiku", "Haiku 4.5");
        model.options.push(ModelOption {
            id: "thinking".into(),
            label: "Thinking".into(),
            choices: vec![
                ModelOptionChoice {
                    id: "off".into(),
                    label: "Off".into(),
                },
                ModelOptionChoice {
                    id: "on".into(),
                    label: "On".into(),
                },
            ],
            default_choice: "off".into(),
        });
        let none = serde_json::Map::new();
        // The default toggle stays silent: the pill reads "Haiku 4.5 - Low",
        // never "Haiku 4.5 Low - Off" (user report).
        assert_eq!(
            pill_traits_summary(Some(&model), Some(ReasoningLevel::Low), &none),
            Some("Low".to_string())
        );
        // A real departure is still named.
        let mut on = serde_json::Map::new();
        on.insert("thinking".into(), serde_json::Value::String("on".into()));
        assert_eq!(
            pill_traits_summary(Some(&model), Some(ReasoningLevel::Low), &on),
            Some("Low \u{b7} On".to_string())
        );
        // Nothing to say at all.
        assert_eq!(pill_traits_summary(Some(&model), None, &none), None);
        assert_eq!(pill_traits_summary(None, None, &none), None);
    }

    #[test]
    fn default_reasoning_prefers_high_then_medium() {
        use ReasoningLevel::*;
        // Recommended default is High (user-corrected), even on full ladders.
        assert_eq!(
            default_reasoning(&[Low, Medium, High, XHigh, Max, Ultracode, Ultrathink]),
            Some(High)
        );
        assert_eq!(default_reasoning(&[Low, Medium, High, Max]), Some(High));
        // No High: Medium.
        assert_eq!(default_reasoning(&[Minimal, Low, Medium]), Some(Medium));
        // Neither offered: first entry.
        assert_eq!(default_reasoning(&[Minimal, Low]), Some(Minimal));
        // Ladder-less model (Haiku): no reasoning at all.
        assert_eq!(default_reasoning(&[]), None);
    }

    #[test]
    fn clamp_reasoning_keeps_offered_levels_and_heals_foreign_ones() {
        use ReasoningLevel::*;
        let ladder = [Low, Medium, High, Max];
        // A pick the ladder offers survives.
        assert_eq!(clamp_reasoning(Some(Max), &ladder), Some(Max));
        // A remembered level the new model doesn't offer heals to its default.
        assert_eq!(clamp_reasoning(Some(XHigh), &ladder), Some(High));
        // No pick at all resolves to the concrete default too.
        assert_eq!(clamp_reasoning(None, &ladder), Some(High));
        assert_eq!(clamp_reasoning(Some(High), &[]), None);
    }

    #[test]
    fn mock_harness_hidden_unless_alone() {
        let descriptor = |id: HarnessId, name: &str| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: zeron_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled: None,
        };
        let mixed = vec![
            descriptor(HarnessId::Mock, "Mock"),
            descriptor(HarnessId::ClaudeCode, "Claude Code"),
        ];
        // Env-independent core: mock hidden in production…
        let visible = visible_harnesses_impl(&mixed, false);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, HarnessId::ClaudeCode);
        let only_mock = vec![descriptor(HarnessId::Mock, "Mock")];
        assert_eq!(visible_harnesses_impl(&only_mock, false).len(), 1);
        // …and opted back in by ZERON_HARNESS=mock (the e2e rig).
        assert_eq!(visible_harnesses_impl(&mixed, true).len(), 2);
        assert_eq!(visible_harnesses_impl(&mixed, true)[0].id, HarnessId::Mock);
    }

    #[test]
    fn offered_harnesses_follow_the_catalog_enabled_flags() {
        let descriptor = |id: HarnessId, name: &str, enabled: Option<bool>| HarnessDescriptor {
            id,
            name: name.into(),
            supports_steering: true,
            steering_mode: zeron_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed: true,
            enabled,
        };
        let catalog = |claude: Option<bool>, codex: Option<bool>, grok: Option<bool>| {
            vec![
                descriptor(HarnessId::Mock, "Mock", Some(false)),
                descriptor(HarnessId::ClaudeCode, "Claude Code", claude),
                descriptor(HarnessId::Codex, "Codex", codex),
                descriptor(HarnessId::Grok, "Grok", grok),
            ]
        };
        // A catalog from an engine predating the flag (all None) follows its
        // installed probes, so every detected real harness is offered.
        let offered = offered_harnesses_impl(&catalog(None, None, None), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::ClaudeCode, HarnessId::Codex, HarnessId::Grok]
        );
        // The device's flags win: Grok on, Codex off; catalog order holds.
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), Some(true)), false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::ClaudeCode, HarnessId::Grok]
        );
        // The dev-rig mock opt-in survives the enabled filter (and Grok's
        // unknown flag still resolves through its installed probe).
        let offered = offered_harnesses_impl(&catalog(Some(true), Some(false), None), true);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Mock, HarnessId::ClaudeCode, HarnessId::Grok]
        );
        // Nothing enabled offers nothing — the composer renders the
        // no-agents empty state instead of resurrecting disabled agents.
        let offered =
            offered_harnesses_impl(&catalog(Some(false), Some(false), Some(false)), false);
        assert!(offered.is_empty());
        // So does a legacy catalog whose installed probes all failed: never
        // resurface unrunnable agents just to avoid an empty picker.
        let mut missing = catalog(None, None, None);
        missing.iter_mut().for_each(|d| d.installed = false);
        assert!(offered_harnesses_impl(&missing, false).is_empty());
    }

    #[test]
    fn offered_harnesses_require_an_installed_cli() {
        let descriptor =
            |id: HarnessId, name: &str, enabled: Option<bool>, installed: bool| HarnessDescriptor {
                id,
                name: name.into(),
                supports_steering: true,
                steering_mode: zeron_proto::SteeringMode::StepBoundary,
                reasoning_levels: vec![],
                installed,
                enabled,
            };
        // Enabled-but-missing-CLI agents stay out of the rail; an installed
        // enabled one rides along. A live engine no longer stamps that
        // combination (enablement follows detection), but a catalog from an
        // older engine still can — the filter is the cross-version defense.
        let catalog = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code", Some(true), false),
            descriptor(HarnessId::Codex, "Codex", Some(true), false),
            descriptor(HarnessId::Grok, "Grok", Some(true), true),
        ];
        let offered = offered_harnesses_impl(&catalog, false);
        assert_eq!(
            offered.iter().map(|d| d.id).collect::<Vec<_>>(),
            vec![HarnessId::Grok]
        );
        // Nothing enabled AND installed: an empty offered set — the fresh
        // machine where the default-enabled Claude/Codex have no CLIs (#128).
        // No fallback: offering them again would only manufacture
        // NotInstalled errors at send; the composer shows the no-agents
        // state and blocks new sends instead.
        let catalog = vec![
            descriptor(HarnessId::ClaudeCode, "Claude Code", Some(true), false),
            descriptor(HarnessId::Codex, "Codex", Some(false), false),
            descriptor(HarnessId::Grok, "Grok", Some(false), true),
        ];
        let offered = offered_harnesses_impl(&catalog, false);
        assert!(offered.is_empty());
    }
}
