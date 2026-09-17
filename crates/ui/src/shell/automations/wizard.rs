//! The "New automation" wizard: three steps that introduce the agent before
//! they ask for work — the face (shape and colour), the name and role, then the
//! task itself. Editing an existing automation opens on step 3 and offers an
//! "Edit" link back to step 2.
//!
//! The dialog owns no validation of its own: [`super::editor`] keeps the form
//! state, the [`Check`], the dropdowns and `build_spec`, and this module only
//! lays the steps out. Motion is the repo's rise-and-fade plus the avatars'
//! shared clock ([`crate::agent_avatar::AvatarMotion`]), pumped through the
//! bounded pulse lease while the dialog is open.

use super::{
    AutomationsPage,
    editor::{
        Check, GazeField, Menu, PERMISSION_MODES, chevron, help_text, label_row, select_trigger,
        text_box,
    },
    schedule,
};
use crate::{
    agent_avatar::{self, Avatar},
    popover,
    settings::widgets,
    theme::{Theme, hairline, ink},
    typography::ui_rems,
};
use gpui::{
    AnyElement, Context, FontWeight, Focusable as _, SharedString, Window, canvas, div,
    prelude::*, px,
};
use zeron_proto::PermissionMode;

/// Width of the wizard dialog.
const DIALOG_WIDTH: f32 = 760.0;
/// Height of the step-1 stage the big figure floats on.
const STAGE_HEIGHT: f32 = 200.0;
/// Side of one shape tile in the picker grid (9 per row).
const SHAPE_TILE: f32 = 64.0;
const SHAPE_GAP: f32 = 6.0;
/// Width of nine tiles plus their gaps, so the grid reads as 9 columns.
const SHAPE_GRID_WIDTH: f32 = 9.0 * SHAPE_TILE + 8.0 * SHAPE_GAP;
/// How long the outgoing step fades before the new one rises in.
const STEP_OUT_SECONDS: f32 = 0.12;
/// How far a hovered shape tile lifts.
const TILE_LIFT: f32 = 2.0;
/// The step indicator's two bar widths (the mockup's 28 -> 40 px).
const STEP_BAR_REST: f32 = 28.0;
const STEP_BAR_ON: f32 = 40.0;

/// What a permission mode means in one line, as the wizard's tiles put it.
fn permission_tile_hint(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Ask => "Every tool waits for you.",
        PermissionMode::AutoEdits => "Edits apply, the rest asks.",
        PermissionMode::Auto => "Routine actions run on their own.",
        PermissionMode::Bypass => "Nothing asks. Trusted folders only.",
    }
}

