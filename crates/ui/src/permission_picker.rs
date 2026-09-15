//! The composer footer's "Permissions" dropdown: how much the agent may do
//! without asking, plus the sandbox level the harnesses that understand one
//! should run in.
//!
//! It is a sibling of the checkout/branch pickers rather than part of them —
//! its state is not the draft's run target but a per-chat preference that
//! outlives the draft, and it must render on non-git projects where the
//! checkout row does not exist at all.
//!
//! Persistence mirrors the harness/model picks ([`PermissionDefaults`]): the
//! last pick becomes the default for the next NEW chat, and a chat that was
//! changed keeps its own pick forever. The composer reads [`Self::choice`] at
//! send time and stamps it onto the run request.

use std::path::PathBuf;

use gpui::prelude::*;
use gpui::{AnyElement, App, Context, Entity, FocusHandle, KeyDownEvent, SharedString, div, px};

use zeron_proto::{HarnessId, PermissionMode, SandboxLevel};

use crate::motion;
use crate::popover::{self, Popup};
use crate::settings::composer::{PermissionChoice, PermissionDefaults};
use crate::state::AppState;
use crate::theme::Theme;

/// Width of the dropdown card. Wide enough for the two-line Auto row without
/// the hint wrapping into three.
const POPOVER_WIDTH: f32 = 300.0;

/// The permission modes in menu order: popover label, the shorter chip
/// label, and the row's one-line hint. `Bypass`'s hint is the warning the
/// popover is required to show, and the only one rendered in the danger
/// color.
const MODES: [(PermissionMode, &str, &str, &str); 4] = [
    (
        PermissionMode::Ask,
        "Ask",
        "Ask",
        "The agent asks before it runs a tool.",
    ),
    (
        PermissionMode::AutoEdits,
        "Auto edits",
        "Auto edits",
        "File edits apply without asking.",
    ),
    (
        PermissionMode::Auto,
        "Auto",
        "Auto",
        "The agent approves routine actions itself.",
    ),
    (
        PermissionMode::Bypass,
        "Bypass permissions",
        "Bypass",
        "The agent runs without asking. Use only in projects you trust.",
    ),
];

/// The sandbox levels in menu order, named for what they let the agent touch
/// rather than for the wire value behind them.
const SANDBOXES: [(SandboxLevel, &str); 3] = [
    (SandboxLevel::ReadOnly, "Read only"),
    (SandboxLevel::WorkspaceWrite, "Project folder only"),
    (SandboxLevel::DangerFullAccess, "Whole system"),
];

/// Height of one option row. Fixed, like the Settings sidebar's nav rows, so
/// a row's highlight is always the same box and two highlights can never
/// touch (the [`ROW_GAP`] between them stays empty).
const ROW_HEIGHT: f32 = 30.0;

/// Vertical gap between option rows.
const ROW_GAP: f32 = 4.0;

/// Height reserved for the group's shared hint line. Two lines' worth plus
/// air, so the popover does not resize as the selection moves between a
/// short hint and the two-line Bypass warning, and the warning never crowds
/// whatever follows it.
const HINT_HEIGHT: f32 = 40.0;

/// The chip's (short) label for a mode.
pub fn mode_label(mode: PermissionMode) -> &'static str {
    MODES
        .iter()
        .find(|(m, _, _, _)| *m == mode)
        .map(|(_, _, chip, _)| *chip)
        .unwrap_or("Ask")
}

/// The popover row's (full) label for a mode.
pub fn mode_row_label(mode: PermissionMode) -> &'static str {
    MODES
        .iter()
        .find(|(m, _, _, _)| *m == mode)
        .map(|(_, label, _, _)| *label)
        .unwrap_or("Ask")
}

/// The one-line hint shown under the group for the selected mode.
pub fn mode_hint(mode: PermissionMode) -> &'static str {
    MODES
        .iter()
        .find(|(m, _, _, _)| *m == mode)
        .map(|(_, _, _, hint)| *hint)
        .unwrap_or("")
}

