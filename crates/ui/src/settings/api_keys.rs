//! Settings → Accounts → "API keys": the provider keys zeron holds so the
//! agent CLIs it spawns find the credential they expect in their environment.
//!
//! One row per stored provider — brand tile, name, masked key, the agents that
//! read it, Remove — and an "Add API key" row at the bottom: a provider
//! listbox (a real popover, keyboard navigable; gpui has no native select
//! anyway, and the app's other pickers are built the same way), a
//! password-style field, Save.
//!
//! The section never sees a plaintext key except the one being typed: the
//! engine replies with masked values only ([`zeron_proto::StoredApiKey`]), and
//! the typed key leaves in a single `SetApiKey` call and is cleared from the
//! input immediately afterwards.

use gpui::{
    AnyElement, Context, Entity, FocusHandle, Focusable as _, SharedString, Subscription, Task,
    Window, div, prelude::*, px,
};

use zeron_proto::{ApiKeyProvider, ApiKeysSnapshot};
use zeron_rpc::methods;

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover::{self, Loadable};
use crate::state::AppState;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Pure: the static provider table
// ---------------------------------------------------------------------------

/// Which agents read a provider's key out of the environment. Static, because
/// it describes the CLIs' documented behaviour, not anything zeron observes.
pub fn used_by(provider: ApiKeyProvider) -> &'static [&'static str] {
    match provider {
        ApiKeyProvider::Anthropic => &["Claude Code", "OpenCode", "Pi"],
        ApiKeyProvider::Openai => &["Codex", "OpenCode", "Pi"],
        ApiKeyProvider::Openrouter => &["OpenCode", "Pi"],
        ApiKeyProvider::Deepseek => &["OpenCode", "Pi"],
        ApiKeyProvider::Groq => &["OpenCode", "Pi"],
        ApiKeyProvider::Xai => &["OpenCode", "Pi", "Grok CLI"],
        ApiKeyProvider::Moonshot => &["Kimi", "OpenCode"],
        ApiKeyProvider::Google => &["OpenCode", "Pi"],
        ApiKeyProvider::Mistral => &["OpenCode", "Pi"],
        ApiKeyProvider::Fireworks => &["OpenCode", "Pi"],
        ApiKeyProvider::Together => &["OpenCode", "Pi"],
        ApiKeyProvider::OllamaCompatible => &["OpenCode"],
    }
}

/// "Used by Claude Code, OpenCode, Pi" — the quiet meta line under a row.
pub fn used_by_label(provider: ApiKeyProvider) -> String {
    format!("Used by {}", used_by(provider).join(", "))
}

/// Where the provider hands out keys — opened externally from the hint line.
pub fn key_page_url(provider: ApiKeyProvider) -> &'static str {
    match provider {
        ApiKeyProvider::Anthropic => "https://console.anthropic.com/settings/keys",
        ApiKeyProvider::Openai => "https://platform.openai.com/api-keys",
        ApiKeyProvider::Openrouter => "https://openrouter.ai/keys",
        ApiKeyProvider::Deepseek => "https://platform.deepseek.com/api_keys",
        ApiKeyProvider::Groq => "https://console.groq.com/keys",
        ApiKeyProvider::Xai => "https://console.x.ai",
        ApiKeyProvider::Moonshot => "https://platform.moonshot.ai/console/api-keys",
        ApiKeyProvider::Google => "https://aistudio.google.com/app/apikey",
        ApiKeyProvider::Mistral => "https://console.mistral.ai/api-keys",
        ApiKeyProvider::Fireworks => "https://app.fireworks.ai/settings/users/api-keys",
        ApiKeyProvider::Together => "https://api.together.ai/settings/api-keys",
        ApiKeyProvider::OllamaCompatible => "https://ollama.com/download",
    }
}

