//! Settings → Agents / accounts (feature-inventory §1.9): provider sections
//! (Claude Code, Codex, Cursor, Kimi) with account rows — email, plan badge, Active,
//! usage meters (indigo → amber ≥80% → red ≥95%, reset time), Switch / Forget — plus
//! the add-account dialogs (paste-code and browser-poll flows), the stored
//! provider API keys ([`crate::settings::api_keys`]) and
//! account-shaped loading skeletons. Zeron retargets devices from the settings
//! sidebar (`targetDeviceId` passthrough kept plumbed, unused single-device).
//!
//! Every add action lives in ONE page-header menu instead of a button per
//! section: the sections themselves are a quiet list. The same menu owns the
//! page's shape — which sections are shown and in what order
//! (`accountsProviderOrder` / `accountsHiddenProviders` in ui-settings.json).
//! A provider whose CLI is not installed and which has no stored account
//! starts hidden, so an agent the user never signed into cannot spend a whole
//! card on saying so; the moment detection finds a login for one of those, it
//! comes back on its own.
//!
//! The accounts RPC surface is being implemented engine-side in parallel —
//! every call here surfaces failures as inline UI states rather than assuming
//! the methods exist.

use chrono::{DateTime, Utc};
use gpui::{
    AnyElement, Context, Entity, FocusHandle, Hsla, ScrollAnchor, ScrollHandle, SharedString,
    Subscription, Task, Window, div, prelude::*, px,
};
use std::time::Duration;

use zeron_engine::registry::HarnessDescriptor;
use zeron_proto::{
    AgentAccount, AgentAccountsSnapshot, AgentLoginMode, AgentLoginPoll, AgentLoginStart,
    AgentLoginStatus, HarnessId,
};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::settings::{AccountsProvider, AccountsProviderOrder, SavePolicy};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure: usage meters + labels
// ---------------------------------------------------------------------------

pub const USAGE_WARN_FRACTION: f32 = 0.80;
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// < 80% — indigo.
    Normal,
    /// ≥ 80% — amber.
    Warn,
    /// ≥ 95% — red.
    Critical,
}

/// Threshold classification of a usage fraction. Pure.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}

pub fn usage_color(level: UsageLevel, theme: &Theme) -> Hsla {
    match level {
        UsageLevel::Normal => theme.accent,
        UsageLevel::Warn => theme.warning,
        UsageLevel::Critical => theme.danger,
    }
}

/// Why a `ListAgentAccounts` load is happening. Pure input to
/// [`force_usage_for`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadTrigger {
    /// Page construction — the visit's first list.
    Mount,
    /// "Click to retry" after a failed load — still the visit's first
    /// successful list.
    Retry,
    /// The explicit Refresh button.
    Refresh,
    /// After a completed add-account login flow.
    PostLogin,
    /// After Switch/Forget succeeds.
    PostAction,
}

/// Whether a load should ask the engine to probe usage (`forceUsage`). The
/// engine only hits the provider when forced; non-forced lists serve the 60s
/// usage cache or nothing (engine/src/agent_accounts.rs module docs — the
/// design expects the UI to force "on page mount/refresh"). The visit's first
/// list (mount, or retry after a failure) must force, or every first open
/// renders "Usage unavailable" until a manual Refresh — the old app fetched
/// usage on every list. Post-Switch/Forget lists ride the still-warm cache.
pub fn force_usage_for(trigger: LoadTrigger) -> bool {
    match trigger {
        LoadTrigger::Mount | LoadTrigger::Retry | LoadTrigger::Refresh | LoadTrigger::PostLogin => {
            true
        }
        LoadTrigger::PostAction => false,
    }
}

/// Compact absolute reset moment (zeron settings.agents.tsx `formatReset`):
/// a local clock time ("3:45 PM") when it lands within ~22h, a short weekday
/// ("Mon") within a week, else month + day ("Sep 14") — a weekday is noise
/// when the window is a Codex free-tier MONTHLY reset weeks out. The caller
/// prefixes "resets ". Pure given `now`.
pub fn format_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Option<String> {
    use chrono::Local;
    let at = resets_at?;
    let local = at.with_timezone(&Local);
    Some(if at.signed_duration_since(now).num_hours() < 22 {
        format!("resets {}", local.format("%-I:%M %p"))
    } else if at.signed_duration_since(now).num_hours() < 24 * 7 {
        format!("resets {}", local.format("%a"))
    } else {
        format!("resets {}", local.format("%b %-d"))
    })
}

/// The provider cards, in display order: (harness, name, CLI command — named
/// in the empty-state copy, zeron settings.agents.tsx `PROVIDERS`).
pub const PROVIDERS: [(HarnessId, &str, &str); 5] = [
    (HarnessId::ClaudeCode, "Claude Code", "claude"),
    (HarnessId::Codex, "Codex", "codex"),
    (HarnessId::Cursor, "Cursor", "cursor-agent"),
    (HarnessId::Kimi, "Kimi", "kimi"),
    (HarnessId::Cline, "Cline", "cline"),
];

/// The quiet line under an account with no usage meters. Kimi's numbers are
/// readable only while the CLI's short-lived access token is fresh, so an empty
/// meter list is normal there and "unavailable" would read as a failure. Pure.
pub fn no_usage_label(harness: HarnessId, switchable: bool) -> &'static str {
    match (harness, switchable) {
        (HarnessId::Kimi, _) => "Signed in \u{2014} usage shows after a recent Kimi CLI session",
        // Cline bills per token against the signed-in account, but the CLI
        // exposes no rate-limit or balance view zeron could read.
        (HarnessId::Cline, _) => "Signed in \u{2014} the Cline CLI reports no usage",
        (_, true) => "Usage unavailable",
        (_, false) => "Credentials unavailable",
    }
}

/// Accounts of one provider, in the engine's order (slot creation). No
/// active-first re-sort: switching accounts must not move the switched-to
/// card — the Active badge already says which one is live, and a list that
/// reshuffles under the click reads as broken. Pure.
pub fn provider_accounts(
    snapshot: &AgentAccountsSnapshot,
    harness: HarnessId,
) -> Vec<&AgentAccount> {
    snapshot
        .accounts
        .iter()
        .filter(|a| a.harness == harness)
        .collect()
}

// ---------------------------------------------------------------------------
// Pure: section order, visibility, and the Add menu
// ---------------------------------------------------------------------------

/// The harness a section speaks for. The API keys section has none — it holds
/// credentials no CLI ever logged in with. Pure.
pub fn provider_harness(provider: AccountsProvider) -> Option<HarnessId> {
    match provider {
        AccountsProvider::ClaudeCode => Some(HarnessId::ClaudeCode),
        AccountsProvider::Codex => Some(HarnessId::Codex),
        AccountsProvider::Cursor => Some(HarnessId::Cursor),
        AccountsProvider::Kimi => Some(HarnessId::Kimi),
        AccountsProvider::Cline => Some(HarnessId::Cline),
        AccountsProvider::ApiKeys => None,
    }
}

/// The CLI a provider signs in with, named in its empty-state copy. Pure.
pub fn provider_cli(provider: AccountsProvider) -> &'static str {
    let harness = provider_harness(provider);
    PROVIDERS
        .iter()
        .find(|(id, _, _)| Some(*id) == harness)
        .map(|(_, _, cli)| *cli)
        .unwrap_or("")
}

/// One row of the page-header Add menu's first group. The wording names the
/// destination, because the menu is the only place an account can be added
/// from now (zeron's per-section "Add account" is gone). Pure.
pub fn add_menu_label(provider: AccountsProvider) -> &'static str {
    match provider {
        AccountsProvider::ClaudeCode => "Add Claude Code account",
        AccountsProvider::Codex => "Add Codex account",
        AccountsProvider::Cursor => "Add Cursor account",
        // Kimi's token set is bound to the CLI's configuration hash, so the
        // only thing zeron can offer is the CLI's own sign-in.
        AccountsProvider::Kimi => "Sign in with Kimi",
        // Same story as Kimi: `cline auth` keeps one live provider set in its
        // own configuration, so zeron can only offer the CLI's own sign-in.
        AccountsProvider::Cline => "Sign in with Cline",
        AccountsProvider::ApiKeys => "Add API key",
    }
}

/// What detection knows about one section. `cli_installed` is always true for
/// the API keys section: it needs no CLI, so it never hides itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPresence {
    pub provider: AccountsProvider,
    pub cli_installed: bool,
    pub has_accounts: bool,
}

/// Fold the accounts snapshot and the device's harness catalog into one
/// presence row per section. A harness the catalog does not mention at all
/// counts as not installed. Pure.
pub fn presence(
    snapshot: &AgentAccountsSnapshot,
    harnesses: &[HarnessDescriptor],
) -> Vec<ProviderPresence> {
    AccountsProvider::ALL
        .into_iter()
        .map(|provider| {
            let harness = provider_harness(provider);
            ProviderPresence {
                provider,
                cli_installed: match harness {
                    None => true,
                    Some(harness) => harnesses
                        .iter()
                        .find(|d| d.id == harness)
                        .is_some_and(|d| d.installed),
                },
                has_accounts: harness.is_some_and(|harness| {
                    snapshot.accounts.iter().any(|a| a.harness == harness)
                }),
            }
        })
        .collect()
}