/// Whether a harness reads [`SandboxLevel`] at all. Only the Codex adapter
/// does (`approvalPolicy` + `sandboxPolicy` on every turn); Claude Code has
/// no sandbox flag and the ACP adapters never send one, so showing them a
/// sandbox control would promise something that does not happen. An unknown
/// harness (catalog still loading on the new-chat canvas) shows nothing
/// rather than a section that may vanish a frame later.
pub fn honors_sandbox(harness: Option<HarnessId>) -> bool {
    harness == Some(HarnessId::Codex)
}

pub struct PermissionPicker {
    state: Entity<AppState>,
    /// Where [`Self::defaults`] persists; `None` = no data dir, picks are
    /// session-only rather than lost loudly.
    data_dir: Option<PathBuf>,
    defaults: PermissionDefaults,
    open: Popup<()>,
    focus: FocusHandle,
    /// In-flight `SetPermissionMode` call; a newer pick replaces it.
    apply_task: Option<gpui::Task<()>>,
}

impl PermissionPicker {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let data_dir = state.read(cx).data_dir.clone();
        let defaults = data_dir
            .as_deref()
            .map(PermissionDefaults::load)
            .unwrap_or_default();
        Self {
            state,
            data_dir,
            defaults,
            open: Popup::default(),
            focus: cx.focus_handle(),
            apply_task: None,
        }
    }

    /// The selected chat's id, or `None` on the new-chat canvas.
    fn chat_id(&self, cx: &App) -> Option<String> {
        self.state.read(cx).selected_chat.clone()
    }

    /// The pick in force for whatever the composer is about to send.
    pub fn choice(&self, cx: &App) -> PermissionChoice {
        self.defaults.permission_for(self.chat_id(cx).as_deref())
    }

    /// Pin the new-chat canvas's pick to the chat id the composer just
    /// minted, so the chat keeps what its first message was sent with.
    pub fn adopt_new_chat(&mut self, chat_id: &str) {
        self.defaults.adopt_permission(chat_id);
        self.save();
    }

    fn save(&self) {
        if let Some(dir) = self.data_dir.as_deref()
            && let Err(err) = self.defaults.save(dir)
        {
            tracing::warn!(error = %err, "composer-permissions save failed");
        }
    }

    fn set_choice(&mut self, choice: PermissionChoice, cx: &mut Context<Self>) {
        let chat_id = self.chat_id(cx);
        let changed = self.choice(cx).mode != choice.mode;
        self.defaults
            .remember_permission(chat_id.as_deref(), choice);
        self.save();
        // Apply it to a run that is ALREADY going, rather than only to the
        // next one: the engine pushes the mode into the live harness and
        // clears any tool-permission question the new mode already answers.
        if changed && let Some(chat_id) = chat_id {
            self.apply_live(chat_id, choice.mode, cx);
        }
        cx.notify();
    }

    /// Fire-and-forget `SetPermissionMode` for the chat's live run. A chat
    /// with no live run replies `applied: false` and needs nothing else —
    /// its next run carries the pick in the request.
    fn apply_live(&mut self, chat_id: String, mode: PermissionMode, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.apply_task = Some(cx.spawn(async move |_, _| {
            let params = serde_json::json!({
                "chatId": chat_id,
                "permission": mode,
            });
            if let Err(err) = engine
                .client()
                .call(zeron_rpc::methods::SET_PERMISSION_MODE, params)
                .await
            {
                tracing::warn!(error = %err, "SetPermissionMode failed");
            }
        }));
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        // The card's `on_mouse_down_out` already began the close on this same
        // press, so a plain toggle would close-and-reopen (see
        // [`Popup::note_trigger_press`]).
        if self.open.take_press_was_open() {
            return;
        }
        if self.open.is_open() {
            self.dismiss(cx);
        } else {
            self.open.open(());
            cx.notify();
        }
    }

    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.open.begin_close() {
            popover::reap_popup(cx, |this: &mut Self| &mut this.open);
            cx.notify();
        }
    }

    fn on_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if event.keystroke.key == "escape" {
            self.dismiss(cx);
            if self.focus.contains_focused(window, cx) {
                window.blur();
            }
        }
    }

    /// The footer chip plus, while open, its anchored dropdown. `harness` is
    /// the run's resolved harness (the composer owns that resolution) and
    /// decides whether the sandbox section has any meaning here.
    pub fn render_chip(
        &mut self,
        harness: Option<HarnessId>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let choice = self.choice(cx);
        let open = self.open.is_open();
        // The two unattended modes are the ones worth noticing from across
        // the room; Ask and Auto edits stay quiet like the neighbouring
        // checkout labels.
        let accented = matches!(choice.mode, PermissionMode::Auto | PermissionMode::Bypass);
        let id = "permission-chip";
        let text = if accented {
            theme.accent
        } else {
            motion::hover_blend(id, theme.text_muted.opacity(0.7), theme.text.opacity(0.8))
        };
        let chip = div()
            .id(id)
            // The dropdown is anchored to THIS box, not to whatever row the
            // chip happens to sit in (it sits at the row's trailing edge).
            .relative()
            .h(px(20.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .rounded(px(6.0))
            .text_size(crate::typography::ui_rems(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(text)
            .bg(if open {
                theme.element_hover
            } else {
                motion::hover_blend(id, gpui::transparent_black(), theme.element_hover)
            })
            .on_hover(motion::hover_listener(id))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| this.open.note_trigger_press()),
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
            .child(
                crate::icons::icon(crate::icons::SHIELD_CHECK)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(if accented {
                        theme.accent
                    } else {
                        theme.text_muted.opacity(0.7)
                    }),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(mode_label(choice.mode))),
            )
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
                    .size(px(12.0))
                    .flex_none()
                    .text_color(theme.text_muted.opacity(0.5)),
            );
        if self.open.get().is_none() {
            return chip.into_any_element();
        }
        let closing = self.open.closing_since();
        let content = self.render_popover(choice, harness, &theme, cx);
        chip.child(popover::anchored_menu_above(
            "permission-popover",
            content,
            closing,
        ))
        .into_any_element()
    }

    /// One option row, styled exactly like the Settings sidebar's nav rows:
    /// a FIXED height, its own rounded highlight with inner padding, and a
    /// gap to the next row (supplied by the group) so two highlights never
    /// touch. Nothing inside wraps.
    fn option_row(
        id: SharedString,
        label: &'static str,
        selected: bool,
        theme: &Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .h(px(ROW_HEIGHT))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .rounded(px(8.0))
            .px(px(Theme::SPACE_SM))
            .text_size(crate::typography::ui_rems(13.0))
            .when(selected, |el| {
                el.bg(crate::theme::glass_selected_bg())
                    .font_weight(gpui::FontWeight::MEDIUM)
            })
            .text_color(if selected {
                theme.text
            } else {
                theme.text_muted
            })
            .cursor_pointer()
            .hover(|s| s.bg(theme.glass_hover()).text_color(theme.text))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(label)),
            )
            .when(selected, |row| {
                row.child(
                    crate::icons::icon(crate::icons::CHECK)
                        .size(px(13.0))
                        .flex_none()
                        .text_color(theme.accent),
                )
            })
    }

    /// The group's shared hint line: one sentence about the CURRENT pick, in
    /// a box tall enough for two lines so the popover never resizes as the
    /// selection moves. The Bypass warning is the only one in danger red.
    fn hint_line(text: &'static str, danger: bool, theme: &Theme) -> gpui::Div {
        div()
            .h(px(HINT_HEIGHT))
            .px(px(Theme::SPACE_SM))
            .pt(px(4.0))
            .text_size(crate::typography::ui_rems(11.0))
            .text_color(if danger {
                theme.danger.opacity(0.9)
            } else {
                theme.text_muted.opacity(0.75)
            })
            .child(SharedString::from(text))
    }

    fn render_popover(
        &mut self,
        choice: PermissionChoice,
        harness: Option<HarnessId>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut modes = div().flex().flex_col().gap(px(ROW_GAP));
        for (mode, label, _chip, _hint) in MODES {
            let selected = choice.mode == mode;
            let id = SharedString::from(format!("permission-mode-{label}"));
            modes = modes.child(Self::option_row(id, label, selected, theme).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.set_choice(
                        PermissionChoice {
                            mode,
                            ..this.choice(cx)
                        },
                        cx,
                    );
                }),
            ));
        }

        let mut card = popover::popover_card(theme)
            .w(px(POPOVER_WIDTH))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.on_key_down(event, window, cx)
            }))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                this.dismiss(cx);
                if this.focus.contains_focused(window, cx) {
                    window.blur();
                }
            }))
            .flex()
            .flex_col()
            .child(popover::menu_heading(theme, "Permissions"))
            .child(modes)
            .child(Self::hint_line(
                mode_hint(choice.mode),
                choice.mode == PermissionMode::Bypass,
                theme,
            ));

        // No sandbox section for a harness that ignores the level: the
        // popover simply ends after the permissions list.
        if !honors_sandbox(harness) {
            return card.into_any_element();
        }

        let mut sandboxes = div().flex().flex_col().gap(px(ROW_GAP));
        for (level, label) in SANDBOXES {
            let selected = choice.sandbox == level;
            let id = SharedString::from(format!("permission-sandbox-{label}"));
            sandboxes = sandboxes.child(Self::option_row(id, label, selected, theme).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.set_choice(
                        PermissionChoice {
                            sandbox: level,
                            ..this.choice(cx)
                        },
                        cx,
                    );
                }),
            ));
        }
        card = card
            .child(popover::menu_separator())
            .child(popover::menu_heading(theme, "Sandbox (Codex)"))
            .child(
                div()
                    .px(px(Theme::SPACE_SM))
                    .pb(px(6.0))
                    .text_size(crate::typography::ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.75))
                    .child(SharedString::from(
                        "Limits what the agent may touch on disk.",
                    )),
            )
            .child(sandboxes);
        card.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_has_a_chip_and_a_row_label() {
        assert_eq!(mode_label(PermissionMode::Ask), "Ask");
        assert_eq!(mode_label(PermissionMode::AutoEdits), "Auto edits");
        assert_eq!(mode_label(PermissionMode::Auto), "Auto");
        // The chip stays short; the popover row spells it out.
        assert_eq!(mode_label(PermissionMode::Bypass), "Bypass");
        assert_eq!(mode_row_label(PermissionMode::Bypass), "Bypass permissions");
        assert_eq!(mode_row_label(PermissionMode::Auto), "Auto");
    }

    #[test]
    fn bypass_carries_the_trust_warning_and_auto_does_not() {
        assert_eq!(
            mode_hint(PermissionMode::Bypass),
            "The agent runs without asking. Use only in projects you trust."
        );
        assert_eq!(
            mode_hint(PermissionMode::Auto),
            "The agent approves routine actions itself."
        );
        // Every mode has a hint, so the shared line is never blank.
        for (mode, _, _, _) in MODES {
            assert!(!mode_hint(mode).is_empty());
        }
    }

    #[test]
    fn only_codex_shows_the_sandbox_section() {
        assert!(honors_sandbox(Some(HarnessId::Codex)));
        // Claude Code has no sandbox flag; the ACP adapters never send one.
        assert!(!honors_sandbox(Some(HarnessId::ClaudeCode)));
        assert!(!honors_sandbox(Some(HarnessId::Kimi)));
        assert!(!honors_sandbox(Some(HarnessId::Opencode)));
        // Unknown (catalog still loading) shows nothing rather than a
        // section that would vanish a frame later.
        assert!(!honors_sandbox(None));
    }
}
