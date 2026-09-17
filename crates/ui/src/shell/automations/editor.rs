//! The "New automation" / "Edit automation" dialog, modeled on a scheduled
//! task sheet: name, instructions with the folder, agent and model pickers in
//! the box's footer, a frequency in words, the permission mode with a one-line
//! explanation, and the result/review settings folded under "Advanced".
use std::sync::Arc;

use super::{
    AutomationsPage, is_active_status, parse_result_settings,
    schedule::{self, IntervalUnit, ModelItem},
    text,
};
use crate::{
    composer::{ComposerInput, ComposerInputEvent},
    popover::{self, Loadable, Popup},
    settings::widgets,
    theme::{Theme, hairline, ink},
    typography::ui_rems,
};
use gpui::{
    AnyElement, App, Context, Entity, EntityInputHandler as _, Focusable as _, MouseButton,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use serde_json::{Value, json};
use zeron_proto::{AutomationSpec, HarnessId, PermissionMode, ReasoningLevel, Space};

/// Which dropdown of the dialog is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Menu {
    Folder,
    Agent,
    Model,
    Frequency,
    Deliver,
    StopAfter,
}

/// Which field the eyes track while it is focused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GazeField {
    Name,
    Role,
}

pub(super) const PERMISSION_MODES: [PermissionMode; 4] = [
    PermissionMode::Ask,
    PermissionMode::AutoEdits,
    PermissionMode::Auto,
    PermissionMode::Bypass,
];

/// Height of one model row; the virtualized list needs uniform items.
const MODEL_ROW_HEIGHT: f32 = 30.0;
const MODEL_LIST_HEIGHT: f32 = 280.0;
/// Width of the Model dropdown, which the Effort slider spans.
const MODEL_MENU_WIDTH: f32 = 340.0;
/// Side padding of the Effort row, as in the composer's picker.
const EFFORT_ROW_PAD: f32 = 8.0;
const EFFORT_TRACK_WIDTH: f32 = MODEL_MENU_WIDTH - 2.0 * (1.0 + EFFORT_ROW_PAD);

pub(super) struct Editor {
    pub(super) original: Option<Value>,
    /// Which wizard step shows (1 = face, 2 = name, 3 = the task).
    pub(super) step: u8,
    /// The agent's face: a shape key from [`crate::agent_avatar`] plus a
    /// palette colour, and the idle loop the figure runs.
    pub(super) shape: &'static str,
    pub(super) color: String,
    pub(super) idle: crate::agent_avatar::IdleMotion,
    /// The role line next to the name ("CEO").
    pub(super) role: Entity<ComposerInput>,
    pub(super) deliver: zeron_proto::Deliver,
    pub(super) max_run_minutes: Option<u32>,
    /// The wizard's single clock; every visible figure reads it.
    pub(super) avatar_motion: crate::agent_avatar::AvatarMotion,
    /// Painted bounds of the name and role fields, for the caret gaze.
    pub(super) name_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(super) role_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(super) gaze_field: Option<GazeField>,
    /// When the current step arrived, and the step it replaced: the wizard
    /// fades the outgoing step out before the new one rises in.
    pub(super) step_at: std::time::Instant,
    pub(super) prev_step: u8,
    /// When Shuffle last ran, for the figure's one-off pop.
    pub(super) shuffled_at: Option<std::time::Instant>,
    /// A run is active: only the name may change until it finishes.
    pub(super) locked: bool,
    pub(super) name: Entity<ComposerInput>,
    pub(super) prompt: Entity<ComposerInput>,
    pub(super) cwd: String,
    pub(super) harness: HarnessId,
    /// The user picked the agent, so a late catalog load must not replace it.
    pub(super) harness_touched: bool,
    pub(super) model: Option<String>,
    pub(super) reasoning: Option<ReasoningLevel>,
    pub(super) permission: PermissionMode,
    /// The preset in force when `custom` is off.
    pub(super) interval: u32,
    pub(super) custom: bool,
    pub(super) custom_value: Entity<ComposerInput>,
    pub(super) custom_unit: IntervalUnit,
    /// "HH:MM" the run is anchored to; only used by day-scale intervals.
    pub(super) time_of_day: Entity<ComposerInput>,
    /// Weekday the run is anchored to, 0 = Monday; only used by weekly intervals.
    pub(super) weekday: u8,
    pub(super) calendar_days: Option<Vec<u8>>,
    /// Track bounds of the Effort slider, for pointer mapping.
    pub(super) effort_bounds: std::rc::Rc<std::cell::Cell<Option<gpui::Bounds<gpui::Pixels>>>>,
    pub(super) result_manifest: Entity<ComposerInput>,
    pub(super) review_url: Entity<ComposerInput>,
    pub(super) review_command: Entity<ComposerInput>,
    pub(super) advanced_open: bool,
    /// Save was pressed once: inline errors are shown from now on.
    pub(super) attempted: bool,
    pub(super) focus_pending: bool,
    pub(super) model_search: Entity<ComposerInput>,
    pub(super) model_scroll: gpui::UniformListScrollHandle,
    pub(super) folder_path: Entity<ComposerInput>,
    pub(super) _subscriptions: Vec<Subscription>,
}

/// Validation of the current form.
pub(super) struct Check {
    pub(super) name: bool,
    pub(super) prompt: bool,
    pub(super) folder: Result<(), &'static str>,
    pub(super) interval: Result<u32, &'static str>,
    /// The typed time of day; only checked while the interval takes one.
    pub(super) time: Result<u16, &'static str>,
    pub(super) results: Result<Option<Vec<String>>, &'static str>,
}

fn trimmed(input: &Entity<ComposerInput>, cx: &App) -> String {
    input.read(cx).text().trim().to_string()
}

fn same_path(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches(['/', '\\']);
    let b = b.trim_end_matches(['/', '\\']);
    if cfg!(windows) {
        a.eq_ignore_ascii_case(b)
    } else {
        a == b
    }
}

fn example_folder() -> &'static str {
    if cfg!(windows) {
        "D:\\Projects\\my-app"
    } else {
        "/home/you/projects/my-app"
    }
}

pub(super) fn label_row(theme: &Theme, label: &str, required: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(3.0))
        .text_size(ui_rems(13.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.text)
        .child(SharedString::from(label.to_string()))
        .when(required, |el| {
            el.child(div().text_color(theme.text_muted.opacity(0.7)).child("*"))
        })
}

pub(super) fn help_text(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .text_size(ui_rems(12.0))
        .line_height(px(17.0))
        .text_color(theme.text_muted.opacity(0.85))
        .child(copy.into())
}

pub(super) fn inline_error(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_start()
        .gap(px(6.0))
        .text_size(ui_rems(12.0))
        .text_color(theme.danger_muted)
        .child(
            div().flex_none().mt(px(2.0)).child(
                crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                    .size(px(12.0))
                    .text_color(theme.danger_muted),
            ),
        )
        .child(div().min_w_0().child(copy.into()))
}

pub(super) fn text_box(theme: &Theme, input: AnyElement, invalid: bool, disabled: bool) -> gpui::Div {
    popover::dialog_field(input)
        .when(invalid, |el| el.border_color(theme.danger.opacity(0.55)))
        .when(disabled, |el| el.opacity(0.6))
}

pub(super) fn chevron(theme: &Theme) -> gpui::Svg {
    crate::icons::icon(crate::icons::ALT_ARROW_DOWN)
        .size(px(12.0))
        .flex_none()
        .text_color(theme.text_muted.opacity(0.6))
}