/// The brand mark for a provider row. Every provider carries its own vendor
/// mark now (`brand-*` in the icon set, plus the harness marks zeron already
/// embedded), so a row is recognisable before its label is read.
pub fn provider_icon(provider: ApiKeyProvider) -> &'static str {
    match provider {
        ApiKeyProvider::Anthropic => crate::icons::CLAUDE_MARK,
        ApiKeyProvider::Openai => crate::icons::OPENAI_MARK,
        // xAI ships no single-colour mark of its own; the Grok glyph is the
        // company's own artwork and the one its console wears.
        ApiKeyProvider::Xai => crate::icons::GROK_MARK,
        ApiKeyProvider::Moonshot => crate::icons::KIMI_MARK,
        ApiKeyProvider::Openrouter => crate::icons::BRAND_OPENROUTER,
        ApiKeyProvider::Deepseek => crate::icons::BRAND_DEEPSEEK,
        ApiKeyProvider::Groq => crate::icons::BRAND_GROQ,
        ApiKeyProvider::Google => crate::icons::BRAND_GOOGLE_GEMINI,
        ApiKeyProvider::Mistral => crate::icons::BRAND_MISTRAL,
        ApiKeyProvider::Fireworks => crate::icons::BRAND_FIREWORKS,
        ApiKeyProvider::Together => crate::icons::BRAND_TOGETHER,
        ApiKeyProvider::OllamaCompatible => crate::icons::BRAND_OLLAMA,
    }
}

/// Field placeholder — the provider's own prefix reads better than "API key".
pub fn key_placeholder(provider: ApiKeyProvider) -> &'static str {
    match provider {
        ApiKeyProvider::Anthropic => "sk-ant-...",
        ApiKeyProvider::Openrouter => "sk-or-...",
        ApiKeyProvider::Groq => "gsk_...",
        ApiKeyProvider::Xai => "xai-...",
        ApiKeyProvider::Google => "AIza...",
        ApiKeyProvider::OllamaCompatible => "http://127.0.0.1:11434",
        ApiKeyProvider::Openai | ApiKeyProvider::Deepseek | ApiKeyProvider::Moonshot => "sk-...",
        _ => "Paste the key",
    }
}

/// Providers still available to add: every one without a stored key, plus the
/// currently selected one so the trigger never shows something absent from its
/// own list. Pure.
pub fn selectable_providers(
    snapshot: Option<&ApiKeysSnapshot>,
    selected: ApiKeyProvider,
) -> Vec<ApiKeyProvider> {
    ApiKeyProvider::ALL
        .into_iter()
        .filter(|provider| {
            *provider == selected
                || !snapshot.is_some_and(|s| s.keys.iter().any(|key| key.provider == *provider))
        })
        .collect()
}

/// The masked field's stand-in: one bullet per character of the real key, so
/// the length is visible and nothing else is. Pure.
pub fn bullets(len: usize) -> String {
    "\u{2022}".repeat(len.min(48))
}

// ---------------------------------------------------------------------------
// Entity
// ---------------------------------------------------------------------------

struct ProviderMenu {
    active: usize,
    focus: FocusHandle,
}

pub struct ApiKeysSection {
    state: Entity<AppState>,
    snapshot: Loadable<ApiKeysSnapshot>,
    provider_menu: popover::Popup<ProviderMenu>,
    draft_provider: ApiKeyProvider,
    key_input: Entity<ComposerInput>,
    /// The field is masked until the user asks to edit it (click, or the eye).
    /// gpui's text input paints its own glyphs, so masking is a swap of the
    /// rendered element rather than a property of the input: the input keeps
    /// the real text and is simply not on screen while hidden.
    revealed: bool,
    saving: bool,
    removing: Option<ApiKeyProvider>,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    _observe: Subscription,
    _key_events: Subscription,
}