/// The sections that start hidden: no CLI on this device AND nothing stored.
/// Anything the user could actually act on stays visible. Pure.
pub fn default_hidden_providers(presence: &[ProviderPresence]) -> Vec<AccountsProvider> {
    presence
        .iter()
        .filter(|row| !row.cli_installed && !row.has_accounts)
        .map(|row| row.provider)
        .collect()
}

/// A section the DEFAULT pass hid, which detection has since found a login
/// for, comes back by itself — once. A section the user hid stays hidden: it
/// is not in `auto_hidden`. Returns the new hidden set, or `None` when
/// nothing changes. Pure.
pub fn auto_revealed(
    hidden: &[AccountsProvider],
    auto_hidden: &[AccountsProvider],
    presence: &[ProviderPresence],
) -> Option<Vec<AccountsProvider>> {
    let reveal: Vec<AccountsProvider> = presence
        .iter()
        .filter(|row| {
            row.has_accounts
                && hidden.contains(&row.provider)
                && auto_hidden.contains(&row.provider)
        })
        .map(|row| row.provider)
        .collect();
    if reveal.is_empty() {
        return None;
    }
    Some(
        hidden
            .iter()
            .copied()
            .filter(|provider| !reveal.contains(provider))
            .collect(),
    )
}

/// The sections to render, in the user's order. A hidden one is GONE — no
/// placeholder, no dimmed card. Pure.
pub fn visible_sections(
    order: &AccountsProviderOrder,
    hidden: &[AccountsProvider],
) -> Vec<AccountsProvider> {
    order
        .0
        .iter()
        .copied()
        .filter(|provider| !hidden.contains(provider))
        .collect()
}

/// Hidden set with one section flipped. Pure.
pub fn toggled_hidden(
    hidden: &[AccountsProvider],
    provider: AccountsProvider,
) -> Vec<AccountsProvider> {
    if hidden.contains(&provider) {
        hidden
            .iter()
            .copied()
            .filter(|candidate| *candidate != provider)
            .collect()
    } else {
        let mut next = hidden.to_vec();
        next.push(provider);
        next
    }
}

/// Move one section up (`-1`) or down (`+1`). `None` at either end, so the
/// caller can dim the control instead of offering a no-op. Hidden sections
/// keep their slot in the sequence, so the move is over the FULL order. Pure.
pub fn moved(
    order: &AccountsProviderOrder,
    provider: AccountsProvider,
    delta: isize,
) -> Option<AccountsProviderOrder> {
    let mut list = order.0.clone();
    let ix = list.iter().position(|candidate| *candidate == provider)?;
    let target = ix as isize + delta;
    if target < 0 || target as usize >= list.len() {
        return None;
    }
    list.swap(ix, target as usize);
    Some(AccountsProviderOrder(list))
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

enum LoginFlow {
    /// StartAgentLogin in flight.
    Starting { harness: HarnessId },
    /// Claude-style: open the URL, paste the code back.
    PasteCode {
        harness: HarnessId,
        start: AgentLoginStart,
        submitting: bool,
        error: Option<SharedString>,
    },
    /// Codex-style: open the URL, poll until the browser flow lands.
    Browser {
        harness: HarnessId,
        start: AgentLoginStart,
        message: Option<SharedString>,
        error: Option<SharedString>,
    },
}

impl LoginFlow {
    /// Dialog title (zeron: "Add Claude account" / "Add Codex account").
    fn title(&self) -> &'static str {
        let harness = match self {
            LoginFlow::Starting { harness }
            | LoginFlow::PasteCode { harness, .. }
            | LoginFlow::Browser { harness, .. } => *harness,
        };
        match harness {
            HarnessId::Codex => "Add Codex account",
            HarnessId::Cursor => "Connect Cursor",
            HarnessId::Kimi => "Sign in with kimi",
            HarnessId::Cline => "Sign in with cline",
            _ => "Add Claude account",
        }
    }
}

/// The open page-header Add menu: one keyboard cursor over both groups (the
/// add actions, then the provider list).
struct AddMenu {
    active: usize,
    focus: FocusHandle,
}

/// A row of the Add menu, in render order — what the keyboard cursor walks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddRow {
    /// Start an add flow for this provider.
    Action(AccountsProvider),
    /// Toggle this section's visibility.
    Provider(AccountsProvider),
}

pub struct AccountsPage {
    state: Entity<AppState>,
    /// Which device's logins are shown; `None` = this device (no passthrough).
    /// Retargeted by the page-header device switcher (zeron parity: the
    /// accounts RPCs are relay-forwardable, CLI logins are per-device).
    target_device: Option<String>,
    device_menu: popover::Popup<()>,
    snapshot: Loadable<AgentAccountsSnapshot>,
    /// Account id with an in-flight Switch/Forget.
    busy_account: Option<String>,
    login: Option<LoginFlow>,
    error: Option<SharedString>,
    code_input: Entity<ComposerInput>,
    /// The stored provider API keys — a section like any other, ordered and
    /// hidden with the rest.
    api_keys: Entity<crate::settings::api_keys::ApiKeysSection>,
    /// The page-header Add menu (add actions + section visibility/order).
    add_menu: popover::Popup<AddMenu>,
    /// The target device's harness catalog — the `installed` probe behind the
    /// default hiding rule. Only ever read for that.
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    /// Sections the DEFAULT pass hid. Only these are revealed automatically
    /// when a login turns up; a section the user hid stays hidden.
    auto_hidden: Vec<AccountsProvider>,
    /// Scroll plumbing for "Add API key", which jumps to the key row.
    page_scroll: ScrollHandle,
    api_keys_anchor: ScrollAnchor,
    load_task: Option<Task<()>>,
    harness_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    poll_task: Option<Task<()>>,
    _observe: Subscription,
    _code_events: Subscription,
}