/// A full-width dropdown trigger shaped like the dialog's text fields.
pub(super) fn select_trigger(
    theme: &Theme,
    id: &'static str,
    open: bool,
    disabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .w_full()
        .h(px(36.0))
        .px(px(12.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(hairline(if open { 0.18 } else { 0.08 }))
        .bg(ink(if open { 0.07 } else { 0.04 }))
        .text_size(ui_rems(13.0))
        .text_color(theme.text)
        .when(!disabled, |el| {
            el.cursor_pointer().hover(|s| s.bg(ink(0.07)))
        })
        .when(disabled, |el| el.opacity(0.6))
}

/// A compact chip in the instructions box footer.
fn footer_chip(
    theme: &Theme,
    id: &'static str,
    open: bool,
    disabled: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .relative()
        .h(px(26.0))
        .min_w_0()
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(7.0))
        .text_size(ui_rems(12.0))
        .text_color(theme.text_muted)
        .when(open, |el| el.bg(theme.element_hover).text_color(theme.text))
        .when(!disabled, |el| {
            el.cursor_pointer()
                .hover(|s| s.bg(theme.element_hover).text_color(theme.text))
        })
        .when(disabled, |el| el.opacity(0.6))
}

pub(super) fn check_mark(theme: &Theme) -> gpui::Svg {
    crate::icons::icon(crate::icons::CHECK)
        .size(px(13.0))
        .flex_none()
        .text_color(theme.accent)
}

fn option_row(
    theme: &Theme,
    id: impl Into<SharedString>,
    selected: bool,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    popover::menu_row(theme, selected, id.clone()).id(id)
}

fn menu_note(theme: &Theme, copy: impl Into<SharedString>) -> gpui::Div {
    div()
        .px(px(8.0))
        .py(px(10.0))
        .text_size(ui_rems(12.0))
        .text_color(theme.text_muted.opacity(0.75))
        .child(copy.into())
}

impl AutomationsPage {
    fn local_spaces(&self, cx: &App) -> Vec<Space> {
        let device = self.local_device_id(cx);
        self.state
            .read(cx)
            .spaces
            .iter()
            .filter(|space| Some(&space.device_id) == device.as_ref())
            .cloned()
            .collect()
    }

    /// Prefill for a new automation: the open project, else the default
    /// workspace, else the first project on this device.
    fn default_folder(&self, cx: &App) -> String {
        let device = self.local_device_id(cx);
        let state = self.state.read(cx);
        let local = |space: &&Space| Some(&space.device_id) == device.as_ref();
        state
            .selected_space_row()
            .filter(local)
            .or_else(|| {
                crate::settings::current(cx)
                    .workspace_space_id
                    .as_deref()
                    .and_then(|id| state.space_row(id))
                    .filter(local)
            })
            .or_else(|| state.spaces.iter().find(local))
            .map(|space| space.path.clone())
            .unwrap_or_default()
    }

    /// The composer's last agent when this device still offers it, else the
    /// first offered agent.
    fn default_harness(&self) -> HarnessId {
        let offered = self.offered_harnesses();
        let offers = |harness: HarnessId| {
            offered
                .as_ref()
                .is_none_or(|list| list.iter().any(|d| d.id == harness))
        };
        self.defaults
            .harness
            .filter(|harness| offers(*harness))
            .or_else(|| offered.as_ref().and_then(|list| list.first().map(|d| d.id)))
            .unwrap_or(HarnessId::ClaudeCode)
    }

    pub(super) fn open_editor(
        &mut self,
        original: Option<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let spec: Option<AutomationSpec> = original
            .as_ref()
            .and_then(|row| serde_json::from_value(row["spec"].clone()).ok());
        if original.is_some() && spec.is_none() {
            self.error = Some("This automation could not be read, so it cannot be edited.".into());
            cx.notify();
            return;
        }
        let locked = original.as_ref().is_some_and(|row| {
            row["history"].as_array().is_some_and(|runs| {
                runs.iter()
                    .any(|run| is_active_status(&text(run, "status")))
            })
        });
        let cwd = spec
            .as_ref()
            .map(|spec| spec.request.cwd.clone())
            .unwrap_or_else(|| self.default_folder(cx));
        let harness = spec
            .as_ref()
            .and_then(|spec| spec.request.harness)
            .unwrap_or_else(|| self.default_harness());
        let interval = spec
            .as_ref()
            .map(|spec| spec.interval_minutes)
            .unwrap_or(schedule::DEFAULT_INTERVAL);
        let (count, unit) = schedule::split_interval(interval);
        let field = |placeholder: String,
                     value: String,
                     single: bool,
                     read_only: bool,
                     cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut field = ComposerInput::new(placeholder, cx);
                if single {
                    field = field.with_single_line();
                }
                field.set_text(value, cx);
                field.read_only = read_only;
                field
            })
        };
        let name = field(
            "Name this automation".into(),
            spec.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            true,
            false,
            cx,
        );
        let role = field(
            "Role".into(),
            spec.as_ref()
                .and_then(|s| s.role.clone())
                .unwrap_or_default(),
            true,
            false,
            cx,
        );
        let prompt = field(
            "What should the agent do each run?".into(),
            spec.as_ref()
                .map(|s| s.request.prompt.clone())
                .unwrap_or_default(),
            false,
            locked,
            cx,
        );
        let custom_value = field("2".into(), count.to_string(), true, locked, cx);
        let anchored_time = spec
            .as_ref()
            .and_then(|s| s.time_of_day)
            .unwrap_or(schedule::DEFAULT_TIME_OF_DAY);
        let time_of_day = field(
            "09:00".into(),
            schedule::format_time_of_day(anchored_time),
            true,
            locked,
            cx,
        );
        let result_manifest = field(
            schedule::DEFAULT_MANIFEST.into(),
            spec.as_ref()
                .and_then(|s| s.result_manifest.clone())
                .unwrap_or_default(),
            true,
            locked,
            cx,
        );
        let review_url = field(
            "http://localhost:8793/?run={{run_id}}".into(),
            spec.as_ref()
                .and_then(|s| s.review_url.clone())
                .unwrap_or_default(),
            true,
            locked,
            cx,
        );
        let review_command = field(
            r#"["npm", "run", "review"]"#.into(),
            spec.as_ref()
                .and_then(|s| s.review_command.as_ref())
                .map(|args| serde_json::to_string(args).unwrap_or_default())
                .unwrap_or_default(),
            true,
            locked,
            cx,
        );
        let model_search = cx.new(|cx| {
            ComposerInput::new("Search models", cx)
                .with_single_line()
                .with_accessibility_role(gpui::Role::SearchInput)
        });
        let folder_path = field(
            format!("Or paste a full path, e.g. {}", example_folder()),
            String::new(),
            true,
            false,
            cx,
        );
        let mut subscriptions = Vec::new();
        let prompt_id = prompt.entity_id();
        let name_id = name.entity_id();
        let prompt_target = prompt.clone();
        for input in [
            &name,
            &role,
            &prompt,
            &custom_value,
            &time_of_day,
            &result_manifest,
            &review_url,
            &review_command,
        ] {
            let prompt_target = prompt_target.clone();
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |page, input, event: &ComposerInputEvent, window, cx| match event {
                    ComposerInputEvent::Edited => cx.notify(),
                    // Ctrl/Cmd+Enter saves from any field.
                    ComposerInputEvent::ModifiedSubmitted => page.save_editor(cx),
                    // Enter writes a new line in the instructions, and moves
                    // from the name on to them.
                    ComposerInputEvent::Submitted if input.entity_id() == prompt_id => {
                        input.update(cx, |input, cx| {
                            input.replace_text_in_range(None, "\n", window, cx)
                        });
                    }
                    ComposerInputEvent::Submitted if input.entity_id() == name_id => {
                        window.focus(&prompt_target.focus_handle(cx), cx);
                    }
                    _ => {}
                },
            ));
        }
        subscriptions.push(cx.subscribe(
            &model_search,
            |page, _, event: &ComposerInputEvent, cx| match event {
                ComposerInputEvent::Edited => {
                    if let Some(editor) = &page.editor {
                        editor
                            .model_scroll
                            .scroll_to_item(0, gpui::ScrollStrategy::Top);
                    }
                    cx.notify();
                }
                ComposerInputEvent::Submitted => page.pick_first_model(cx),
                _ => {}
            },
        ));
        subscriptions.push(cx.subscribe(
            &folder_path,
            |page, input, event: &ComposerInputEvent, cx| {
                if *event == ComposerInputEvent::Submitted {
                    let path = input.read(cx).text().trim().to_string();
                    if !path.is_empty() {
                        page.set_folder(path, cx);
                        page.close_menu(cx);
                    }
                }
            },
        ));
        let advanced_open = spec.as_ref().is_some_and(|s| {
            s.result_manifest.is_some() || s.review_url.is_some() || s.review_command.is_some()
        });
        // A brand new agent starts as a random shape, colour and idle loop.
        let (random_shape, random_color, random_idle) =
            crate::agent_avatar::random_pick(crate::agent_avatar::clock_seed());
        let avatar = spec.as_ref().and_then(|s| s.avatar.clone());
        let shape = avatar
            .as_ref()
            .and_then(|a| crate::agent_avatar::shape(&a.shape))
            .map(|s| s.key)
            .unwrap_or(random_shape.key);
        let color = avatar
            .as_ref()
            .map(|a| a.color.clone())
            .unwrap_or_else(|| random_color.hex());
        self.editor = Some(Editor {
            step: if spec.is_some() { 3 } else { 1 },
            shape,
            color,
            idle: random_idle,
            role,
            deliver: spec
                .as_ref()
                .and_then(|s| s.deliver)
                .unwrap_or_default(),
            max_run_minutes: match spec.as_ref() {
                Some(spec) => spec.max_run_minutes,
                None => Some(schedule::DEFAULT_MAX_RUN_MINUTES),
            },
            avatar_motion: crate::agent_avatar::AvatarMotion::default(),
            name_bounds: std::rc::Rc::default(),
            role_bounds: std::rc::Rc::default(),
            gaze_field: None,
            step_at: std::time::Instant::now(),
            prev_step: if spec.is_some() { 3 } else { 1 },
            shuffled_at: None,
            locked,
            name,
            prompt,
            cwd,
            harness,
            harness_touched: spec.is_some(),
            model: spec.as_ref().and_then(|s| s.request.model.clone()),
            reasoning: spec.as_ref().and_then(|s| s.request.reasoning),
            // Auto is the default for a new automation: nobody is there to
            // answer a prompt at 09:00.
            permission: spec
                .as_ref()
                .map(|s| s.request.permission_mode())
                .unwrap_or(PermissionMode::Auto),
            interval,
            custom: !schedule::is_preset(interval),
            custom_value,
            custom_unit: unit,
            time_of_day,
            calendar_days: spec.as_ref().and_then(|s| s.weekdays.clone()).or_else(|| (interval == 1440).then(|| (0..7).collect())),
            weekday: spec
                .as_ref()
                .and_then(|s| s.weekday)
                .unwrap_or(schedule::DEFAULT_WEEKDAY)
                .min(6),
            effort_bounds: std::rc::Rc::new(std::cell::Cell::new(None)),
            result_manifest,
            review_url,
            review_command,
            advanced_open,
            attempted: false,
            focus_pending: true,
            model_search,
            model_scroll: gpui::UniformListScrollHandle::new(),
            folder_path,
            original,
            _subscriptions: subscriptions,
        });
        self.menu = Popup::default();
        self.error = None;
        self.ensure_harnesses(false, cx);
        self.ensure_models(harness, false, cx);
        cx.notify();
    }

    /// A new automation whose remembered agent turned out not to be offered
    /// on this device switches to the first offered agent.
    pub(super) fn adopt_offered_harness(&mut self, cx: &mut Context<Self>) {
        let Some(offered) = self.offered_harnesses() else {
            return;
        };
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        if editor.original.is_some()
            || editor.harness_touched
            || offered.iter().any(|d| d.id == editor.harness)
        {
            return;
        }
        let Some(first) = offered.first() else {
            return;
        };
        editor.harness = first.id;
        editor.model = None;
        editor.reasoning = None;
        let harness = editor.harness;
        self.ensure_models(harness, false, cx);
    }

    pub(super) fn close_editor(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.editor = None;
        self.menu = Popup::default();
        self.error = None;
        cx.notify();
    }

    pub(super) fn close_menu(&mut self, cx: &mut Context<Self>) {
        if self.menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.menu);
        }
        cx.notify();
    }

    fn toggle_menu(&mut self, kind: Menu, window: &mut Window, cx: &mut Context<Self>) {
        // The card's outside-press already began closing this same menu.
        if self.menu.take_press_was_open() {
            return;
        }
        if self.menu.as_open() == Some(&kind) {
            self.close_menu(cx);
            return;
        }
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let harness = editor.harness;
        match kind {
            Menu::Model => {
                let search = editor.model_search.clone();
                search.update(cx, |input, cx| input.set_text("", cx));
                window.focus(&search.focus_handle(cx), cx);
                self.ensure_models(harness, false, cx);
            }
            Menu::Agent => self.ensure_harnesses(false, cx),
            Menu::Folder => {
                let path = editor.folder_path.clone();
                path.update(cx, |input, cx| input.set_text("", cx));
            }
            Menu::Frequency | Menu::Deliver | Menu::StopAfter => {}
        }
        self.menu.open(kind);
        cx.notify();
    }

    /// Wire a trigger to open `kind`, and mount the menu under it while open.
    pub(super) fn menu_trigger(
        &mut self,
        trigger: gpui::Stateful<gpui::Div>,
        kind: Menu,
        disabled: bool,
        align_end: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        if disabled {
            return trigger;
        }
        let trigger = trigger
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |page, _, _, _| {
                    page.menu.note_trigger_press_matching(|open| *open == kind)
                }),
            )
            .on_click(cx.listener(move |page, _, window, cx| page.toggle_menu(kind, window, cx)));
        if self.menu.get() != Some(&kind) {
            return trigger;
        }
        let closing = self.menu.closing_since();
        let content = self.render_menu(kind, cx);
        trigger.child(popover::dialog_menu_below(
            SharedString::from(format!("automation-menu-{kind:?}")),
            content,
            closing,
            align_end,
        ))
    }

    fn set_folder(&mut self, path: String, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut() {
            editor.cwd = path;
        }
        cx.notify();
    }

    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        self.close_menu(cx);
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose folder".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = receiver.await
                && let Some(path) = paths.pop()
            {
                this.update(cx, |page, cx| {
                    page.set_folder(path.to_string_lossy().into_owned(), cx)
                })
                .ok();
            }
        })
        .detach();
    }

    fn model_ladder(&self, harness: HarnessId, model: &str) -> Vec<ReasoningLevel> {
        self.models
            .get(&harness)
            .and_then(Loadable::ready)
            .and_then(|models| models.iter().find(|m| m.id == model))
            .map(|m| m.reasoning_levels.clone())
            .filter(|ladder| !ladder.is_empty())
            .or_else(|| {
                self.harnesses
                    .ready()
                    .and_then(|list| list.iter().find(|d| d.id == harness))
                    .map(|d| d.reasoning_levels.clone())
            })
            .unwrap_or_default()
    }

    fn pick_model(&mut self, model: Option<String>, cx: &mut Context<Self>) {
        let Some(harness) = self.editor.as_ref().map(|editor| editor.harness) else {
            return;
        };
        // A model starts on the Effort last used with it in the composer,
        // else its own default.
        let reasoning = model.as_deref().and_then(|id| {
            let ladder = self.model_ladder(harness, id);
            (!ladder.is_empty())
                .then(|| {
                    crate::pickers::resolve_effort(self.defaults.effort_for(harness, id), &ladder)
                })
                .flatten()
        });
        if let Some(editor) = self.editor.as_mut() {
            editor.model = model;
            editor.reasoning = reasoning;
        }
        self.close_menu(cx);
    }

    fn model_items(&self, cx: &App) -> Vec<ModelItem> {
        let Some(editor) = self.editor.as_ref() else {
            return Vec::new();
        };
        let query = editor.model_search.read(cx).text().to_string();
        self.models
            .get(&editor.harness)
            .and_then(Loadable::ready)
            .map(|models| schedule::model_items(editor.harness, models, &query))
            .unwrap_or_default()
    }

    fn pick_first_model(&mut self, cx: &mut Context<Self>) {
        if self.menu.as_open() != Some(&Menu::Model) {
            return;
        }
        let Some(harness) = self.editor.as_ref().map(|editor| editor.harness) else {
            return;
        };
        let first = self
            .model_items(cx)
            .into_iter()
            .find_map(|item| match item {
                ModelItem::Model(ix) => self
                    .models
                    .get(&harness)
                    .and_then(Loadable::ready)
                    .and_then(|models| models.get(ix))
                    .map(|m| Some(m.id.clone())),
                ModelItem::Default => Some(None),
                ModelItem::Divider => None,
            });
        if let Some(model) = first {
            self.pick_model(model, cx);
        }
    }

    /// A press on the Effort slider snaps to the nearest rung; the dialog's
    /// root continues the drag from there.
    fn on_effort_mouse_down(
        &mut self,
        event: &gpui::MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_effort_to(event.position.x, cx);
        cx.stop_propagation();
    }

    pub(super) fn on_effort_drag_move(
        &mut self,
        event: &gpui::DragMoveEvent<crate::reasoning_slider::EffortDrag>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.drag_effort_to(event.event.position.x, cx);
    }

    /// Map a window-space pointer x onto the track and pick that rung.
    fn drag_effort_to(&mut self, pointer_x: gpui::Pixels, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let Some(bounds) = editor.effort_bounds.get() else {
            return; // never painted - nothing to map against
        };
        let Some(model) = editor.model.clone() else {
            return;
        };
        let ladder = self.model_ladder(editor.harness, &model);
        if ladder.is_empty() {
            return;
        }
        let ix = crate::reasoning_slider::snap_step(
            f32::from(pointer_x - bounds.left()),
            f32::from(bounds.size.width),
            ladder.len(),
        );
        let Some(level) = ladder.get(ix).copied() else {
            return;
        };
        if let Some(editor) = self.editor.as_mut() {
            if editor.reasoning == Some(level) {
                return; // already there: no repaint churn while dragging
            }
            editor.reasoning = Some(level);
        }
        cx.notify();
    }

    pub(super) fn check(&self, cx: &App) -> Option<Check> {
        let editor = self.editor.as_ref()?;
        let cwd = editor.cwd.trim();
        let folder = if cwd.is_empty() {
            Err("Choose a project or folder for the agent to work in.")
        } else if !std::path::Path::new(cwd).is_absolute() {
            Err("Use a full folder path, starting from the drive or root.")
        } else {
            Ok(())
        };
        let interval = if editor.custom {
            schedule::custom_interval(editor.custom_value.read(cx).text(), editor.custom_unit)
        } else {
            Ok(editor.interval)
        };
        Some(Check {
            name: !trimmed(&editor.name, cx).is_empty(),
            prompt: !trimmed(&editor.prompt, cx).is_empty(),
            folder,
            interval,
            time: schedule::parse_time_of_day(editor.time_of_day.read(cx).text()),
            results: parse_result_settings(
                &trimmed(&editor.result_manifest, cx),
                &trimmed(&editor.review_url, cx),
                &trimmed(&editor.review_command, cx),
            ),
        })
    }

    pub(super) fn build_spec(&self, check: &Check, cx: &App) -> Option<Value> {
        let editor = self.editor.as_ref()?;
        let original = editor.original.as_ref().map(|row| row["spec"].clone());
        let mut spec = original.clone().unwrap_or_else(|| {
            json!({
                "paused": true,
                "request": {"sandbox": "workspace-write", "permission": "ask", "autoApprove": false,
                    "model": null, "reasoning": null, "resume": null}
            })
        });
        spec["name"] = json!(trimmed(&editor.name, cx));
        spec["avatar"] = json!({"shape": editor.shape, "color": editor.color});
        let role = trimmed(&editor.role, cx);
        spec["role"] = json!((!role.is_empty()).then_some(role));
        if editor.locked {
            // The engine only accepts a rename (and pause) while a run is active.
            return Some(spec);
        }
        let manifest = trimmed(&editor.result_manifest, cx);
        let review_url = trimmed(&editor.review_url, cx);
        spec["resultManifest"] = json!((!manifest.is_empty()).then_some(manifest));
        spec["reviewUrl"] = json!((!review_url.is_empty()).then_some(review_url));
        spec["reviewCommand"] = json!(check.results.clone().ok().flatten());
        spec["deliver"] = json!(editor.deliver);
        spec["maxRunMinutes"] = json!(editor.max_run_minutes);
        let minutes = check.interval.ok()?;
        if editor.calendar_days.as_ref().is_some_and(|days| days.is_empty()) { return None; }
        spec["weekdays"] = json!(editor.calendar_days);
        spec["intervalMinutes"] = json!(minutes);
        // Only day-scale schedules carry an anchor; a weekday needs a time.
        let time = schedule::supports_time_of_day(minutes)
            .then(|| check.time.ok())
            .flatten();
        spec["timeOfDay"] = json!(time);
        spec["weekday"] =
            json!(time.and_then(|_| schedule::supports_weekday(minutes).then_some(editor.weekday)));
        let cwd = editor.cwd.trim().to_string();
        spec["spaceId"] = json!(
            self.local_spaces(cx)
                .into_iter()
                .find(|space| same_path(&space.path, &cwd))
                .map(|space| space.id)
        );
        let request = &mut spec["request"];
        let original_request = original.as_ref().map(|spec| &spec["request"]);
        let harness = json!(editor.harness);
        let model = json!(editor.model);
        // Option picks were validated for the previous agent/model only.
        let run_target_changed =
            original_request.is_some_and(|r| r["harness"] != harness || r["model"] != model);
        request["prompt"] = json!(trimmed(&editor.prompt, cx));
        request["cwd"] = json!(cwd);
        request["harness"] = harness;
        request["model"] = model;
        request["reasoning"] = json!(editor.model.as_ref().and(editor.reasoning));
        request["permission"] = json!(editor.permission);
        request["autoApprove"] = json!(editor.permission.auto_approves());
        if run_target_changed && let Some(options) = request["modelOptions"].as_object_mut() {
            options.retain(|key, _| key == "instructionRoot" || key == "originalProjectPath");
        }
        Some(spec)
    }

    pub(super) fn save_editor(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(check) = self.check(cx) else {
            return;
        };
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        let locked = editor.locked;
        let anchored = check
            .interval
            .is_ok_and(|minutes| schedule::supports_time_of_day(minutes));
        let blocked = !check.name
            || (!locked
                && (!check.prompt
                    || check.folder.is_err()
                    || check.interval.is_err()
                    || editor.calendar_days.as_ref().is_some_and(|days| days.is_empty())
                    || (anchored && check.time.is_err())
                    || check.results.is_err()));
        if blocked {
            editor.attempted = true;
            if check.results.is_err() {
                editor.advanced_open = true;
            }
            cx.notify();
            return;
        }
        let id = editor.original.as_ref().map(|row| text(row, "id"));
        let Some(spec) = self.build_spec(&check, cx) else {
            return;
        };
        self.persist(id, spec, true, !locked, cx);
    }

    fn render_menu(&mut self, kind: Menu, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let width = match kind {
            Menu::Folder => 380.0,
            Menu::Agent => 240.0,
            Menu::Model => MODEL_MENU_WIDTH,
            Menu::Frequency => 240.0,
            Menu::Deliver => 280.0,
            Menu::StopAfter => 200.0,
        };
        let card = popover::popover_card(&theme)
            .w(px(width))
            .flex()
            .flex_col()
            .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_menu(cx)));
        let body = match kind {
            Menu::Folder => self.render_folder_menu(&theme, cx),
            Menu::Agent => self.render_agent_menu(&theme, cx),
            Menu::Model => self.render_model_menu(&theme, cx),
            Menu::Frequency => self.render_frequency_menu(&theme, cx),
            Menu::Deliver => self.render_deliver_menu(&theme, cx),
            Menu::StopAfter => self.render_stop_after_menu(&theme, cx),
        };
        card.child(body).into_any_element()
    }

    fn render_folder_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return div().into_any_element();
        };
        let cwd = editor.cwd.clone();
        let folder_path = editor.folder_path.clone();
        let spaces = self.local_spaces(cx);
        let mut list = div()
            .id("automation-folder-list")
            .max_h(px(260.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(2.0));
        let folder_row = |id: String, name: String, path: &str, selected: bool| {
            option_row(theme, id, selected)
                .child(
                    crate::icons::icon(crate::icons::FOLDER)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().truncate().child(SharedString::from(name)))
                        .child(
                            div()
                                .truncate()
                                .text_size(ui_rems(11.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(schedule::display_path(path))),
                        ),
                )
                .when(selected, |el| el.child(check_mark(theme)))
        };
        if !cwd.is_empty() && !spaces.iter().any(|space| same_path(&space.path, &cwd)) {
            list = list
                .child(popover::menu_heading(theme, "Selected folder"))
                .child(folder_row(
                    "automation-folder-current".into(),
                    schedule::folder_name(&schedule::display_path(&cwd)),
                    &cwd,
                    true,
                ));
        }
        list = list.child(popover::menu_heading(theme, "Projects"));
        if spaces.is_empty() {
            list = list.child(menu_note(
                theme,
                "No projects on this device yet. Choose a folder instead.",
            ));
        }
        for space in spaces {
            let selected = same_path(&space.path, &cwd);
            let path = space.path.clone();
            list = list.child(
                folder_row(
                    format!("automation-folder-{}", space.id),
                    space.display_name().to_string(),
                    &space.path,
                    selected,
                )
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.set_folder(path.clone(), cx);
                    page.close_menu(cx);
                })),
            );
        }
        div()
            .flex()
            .flex_col()
            .child(list)
            .child(popover::menu_separator())
            .child(
                option_row(theme, "automation-folder-choose", false)
                    .child(
                        crate::icons::icon(crate::icons::FOLDER_WITH_FILES)
                            .size(px(14.0))
                            .flex_none()
                            .text_color(theme.text_muted),
                    )
                    .child("Choose folder...")
                    .on_click(cx.listener(|page, _, _, cx| page.choose_folder(cx))),
            )
            .child(div().mt(px(4.0)).child(popover::search_input_frame(
                theme,
                folder_path.into_any_element(),
            )))
            .into_any_element()
    }

    fn render_agent_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(current) = self.editor.as_ref().map(|editor| editor.harness) else {
            return div().into_any_element();
        };
        let mut menu = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(theme, "Agent"));
        match &self.harnesses {
            Loadable::Ready(_) => {
                let offered = self.offered_harnesses().unwrap_or_default();
                if offered.is_empty() {
                    menu = menu.child(menu_note(
                        theme,
                        "No agents available. Enable an installed agent in Settings > Agents.",
                    ));
                }
                for descriptor in offered {
                    let harness = descriptor.id;
                    let selected = harness == current;
                    let (icon_path, tint) = crate::pickers::harness_brand_icon(harness);
                    menu = menu.child(
                        option_row(theme, format!("automation-agent-{harness:?}"), selected)
                            .child(
                                crate::icons::icon(icon_path)
                                    .size(px(14.0))
                                    .flex_none()
                                    .text_color(tint.unwrap_or(theme.text)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(SharedString::from(descriptor.name.clone())),
                            )
                            .when(selected, |el| el.child(check_mark(theme)))
                            .on_click(cx.listener(move |page, _, _, cx| {
                                if let Some(editor) = page.editor.as_mut() {
                                    if editor.harness != harness {
                                        editor.harness = harness;
                                        editor.model = None;
                                        editor.reasoning = None;
                                    }
                                    editor.harness_touched = true;
                                }
                                page.ensure_models(harness, false, cx);
                                page.close_menu(cx);
                            })),
                    );
                }
            }
            Loadable::Error(message) => {
                menu = menu
                    .child(popover::error_row(
                        theme,
                        &format!("Could not load agents: {message}"),
                    ))
                    .child(
                        option_row(theme, "automation-agent-retry", false)
                            .child("Retry")
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.ensure_harnesses(true, cx);
                                cx.notify();
                            })),
                    );
            }
            Loadable::Idle | Loadable::Loading => {
                menu = menu.child(popover::skeleton_menu_rows(
                    "automation-agent-skeleton",
                    theme,
                    4,
                    cx.entity_id(),
                    cx,
                ));
            }
        }
        menu.into_any_element()
    }

    fn render_model_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return div().into_any_element();
        };
        let harness = editor.harness;
        let search = editor.model_search.clone();
        let scroll = editor.model_scroll.clone();
        let selected_model = editor.model.clone();
        let reasoning = editor.reasoning;
        let searching = !search.read(cx).text().trim().is_empty();
        let search_row = popover::search_input_frame(
            theme,
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    crate::icons::icon(crate::icons::MAGNIFER)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.7)),
                )
                .child(div().flex_1().min_w_0().child(search))
                .into_any_element(),
        );
        let list: AnyElement = match self.models.get(&harness) {
            Some(Loadable::Ready(_)) => {
                let items = Arc::new(self.model_items(cx));
                if items.is_empty() {
                    menu_note(
                        theme,
                        if searching {
                            "No models match your search."
                        } else {
                            "This agent did not list any models."
                        },
                    )
                    .into_any_element()
                } else {
                    let entity = cx.entity();
                    gpui::uniform_list(
                        "automation-model-list",
                        items.len(),
                        move |range, _, app| {
                            entity.update(app, |page, cx| {
                                range
                                    .map(|ix| page.render_model_item(harness, items[ix], ix, cx))
                                    .collect::<Vec<_>>()
                            })
                        },
                    )
                    .size_full()
                    .track_scroll(&scroll)
                    .into_any_element()
                }
            }
            Some(Loadable::Error(message)) => div()
                .flex()
                .flex_col()
                .child(popover::error_row(
                    theme,
                    &format!("Could not load models: {message}"),
                ))
                .child(
                    option_row(theme, "automation-model-retry", false)
                        .child("Retry")
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.ensure_models(harness, true, cx);
                            cx.notify();
                        })),
                )
                .into_any_element(),
            _ => popover::skeleton_menu_rows(
                "automation-model-skeleton",
                theme,
                6,
                cx.entity_id(),
                cx,
            ),
        };
        let mut menu = div()
            .flex()
            .flex_col()
            .child(search_row)
            .child(div().h(px(MODEL_LIST_HEIGHT)).child(list));
        let ladder = selected_model
            .as_deref()
            .map(|id| self.model_ladder(harness, id))
            .unwrap_or_default();
        let effort_bounds = self
            .editor
            .as_ref()
            .map(|editor| editor.effort_bounds.clone())
            .unwrap_or_default();
        if !ladder.is_empty() {
            // The same slider the composer's model picker uses, so the two
            // read identically: provider ink, ticks, sparkles on the top rung.
            let current_ix = reasoning
                .and_then(|level| ladder.iter().position(|l| *l == level))
                .unwrap_or(0);
            let at_max = current_ix + 1 == ladder.len();
            let sparkle_t = (at_max && !crate::motion::reduced_motion(cx)).then(|| {
                crate::motion::pulse_delta(&crate::motion::EFFORT_SPARKLE, cx.entity_id(), cx)
            });
            let level_label = ladder
                .get(current_ix)
                .copied()
                .map(crate::pickers::reasoning_label)
                .unwrap_or("");
            let slider = crate::reasoning_slider::EffortSlider {
                id: "automation-effort",
                levels: ladder,
                current: current_ix,
                harness: Some(harness),
                width: EFFORT_TRACK_WIDTH,
                sparkle_t,
                bounds: effort_bounds,
            }
            .render(theme, cx);
            let caption = div()
                .w_full()
                .flex()
                .items_baseline()
                .justify_between()
                .pb(px(3.0))
                .child(
                    div()
                        .text_size(ui_rems(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(popover::tracked_upper(
                            crate::reasoning_slider::EFFORT_LABEL,
                        ))),
                )
                .child(
                    div()
                        .text_size(ui_rems(11.0))
                        .text_color(theme.text.opacity(0.85))
                        .child(level_label),
                );
            menu = menu.child(popover::menu_separator()).child(
                div()
                    .px(px(EFFORT_ROW_PAD))
                    .pt(px(6.0))
                    .pb(px(8.0))
                    .flex()
                    .flex_col()
                    .child(caption)
                    .child(
                        div()
                            .id("automation-effort-slider")
                            .w(px(EFFORT_TRACK_WIDTH))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(Self::on_effort_mouse_down),
                            )
                            .on_drag(crate::reasoning_slider::EffortDrag, |_, _, _, cx| {
                                cx.stop_propagation();
                                cx.new(|_| crate::reasoning_slider::EffortDragGhost)
                            })
                            .child(slider),
                    ),
            );
        }
        menu.into_any_element()
    }

    fn render_model_item(
        &mut self,
        harness: HarnessId,
        item: ModelItem,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let selected_id = self.editor.as_ref().and_then(|editor| editor.model.clone());
        let (name, tagline, id, selected) = match item {
            ModelItem::Divider => {
                return div()
                    .h(px(MODEL_ROW_HEIGHT))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .text_size(ui_rems(10.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text_muted.opacity(0.55))
                            .child(SharedString::from(popover::tracked_upper("Other"))),
                    )
                    .child(div().flex_1().h(px(1.0)).bg(hairline(0.10)))
                    .into_any_element();
            }
            ModelItem::Default => (
                "Default model".to_string(),
                Some("The agent's own choice".to_string()),
                None,
                selected_id.is_none(),
            ),
            ModelItem::Model(model_ix) => {
                let Some(model) = self
                    .models
                    .get(&harness)
                    .and_then(Loadable::ready)
                    .and_then(|models| models.get(model_ix))
                else {
                    return div().h(px(MODEL_ROW_HEIGHT)).into_any_element();
                };
                let display = crate::model_display::display_model(harness, &model.id, &model.label);
                let tagline = display.tagline(false).or_else(|| {
                    model
                        .description
                        .as_deref()
                        .map(str::trim)
                        .filter(|d| !d.is_empty())
                        .map(str::to_owned)
                });
                (
                    display.name,
                    tagline,
                    Some(model.id.clone()),
                    selected_id.as_deref() == Some(model.id.as_str()),
                )
            }
        };
        div()
            .id(("automation-model-row", ix))
            .h(px(MODEL_ROW_HEIGHT))
            .px(px(8.0))
            .rounded(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .when(selected, |el| el.bg(crate::theme::card_selected_bg()))
            .when(!selected, |el| el.hover(|s| s.bg(ink(0.05))))
            .child(
                div()
                    .flex_none()
                    .max_w(px(200.0))
                    .truncate()
                    .text_size(ui_rems(12.5))
                    .font_weight(gpui::FontWeight::MEDIUM)
                    .text_color(theme.text)
                    .child(SharedString::from(name)),
            )
            .children(tagline.map(|tagline| {
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(ui_rems(11.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child(SharedString::from(tagline))
            }))
            .child(div().flex_1())
            .when(selected, |el| el.child(check_mark(&theme)))
            .on_click(cx.listener(move |page, _, _, cx| page.pick_model(id.clone(), cx)))
            .into_any_element()
    }

    fn render_frequency_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(editor) = self.editor.as_ref() else {
            return div().into_any_element();
        };
        let (custom, interval) = (editor.custom, editor.interval);
        let mut menu = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(theme, "Frequency"));
        for minutes in schedule::PRESETS {
            let selected = !custom && interval == minutes
                && !editor.calendar_days.as_ref().is_some_and(|days| days.len() != 7);
            menu = menu.child(
                option_row(theme, format!("automation-frequency-{minutes}"), selected)
                    .child(
                        div()
                            .flex_1()
                            .child(SharedString::from(schedule::describe_interval(minutes))),
                    )
                    .when(selected, |el| el.child(check_mark(theme)))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        if let Some(editor) = page.editor.as_mut() {
                            editor.custom = false;
                            editor.interval = minutes;
                            editor.calendar_days = (minutes == 1440).then(|| (0..7).collect());
                        }
                        page.close_menu(cx);
                    })),
            );
        }
        menu.child(popover::menu_separator())
            .child(
                option_row(theme, "automation-frequency-custom", custom)
                    .child(div().flex_1().child("Custom interval..."))
                    .when(custom, |el| el.child(check_mark(theme)))
                    .on_click(cx.listener(|page, _, window, cx| {
                        if let Some(editor) = page.editor.as_mut() {
                            if !editor.custom {
                                let (count, unit) = schedule::split_interval(editor.interval);
                                editor.custom = true;
                                editor.calendar_days = None;
                                editor.custom_unit = unit;
                                editor
                                    .custom_value
                                    .update(cx, |input, cx| input.set_text(count.to_string(), cx));
                            }
                            window.focus(&editor.custom_value.focus_handle(cx), cx);
                        }
                        page.close_menu(cx);
                    })),
            )
            .into_any_element()
    }

    fn render_deliver_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(current) = self.editor.as_ref().map(|editor| editor.deliver) else {
            return div().into_any_element();
        };
        let mut menu = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(theme, "Deliver result to"));
        for choice in schedule::DELIVER_CHOICES {
            let selected = choice == current;
            menu = menu.child(
                option_row(
                    theme,
                    format!(
                        "automation-deliver-{}-{}",
                        choice.inbox, choice.desktop_notification
                    ),
                    selected,
                )
                .child(
                    div()
                        .flex_1()
                        .child(SharedString::from(schedule::deliver_label(choice))),
                )
                .when(selected, |el| el.child(check_mark(theme)))
                .on_click(cx.listener(move |page, _, _, cx| {
                    if let Some(editor) = page.editor.as_mut() {
                        editor.deliver = choice;
                    }
                    page.close_menu(cx);
                })),
            );
        }
        menu.into_any_element()
    }

    fn render_stop_after_menu(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(current) = self.editor.as_ref().map(|editor| editor.max_run_minutes) else {
            return div().into_any_element();
        };
        let mut menu = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(popover::menu_heading(theme, "Stop after"));
        for choice in schedule::STOP_AFTER {
            let selected = choice == current;
            menu = menu.child(
                option_row(
                    theme,
                    format!("automation-stop-after-{}", choice.unwrap_or(0)),
                    selected,
                )
                .child(
                    div()
                        .flex_1()
                        .child(SharedString::from(schedule::stop_after_label(choice))),
                )
                .when(selected, |el| el.child(check_mark(theme)))
                .on_click(cx.listener(move |page, _, _, cx| {
                    if let Some(editor) = page.editor.as_mut() {
                        editor.max_run_minutes = choice;
                    }
                    page.close_menu(cx);
                })),
            );
        }
        menu.into_any_element()
    }

    pub(super) fn render_instructions(
        &mut self,
        theme: &Theme,
        check: &Check,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let Some(editor) = self.editor.as_ref() else {
            return div();
        };
        let locked = editor.locked;
        let attempted = editor.attempted;
        let prompt = editor.prompt.clone();
        let cwd = editor.cwd.clone();
        let harness = editor.harness;
        let model = editor.model.clone();
        let reasoning = editor.reasoning;
        let open_menu = self.menu.as_open().copied();
        let menu_open = |kind| open_menu == Some(kind);

        let folder_chip = footer_chip(theme, "automation-folder", menu_open(Menu::Folder), locked)
            .flex_shrink_1()
            .child(
                crate::icons::icon(crate::icons::FOLDER)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
            .map(|el| {
                if cwd.trim().is_empty() {
                    el.child(div().truncate().child("Work in a project or folder"))
                } else {
                    let display = schedule::display_path(&cwd);
                    el.child(
                        div()
                            .flex_none()
                            .max_w(px(160.0))
                            .truncate()
                            .text_color(theme.text)
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(schedule::folder_name(&display))),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(display)),
                    )
                }
            })
            .child(chevron(theme));
        let folder_chip = self.menu_trigger(folder_chip, Menu::Folder, locked, false, cx);

        let (icon_path, tint) = crate::pickers::harness_brand_icon(harness);
        let agent_chip = footer_chip(theme, "automation-agent", menu_open(Menu::Agent), locked)
            .flex_none()
            .child(
                crate::icons::icon(icon_path)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(tint.unwrap_or(theme.text)),
            )
            .child(SharedString::from(self.agent_label(harness)))
            .child(chevron(theme));
        let agent_chip = self.menu_trigger(agent_chip, Menu::Agent, locked, true, cx);

        let model_text = match &model {
            Some(id) => {
                let name = self.model_label(harness, id);
                match reasoning {
                    Some(level) => format!(
                        "{name} · {}",
                        crate::reasoning_slider::short_level_label(level)
                    ),
                    None => name,
                }
            }
            None => "Default model".to_string(),
        };
        let model_chip = footer_chip(theme, "automation-model", menu_open(Menu::Model), locked)
            .flex_shrink_1()
            .child(
                div()
                    .min_w_0()
                    .max_w(px(180.0))
                    .truncate()
                    .child(SharedString::from(model_text)),
            )
            .child(chevron(theme));
        let model_chip = self.menu_trigger(model_chip, Menu::Model, locked, true, cx);

        let prompt_focus = prompt.clone();
        let invalid = attempted && !check.prompt;
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(label_row(theme, "Instructions", true))
            .child(
                div()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(if invalid {
                        theme.danger.opacity(0.55)
                    } else {
                        hairline(0.08)
                    })
                    .bg(ink(0.04))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("automation-prompt-area")
                            .px(px(12.0))
                            .pt(px(10.0))
                            .pb(px(8.0))
                            .min_h(px(120.0))
                            .max_h(px(260.0))
                            .text_size(ui_rems(14.0))
                            .cursor_text()
                            .when(locked, |el| el.opacity(0.6))
                            .on_click(cx.listener(move |_, _, window, cx| {
                                window.focus(&prompt_focus.focus_handle(cx), cx);
                            }))
                            .child(prompt),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .px(px(6.0))
                            .py(px(5.0))
                            .border_t_1()
                            .border_color(hairline(0.06))
                            .child(div().min_w_0().flex().child(folder_chip))
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(2.0))
                                    .child(agent_chip)
                                    .child(model_chip),
                            ),
                    ),
            )
            .when(invalid, |el| {
                el.child(inline_error(theme, "Describe what the agent should do."))
            })
            .when_some(
                check.folder.err().filter(|_| attempted && !locked),
                |el, message| el.child(inline_error(theme, message)),
            )
    }

    pub(super) fn render_frequency(
        &mut self,
        theme: &Theme,
        check: &Check,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let Some(editor) = self.editor.as_ref() else {
            return div();
        };
        let locked = editor.locked;
        let custom = editor.custom;
        let unit = editor.custom_unit;
        let value = editor.custom_value.clone();
        let interval = editor.interval;
        let time_input = editor.time_of_day.clone();
        let weekday = editor.weekday;
        let calendar_days = editor.calendar_days.clone();
        let label = if custom || calendar_days.as_ref().is_some_and(|days| days.len() != 7) {
            "Custom".to_string()
        } else {
            schedule::describe_interval(interval)
        };
        let trigger = select_trigger(
            theme,
            "automation-frequency",
            self.menu.as_open() == Some(&Menu::Frequency),
            locked,
        )
        .child(
            crate::icons::icon(crate::icons::CLOCK_CIRCLE)
                .size(px(14.0))
                .flex_none()
                .text_color(theme.text_muted),
        )
        .child(div().flex_1().child(SharedString::from(label)))
        .child(chevron(theme));
        let trigger = self.menu_trigger(trigger, Menu::Frequency, locked, false, cx);
        let frequency = div().flex_1().min_w_0().flex().flex_col().gap(px(6.0))
            .child(label_row(theme, "Frequency", false)).child(trigger);
        let mut schedule_row = div().flex().items_end().gap(px(12.0)).child(frequency);
        let mut section = div().flex().flex_col().gap(px(8.0));
        if custom {
            let mut units = div().flex().items_center().gap(px(2.0));
            for choice in IntervalUnit::ALL {
                let selected = choice == unit;
                units = units.child(
                    div()
                        .id(SharedString::from(format!("automation-unit-{choice:?}")))
                        .px(px(10.0))
                        .py(px(6.0))
                        .rounded(px(7.0))
                        .text_size(ui_rems(12.5))
                        .when(selected, |el| {
                            el.bg(ink(0.10))
                                .text_color(theme.text)
                                .font_weight(gpui::FontWeight::MEDIUM)
                        })
                        .when(!selected, |el| el.text_color(theme.text_muted))
                        .when(!locked && !selected, |el| {
                            el.cursor_pointer()
                                .hover(|s| s.bg(ink(0.06)).text_color(theme.text))
                        })
                        .child(choice.label())
                        .when(!locked, |el| {
                            el.on_click(cx.listener(move |page, _, _, cx| {
                                if let Some(editor) = page.editor.as_mut() {
                                    editor.custom_unit = choice;
                                }
                                cx.notify();
                            }))
                        }),
                );
            }
            section = section
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(
                            div()
                                .text_size(ui_rems(13.0))
                                .text_color(theme.text_muted)
                                .child("Every"),
                        )
                        .child(div().w(px(84.0)).child(text_box(
                            theme,
                            value.into_any_element(),
                            check.interval.is_err(),
                            locked,
                        )))
                        .child(units),
                )
                .child(match check.interval {
                    Ok(minutes) => help_text(
                        theme,
                        format!(
                            "Runs {}.",
                            schedule::describe_interval(minutes).to_lowercase()
                        ),
                    ),
                    Err(message) => inline_error(theme, message),
                });
        }
        let minutes = check.interval.unwrap_or(interval);
        if schedule::supports_time_of_day(minutes) {
            schedule_row = schedule_row.child(div().w(px(86.0)).flex_none().flex().flex_col().gap(px(6.0))
                .child(label_row(theme, "At", false))
                .child(text_box(theme, time_input.into_any_element(), check.time.is_err(), locked)));
            let selected_days = calendar_days.clone().unwrap_or_else(|| {
                if schedule::supports_weekday(minutes) { vec![weekday] } else { (0..7).collect() }
            });
            let mut days = div().flex_none().flex().items_center().gap(px(4.0)).h(px(38.0));
            for (ix, name) in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"].iter().enumerate() {
                let day = ix as u8;
                let selected = selected_days.contains(&day);
                let before = selected_days.clone();
                days = days.child(div().id(SharedString::from(format!("automation-weekday-{ix}")))
                    .w(px(32.0)).h(px(32.0)).flex().items_center().justify_center().rounded(px(8.0))
                    .border_1().border_color(if selected { theme.text } else { theme.border })
                    .bg(if selected { theme.text } else { gpui::transparent_black() })
                    .text_color(if selected { theme.surface } else { theme.text_muted }).text_size(ui_rems(12.0))
                    .child(*name).when(!locked, |el| el.cursor_pointer().on_click(cx.listener(move |page, _, _, cx| {
                        if let Some(editor) = page.editor.as_mut() {
                            let mut days = before.clone();
                            if days.contains(&day) { days.retain(|d| *d != day); } else { days.push(day); days.sort_unstable(); }
                            editor.calendar_days = Some(days); editor.interval = 1440; editor.custom = false;
                        }
                        cx.notify();
                    }))));
            }
            schedule_row = schedule_row.child(days);
            if selected_days.is_empty() { section = section.child(inline_error(theme, "Select at least one day.")); }
            if let Err(message) = check.time { section = section.child(inline_error(theme, message)); }
        }
        div().flex().flex_col().gap(px(8.0)).child(schedule_row).child(section)
    }

    pub(super) fn render_advanced(
        &mut self,
        theme: &Theme,
        check: &Check,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let Some(editor) = self.editor.as_ref() else {
            return div();
        };
        let open = editor.advanced_open;
        let locked = editor.locked;
        let attempted = editor.attempted;
        let manifest_input = editor.result_manifest.clone();
        let manifest = trimmed(&editor.result_manifest, cx);
        let prompt_text = editor.prompt.read(cx).text().to_string();
        let toggle = div()
            .id("automation-advanced")
            .flex()
            .items_center()
            .gap(px(6.0))
            .cursor_pointer()
            .text_size(ui_rems(13.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text)
            .child(
                crate::icons::icon(if open {
                    crate::icons::ALT_ARROW_DOWN
                } else {
                    crate::icons::ALT_ARROW_RIGHT
                })
                .size(px(13.0))
                .flex_none()
                .text_color(theme.text_muted),
            )
            .child("Advanced (optional)")
            .child(
                div()
                    .font_weight(gpui::FontWeight::NORMAL)
                    .text_size(ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.7))
                    .child("Result files and a local review page"),
            )
            .on_click(cx.listener(|page, _, _, cx| {
                if let Some(editor) = page.editor.as_mut() {
                    editor.advanced_open = !editor.advanced_open;
                }
                cx.notify();
            }));
        let mut section = div().flex().flex_col().gap(px(12.0)).child(toggle);
        if !open {
            return section;
        }
        let small_action = |id: &'static str, label: &'static str| {
            widgets::ghost_action(theme)
                .id(id)
                .flex_none()
                .hover(|s| widgets::ghost_hover(theme, s))
                .child(label)
        };
        let manifest_row = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w_0().child(text_box(
                theme,
                manifest_input.clone().into_any_element(),
                false,
                locked,
            )))
            .when(manifest.is_empty() && !locked, |el| {
                el.child(
                    small_action("automation-manifest-default", "Use default").on_click(
                        cx.listener(move |_, _, _, cx| {
                            manifest_input.update(cx, |input, cx| {
                                input.set_text(schedule::DEFAULT_MANIFEST, cx)
                            });
                        }),
                    ),
                )
            });
        let manifest_ok = !manifest.is_empty() && check.results.is_ok();
        let needs_instructions =
            manifest_ok && !prompt_text.contains(&schedule::manifest_instructions(&manifest));
        section = section
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .pl(px(19.0))
                    .child(label_row(theme, "Result manifest", false))
                    .child(help_text(
                        theme,
                        "A JSON file the agent writes when a run ends, inside the working folder. \
                         Its summary and links show up in your Inbox. Keep {{run_id}} in the path so \
                         each run gets its own file; the same id is filled into your instructions.",
                    ))
                    .child(manifest_row)
                    .when(needs_instructions && !locked, |el| {
                        el.child(
                            div().flex().child(
                                small_action(
                                    "automation-manifest-instructions",
                                    "Add writing instructions to the task",
                                )
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    if let Some(editor) = page.editor.as_ref() {
                                        let prompt = editor.prompt.clone();
                                        let manifest = manifest.clone();
                                        prompt.update(cx, |input, cx| {
                                            let next = schedule::with_manifest_instructions(
                                                input.text(),
                                                &manifest,
                                            );
                                            input.set_text(next, cx);
                                        });
                                    }
                                })),
                            ),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .pl(px(19.0))
                    .child(label_row(theme, "Review page URL", false))
                    .child(help_text(
                        theme,
                        "A page on this computer that shows a run's results, for example a small \
                         dashboard. The Inbox then offers Open review. {{run_id}} is replaced with \
                         the run's id.",
                    ))
                    .child(text_box(
                        theme,
                        editor.review_url.clone().into_any_element(),
                        false,
                        locked,
                    )),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .pl(px(19.0))
                    .child(label_row(theme, "Review server command", false))
                    .child(help_text(
                        theme,
                        "Starts the review page's server when you click Open review and it is not \
                         already running. It never runs on a schedule. Write it as a JSON list of \
                         the program and its arguments.",
                    ))
                    .child(text_box(
                        theme,
                        editor.review_command.clone().into_any_element(),
                        false,
                        locked,
                    )),
            );
        let results_error = check
            .results
            .as_ref()
            .err()
            .copied()
            .filter(|_| attempted && !locked);
        section.when_some(results_error, |el, message| {
            el.child(div().pl(px(19.0)).child(inline_error(theme, message)))
        })
    }

}