impl ApiKeysSection {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let key_input = cx.new(|cx| {
            ComposerInput::with_context(key_placeholder(ApiKeyProvider::Anthropic), "Composer", cx)
                .with_single_line()
                .with_accessibility_role(gpui::Role::TextInput)
        });
        let key_events = cx.subscribe(&key_input, |this: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => this.save(cx),
            ComposerInputEvent::Edited => {
                this.error = None;
                cx.notify();
            }
            _ => {}
        });
        let mut section = Self {
            state,
            snapshot: Loadable::Idle,
            provider_menu: popover::Popup::default(),
            draft_provider: ApiKeyProvider::Anthropic,
            key_input,
            revealed: false,
            saving: false,
            removing: None,
            error: None,
            load_task: None,
            action_task: None,
            _observe: observe,
            _key_events: key_events,
        };
        section.load(cx);
        section
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load(cx);
    }

    /// Put the caret in the key field — the landing spot for the page header's
    /// "Add API key". Revealing the field first is what makes the focus
    /// visible: the masked stand-in paints bullets, not a caret.
    pub fn focus_key_field(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.revealed = true;
        self.error = None;
        window.focus(&self.key_input.focus_handle(cx), cx);
        cx.notify();
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.snapshot = Loadable::Error("Engine not connected".into());
            return;
        };
        self.snapshot = Loadable::Loading;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_API_KEYS, serde_json::json!({}))
                .await;
            this.update(cx, |section, cx| {
                section.snapshot = match result {
                    Ok(value) => match serde_json::from_value::<ApiKeysSnapshot>(value) {
                        Ok(snapshot) => Loadable::Ready(snapshot),
                        Err(err) => Loadable::Error(err.to_string()),
                    },
                    Err(err) => Loadable::Error(err.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn save(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let key = self.key_input.read(cx).text().trim().to_string();
        if key.is_empty() {
            self.error = Some("Paste a key first.".into());
            cx.notify();
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let provider = self.draft_provider;
        self.saving = true;
        self.error = None;
        let params = serde_json::json!({ "provider": provider, "key": key });
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::SET_API_KEY, params).await;
            this.update(cx, |section, cx| {
                section.saving = false;
                match result {
                    Ok(value) => {
                        // The plaintext key never lingers in the UI.
                        section
                            .key_input
                            .update(cx, |input, cx| input.set_text("", cx));
                        section.revealed = false;
                        if let Ok(snapshot) = serde_json::from_value::<ApiKeysSnapshot>(value) {
                            section.draft_provider =
                                selectable_providers(Some(&snapshot), section.draft_provider)
                                    .into_iter()
                                    .find(|p| *p != section.draft_provider)
                                    .unwrap_or(section.draft_provider);
                            section.snapshot = Loadable::Ready(snapshot);
                        } else {
                            section.load(cx);
                        }
                    }
                    Err(err) => section.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn remove(&mut self, provider: ApiKeyProvider, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.removing = Some(provider);
        self.error = None;
        let params = serde_json::json!({ "provider": provider });
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::REMOVE_API_KEY, params).await;
            this.update(cx, |section, cx| {
                section.removing = None;
                match result {
                    Ok(value) => match serde_json::from_value::<ApiKeysSnapshot>(value) {
                        Ok(snapshot) => section.snapshot = Loadable::Ready(snapshot),
                        Err(_) => section.load(cx),
                    },
                    Err(err) => section.error = Some(format!("{err}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    // ---- provider listbox ----

    fn close_provider_menu(&mut self, cx: &mut Context<Self>) {
        if self.provider_menu.begin_close() {
            popover::reap_popup(cx, |section: &mut Self| &mut section.provider_menu);
            cx.notify();
        }
    }

    fn open_provider_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = selectable_providers(self.snapshot.ready(), self.draft_provider);
        let active = rows
            .iter()
            .position(|p| *p == self.draft_provider)
            .unwrap_or(0);
        let focus = cx.focus_handle();
        self.provider_menu.open(ProviderMenu {
            active,
            focus: focus.clone(),
        });
        window.focus(&focus, cx);
        cx.notify();
    }

    fn pick_provider(&mut self, provider: ApiKeyProvider, cx: &mut Context<Self>) {
        self.close_provider_menu(cx);
        self.draft_provider = provider;
        self.error = None;
        let placeholder = key_placeholder(provider);
        self.key_input
            .update(cx, |input, cx| input.set_placeholder(placeholder, cx));
        cx.notify();
    }

    /// ↑↓ move the highlight, ⏎ picks, esc closes.
    fn provider_menu_key(&mut self, event: &gpui::KeyDownEvent, cx: &mut Context<Self>) {
        if !self.provider_menu.is_open() {
            return;
        }
        let rows = selectable_providers(self.snapshot.ready(), self.draft_provider);
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        match key {
            popover::MenuKey::Escape => {
                self.close_provider_menu(cx);
                cx.stop_propagation();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                if let Some(menu) = self.provider_menu.open_mut() {
                    menu.active =
                        popover::menu_step(Some(menu.active), rows.len(), delta).unwrap_or(0);
                    cx.notify();
                }
                cx.stop_propagation();
            }
            popover::MenuKey::Enter | popover::MenuKey::ModEnter => {
                let active = self.provider_menu.get().map(|m| m.active).unwrap_or(0);
                if let Some(provider) = rows.get(active).copied() {
                    self.pick_provider(provider, cx);
                }
                cx.stop_propagation();
            }
            popover::MenuKey::Backspace | popover::MenuKey::Other => {}
        }
    }

    // ---- render pieces ----

    fn render_stored_row(
        &self,
        key: &zeron_proto::StoredApiKey,
        ix: usize,
        first: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        use crate::settings::widgets;
        let provider = key.provider;
        let busy = self.removing == Some(provider);
        let tint = (provider == ApiKeyProvider::Anthropic).then(crate::icons::claude_brand);
        widgets::card_row(theme, first)
            .child(
                div()
                    .flex_none()
                    .size(px(36.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::ink(0.03))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        crate::icons::icon(provider_icon(provider))
                            .size(px(16.0))
                            .text_color(tint.unwrap_or(theme.text_muted)),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(8.0))
                            .child(widgets::row_title(theme, provider.label()))
                            .child(
                                div()
                                    .flex_none()
                                    .font_family(theme.font_mono.clone())
                                    .text_size(crate::typography::ui_rems(11.5))
                                    .text_color(theme.text_muted.opacity(0.75))
                                    .child(SharedString::from(key.masked.clone())),
                            ),
                    )
                    .child(widgets::meta_line(
                        theme,
                        vec![
                            div()
                                .child(SharedString::from(used_by_label(provider)))
                                .into_any_element(),
                            div()
                                .font_family(theme.font_mono.clone())
                                .child(SharedString::from(key.env_var.clone()))
                                .into_any_element(),
                        ],
                    )),
            )
            // A variable the user exported themselves outranks the stored key;
            // say so instead of pretending this row is in effect.
            .when(!key.applied, |el| {
                el.child(widgets::badge(theme, "Overridden by your environment"))
            })
            .child(
                div()
                    .id(("api-key-remove", ix))
                    .flex_none()
                    .rounded(px(6.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .cursor_pointer()
                    .when(busy, |el| el.opacity(0.5))
                    .hover(|s| s.bg(crate::theme::ink(0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| this.remove(provider, cx)))
                    .child(
                        crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    ),
            )
            .into_any_element()
    }

    fn render_provider_trigger(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let open = self.provider_menu.is_open();
        let rows = selectable_providers(self.snapshot.ready(), self.draft_provider);
        let label: SharedString = self.draft_provider.label().into();
        let mut trigger = div()
            .id("api-key-provider-trigger")
            .flex_none()
            .w(px(184.0))
            .h(px(30.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .bg(if open {
                crate::theme::ink(0.06)
            } else {
                gpui::transparent_black()
            })
            .when(!open, |el| el.hover(|s| s.bg(crate::theme::ink(0.04))))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.provider_menu.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if this.provider_menu.take_press_was_open() {
                    this.close_provider_menu(cx);
                } else {
                    this.open_provider_menu(window, cx);
                }
            }))
            .child(
                crate::icons::icon(provider_icon(self.draft_provider))
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text)
                    .child(label),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(14.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(if open { 0.9 } else { 0.45 })),
            );

        if let Some(menu) = self.provider_menu.get() {
            let active = menu.active;
            let focus = menu.focus.clone();
            let closing = self.provider_menu.closing_since();
            let selected = self.draft_provider;
            let card = popover::popover_card(theme)
                .w(px(248.0))
                .track_focus(&focus)
                .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                    this.provider_menu_key(event, cx);
                }))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| this.close_provider_menu(cx)))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(popover::menu_heading(theme, "Provider"))
                .children(rows.into_iter().enumerate().map(|(ix, provider)| {
                    popover::menu_row_nav(
                        theme,
                        provider == selected,
                        ix == active,
                        format!("api-key-provider-row-{ix}"),
                    )
                    .id(("api-key-provider-row", ix))
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_provider(provider, cx)))
                    .child(
                        crate::icons::icon(provider_icon(provider))
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(provider.label())),
                    )
                }))
                .into_any_element();
            trigger = trigger.child(popover::anchored_menu_below(
                "api-key-provider-menu",
                card,
                closing,
            ));
        }
        trigger.into_any_element()
    }

    /// The password-style field. Masked by default; clicking it (or the eye)
    /// swaps the bullets for the real input and focuses it.
    fn render_key_field(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let length = self.key_input.read(cx).text().chars().count();
        let revealed = self.revealed;
        let input_focus = self.key_input.focus_handle(cx);
        let placeholder = key_placeholder(self.draft_provider);
        let body: AnyElement = if revealed {
            self.key_input.clone().into_any_element()
        } else {
            div()
                .id("api-key-masked")
                .size_full()
                .flex()
                .items_center()
                .cursor_text()
                .font_family(theme.font_mono.clone())
                .text_size(crate::typography::ui_rems(12.5))
                .text_color(if length == 0 {
                    theme.text_muted.opacity(0.5)
                } else {
                    theme.text
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.revealed = true;
                    window.focus(&this.key_input.focus_handle(cx), cx);
                    cx.notify();
                }))
                .child(SharedString::from(if length == 0 {
                    placeholder.to_string()
                } else {
                    bullets(length)
                }))
                .into_any_element()
        };
        div()
            .flex_1()
            .min_w(px(160.0))
            .h(px(30.0))
            .px(px(10.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w_0().child(body))
            .child(
                div()
                    .id("api-key-reveal")
                    .flex_none()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.revealed = !this.revealed;
                        if this.revealed {
                            window.focus(&input_focus, cx);
                        }
                        cx.notify();
                    }))
                    .child(
                        crate::icons::icon(if revealed {
                            crate::icons::EYE_CLOSED
                        } else {
                            crate::icons::EYE
                        })
                        .size(px(14.0))
                        .text_color(theme.text_muted.opacity(0.7)),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ApiKeysSection {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let provider = self.draft_provider;
        let typed = self.key_input.read(cx).text().trim().to_string();
        let hint = zeron_engine::api_keys::shape_hint(provider, &typed);
        let url = key_page_url(provider);
        let saving = self.saving;

        let rows: Vec<AnyElement> = match &self.snapshot {
            Loadable::Ready(snapshot) => {
                let keys = snapshot.keys.clone();
                keys.iter()
                    .enumerate()
                    .map(|(ix, key)| self.render_stored_row(key, ix, ix == 0, &theme, cx))
                    .collect()
            }
            Loadable::Error(message) => vec![
                widgets::error_strip(&theme, message.clone())
                    .id("api-keys-load-error")
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.load(cx)))
                    .into_any_element(),
            ],
            Loadable::Idle | Loadable::Loading => vec![
                div()
                    .px(px(20.0))
                    .py(px(14.0))
                    .child(popover::skeleton_bar(180.0, cx.entity_id(), cx))
                    .into_any_element(),
            ],
        };
        let empty = matches!(&self.snapshot, Loadable::Ready(s) if s.keys.is_empty());

        let add_row = div()
            .px(px(20.0))
            .py(px(14.0))
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(widgets::field_label(&theme, "Add API key"))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(self.render_provider_trigger(&theme, cx))
                    .child(self.render_key_field(&theme, cx))
                    .child(
                        popover::btn_primary(
                            &theme,
                            if saving { "Saving\u{2026}" } else { "Save" },
                        )
                        .id("api-key-save")
                        .flex_none()
                        .when(saving, |el| el.opacity(0.5))
                        .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
            .child(
                div()
                    .id("api-key-provider-hint")
                    .text_size(crate::typography::ui_rems(11.5))
                    .text_color(theme.text_muted.opacity(0.6))
                    .cursor_pointer()
                    .hover(|s| s.text_color(theme.text))
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(url)))
                    .child(SharedString::from(format!("Get a key at {url}"))),
            )
            .when_some(hint, |el, hint| {
                el.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.warning.opacity(0.9))
                        .child(SharedString::from(hint)),
                )
            })
            .when_some(self.error.clone(), |el, message| {
                el.child(
                    div()
                        .text_size(crate::typography::ui_rems(11.5))
                        .text_color(theme.danger_muted.opacity(0.9))
                        .child(message),
                )
            });

        div()
            .mt(px(18.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .size(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                crate::icons::icon(crate::icons::KEY_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .text_size(crate::typography::ui_rems(14.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from("API keys")),
                    ),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .line_height(px(19.0))
                    .text_color(theme.text_muted.opacity(0.6))
                    .child(SharedString::from(
                        "Stored on this device and exported into the environment of every \
                         agent zeron starts, so the CLIs find their provider keys without \
                         a shell profile.",
                    )),
            )
            .child(
                widgets::section_card(&theme)
                    .mt(px(6.0))
                    .map(|card| {
                        if empty {
                            card.child(
                                div()
                                    .px(px(20.0))
                                    .py(px(22.0))
                                    .text_center()
                                    .text_size(crate::typography::ui_rems(13.0))
                                    .text_color(theme.text_muted.opacity(0.6))
                                    .child(SharedString::from("No API keys stored yet.")),
                            )
                        } else {
                            card.children(rows)
                        }
                    })
                    .child(add_row),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AssetSource as _;
    use zeron_proto::StoredApiKey;

    fn stored(provider: ApiKeyProvider) -> StoredApiKey {
        StoredApiKey {
            provider,
            masked: "sk-or-\u{2026}4f2a".into(),
            env_var: "OPENROUTER_API_KEY".into(),
            applied: true,
            updated_at: 0,
        }
    }

    #[test]
    fn every_provider_names_the_agents_that_read_it() {
        for provider in ApiKeyProvider::ALL {
            assert!(!used_by(provider).is_empty(), "{provider:?}");
            assert!(
                key_page_url(provider).starts_with("https://"),
                "{provider:?}"
            );
        }
        assert_eq!(
            used_by_label(ApiKeyProvider::Xai),
            "Used by OpenCode, Pi, Grok CLI"
        );
        assert_eq!(
            used_by_label(ApiKeyProvider::Anthropic),
            "Used by Claude Code, OpenCode, Pi"
        );
        assert_eq!(
            used_by_label(ApiKeyProvider::Moonshot),
            "Used by Kimi, OpenCode"
        );
    }

    #[test]
    fn the_add_list_hides_providers_that_already_have_a_key() {
        let snapshot = ApiKeysSnapshot {
            keys: vec![
                stored(ApiKeyProvider::Openrouter),
                stored(ApiKeyProvider::Groq),
            ],
        };
        let rows = selectable_providers(Some(&snapshot), ApiKeyProvider::Anthropic);
        assert!(!rows.contains(&ApiKeyProvider::Openrouter));
        assert!(!rows.contains(&ApiKeyProvider::Groq));
        assert!(rows.contains(&ApiKeyProvider::Anthropic));
        assert_eq!(rows.len(), ApiKeyProvider::ALL.len() - 2);
        // The selected provider stays listed even once it has a key, so the
        // trigger never shows a row its own list does not contain.
        let rows = selectable_providers(Some(&snapshot), ApiKeyProvider::Groq);
        assert!(rows.contains(&ApiKeyProvider::Groq));
        // Nothing loaded yet: everything is offered.
        assert_eq!(
            selectable_providers(None, ApiKeyProvider::Anthropic).len(),
            ApiKeyProvider::ALL.len()
        );
    }

    #[test]
    fn every_provider_row_wears_its_own_brand_mark() {
        // No generic key/globe fallback: a row is identified by its logo.
        let mut marks = Vec::new();
        for provider in ApiKeyProvider::ALL {
            let mark = provider_icon(provider);
            assert_ne!(mark, crate::icons::KEY_MINIMALISTIC, "{provider:?}");
            assert_ne!(mark, crate::icons::GLOBE, "{provider:?}");
            assert!(
                crate::icons::Assets
                    .load(mark)
                    .expect("asset source")
                    .is_some(),
                "{provider:?} points at an unregistered icon"
            );
            marks.push(mark);
        }
        // Each vendor gets a distinct glyph.
        let mut unique = marks.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), marks.len(), "two providers share a mark");
    }

    #[test]
    fn the_masked_field_shows_length_and_nothing_else() {
        assert_eq!(bullets(0), "");
        assert_eq!(bullets(4), "\u{2022}\u{2022}\u{2022}\u{2022}");
        // A very long key does not stretch the field off the row.
        assert_eq!(bullets(500).chars().count(), 48);
    }

    #[test]
    fn placeholders_follow_the_providers_prefix() {
        assert_eq!(key_placeholder(ApiKeyProvider::Anthropic), "sk-ant-...");
        assert_eq!(key_placeholder(ApiKeyProvider::Groq), "gsk_...");
        assert_eq!(key_placeholder(ApiKeyProvider::Xai), "xai-...");
        assert!(key_placeholder(ApiKeyProvider::OllamaCompatible).starts_with("http"));
    }
}