impl AccountsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let code_input = cx.new(|cx| ComposerInput::new("Paste the authorization code", cx));
        let code_events = cx.subscribe(&code_input, |this: &mut Self, _, event, cx| {
            if matches!(event, ComposerInputEvent::Submitted) {
                this.submit_code(cx);
            }
        });
        let api_keys =
            cx.new(|cx| crate::settings::api_keys::ApiKeysSection::new(state.clone(), cx));
        let page_scroll = ScrollHandle::new();
        let api_keys_anchor = ScrollAnchor::for_handle(page_scroll.clone());
        let mut page = Self {
            state,
            target_device: None,
            device_menu: popover::Popup::default(),
            snapshot: Loadable::Idle,
            busy_account: None,
            login: None,
            error: None,
            code_input,
            api_keys,
            add_menu: popover::Popup::default(),
            harnesses: Loadable::Idle,
            auto_hidden: Vec::new(),
            page_scroll,
            api_keys_anchor,
            load_task: None,
            harness_task: None,
            action_task: None,
            poll_task: None,
            _observe: observe,
            _code_events: code_events,
        };
        // Force the usage probe on the visit's first list — a plain list
        // returns no usage windows on a cold engine cache, which rendered
        // every account as "Usage unavailable" until a manual Refresh. The
        // Loading skeleton (meter ghosts) covers the probe latency, so
        // "Usage unavailable" is reserved for a probe that genuinely failed.
        page.load(force_usage_for(LoadTrigger::Mount), cx);
        page.load_harnesses(cx);
        page
    }

    /// Retarget the page at another device's logins: every accounts RPC is
    /// relay-forwardable, so the whole page — list, usage probes, switch,
    /// forget, login flows — follows the passthrough.
    fn close_device_menu(&mut self, cx: &mut Context<Self>) {
        if self.device_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.device_menu);
            cx.notify();
        }
    }

    fn set_target_device(&mut self, target: Option<String>, cx: &mut Context<Self>) {
        self.close_device_menu(cx);
        if self.target_device == target {
            cx.notify();
            return;
        }
        self.target_device = target;
        // A different device = a different accounts world: drop in-flight
        // login/action state and reload with a forced usage probe (the new
        // device's cache is cold).
        self.login = None;
        self.busy_account = None;
        self.error = None;
        self.load(force_usage_for(LoadTrigger::Mount), cx);
        // Installs are per device, and the default hiding rule reads them.
        self.harnesses = Loadable::Idle;
        self.load_harnesses(cx);
    }

    /// Params with the `targetDeviceId` passthrough merged in.
    fn params(&self, value: serde_json::Value) -> serde_json::Value {
        let mut value = value;
        if let (Some(target), Some(object)) = (&self.target_device, value.as_object_mut()) {
            object.insert("targetDeviceId".into(), serde_json::json!(target));
        }
        value
    }

    /// The page-header device switcher (zeron device-switcher.tsx): a quiet
    /// trigger — platform glyph · name · presence dot · sort glyph — opening a
    /// dropdown of every registered device. Selecting one retargets the page.
    fn render_device_switcher(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::icons::{self, icon};
        let (mut devices, local_id) = {
            let s = self.state.read(cx);
            (s.devices.clone(), s.local_device_id.clone())
        };
        // Stable row order (registration time, then id) — zeron's switcher
        // sorts the same way so rows never reshuffle on heartbeats.
        devices.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let effective = self.target_device.clone().or_else(|| local_id.clone());
        let selected = devices
            .iter()
            .find(|d| Some(d.id.as_str()) == effective.as_deref())
            .cloned();
        let platform_glyph = |platform: &str| match platform {
            "macos" | "darwin" => icons::LAPTOP,
            "ios" | "android" => icons::SMARTPHONE,
            _ => icons::MONITOR,
        };
        let trigger_glyph = platform_glyph(
            selected
                .as_ref()
                .map(|d| d.platform.as_str())
                .unwrap_or("macos"),
        );
        let trigger_label: SharedString = selected
            .as_ref()
            .map(|d| d.name.clone().into())
            .unwrap_or_else(|| SharedString::from("This device"));
        let emerald = theme.success;
        let open = self.device_menu.is_open();

        let mut trigger =
            div()
                .id("accounts-device-switcher")
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .rounded(px(6.0))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .cursor_pointer()
                .bg(if open {
                    crate::theme::ink(0.06)
                } else {
                    gpui::transparent_black()
                })
                .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.04))))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _, _, _| this.device_menu.note_trigger_press()),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    // A press that found the menu open closes it (the card's
                    // mouse-down-out already began the close) — never reopen.
                    if this.device_menu.take_press_was_open() {
                        this.close_device_menu(cx);
                    } else {
                        this.device_menu.open(());
                    }
                    cx.notify();
                }))
                .child(
                    icon(trigger_glyph)
                        .size(px(16.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(crate::typography::ui_rems(12.5))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(trigger_label),
                )
                .child(div().size(px(6.0)).rounded_full().flex_none().bg(
                    if effective == local_id {
                        emerald
                    } else {
                        crate::theme::ink(0.2)
                    },
                ))
                .child(
                    icon(icons::SORT_VERTICAL)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.4 })),
                );

        if self.device_menu.get().is_some() {
            let closing = self.device_menu.closing_since();
            let menu = popover::popover_card(theme)
                .w(px(220.0))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    this.close_device_menu(cx);
                }))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Devices"))
                .children(devices.into_iter().enumerate().map(|(ix, d)| {
                    let is_active = Some(d.id.as_str()) == effective.as_deref();
                    let is_local = local_id.as_deref() == Some(d.id.as_str());
                    let glyph = platform_glyph(&d.platform);
                    let name: SharedString = d.name.clone().into();
                    let pick_local = is_local;
                    let pick_id = d.id.clone();
                    popover::menu_row(theme, is_active, format!("accounts-device-row-{ix}"))
                        .id(("accounts-device-row", ix))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            // Local device = no passthrough (calls stay direct).
                            let target = (!pick_local).then(|| pick_id.clone());
                            this.set_target_device(target, cx);
                        }))
                        .child(
                            icon(glyph)
                                .size(px(16.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child(div().flex_1().min_w_0().truncate().child(name))
                        .when(is_local, |el| {
                            el.child(
                                div()
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(10.5))
                                    .text_color(theme.text_muted.opacity(0.35))
                                    .child(SharedString::from("You")),
                            )
                        })
                        .child(
                            div()
                                .size(px(6.0))
                                .rounded_full()
                                .flex_none()
                                .bg(if is_local {
                                    emerald
                                } else {
                                    crate::theme::ink(0.2)
                                }),
                        )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu(
                "accounts-device-menu",
                menu,
                closing,
            ));
        }
        trigger.into_any_element()
    }

    fn load(&mut self, force_usage: bool, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        let params = self.params(serde_json::json!({ "forceUsage": force_usage }));
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_AGENT_ACCOUNTS, params)
                .await;
            this.update(cx, |page, cx| {
                page.snapshot = match result {
                    Ok(value) => match serde_json::from_value::<AgentAccountsSnapshot>(value) {
                        Ok(snapshot) => Loadable::Ready(snapshot),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                page.apply_default_visibility(cx);
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// `ListHarnesses` against the target device — read ONLY for the
    /// `installed` probe behind the default hiding rule, so a failure is
    /// silent: the page is perfectly usable without it, every section simply
    /// counts as installed until the catalog lands.
    fn load_harnesses(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if matches!(self.harnesses, Loadable::Loading) {
            return;
        }
        self.harnesses = Loadable::Loading;
        let params = self.params(serde_json::json!({}));
        self.harness_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::LIST_HARNESSES, params).await;
            this.update(cx, |page, cx| {
                page.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                page.apply_default_visibility(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    // ---- section order and visibility ----

    fn order(&self, cx: &Context<Self>) -> AccountsProviderOrder {
        crate::settings::current(cx)
            .accounts_provider_order
            .normalized()
    }

    fn hidden(&self, cx: &Context<Self>) -> Vec<AccountsProvider> {
        crate::settings::current(cx)
            .accounts_hidden_providers
            .unwrap_or_default()
    }

    /// Seed the hidden set once from detection, and afterwards let a section
    /// the seeding hid return as soon as a login for it shows up. Needs both
    /// halves of detection (accounts + the installed probe); until then the
    /// page shows everything, which is the honest state of "not known yet".
    fn apply_default_visibility(&mut self, cx: &mut Context<Self>) {
        let presence = match (self.snapshot.ready(), self.harnesses.ready()) {
            (Some(snapshot), Some(harnesses)) => presence(snapshot, harnesses),
            _ => return,
        };
        match crate::settings::current(cx).accounts_hidden_providers {
            None => {
                let hidden = default_hidden_providers(&presence);
                self.auto_hidden = hidden.clone();
                self.write_hidden(hidden, cx);
            }
            Some(hidden) => {
                if let Some(next) = auto_revealed(&hidden, &self.auto_hidden, &presence) {
                    self.auto_hidden.retain(|provider| next.contains(provider));
                    self.write_hidden(next, cx);
                }
            }
        }
    }

    fn write_hidden(&mut self, hidden: Vec<AccountsProvider>, cx: &mut Context<Self>) {
        crate::settings::update(SavePolicy::Debounced, cx, |settings| {
            settings.accounts_hidden_providers = Some(hidden);
        });
        cx.notify();
    }

    /// Show/hide one section from the Add menu. A manual hide takes the
    /// section out of [`Self::auto_hidden`], so detection never overrules it.
    fn toggle_provider(&mut self, provider: AccountsProvider, cx: &mut Context<Self>) {
        let next = toggled_hidden(&self.hidden(cx), provider);
        self.auto_hidden.retain(|candidate| *candidate != provider);
        self.write_hidden(next, cx);
    }

    fn move_provider(&mut self, provider: AccountsProvider, delta: isize, cx: &mut Context<Self>) {
        let Some(next) = moved(&self.order(cx), provider, delta) else {
            return;
        };
        crate::settings::update(SavePolicy::Debounced, cx, |settings| {
            settings.accounts_provider_order = next;
        });
        cx.notify();
    }

    /// Switch / Forget an account.
    fn account_action(
        &mut self,
        method: &'static str,
        account: &AgentAccount,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy_account = Some(account.id.clone());
        self.error = None;
        // Tolerant param shape: both `id` and `accountId` plus the harness.
        let params = self.params(serde_json::json!({
            "id": account.id,
            "accountId": account.id,
            "harness": account.harness,
        }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy_account = None;
                match result {
                    Ok(_) => page.load(force_usage_for(LoadTrigger::PostAction), cx),
                    Err(err) => page.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- add-account flows ----

    fn start_login(&mut self, harness: HarnessId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.login = Some(LoginFlow::Starting { harness });
        self.error = None;
        let params = self.params(serde_json::json!({ "harness": harness }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::START_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<AgentLoginStart>(value)
                        .map_err(|e| zeron_rpc::RpcError::Failed(e.to_string()))
                }) {
                    Ok(start) => {
                        // Kimi's sign-in happens in the CLI's own console; it
                        // hands back no URL to open.
                        if !start.url.is_empty() {
                            cx.open_url(&start.url);
                        }
                        match start.mode {
                            AgentLoginMode::PasteCode => {
                                page.code_input
                                    .update(cx, |input, cx| input.set_text("", cx));
                                page.login = Some(LoginFlow::PasteCode {
                                    harness,
                                    start,
                                    submitting: false,
                                    error: None,
                                });
                            }
                            AgentLoginMode::Browser => {
                                page.login = Some(LoginFlow::Browser {
                                    harness,
                                    start,
                                    message: None,
                                    error: None,
                                });
                                page.spawn_poll(cx);
                            }
                        }
                    }
                    Err(err) => {
                        page.login = None;
                        page.error = Some(format!("Login failed to start: {err}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn submit_code(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::PasteCode {
            start, submitting, ..
        }) = &mut self.login
        else {
            return;
        };
        if *submitting {
            return;
        }
        let code = self.code_input.read(cx).text().trim().to_string();
        if code.is_empty() {
            return;
        }
        let login_id = start.login_id.clone();
        *submitting = true;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id, "code": code }));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::COMPLETE_AGENT_LOGIN, params)
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.login = None;
                        page.load(force_usage_for(LoadTrigger::PostLogin), cx);
                    }
                    Err(err) => {
                        if let Some(LoginFlow::PasteCode {
                            submitting, error, ..
                        }) = &mut page.login
                        {
                            *submitting = false;
                            *error = Some(format!("{err}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// The browser-wait poll loop: PollAgentLogin every 1.5s until Done/Error.
    fn spawn_poll(&mut self, cx: &mut Context<Self>) {
        let Some(LoginFlow::Browser { start, .. }) = &self.login else {
            return;
        };
        let login_id = start.login_id.clone();
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let params = self.params(serde_json::json!({ "loginId": login_id }));
        self.poll_task = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1500))
                    .await;
                let result = engine
                    .client()
                    .call(methods::POLL_AGENT_LOGIN, params.clone())
                    .await;
                let outcome = this.update(cx, |page, cx| {
                    let Some(LoginFlow::Browser { message, error, .. }) = &mut page.login else {
                        return true; // dialog dismissed — stop polling
                    };
                    match result.as_ref().ok().and_then(|value| {
                        serde_json::from_value::<AgentLoginPoll>(value.clone()).ok()
                    }) {
                        Some(poll) => match poll.status {
                            AgentLoginStatus::Done => {
                                page.login = None;
                                page.load(force_usage_for(LoadTrigger::PostLogin), cx);
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Error => {
                                *error = Some(
                                    poll.message
                                        .unwrap_or_else(|| "Login failed".to_string())
                                        .into(),
                                );
                                cx.notify();
                                true
                            }
                            AgentLoginStatus::Pending => {
                                if let Some(text) = poll.message {
                                    *message = Some(text.into());
                                }
                                cx.notify();
                                false
                            }
                        },
                        None => {
                            let text = match &result {
                                Err(err) => format!("Poll failed: {err}"),
                                Ok(_) => "Poll failed: malformed reply".to_string(),
                            };
                            *error = Some(text.into());
                            cx.notify();
                            true
                        }
                    }
                });
                match outcome {
                    Ok(true) | Err(_) => break,
                    Ok(false) => {}
                }
            }
        }));
    }

    fn cancel_login(&mut self, cx: &mut Context<Self>) {
        let login_id = match &self.login {
            Some(LoginFlow::PasteCode { start, .. }) | Some(LoginFlow::Browser { start, .. }) => {
                Some(start.login_id.clone())
            }
            _ => None,
        };
        self.login = None;
        self.poll_task = None;
        if let (Some(login_id), Some(engine)) = (login_id, self.state.read(cx).engine().cloned()) {
            let params = self.params(serde_json::json!({ "loginId": login_id }));
            self.action_task = Some(cx.spawn(async move |_, _| {
                if let Err(err) = engine
                    .client()
                    .call(methods::CANCEL_AGENT_LOGIN, params)
                    .await
                {
                    tracing::debug!(error = %err, "CancelAgentLogin failed (best-effort)");
                }
            }));
        }
        cx.notify();
    }

    // ---- render pieces ----

    /// One usage window (zeron settings.agents.tsx `UsageMeter`): label ·
    /// 5px rounded-full bar (indigo → amber ≥80% → red ≥95%) · "NN% used" ·
    /// quiet reset time.
    fn render_usage_meter(
        &self,
        window: &zeron_proto::AgentUsageWindow,
        theme: &Theme,
        now: DateTime<Utc>,
    ) -> AnyElement {
        let fraction = window.used_fraction.clamp(0.0, 1.0);
        let level = usage_level(fraction);
        let fill = usage_color(level, theme).opacity(match level {
            UsageLevel::Normal => 0.8,
            _ => 0.85,
        });
        let reset = format_reset(window.resets_at, now);
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .text_size(crate::typography::ui_rems(11.5))
            .text_color(theme.text_muted.opacity(0.7))
            .child(
                div()
                    .w(px(48.0))
                    .flex_none()
                    .truncate()
                    .child(SharedString::from(window.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w(px(56.0))
                    .max_w(px(230.0))
                    .h(px(5.0))
                    .rounded_full()
                    .overflow_hidden()
                    .bg(crate::theme::ink(0.07))
                    .when(fraction > 0.0, |el| {
                        el.child(
                            div()
                                .h_full()
                                // A 1.5% floor keeps tiny non-zero usage
                                // visible (zeron `max(used, 1.5)%`).
                                .w(gpui::relative(fraction.max(0.015)))
                                .rounded_full()
                                .bg(fill),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(64.0))
                    .flex_none()
                    .text_right()
                    .child(SharedString::from(format!(
                        "{}% used",
                        (fraction * 100.0).round() as u32
                    ))),
            )
            .when_some(reset, |el, reset| {
                el.child(
                    div()
                        .flex_none()
                        .truncate()
                        .text_color(theme.text_muted.opacity(0.45))
                        .child(SharedString::from(reset)),
                )
            })
            .into_any_element()
    }

    /// One account row (zeron settings.agents.tsx `AccountRow`): initial
    /// avatar, email + usage meters left; badges over the Switch/Forget
    /// actions right-anchored.
    fn render_account_row(
        &self,
        account: &AgentAccount,
        ix: usize,
        first: bool,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let is_busy = self.busy_account.as_deref() == Some(account.id.as_str());
        let email: SharedString = account
            .email
            .clone()
            .or_else(|| account.display_name.clone())
            .unwrap_or_else(|| "Unknown account".into())
            .into();
        let initial: SharedString = email
            .chars()
            .next()
            .map(|c| c.to_uppercase().to_string())
            .unwrap_or_else(|| "?".into())
            .into();
        let switch_account = account.clone();
        let forget_account = account.clone();

        let badges = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .when(account.active, |el| {
                el.child(widgets::badge_active(theme, "Active"))
            })
            .when_some(account.plan_label.clone(), |el, plan| {
                el.child(widgets::badge(theme, plan))
            });

        // Actions only on INACTIVE accounts (zeron `{!account.active && …}`):
        // an icon-only Forget (trash, hover → foreground) then Switch, which
        // reads "Switching…" while the activate round-trips.
        let actions: Option<gpui::Div> = (!account.active).then(|| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id(("account-forget", ix))
                        .rounded(px(6.0))
                        .px(px(6.0))
                        .py(px(4.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .when(is_busy, |el| el.opacity(0.5))
                        .hover(|s| s.bg(crate::theme::ink(0.06)).text_color(theme.text))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(methods::FORGET_AGENT_ACCOUNT, &forget_account, cx);
                        }))
                        .child(
                            crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        ),
                )
                .when(account.switchable, |el| {
                    el.child(
                        crate::popover::btn_primary(
                            theme,
                            if is_busy { "Switching…" } else { "Switch" },
                        )
                        .id(("account-switch", ix))
                        .px(px(8.0))
                        .py(px(4.0))
                        .rounded(px(6.0))
                        .text_size(crate::typography::ui_rems(11.5))
                        .when(is_busy, |el| el.opacity(0.5))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.account_action(
                                methods::ACTIVATE_AGENT_ACCOUNT,
                                &switch_account,
                                cx,
                            );
                        })),
                    )
                })
        });

        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                // Initial avatar: size-8 rounded-full border bg-white/[0.03].
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(crate::typography::ui_rems(12.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text_muted)
                    .child(initial),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(widgets::row_title(theme, email))
                    .map(|el| {
                        // Meters XOR the quiet fallback line — never both
                        // (zeron: `usage ? meters : "Usage unavailable"…`).
                        if account.usage_windows.is_empty() {
                            el.child(
                                div()
                                    .mt(px(6.0))
                                    .truncate()
                                    .text_size(crate::typography::ui_rems(11.5))
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from(no_usage_label(
                                        account.harness,
                                        account.switchable,
                                    ))),
                            )
                        } else {
                            el.child(
                                div().mt(px(6.0)).flex().flex_col().gap(px(4.0)).children(
                                    account
                                        .usage_windows
                                        .iter()
                                        .map(|w| self.render_usage_meter(w, theme, now)),
                                ),
                            )
                        }
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .justify_between()
                    .gap(px(8.0))
                    .child(badges)
                    .children(actions),
            )
            .into_any_element()
    }

    fn render_login_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = Theme::of(cx).clone();
        let red_text = theme.danger_muted.opacity(0.9); // red-300
        let login = self.login.as_ref()?;
        let title = login.title();
        let url_link =
            |id: &'static str, label: &'static str, url: &str, cx: &mut Context<Self>| {
                let open_url = url.to_string();
                // "Reopen the …" text link (zeron: `text-[12px]
                // text-muted-foreground/60 hover:underline`).
                div()
                    .id(id)
                    .mt(px(6.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .truncate()
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.open_url(&open_url);
                    }))
                    .child(SharedString::from(label))
            };
        let body: AnyElement = match login {
            LoginFlow::Starting { .. } => div()
                .mt(px(8.0))
                .child(popover::skeleton_rows(
                    "login-starting",
                    &theme,
                    2,
                    cx.entity_id(),
                    cx,
                ))
                .into_any_element(),
            LoginFlow::PasteCode {
                start,
                submitting,
                error,
                ..
            } => {
                let submitting = *submitting;
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(
                        &theme,
                        "A browser window opened. Sign in to the account you want to add, \
                         approve access, then paste the code Anthropic shows you below. Your \
                         current login is untouched until you switch.",
                    )))
                    .child(url_link(
                        "login-open-url",
                        "Reopen the authorization page",
                        &start.url,
                        cx,
                    ))
                    .child(
                        div().mt(px(12.0)).child(
                            popover::dialog_field(self.code_input.clone().into_any_element())
                                .font_family(theme.font_mono.clone())
                                .text_size(crate::typography::ui_rems(13.0)),
                        ),
                    )
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(8.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div()
                            .mt(px(16.0))
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Cancel", "login-cancel")
                                    .id("login-cancel")
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                            )
                            .child(
                                popover::btn_primary(
                                    &theme,
                                    if submitting {
                                        "Verifying…"
                                    } else {
                                        "Add account"
                                    },
                                )
                                .id("login-submit-code")
                                .when(submitting, |el| el.opacity(0.5))
                                .on_click(cx.listener(|this, _, _, cx| this.submit_code(cx))),
                            ),
                    )
                    .into_any_element()
            }
            LoginFlow::Browser {
                harness,
                start,
                message,
                error,
            } => {
                let has_error = error.is_some();
                let body = match harness {
                    HarnessId::Cline => {
                        "A terminal opened running `cline auth`. Pick a provider there \
                         and finish the sign-in in the browser it opens. Zeron picks the \
                         new login up as soon as the CLI stores it."
                    }
                    HarnessId::Kimi => {
                        "A terminal opened running `kimi login`. Follow the device-code \
                         prompt there — open the page it prints and enter the code it \
                         shows. Zeron picks the new login up as soon as the CLI stores it."
                    }
                    HarnessId::Cursor => {
                        "Finish signing in to Cursor in your browser. This mints a \
                         zeron-named API key you can revoke any time from Cursor's \
                         dashboard — it is separate from `cursor-agent login`."
                    }
                    _ => {
                        "Finish signing in to OpenAI in your browser. The new login is \
                         captured in an isolated profile — your current session is untouched \
                         until you switch."
                    }
                };
                div()
                    .flex()
                    .flex_col()
                    .child(div().mt(px(8.0)).child(popover::dialog_body(&theme, body)))
                    // Kimi's CLI owns its own console and hands back no URL.
                    .when(!start.url.is_empty(), |el| {
                        el.child(url_link(
                            "login-open-url-browser",
                            "Reopen the sign-in page",
                            &start.url,
                            cx,
                        ))
                    })
                    .when(!has_error, |el| {
                        el.child(
                            div()
                                .mt(px(16.0))
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap(px(8.0))
                                .child(crate::loaders::gradient_spinner(
                                    "login-poll",
                                    &theme,
                                    3.0,
                                    cx.entity_id(),
                                    cx,
                                ))
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(12.5))
                                        .text_color(theme.text_muted.opacity(0.7))
                                        .child(message.clone().unwrap_or_else(|| {
                                            SharedString::from("Waiting for the browser…")
                                        })),
                                ),
                        )
                    })
                    .when_some(error.clone(), |el, message| {
                        el.child(
                            div()
                                .mt(px(12.0))
                                .text_size(crate::typography::ui_rems(12.0))
                                .text_color(red_text)
                                .child(message),
                        )
                    })
                    .child(
                        div().mt(px(16.0)).flex().flex_row().justify_end().child(
                            popover::btn_ghost(
                                &theme,
                                if has_error { "Close" } else { "Cancel" },
                                "login-cancel",
                            )
                            .id("login-cancel")
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_login(cx))),
                        ),
                    )
                    .into_any_element()
            }
        };
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(&theme, title))
            .child(body)
            .into_any_element();
        Some(popover::modal("add-account-dialog", viewport, card))
    }

    /// A ghost account row (zeron settings.agents.tsx `SkeletonRow`): avatar,
    /// email line, two usage-meter ghosts, a badge — same geometry as the real
    /// row so loaded data lands without a layout jump. `dim` fades row two.
    fn render_skeleton_row(
        &self,
        _id: (&'static str, usize),
        dim: bool,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::motion;
        let delta = motion::pulse_delta(&motion::ZERON_PULSE, cx.entity_id(), cx);
        let ghost = |w: gpui::Length, h: f32, round_full: bool| {
            div()
                .w(w)
                .h(px(h))
                .flex_none()
                .map(|el| {
                    if round_full {
                        el.rounded_full()
                    } else {
                        el.rounded(px(4.0))
                    }
                })
                .bg(crate::theme::ink(0.05))
        };
        let meters = div()
            .mt(px(8.0))
            .flex()
            .flex_col()
            .gap(px(7.0))
            .children((0..2).map(|_| {
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(ghost(px(48.0).into(), 9.0, false))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(56.0))
                            .max_w(px(230.0))
                            .h(px(5.0))
                            .rounded_full()
                            .bg(crate::theme::ink(0.04)),
                    )
                    .child(ghost(px(64.0).into(), 9.0, false))
            }));
        let inner = div()
            .flex()
            .flex_row()
            .items_stretch()
            .gap(px(12.0))
            .child(
                div()
                    .flex_none()
                    .self_center()
                    .size(px(32.0))
                    .rounded_full()
                    .bg(crate::theme::ink(0.05)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(ghost(px(176.0).into(), 13.0, false).max_w(gpui::relative(0.6)))
                    .child(meters),
            )
            .child(div().flex_none().flex().flex_col().items_end().child(ghost(
                px(64.0).into(),
                21.0,
                true,
            )));
        div()
            .px(px(20.0))
            .py(px(14.0))
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .when(dim, |el| el.opacity(0.6))
            .child(inner.opacity(0.55 + 0.35 * motion::pulse_wave(delta)))
            .into_any_element()
    }
}