/// A step's centred heading and lead paragraph.
fn step_heading(theme: &Theme, title: &str, lead: &str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .gap(px(4.0))
        .pt(px(10.0))
        .pb(px(18.0))
        .child(
            div()
                .text_size(ui_rems(20.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(SharedString::from(title.to_string())),
        )
        .child(
            div()
                .max_w(px(460.0))
                .text_center()
                .text_size(ui_rems(13.0))
                .text_color(theme.text_muted)
                .child(SharedString::from(lead.to_string())),
        )
}

/// "Name | Role" — the way the agent introduces itself. A missing part shows
/// its placeholder in the muted tone.
fn identity_line(
    theme: &Theme,
    name: &str,
    role: &str,
    name_size: f32,
    role_size: f32,
) -> gpui::Div {
    let empty_name = name.trim().is_empty();
    let empty_role = role.trim().is_empty();
    div()
        .flex()
        .items_center()
        .justify_center()
        .gap(px(10.0))
        .min_w_0()
        .child(
            div()
                .flex_none()
                .text_size(ui_rems(name_size))
                .font_weight(if empty_name {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::SEMIBOLD
                })
                .text_color(if empty_name {
                    theme.text_muted.opacity(0.7)
                } else {
                    theme.text
                })
                .child(SharedString::from(if empty_name {
                    "Name".to_string()
                } else {
                    name.trim().to_string()
                })),
        )
        .child(
            div()
                .flex_none()
                .w(px(1.0))
                .h(px(name_size))
                .bg(hairline(0.14)),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .text_size(ui_rems(role_size))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if empty_role {
                    theme.text_muted.opacity(0.7)
                } else {
                    theme.text.opacity(0.48)
                })
                .child(SharedString::from(if empty_role {
                    "Role".to_string()
                } else {
                    role.trim().to_string()
                })),
        )
}

/// A quiet pill chip the user can click (role suggestions, templates).
fn suggestion_chip(theme: &Theme, id: SharedString, label: &str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .px(px(10.0))
        .py(px(4.0))
        .rounded_full()
        .border_1()
        .border_color(hairline(0.08))
        .text_size(ui_rems(12.0))
        .text_color(theme.text_muted)
        .cursor_pointer()
        .hover(|s| s.border_color(hairline(0.14)).text_color(theme.text))
        .child(SharedString::from(label.to_string()))
}

impl AutomationsPage {
    /// The seconds every visible figure of the wizard reads, or `None` when the
    /// user asked for reduced motion (then the figures stand still).
    fn avatar_seconds(&self, cx: &mut Context<Self>) -> Option<f32> {
        if crate::motion::reduced_motion(cx) {
            return None;
        }
        let seconds = self.editor.as_ref()?.avatar_motion.seconds();
        // Keep frames coming through the shared bounded clock, never a
        // per-element animation whose id clock restarts on rebuild.
        crate::motion::pulse_lease(cx.entity_id(), cx);
        Some(seconds)
    }

    /// Where the eyes look: the caret of the focused field, else the pointer.
    fn avatar_gaze(&self, cx: &gpui::App) -> Option<gpui::Point<gpui::Pixels>> {
        let editor = self.editor.as_ref()?;
        match editor.gaze_field {
            Some(GazeField::Name) => editor.name_bounds.get().map(|bounds| {
                agent_avatar::caret_target(bounds, editor.name.read(cx).text().chars().count())
            }),
            Some(GazeField::Role) => editor.role_bounds.get().map(|bounds| {
                agent_avatar::caret_target(bounds, editor.role.read(cx).text().chars().count())
            }),
            None => None,
        }
        .or(editor.avatar_motion.gaze)
    }

    /// One figure of the wizard, on the shared clock and looking at the target.
    fn wizard_avatar(&self, size: f32, seconds: Option<f32>, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return div().size(px(size)).into_any_element();
        };
        let gaze = self.avatar_gaze(cx);
        // Shuffle pops the figure once; the clock is wall time, so a rebuild
        // mid-pop continues it instead of replaying it.
        let pop = match editor.shuffled_at.filter(|_| seconds.is_some()) {
            Some(at) => agent_avatar::pop_scale(
                at.elapsed().as_secs_f32() / agent_avatar::POP_SECONDS,
            ),
            None => 1.0,
        };
        Avatar::new(editor.shape, agent_avatar::hex_color(&editor.color), size)
            .motion(editor.idle)
            .time(seconds)
            .gaze(gaze)
            .pop(pop)
            .render()
    }

    fn set_step(&mut self, step: u8, window: &mut Window, cx: &mut Context<Self>) {
        self.close_menu(cx);
        if let Some(editor) = self.editor.as_mut() {
            let next = step.clamp(1, 3);
            if next != editor.step {
                editor.prev_step = editor.step;
                editor.step_at = std::time::Instant::now();
            }
            editor.step = next;
            editor.gaze_field = None;
            // Focus is taken once the step is actually on screen: while the
            // outgoing step fades, the field does not exist to focus.
            editor.focus_pending = next == 2;
        }
        let _ = window;
        cx.notify();
    }

    fn shuffle_avatar(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut() {
            let (shape, color, idle) = agent_avatar::random_pick(agent_avatar::clock_seed());
            editor.shape = shape.key;
            editor.color = color.hex();
            editor.idle = idle;
            editor.shuffled_at = Some(std::time::Instant::now());
        }
        cx.notify();
    }

    /// The three progress bars, filled up to the current step. A step change
    /// grows the reached bars to [`STEP_BAR_ON`] and tints them to the text
    /// colour over [`crate::motion::RESIZE`]; the rest shrink back.
    fn render_step_bars(&self, theme: &Theme, step: u8, cx: &mut Context<Self>) -> gpui::Div {
        let reduced = crate::motion::reduced_motion(cx);
        let mut bars = div().flex().items_center().gap(px(6.0));
        let mut gliding = false;
        for index in 1u8..=3 {
            let key = format!("automation-step-bar-{index}");
            let t = crate::motion::value_tween(
                &key,
                if index <= step { 1.0 } else { 0.0 },
                &crate::motion::RESIZE,
                reduced,
            );
            gliding |= crate::motion::value_tween_active(&key);
            bars = bars.child(
                div()
                    .h(px(4.0))
                    .w(px(crate::motion::lerp(STEP_BAR_REST, STEP_BAR_ON, t)))
                    .rounded(px(4.0))
                    .bg(crate::motion::mix(hairline(0.14), theme.text, t)),
            );
        }
        if gliding {
            crate::motion::pulse_lease(cx.entity_id(), cx);
        }
        bars
    }

    /// A soft glow under the big figure — gpui has no radial gradient, so the
    /// falloff is three concentric washes. It breathes on the avatar clock:
    /// one slow swell, in step with nothing else on screen.
    fn render_stage_glow(&self, breath: f32) -> gpui::Div {
        let mut glow = div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center();
        for (size, alpha) in [(300.0, 0.015), (220.0, 0.02), (150.0, 0.03)] {
            glow = glow.child(
                div()
                    .absolute()
                    .w(px(size * (1.0 + 0.06 * breath)))
                    .h(px(size * 0.6 * (1.0 + 0.06 * breath)))
                    .rounded_full()
                    .bg(ink(alpha * (0.75 + 0.45 * breath))),
            );
        }
        glow
    }

    /// The glow's breath, 0..1, off the avatar clock (0 when it stands still).
    fn stage_breath(&self, seconds: Option<f32>) -> f32 {
        match seconds {
            Some(t) => 0.5 - 0.5 * (t / 2.6 * std::f32::consts::TAU).cos(),
            None => 0.0,
        }
    }

    fn render_step_face(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let seconds = self.avatar_seconds(cx);
        let Some((shape, color)) = self
            .editor
            .as_ref()
            .map(|editor| (editor.shape, editor.color.clone()))
        else {
            return div().into_any_element();
        };
        let stage = div()
            .relative()
            .w_full()
            .h(px(STAGE_HEIGHT))
            .rounded(px(16.0))
            .flex()
            .items_center()
            .justify_center()
            .child(self.render_stage_glow(self.stage_breath(seconds)))
            .child(self.wizard_avatar(150.0, seconds, cx))
            .child(
                div().absolute().right(px(12.0)).top(px(12.0)).child(
                    widgets::ghost_action(theme)
                        .id("automation-shuffle")
                        .hover(|s| widgets::ghost_hover(theme, s))
                        .child(
                            crate::icons::icon(crate::icons::REFRESH)
                                .size(px(13.0))
                                .flex_none()
                                .text_color(theme.text_muted),
                        )
                        .child("Shuffle")
                        .on_click(cx.listener(|page, _, _, cx| page.shuffle_avatar(cx))),
                ),
            );

        let mut colors = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(10.0))
            .mx_auto()
            .rounded_full()
            .border_1()
            .border_color(hairline(0.08))
            .bg(ink(0.035));
        let reduced = crate::motion::reduced_motion(cx);
        let mut gliding = false;
        for entry in agent_avatar::PALETTE {
            let selected = entry.hex().eq_ignore_ascii_case(&color);
            let hex = entry.hex();
            // The selection ring fades in and the dot swells, rather than
            // snapping between two states.
            let key = format!("automation-color-t-{}", entry.id);
            let t = crate::motion::value_tween(
                &key,
                if selected { 1.0 } else { 0.0 },
                &crate::motion::SWITCH_GLIDE,
                reduced,
            );
            gliding |= crate::motion::value_tween_active(&key);
            colors = colors.child(
                div()
                    .id(SharedString::from(format!("automation-color-{}", entry.id)))
                    .flex_none()
                    .size(px(28.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .child(
                        div()
                            .size(px(crate::motion::lerp(22.0, 26.0, t)))
                            .rounded_full()
                            .bg(entry.hsla())
                            .border_2()
                            .border_color(theme.text.opacity(t)),
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        if let Some(editor) = page.editor.as_mut() {
                            editor.color = hex.clone();
                        }
                        cx.notify();
                    })),
            );
        }

        let mut shapes = div()
            .w(px(SHAPE_GRID_WIDTH))
            .max_w_full()
            .mx_auto()
            .flex()
            .flex_wrap()
            .justify_center()
            .gap(px(SHAPE_GAP));
        for entry in agent_avatar::SHAPES {
            let selected = entry.key == shape;
            let key = entry.key;
            let select_key = format!("automation-shape-t-{key}");
            let select = crate::motion::value_tween(
                &select_key,
                if selected { 1.0 } else { 0.0 },
                &crate::motion::SWITCH_GLIDE,
                reduced,
            );
            gliding |= crate::motion::value_tween_active(&select_key);
            // The hover fade lifts the tile and blinks its little figure once.
            let hover_key = SharedString::from(format!("automation-shape-h-{key}"));
            let hovered = crate::motion::hover_t(&hover_key);
            let tint = crate::motion::mix(
                theme.text_muted.opacity(0.75),
                agent_avatar::hex_color(&color),
                select,
            );
            let mut tile = div()
                .id(SharedString::from(format!("automation-shape-{key}")))
                .flex_none()
                .relative()
                .top(px(-TILE_LIFT * hovered))
                .size(px(SHAPE_TILE))
                .p(px(6.0))
                .rounded(px(10.0))
                .border_1()
                .border_color(crate::motion::mix(
                    hairline(0.08 + 0.06 * hovered),
                    theme.text,
                    select,
                ))
                .bg(ink(0.025 + 0.035 * select.max(hovered)))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .child(
                    Avatar::new(key, tint, SHAPE_TILE - 12.0)
                        .blink(Some(agent_avatar::blink_from_hover(hovered)))
                        .render(),
                )
                .on_click(cx.listener(move |page, _, _, cx| {
                    if let Some(editor) = page.editor.as_mut() {
                        editor.shape = key;
                    }
                    cx.notify();
                }));
            tile.interactivity()
                .on_hover(crate::motion::hover_listener(hover_key));
            shapes = shapes.child(tile);
        }
        if gliding {
            crate::motion::pulse_lease(cx.entity_id(), cx);
        }

        div()
            .flex()
            .flex_col()
            .child(step_heading(
                theme,
                "Meet your new agent",
                "Every agent starts as a random shape and colour. Change it, or shuffle.",
            ))
            .child(stage)
            .child(div().mt(px(18.0)).child(colors))
            .child(
                div()
                    .mt(px(18.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_center()
                            .text_size(ui_rems(10.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(popover::tracked_upper("Shape"))),
                    )
                    .child(shapes),
            )
            .child(
                div()
                    .mt(px(26.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        popover::btn_primary(theme, "Next")
                            .id("automation-step1-next")
                            .on_click(cx.listener(|page, _, window, cx| {
                                page.set_step(2, window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    /// A text field that reports its painted bounds, so the eyes can aim at
    /// the caret while it is focused.
    fn tracked_field(
        &self,
        theme: &Theme,
        which: GazeField,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Div> {
        let editor = self.editor.as_ref()?;
        let (input, cell) = match which {
            GazeField::Name => (editor.name.clone(), editor.name_bounds.clone()),
            GazeField::Role => (editor.role.clone(), editor.role_bounds.clone()),
        };
        let focus = input.clone();
        Some(
            div()
                .relative()
                .child(text_box(theme, input.into_any_element(), false, false))
                .child(
                    canvas(
                        move |bounds, _, _| cell.set(Some(bounds)),
                        |_, _, _, _| (),
                    )
                    .absolute()
                    .inset_0(),
                )
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |page, _, window, cx| {
                        if let Some(editor) = page.editor.as_mut() {
                            editor.gaze_field = Some(which);
                        }
                        window.focus(&focus.focus_handle(cx), cx);
                        cx.notify();
                    }),
                ),
        )
    }

    fn render_step_name(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let seconds = self.avatar_seconds(cx);
        let Some((name, role)) = self.editor.as_ref().map(|editor| {
            (
                editor.name.read(cx).text().to_string(),
                editor.role.read(cx).text().to_string(),
            )
        }) else {
            return div().into_any_element();
        };
        let has_name = !name.trim().is_empty();
        let role_input = self.editor.as_ref().map(|editor| editor.role.clone());
        let mut chips = div().flex().flex_wrap().gap(px(6.0)).mt(px(2.0));
        for suggestion in schedule::ROLE_SUGGESTIONS {
            let target = role_input.clone();
            chips = chips.child(
                suggestion_chip(
                    theme,
                    SharedString::from(format!("automation-role-{suggestion}")),
                    suggestion,
                )
                .on_click(cx.listener(move |page, _, window, cx| {
                    if let Some(input) = target.clone() {
                        input.update(cx, |input, cx| input.set_text(suggestion, cx));
                        window.focus(&input.focus_handle(cx), cx);
                    }
                    if let Some(editor) = page.editor.as_mut() {
                        editor.gaze_field = Some(GazeField::Role);
                    }
                    cx.notify();
                })),
            );
        }
        let name_field = self.tracked_field(theme, GazeField::Name, cx);
        let role_field = self.tracked_field(theme, GazeField::Role, cx);
        let field_label = |label: &'static str| {
            div()
                .text_size(ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(label)
        };
        div()
            .flex()
            .flex_col()
            .child(step_heading(
                theme,
                "Give them a name",
                "A name and a role, the way a teammate would introduce themselves.",
            ))
            .child(
                div()
                    .relative()
                    .w_full()
                    .h(px(170.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(self.render_stage_glow(self.stage_breath(seconds)))
                    .child(self.wizard_avatar(120.0, seconds, cx)),
            )
            .child(
                div()
                    .mt(px(10.0))
                    .child(identity_line(theme, &name, &role, 20.0, 16.0)),
            )
            .child(
                div()
                    .mt(px(20.0))
                    .w_full()
                    .max_w(px(520.0))
                    .mx_auto()
                    .flex()
                    .flex_col()
                    .gap(px(14.0))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(field_label("Name"))
                            .children(name_field),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(field_label("Role"))
                            .children(role_field)
                            .child(chips),
                    ),
            )
            .child(
                div()
                    .mt(px(26.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        popover::btn_ghost(theme, "Back", "automation-step2-back")
                            .id("automation-step2-back")
                            .on_click(cx.listener(|page, _, window, cx| {
                                page.set_step(1, window, cx)
                            })),
                    )
                    .child(if has_name {
                        popover::btn_primary(theme, "Next")
                            .id("automation-step2-next")
                            .on_click(cx.listener(|page, _, window, cx| {
                                page.set_step(3, window, cx)
                            }))
                    } else {
                        div()
                            .id("automation-step2-next")
                            .px(px(12.0))
                            .py(px(6.0))
                            .rounded(px(8.0))
                            .bg(theme.text.opacity(0.3))
                            .text_size(ui_rems(13.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.on_solid)
                            .child("Next")
                    }),
            )
            .into_any_element()
    }

    /// The identity card on step 3: the small figure, the introduction line and
    /// the way back into step 2.
    fn render_identity_card(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let seconds = self.avatar_seconds(cx);
        let (name, role) = self
            .editor
            .as_ref()
            .map(|editor| {
                (
                    editor.name.read(cx).text().to_string(),
                    editor.role.read(cx).text().to_string(),
                )
            })
            .unwrap_or_default();
        div()
            .flex()
            .items_center()
            .gap(px(14.0))
            .px(px(14.0))
            .py(px(12.0))
            .rounded(px(12.0))
            .border_1()
            .border_color(hairline(0.08))
            .bg(ink(0.035))
            .child(self.wizard_avatar(40.0, seconds, cx))
            .child(
                div()
                    .min_w_0()
                    .child(identity_line(theme, &name, &role, 16.0, 13.0)),
            )
            .child(div().flex_1())
            .child(
                popover::btn_ghost(theme, "Edit", "automation-identity-edit")
                    .id("automation-identity-edit")
                    .on_click(cx.listener(|page, _, window, cx| page.set_step(2, window, cx))),
            )
    }

    /// The template chips above the instructions box.
    fn render_templates(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let prompt = self.editor.as_ref().map(|editor| editor.prompt.clone());
        let mut chips = div().flex().flex_wrap().gap(px(6.0));
        for (label, body) in schedule::TEMPLATES {
            let target = prompt.clone();
            chips = chips.child(
                suggestion_chip(
                    theme,
                    SharedString::from(format!("automation-template-{label}")),
                    label,
                )
                .on_click(cx.listener(move |_, _, window, cx| {
                    if let Some(input) = target.clone() {
                        input.update(cx, |input, cx| input.set_text(body, cx));
                        window.focus(&input.focus_handle(cx), cx);
                    }
                })),
            );
        }
        chips
    }

    fn render_permission_tiles(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let Some((current, locked)) = self
            .editor
            .as_ref()
            .map(|editor| (editor.permission, editor.locked))
        else {
            return div();
        };
        let mut tiles = div().flex().items_stretch().gap(px(8.0));
        for mode in PERMISSION_MODES {
            let selected = mode == current;
            let danger = mode == PermissionMode::Bypass;
            tiles = tiles.child(
                div()
                    .id(SharedString::from(format!("automation-perm-{mode:?}")))
                    .flex_1()
                    .min_w_0()
                    .px(px(10.0))
                    .py(px(10.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(if selected {
                        if danger {
                            theme.danger.opacity(0.55)
                        } else {
                            theme.accent
                        }
                    } else {
                        hairline(0.08)
                    })
                    .when(selected, |el| {
                        el.bg(if danger {
                            theme.danger.opacity(0.08)
                        } else {
                            theme.accent.opacity(0.10)
                        })
                    })
                    .when(!selected && !locked, |el| el.hover(|s| s.bg(ink(0.05))))
                    .when(locked, |el| el.opacity(0.6))
                    .when(!locked, |el| el.cursor_pointer())
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(ui_rems(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(if selected {
                                if danger {
                                    theme.danger_muted
                                } else {
                                    theme.accent
                                }
                            } else {
                                theme.text
                            })
                            .child(crate::permission_picker::mode_row_label(mode)),
                    )
                    .child(
                        div()
                            .text_size(ui_rems(11.5))
                            .line_height(px(15.0))
                            .text_color(theme.text_muted.opacity(0.75))
                            .child(permission_tile_hint(mode)),
                    )
                    .when(!locked, |el| {
                        el.on_click(cx.listener(move |page, _, _, cx| {
                            if let Some(editor) = page.editor.as_mut() {
                                editor.permission = mode;
                            }
                            cx.notify();
                        }))
                    }),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(label_row(theme, "Permissions", false))
            .child(tiles)
            .child(help_text(
                theme,
                "Auto is the default for automations: nobody is there to answer a prompt at 09:00.",
            ))
    }

    /// The two result selects: where a finished run reports, and its budget.
    fn render_delivery(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let Some((deliver, limit, locked)) = self.editor.as_ref().map(|editor| {
            (
                editor.deliver,
                editor.max_run_minutes,
                editor.locked,
            )
        }) else {
            return div();
        };
        let deliver_trigger = select_trigger(
            theme,
            "automation-deliver",
            self.menu.as_open() == Some(&Menu::Deliver),
            locked,
        )
        .child(
            crate::icons::icon(crate::icons::BELL)
                .size(px(14.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(SharedString::from(schedule::deliver_label(deliver))),
        )
        .child(chevron(theme));
        let deliver_trigger = self.menu_trigger(deliver_trigger, Menu::Deliver, locked, false, cx);
        let stop_trigger = select_trigger(
            theme,
            "automation-stop-after",
            self.menu.as_open() == Some(&Menu::StopAfter),
            locked,
        )
        .child(
            crate::icons::icon(crate::icons::CLOCK_CIRCLE)
                .size(px(14.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(SharedString::from(schedule::stop_after_label(limit))),
        )
        .child(chevron(theme));
        let stop_trigger = self.menu_trigger(stop_trigger, Menu::StopAfter, locked, false, cx);
        div()
            .flex()
            .items_start()
            .gap(px(14.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(label_row(theme, "Deliver result to", false))
                    .child(deliver_trigger),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(label_row(theme, "Stop after", false))
                    .child(stop_trigger),
            )
    }

    fn render_step_task(
        &mut self,
        theme: &Theme,
        check: &Check,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let identity = self.render_identity_card(theme, cx);
        let templates = self.render_templates(theme, cx);
        let instructions = self.render_instructions(theme, check, cx);
        let frequency = self.render_frequency(theme, check, cx);
        let delivery = self.render_delivery(theme, cx);
        let permissions = self.render_permission_tiles(theme, cx);
        let advanced = self.render_advanced(theme, check, cx);
        div()
            .flex()
            .flex_col()
            .child(step_heading(
                theme,
                "What should they do?",
                "Instructions run in a fresh chat every time. Saved paused; turn it on from the list.",
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(18.0))
                    .child(identity)
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(templates)
                            .child(instructions),
                    )
                    .child(frequency)
                    .child(delivery)
                    .child(permissions)
                    .child(advanced),
            )
            .into_any_element()
    }

    pub(super) fn render_editor_overlay(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let editor = self.editor.as_mut()?;
        let step = editor.step.clamp(1, 3);
        // The eyes follow the OS cursor, not gpui's last in-window mouse move,
        // so leaving the window still turns them. Re-read every frame: the
        // avatars already hold a pulse lease while the dialog is open.
        if let Some(cursor) = agent_avatar::os_cursor(window) {
            editor.avatar_motion.gaze = Some(cursor);
        }
        let since_step = editor.step_at.elapsed().as_secs_f32();
        let prev_step = editor.prev_step.clamp(1, 3);
        let theme = Theme::of(cx).clone();
        let viewport = window.viewport_size();
        let check = self.check(cx)?;
        let locked = self.editor.as_ref()?.locked;
        let busy = self.busy;
        let can_save = check.name && (locked || check.prompt) && !busy;

        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .px(px(22.0))
            .pt(px(18.0))
            .child(self.render_step_bars(&theme, step, cx))
            .child(
                div()
                    .id("automation-close")
                    .flex_none()
                    .size(px(30.0))
                    .rounded(px(8.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(ink(0.06)))
                    .child(
                        crate::icons::icon(crate::icons::CLOSE)
                            .size(px(14.0))
                            .text_color(theme.text_muted),
                    )
                    .on_click(cx.listener(|page, _, _, cx| page.close_editor(cx))),
            );

        // A step change first fades the outgoing step out, then rises the new
        // one in. Both phases run off one wall clock (`step_at`), so a rebuild
        // mid-transition continues it instead of replaying it from zero - a
        // `value_tween` cannot do this, it paints a new key at its target.
        let reduced = crate::motion::reduced_motion(cx);
        let rise_spec = &crate::motion::SETTINGS_RESULTS_RISE;
        let rise_seconds = rise_spec.duration_ms as f32 / 1000.0;
        let leaving = !reduced && prev_step != step && since_step < STEP_OUT_SECONDS;
        let (shown, opacity, lift) = if reduced {
            (step, 1.0, 0.0)
        } else if leaving {
            (prev_step, 1.0 - since_step / STEP_OUT_SECONDS, 0.0)
        } else {
            let t = rise_spec
                .progress(((since_step - STEP_OUT_SECONDS) / rise_seconds).clamp(0.0, 1.0));
            (
                step,
                t,
                crate::motion::lerp(crate::motion::SETTINGS_RESULTS_RISE_DISTANCE, 0.0, t),
            )
        };
        if !reduced && since_step < STEP_OUT_SECONDS + rise_seconds {
            crate::motion::pulse_lease(cx.entity_id(), cx);
        }
        if shown != 1
            && !leaving
            && let Some(editor) = self.editor.as_mut()
            && std::mem::take(&mut editor.focus_pending)
        {
            let name = editor.name.clone();
            window.focus(&name.focus_handle(cx), cx);
        }
        let content = match shown {
            1 => self.render_step_face(&theme, cx),
            2 => self.render_step_name(&theme, cx),
            _ => self.render_step_task(&theme, &check, cx),
        };
        let body = div()
            .id("automation-editor-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(px(22.0))
            .pb(px(22.0))
            .flex()
            .flex_col()
            .when_some(self.error.clone(), |el, error| {
                el.child(widgets::error_strip(&theme, error).mt(px(0.0)))
            })
            .when(locked, |el| {
                el.child(
                    widgets::warning_strip(
                        &theme,
                        "A run is in progress. You can rename this automation now; the other \
                         settings unlock when the run finishes.",
                    )
                    .mt(px(0.0)),
                )
            })
            .child(
                div()
                    .opacity(opacity)
                    .top(px(lift))
                    .relative()
                    .child(content),
            );

        // Translucent like the rest of the app, but denser than the sidebar so
        // the form stays legible over whatever is on the desktop: the theme's
        // glass fill with its alpha raised, over the modal's backdrop blur.
        let fill = if theme.is_frost() {
            let glass = theme.glass();
            theme.surface.opacity((glass.a + 0.10).clamp(0.0, 1.0).max(0.86))
        } else {
            theme.surface_dialog
        };
        let mut card = popover::dialog_card(&theme)
            .bg(fill)
            .w(px(DIALOG_WIDTH))
            .on_drag_move(cx.listener(Self::on_effort_drag_move))
            .max_w(viewport.width - px(32.0))
            .max_h(viewport.height - px(48.0))
            .p(px(0.0))
            // The eyes follow the pointer across the whole dialog.
            .on_mouse_move(cx.listener(
                |page, event: &gpui::MouseMoveEvent, _, cx| {
                    if let Some(editor) = page.editor.as_mut() {
                        editor.avatar_motion.gaze = Some(event.position);
                        editor.gaze_field = None;
                    }
                    cx.notify();
                },
            ))
            .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    if page.menu.is_open() {
                        page.close_menu(cx);
                    } else {
                        page.close_editor(cx);
                    }
                    cx.stop_propagation();
                }
            }))
            .child(header)
            .child(body);
        if step == 3 {
            let save_label = if busy { "Saving..." } else { "Save" };
            let save = if can_save {
                popover::btn_primary(&theme, save_label)
                    .id("automation-save")
                    .on_click(cx.listener(|page, _, _, cx| page.save_editor(cx)))
            } else {
                div()
                    .id("automation-save")
                    .px(px(12.0))
                    .py(px(6.0))
                    .rounded(px(8.0))
                    .bg(theme.text.opacity(0.3))
                    .text_size(ui_rems(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.on_solid)
                    .child(save_label)
            };
            let test_run = if can_save {
                popover::btn_ghost(&theme, "Test run", "automation-test-run")
                    .id("automation-test-run")
                    .border_1()
                    .border_color(hairline(0.08))
                    .on_click(cx.listener(|page, _, _, cx| page.test_run(cx)))
            } else {
                popover::btn_ghost(&theme, "Test run", "automation-test-run-off")
                    .id("automation-test-run")
                    .border_1()
                    .border_color(hairline(0.08))
                    .opacity(0.45)
            };
            card = card.child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(16.0))
                    .px(px(22.0))
                    .py(px(14.0))
                    .border_t_1()
                    .border_color(hairline(0.06))
                    .child(
                        div()
                            .min_w_0()
                            .text_size(ui_rems(11.5))
                            .line_height(px(16.0))
                            .text_color(theme.text_muted.opacity(0.8))
                            .child(
                                "Runs only while the engine is running on this device. \
                                 Settings > General starts it at login.",
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                popover::btn_ghost(&theme, "Back", "automation-step3-back")
                                    .id("automation-step3-back")
                                    .on_click(cx.listener(|page, _, window, cx| {
                                        page.set_step(2, window, cx)
                                    })),
                            )
                            .child(test_run)
                            .child(save),
                    ),
            );
        }
        Some(popover::modal(
            "automation-editor-dialog",
            viewport,
            card.into_any_element(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_tile_explains_itself_in_one_line() {
        for mode in PERMISSION_MODES {
            let hint = permission_tile_hint(mode);
            assert!(hint.ends_with('.'), "{mode:?}: {hint}");
            assert!(hint.len() <= 42, "{mode:?} is too long: {hint}");
        }
        assert!(permission_tile_hint(PermissionMode::Bypass).contains("Trusted"));
    }

    #[test]
    fn the_shape_grid_is_nine_columns_wide() {
        assert_eq!(SHAPE_GRID_WIDTH, 9.0 * SHAPE_TILE + 8.0 * SHAPE_GAP);
        // Nine tiles fit inside the dialog with room for its padding.
        assert!(SHAPE_GRID_WIDTH < DIALOG_WIDTH - 44.0);
    }
}