/// The brand mark of a section, plus the tint gpui cannot take from the asset
/// (it paints SVGs in the text colour). Pure.
pub fn section_mark(provider: AccountsProvider) -> (&'static str, Option<Hsla>) {
    match provider {
        AccountsProvider::Codex => (crate::icons::OPENAI_MARK, None),
        AccountsProvider::Cursor => (crate::icons::CURSOR_MARK, None),
        AccountsProvider::Kimi => (crate::icons::KIMI_MARK, None),
        AccountsProvider::Cline => (crate::icons::CLINE_MARK, None),
        AccountsProvider::ApiKeys => (crate::icons::KEY_MINIMALISTIC, None),
        AccountsProvider::ClaudeCode => (
            crate::icons::CLAUDE_MARK,
            Some(crate::icons::claude_brand()),
        ),
    }
}

/// Brand mark inside a 24px centered box (zeron: `grid size-6
/// place-items-center [&_svg]:size-4`).
fn provider_mark(provider: AccountsProvider, theme: &Theme, size: f32) -> gpui::Div {
    let (mark, tint) = section_mark(provider);
    div()
        .flex_none()
        .size(px(24.0))
        .flex()
        .items_center()
        .justify_center()
        .child(
            crate::icons::icon(mark)
                .size(px(size))
                .text_color(tint.unwrap_or(theme.text_muted)),
        )
}

/// The vertical rhythm between sections. Tighter than zeron's, because the
/// header rows lost their trailing button: the page reads as one list.
fn section_shell() -> gpui::Div {
    div().mt(px(18.0)).flex().flex_col()
}

fn section_header(provider: AccountsProvider, theme: &Theme) -> gpui::Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .child(provider_mark(provider, theme, 16.0))
        .child(
            div()
                .text_size(crate::typography::ui_rems(14.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(SharedString::from(provider.label())),
        )
}

impl AccountsPage {
    // ---- the page-header Add menu ----

    fn close_add_menu(&mut self, cx: &mut Context<Self>) {
        if self.add_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.add_menu);
            cx.notify();
        }
    }

    fn open_add_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = cx.focus_handle();
        self.add_menu.open(AddMenu {
            active: 0,
            focus: focus.clone(),
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    /// Every row the keyboard cursor walks: the add actions in a fixed order,
    /// then the sections in the user's own order.
    fn add_menu_rows(&self, cx: &Context<Self>) -> Vec<AddRow> {
        let mut rows: Vec<AddRow> = AccountsProvider::ALL.into_iter().map(AddRow::Action).collect();
        rows.extend(self.order(cx).0.into_iter().map(AddRow::Provider));
        rows
    }

    fn activate_add_row(&mut self, row: AddRow, window: &mut Window, cx: &mut Context<Self>) {
        match row {
            AddRow::Action(AccountsProvider::ApiKeys) => {
                self.close_add_menu(cx);
                self.reveal_api_keys(window, cx);
            }
            AddRow::Action(provider) => {
                let Some(harness) = provider_harness(provider) else {
                    return;
                };
                self.close_add_menu(cx);
                // A login that lands in a hidden section would look lost.
                if self.hidden(cx).contains(&provider) {
                    self.toggle_provider(provider, cx);
                }
                self.start_login(harness, cx);
            }
            // Visibility is a series of decisions - the menu stays open.
            AddRow::Provider(provider) => self.toggle_provider(provider, cx),
        }
    }

    /// "Add API key": show the section if it was hidden, scroll it into view,
    /// and put the caret in the key field.
    fn reveal_api_keys(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.hidden(cx).contains(&AccountsProvider::ApiKeys) {
            self.toggle_provider(AccountsProvider::ApiKeys, cx);
        }
        self.api_keys_anchor.scroll_to(window, cx);
        self.api_keys
            .update(cx, |section, cx| section.focus_key_field(window, cx));
        cx.notify();
    }

    /// Arrow keys move the highlight, Enter activates, alt+arrows reorder the
    /// highlighted section, Escape closes.
    fn add_menu_key(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.add_menu.is_open() {
            return;
        }
        let rows = self.add_menu_rows(cx);
        let active = self.add_menu.get().map(|menu| menu.active).unwrap_or(0);
        let raw = event.keystroke.key.as_str();
        if event.keystroke.modifiers.alt && (raw == "up" || raw == "down") {
            if let Some(AddRow::Provider(provider)) = rows.get(active).copied() {
                self.move_provider(provider, if raw == "up" { -1 } else { 1 }, cx);
                // The cursor follows the row it just moved.
                let moved_to = self
                    .order(cx)
                    .0
                    .iter()
                    .position(|candidate| *candidate == provider);
                if let (Some(menu), Some(ix)) = (self.add_menu.open_mut(), moved_to) {
                    menu.active = AccountsProvider::ALL.len() + ix;
                }
                cx.notify();
            }
            cx.stop_propagation();
            return;
        }
        let key = popover::classify_key(
            raw,
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.close_add_menu(cx);
                cx.stop_propagation();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(menu) = self.add_menu.open_mut() {
                    menu.active =
                        popover::menu_step(Some(menu.active), rows.len(), delta).unwrap_or(0);
                    cx.notify();
                }
                cx.stop_propagation();
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                if let Some(row) = rows.get(active).copied() {
                    self.activate_add_row(row, window, cx);
                }
                cx.stop_propagation();
            }
            popover::MenuKey::Backspace | popover::MenuKey::Other => {}
        }
    }

    fn render_add_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::settings::widgets;
        let open = self.add_menu.is_open();
        let mut trigger = widgets::ghost_action(theme)
            .id("accounts-add")
            .flex_none()
            .text_size(crate::typography::ui_rems(12.5))
            .when(open, |el| {
                el.bg(crate::theme::ink(0.06)).text_color(theme.text)
            })
            .when(!open, |el| el.hover(|s| widgets::ghost_hover(theme, s)))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.add_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if this.add_menu.take_press_was_open() {
                    this.close_add_menu(cx);
                } else {
                    this.open_add_menu(window, cx);
                }
            }))
            .child(
                crate::icons::icon(crate::icons::ADD_CIRCLE)
                    .size(px(16.0))
                    .text_color(theme.text_muted),
            )
            .child(SharedString::from("Add"));

        if let Some(menu) = self.add_menu.get() {
            let active = menu.active;
            let focus = menu.focus.clone();
            let closing = self.add_menu.closing_since();
            let order = self.order(cx);
            let hidden = self.hidden(cx);
            let last = order.0.len().saturating_sub(1);
            let actions = AccountsProvider::ALL;
            let card = popover::popover_card(theme)
                .w(px(268.0))
                .track_focus(&focus)
                .on_key_down(
                    cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                        this.add_menu_key(event, window, cx);
                    }),
                )
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_add_menu(cx)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Add"))
                .children(actions.into_iter().enumerate().map(|(ix, provider)| {
                    popover::menu_row_nav(theme, false, ix == active, format!("accounts-add-{ix}"))
                        .id(("accounts-add-action", ix))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.activate_add_row(AddRow::Action(provider), window, cx);
                        }))
                        .child(provider_mark(provider, theme, 14.0))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(SharedString::from(add_menu_label(provider))),
                        )
                }))
                .child(popover::menu_separator())
                .child(popover::menu_heading(theme, "Providers"))
                .children(order.0.iter().copied().enumerate().map(|(ix, provider)| {
                    let shown = !hidden.contains(&provider);
                    let row_ix = actions.len() + ix;
                    let arrow = |glyph: &'static str, delta: isize, enabled: bool, id: usize| {
                        div()
                            .id(("accounts-provider-move", id))
                            .flex_none()
                            .rounded(px(5.0))
                            .px(px(3.0))
                            .py(px(2.0))
                            .when(!enabled, |el| el.opacity(0.25))
                            .when(enabled, |el| {
                                el.cursor_pointer()
                                    .hover(|s| s.bg(crate::theme::ink(0.08)))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        // The row itself toggles visibility.
                                        cx.stop_propagation();
                                        this.move_provider(provider, delta, cx);
                                    }))
                            })
                            .child(
                                crate::icons::icon(glyph)
                                    .size(px(13.0))
                                    .text_color(theme.text_muted),
                            )
                    };
                    popover::menu_row_nav(
                        theme,
                        false,
                        row_ix == active,
                        format!("accounts-provider-{ix}"),
                    )
                    .id(("accounts-provider-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle_provider(provider, cx);
                    }))
                    .child(
                        div()
                            .flex_none()
                            .size(px(14.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .when(shown, |el| {
                                el.child(
                                    crate::icons::icon(crate::icons::CHECK)
                                        .size(px(13.0))
                                        .text_color(theme.text),
                                )
                            }),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(!shown, |el| el.text_color(theme.text_muted.opacity(0.6)))
                            .child(SharedString::from(provider.label())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(2.0))
                            .child(arrow(crate::icons::ALT_ARROW_UP, -1, ix > 0, ix * 2))
                            .child(arrow(
                                crate::icons::ALT_ARROW_DOWN,
                                1,
                                ix < last,
                                ix * 2 + 1,
                            )),
                    )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_below_end(
                "accounts-add-menu",
                card,
                closing,
            ));
        }
        trigger.into_any_element()
    }

    // ---- sections ----

    /// One provider section: brand header, then the account rows card. The
    /// header carries no action any more - everything that adds lives in the
    /// page-header Add menu.
    fn render_provider_section(
        &mut self,
        provider: AccountsProvider,
        harness: HarnessId,
        snapshot: &AgentAccountsSnapshot,
        theme: &Theme,
        now: DateTime<Utc>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let name = provider.label();
        let cli = provider_cli(provider);
        let accounts = provider_accounts(snapshot, harness);
        // EVERY warning renders its own strip (zeron maps them).
        let warnings: Vec<String> = snapshot
            .warnings
            .iter()
            .filter(|w| w.harness == harness)
            .map(|w| w.message.clone())
            .collect();
        let rows: Vec<AnyElement> = accounts
            .iter()
            .enumerate()
            .map(|(ix, account)| self.render_account_row(account, ix, ix == 0, theme, now, cx))
            .collect();
        let empty_copy = match harness {
            // Cursor's app login is SEPARATE from `cursor-agent login` -
            // pointing at the CLI would send users to a sign-in that does not
            // light this up.
            HarnessId::Cursor => format!(
                "{name} isn\u{2019}t connected on this device \u{2014} connect it to run \
                 Cursor sessions."
            ),
            _ => format!(
                "No {name} login detected on this device \u{2014} sign in \
                 with \u{201C}{cli}\u{201D} or add an account."
            ),
        };
        let card = widgets::section_card(theme).mt(px(6.0));
        let card = if rows.is_empty() {
            card.child(
                div()
                    .px(px(20.0))
                    .py(px(28.0))
                    .text_center()
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(empty_copy)),
            )
        } else {
            card.children(rows)
        };
        section_shell()
            .child(section_header(provider, theme))
            .children(
                warnings
                    .into_iter()
                    .map(|warning| widgets::warning_strip(theme, warning)),
            )
            .child(card)
            .into_any_element()
    }

    /// The same section shape with ghost rows, so loaded data lands without a
    /// layout jump.
    fn render_provider_skeleton(
        &mut self,
        provider: AccountsProvider,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let skeleton_id = match provider {
            AccountsProvider::Codex => "accounts-skeleton-codex",
            AccountsProvider::Cursor => "accounts-skeleton-cursor",
            AccountsProvider::Kimi => "accounts-skeleton-kimi",
            AccountsProvider::Cline => "accounts-skeleton-cline",
            _ => "accounts-skeleton-claude",
        };
        section_shell()
            .child(section_header(provider, theme))
            .child(
                widgets::section_card(theme)
                    .mt(px(6.0))
                    .child(self.render_skeleton_row((skeleton_id, 0), false, true, theme, cx))
                    .child(self.render_skeleton_row((skeleton_id, 1), true, false, theme, cx)),
            )
            .into_any_element()
    }
}

impl Render for AccountsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = Utc::now();
        let dialog = self.render_login_dialog(window.viewport_size(), cx);
        let refreshing = matches!(self.snapshot, Loadable::Loading);
        let account_count = self
            .snapshot
            .ready()
            .map(|s| s.accounts.len())
            .filter(|&n| n > 0);
        let order = self.order(cx);
        let hidden = self.hidden(cx);
        let visible = visible_sections(&order, &hidden);

        let mut sections: Vec<AnyElement> = Vec::new();
        if let Loadable::Error(message) = &self.snapshot {
            let message = message.clone();
            sections.push(
                widgets::error_strip(&theme, message)
                    .id("accounts-load-error")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| {
                        // Retry IS the visit's first successful list - force usage.
                        this.load(force_usage_for(LoadTrigger::Retry), cx)
                    }))
                    .child(
                        div()
                            .mt(px(4.0))
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.text_muted)
                            .child(SharedString::from("Click to retry")),
                    )
                    .into_any_element(),
            );
        }
        let snapshot = self.snapshot.ready().cloned();
        let failed = matches!(self.snapshot, Loadable::Error(_));
        for provider in visible.iter().copied() {
            if provider == AccountsProvider::ApiKeys {
                sections.push(
                    div()
                        // Stateful: the anchor lives on the interactivity of
                        // an identified element, like the scroll handle does.
                        .id("accounts-api-keys")
                        .anchor_scroll(Some(self.api_keys_anchor.clone()))
                        .child(self.api_keys.clone())
                        .into_any_element(),
                );
                continue;
            }
            let Some(harness) = provider_harness(provider) else {
                continue;
            };
            let section = match &snapshot {
                Some(snapshot) => {
                    self.render_provider_section(provider, harness, snapshot, &theme, now, cx)
                }
                // A failed list already renders its own strip above.
                None if failed => continue,
                None => self.render_provider_skeleton(provider, &theme, cx),
            };
            sections.push(section);
        }
        if visible.is_empty() {
            sections.push(
                div()
                    .mt(px(24.0))
                    .text_size(crate::typography::ui_rems(13.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(
                        "All providers hidden. Use Add to show them.",
                    ))
                    .into_any_element(),
            );
        }

        div()
            .id("accounts-page")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.page_scroll)
            .child(
                widgets::page_column()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(10.0))
                            .child(widgets::page_header(&theme, "Accounts", account_count))
                            .child(div().flex_1())
                            .child(self.render_add_menu(&theme, cx))
                            .child(
                                // `text-[12.5px]` + leading 16px Refresh icon,
                                // dimmed while a refresh is in flight (zeron
                                // `disabled:opacity-50`).
                                widgets::ghost_action(&theme)
                                    .id("accounts-refresh")
                                    .flex_none()
                                    .text_size(crate::typography::ui_rems(12.5))
                                    .hover(|s| widgets::ghost_hover(&theme, s))
                                    .when(refreshing, |el| el.opacity(0.5))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load(force_usage_for(LoadTrigger::Refresh), cx)
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::REFRESH)
                                            .size(px(16.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(SharedString::from("Refresh")),
                            )
                            .child(self.render_device_switcher(&theme, cx)),
                    )
                    .child(widgets::page_subtitle(
                        &theme,
                        "The Claude Code, Codex, Cursor, Kimi, and Cline logins on this device, plus \
                         the provider API keys zeron passes to every agent it starts. Zeron \
                         detects the live session, keeps each account backed up, and can \
                         swap between them.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("accounts-action-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(sections)
                    // Footer note (zeron: `mt-6 text-[12px] leading-relaxed
                    // text-muted-foreground/60`).
                    .child(
                        div()
                            .mt(px(24.0))
                            .text_size(crate::typography::ui_rems(12.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Switching rewrites the CLI\u{2019}s stored login, so new \
                                 agent sessions use the selected account immediately. On \
                                 macOS, an already-running Claude Code can hold the previous \
                                 login for up to ~30 seconds (Keychain cache).",
                            )),
                    ),
            )
            .when_some(dialog, |el, dialog| el.child(dialog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeDelta;
    use gpui::AssetSource as _;

    #[test]
    fn first_load_of_a_visit_forces_the_usage_probe() {
        // The engine only probes usage when forced (M5c); without forcing on
        // mount, the first Accounts open always rendered "Usage unavailable".
        assert!(force_usage_for(LoadTrigger::Mount));
        // A retry after a failed load is still the visit's first successful
        // list — same requirement.
        assert!(force_usage_for(LoadTrigger::Retry));
        // Explicit refresh and a just-completed login always re-probe.
        assert!(force_usage_for(LoadTrigger::Refresh));
        assert!(force_usage_for(LoadTrigger::PostLogin));
        // Switch/Forget re-lists ride the still-warm 60s cache.
        assert!(!force_usage_for(LoadTrigger::PostAction));
    }

    #[test]
    fn kimi_is_a_read_only_provider_section() {
        // "Credentials unavailable" would read as a failure for a login that
        // is perfectly fine and simply has no fresh token to read usage with.
        assert_eq!(
            no_usage_label(HarnessId::Kimi, false),
            "Signed in \u{2014} usage shows after a recent Kimi CLI session"
        );
        assert_eq!(no_usage_label(HarnessId::Codex, true), "Usage unavailable");
        assert_eq!(
            no_usage_label(HarnessId::ClaudeCode, false),
            "Credentials unavailable"
        );
        // Kimi has a card of its own, after the three swappable providers.
        assert_eq!(PROVIDERS[3].0, HarnessId::Kimi);
        assert_eq!(PROVIDERS[3].2, "kimi");
        // Cline is read-only for the same reason (one live provider set the
        // CLI owns), and sits after Kimi.
        assert_eq!(
            no_usage_label(HarnessId::Cline, false),
            "Signed in \u{2014} the Cline CLI reports no usage"
        );
        assert_eq!(PROVIDERS[4].0, HarnessId::Cline);
        assert_eq!(PROVIDERS[4].2, "cline");
    }

    #[test]
    fn usage_thresholds_match_zeron() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
    }

    #[test]
    fn usage_colors_map_to_theme_accents() {
        let theme = Theme::dark();
        assert_eq!(usage_color(UsageLevel::Normal, &theme), theme.accent);
        assert_eq!(usage_color(UsageLevel::Warn, &theme), theme.warning);
        assert_eq!(usage_color(UsageLevel::Critical, &theme), theme.danger);
    }

    #[test]
    fn reset_formatting_is_absolute() {
        use chrono::Local;
        let now = Utc::now();
        assert_eq!(format_reset(None, now), None);
        // Within ~22h: a local clock time ("resets 3:45 PM").
        let soon = now + TimeDelta::minutes(125);
        assert_eq!(
            format_reset(Some(soon), now),
            Some(format!(
                "resets {}",
                soon.with_timezone(&Local).format("%-I:%M %p")
            ))
        );
        // Within a week: a short weekday ("resets Mon").
        let later = now + TimeDelta::days(3);
        assert_eq!(
            format_reset(Some(later), now),
            Some(format!(
                "resets {}",
                later.with_timezone(&Local).format("%a")
            ))
        );
        // Beyond a week (Codex free tier resets ~monthly): month + day
        // ("resets Sep 14") — a weekday 4 weeks out carries no information.
        let monthly = now + TimeDelta::days(26);
        assert_eq!(
            format_reset(Some(monthly), now),
            Some(format!(
                "resets {}",
                monthly.with_timezone(&Local).format("%b %-d")
            ))
        );
    }

    fn descriptor(id: HarnessId, installed: bool) -> HarnessDescriptor {
        HarnessDescriptor {
            id,
            name: format!("{id:?}"),
            supports_steering: true,
            steering_mode: zeron_proto::SteeringMode::StepBoundary,
            reasoning_levels: vec![],
            installed,
            enabled: None,
        }
    }

    fn login(harness: HarnessId) -> AgentAccount {
        AgentAccount {
            id: format!("{harness:?}-1"),
            harness,
            email: Some("remo@example.com".into()),
            plan_label: None,
            active: true,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        }
    }

    #[test]
    fn a_section_with_no_cli_and_no_login_starts_hidden() {
        // The complaint this rule answers: a big empty "Cursor isn't
        // connected" card on a device that never had cursor-agent.
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![login(HarnessId::ClaudeCode), login(HarnessId::Codex)],
            warnings: vec![],
        };
        let harnesses = vec![
            descriptor(HarnessId::ClaudeCode, true),
            descriptor(HarnessId::Codex, true),
            descriptor(HarnessId::Cursor, false),
            descriptor(HarnessId::Kimi, true),
            descriptor(HarnessId::Cline, true),
        ];
        let rows = presence(&snapshot, &harnesses);
        assert_eq!(
            default_hidden_providers(&rows),
            vec![AccountsProvider::Cursor]
        );
        // API keys need no CLI, so they are never hidden by the default pass.
        assert!(rows.iter().all(|row| row.provider != AccountsProvider::ApiKeys
            || (row.cli_installed && !row.has_accounts)));
        // An installed CLI with no login keeps its section: the user can act
        // on it. A missing CLI with a stored login keeps its section too.
        let harnesses = vec![
            descriptor(HarnessId::ClaudeCode, false),
            descriptor(HarnessId::Codex, true),
            descriptor(HarnessId::Cursor, true),
            descriptor(HarnessId::Kimi, false),
            descriptor(HarnessId::Cline, true),
        ];
        let rows = presence(&snapshot, &harnesses);
        assert_eq!(default_hidden_providers(&rows), vec![AccountsProvider::Kimi]);
    }

    #[test]
    fn a_detected_login_reveals_an_auto_hidden_section_but_not_a_user_hidden_one() {
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![login(HarnessId::Cursor), login(HarnessId::Kimi)],
            warnings: vec![],
        };
        let harnesses = vec![
            descriptor(HarnessId::ClaudeCode, true),
            descriptor(HarnessId::Codex, true),
            descriptor(HarnessId::Cursor, false),
            descriptor(HarnessId::Kimi, false),
        ];
        let rows = presence(&snapshot, &harnesses);
        let hidden = vec![AccountsProvider::Cursor, AccountsProvider::Kimi];
        // Cursor was hidden by the default pass, Kimi by the user: only
        // Cursor comes back when a login turns up.
        let auto_hidden = vec![AccountsProvider::Cursor];
        assert_eq!(
            auto_revealed(&hidden, &auto_hidden, &rows),
            Some(vec![AccountsProvider::Kimi])
        );
        // Nothing to reveal a second time: the section is no longer hidden.
        assert_eq!(
            auto_revealed(&[AccountsProvider::Kimi], &auto_hidden, &rows),
            None
        );
        // Without a detected login nothing moves on its own.
        let empty = AgentAccountsSnapshot::default();
        assert_eq!(
            auto_revealed(&hidden, &auto_hidden, &presence(&empty, &harnesses)),
            None
        );
    }

    #[test]
    fn hidden_sections_are_gone_but_keep_their_slot_in_the_order() {
        let order = AccountsProviderOrder::default();
        let hidden = vec![AccountsProvider::Cursor];
        assert_eq!(
            visible_sections(&order, &hidden),
            vec![
                AccountsProvider::ClaudeCode,
                AccountsProvider::Codex,
                AccountsProvider::Kimi,
                AccountsProvider::Cline,
                AccountsProvider::ApiKeys,
            ]
        );
        // Showing it again restores the position, not the end of the list.
        let shown = toggled_hidden(&hidden, AccountsProvider::Cursor);
        assert!(shown.is_empty());
        assert_eq!(visible_sections(&order, &shown), order.0);
        assert_eq!(
            toggled_hidden(&shown, AccountsProvider::ApiKeys),
            vec![AccountsProvider::ApiKeys]
        );
        // Everything hidden is a legal state (the page says so in one line).
        let all: Vec<AccountsProvider> = AccountsProvider::ALL.to_vec();
        assert!(visible_sections(&order, &all).is_empty());
    }

    #[test]
    fn reordering_moves_one_section_and_stops_at_the_ends() {
        let order = AccountsProviderOrder::default();
        let moved_down = moved(&order, AccountsProvider::ClaudeCode, 1).unwrap();
        assert_eq!(
            moved_down.0,
            vec![
                AccountsProvider::Codex,
                AccountsProvider::ClaudeCode,
                AccountsProvider::Cursor,
                AccountsProvider::Kimi,
                AccountsProvider::Cline,
                AccountsProvider::ApiKeys,
            ]
        );
        assert!(moved(&order, AccountsProvider::ClaudeCode, -1).is_none());
        assert!(moved(&order, AccountsProvider::ApiKeys, 1).is_none());
        // A hidden section still occupies a slot, so moving over it works.
        let up = moved(&order, AccountsProvider::ApiKeys, -1).unwrap();
        assert_eq!(up.0.last(), Some(&AccountsProvider::Cline));
    }

    #[test]
    fn the_order_and_the_hidden_set_persist_in_ui_settings() {
        use crate::settings::UiSettings;
        let settings = UiSettings {
            accounts_provider_order: AccountsProviderOrder(vec![
                AccountsProvider::ApiKeys,
                AccountsProvider::Codex,
                AccountsProvider::ClaudeCode,
                AccountsProvider::Kimi,
                AccountsProvider::Cline,
                AccountsProvider::Cursor,
            ]),
            accounts_hidden_providers: Some(vec![AccountsProvider::Cursor]),
            ..Default::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        assert_eq!(
            value["accountsProviderOrder"],
            serde_json::json!(["apiKeys", "codex", "claudeCode", "kimi", "cline", "cursor"])
        );
        assert_eq!(
            value["accountsHiddenProviders"],
            serde_json::json!(["cursor"])
        );
        let restored: UiSettings = serde_json::from_value(value).unwrap();
        assert_eq!(restored, settings);

        // A file written before the setting existed keeps the canonical order
        // and leaves visibility undecided, so detection may still seed it.
        let old: UiSettings = serde_json::from_str(r#"{"sidebarWidth":300}"#).unwrap();
        assert_eq!(old.accounts_provider_order, AccountsProviderOrder::default());
        assert_eq!(old.accounts_hidden_providers, None);
        // A hand-edited file that names a section twice, or forgets one,
        // still renders every section exactly once.
        let patchy: UiSettings = serde_json::from_str(
            r#"{"accountsProviderOrder":["kimi","kimi","apiKeys"]}"#,
        )
        .unwrap();
        assert_eq!(
            patchy.clamped().accounts_provider_order.0,
            vec![
                AccountsProvider::Kimi,
                AccountsProvider::ApiKeys,
                AccountsProvider::ClaudeCode,
                AccountsProvider::Codex,
                AccountsProvider::Cursor,
                AccountsProvider::Cline,
            ]
        );
    }

    #[test]
    fn the_add_menu_names_its_destination_for_every_section() {
        // One action per section, each naming where it lands - the sections
        // carry no Add button of their own any more.
        assert_eq!(
            add_menu_label(AccountsProvider::ClaudeCode),
            "Add Claude Code account"
        );
        assert_eq!(add_menu_label(AccountsProvider::Codex), "Add Codex account");
        assert_eq!(
            add_menu_label(AccountsProvider::Cursor),
            "Add Cursor account"
        );
        assert_eq!(add_menu_label(AccountsProvider::Kimi), "Sign in with Kimi");
        assert_eq!(
            add_menu_label(AccountsProvider::Cline),
            "Sign in with Cline"
        );
        assert_eq!(add_menu_label(AccountsProvider::ApiKeys), "Add API key");
        for provider in AccountsProvider::ALL {
            // No emoji, no trailing punctuation, and a real brand mark.
            assert!(add_menu_label(provider).is_ascii());
            assert!(crate::icons::Assets
                .load(section_mark(provider).0)
                .unwrap()
                .is_some());
        }
        // Every CLI section maps to the harness it signs in, and names the
        // command its empty state points at.
        assert_eq!(
            provider_harness(AccountsProvider::Cursor),
            Some(HarnessId::Cursor)
        );
        assert_eq!(provider_harness(AccountsProvider::ApiKeys), None);
        assert_eq!(provider_cli(AccountsProvider::Kimi), "kimi");
        assert_eq!(provider_cli(AccountsProvider::Cline), "cline");
        assert_eq!(provider_cli(AccountsProvider::Cursor), "cursor-agent");
    }

    #[test]
    fn provider_grouping_keeps_engine_order_even_when_active_is_later() {
        let account = |id: &str, harness: HarnessId, active: bool| AgentAccount {
            id: id.into(),
            harness,
            email: None,
            plan_label: None,
            active,
            usage_windows: vec![],
            display_name: None,
            organization: None,
            auth_kind: None,
            switchable: true,
            saved_at: None,
        };
        let snapshot = AgentAccountsSnapshot {
            accounts: vec![
                account("c1", HarnessId::ClaudeCode, false),
                account("x1", HarnessId::Codex, false),
                account("c2", HarnessId::ClaudeCode, true),
            ],
            warnings: vec![],
        };
        let claude = provider_accounts(&snapshot, HarnessId::ClaudeCode);
        let ids: Vec<&str> = claude.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["c1", "c2"],
            "engine (creation) order holds — switching must not move a card"
        );
        assert_eq!(provider_accounts(&snapshot, HarnessId::Codex).len(), 1);
        assert!(provider_accounts(&snapshot, HarnessId::Cursor).is_empty());
    }
}
