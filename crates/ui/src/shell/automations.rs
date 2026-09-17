//! Local automation management. The engine owns schedules and run history;
//! this page lists them and hosts the create/edit dialog ([`editor`]).
mod editor;
mod schedule;
mod wizard;

use std::collections::HashMap;

use crate::{
    popover::{self, Loadable, Popup},
    settings::{composer::ComposerDefaults, widgets},
    state::AppState,
    theme::Theme,
};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, SharedString, Subscription, Task, Window, div,
    prelude::*, px,
};
use serde_json::{Value, json};
use zeron_engine::registry::HarnessDescriptor;
use zeron_proto::{HarnessId, Model};
use zeron_rpc::methods;

pub(super) enum AutomationsEvent {
    OpenChat(String),
}

pub(super) struct AutomationsPage {
    state: Entity<AppState>,
    rows: Vec<Value>,
    editor: Option<editor::Editor>,
    /// The editor dialog's one open dropdown.
    menu: Popup<editor::Menu>,
    error: Option<SharedString>,
    load_error: Option<SharedString>,
    save_generation: u64,
    loaded: bool,
    busy: bool,
    expanded: Option<String>,
    delete_confirm: Option<String>,
    /// A short confirmation under the header ("<Name> is ready, paused ...").
    notice: Option<SharedString>,
    /// After this save succeeds, start one Test run of the saved automation.
    pending_test_run: bool,
    /// The one clock behind the row avatars.
    row_motion: crate::agent_avatar::AvatarMotion,
    task: Option<Task<()>>,
    /// Agent catalog of THIS device's engine (automations always run here).
    harnesses: Loadable<Vec<HarnessDescriptor>>,
    models: HashMap<HarnessId, Loadable<Vec<Model>>>,
    catalog_task: Option<Task<()>>,
    /// The composer's remembered picks, read-only: the default agent for a
    /// new automation, the Effort remembered per model, and model labels for
    /// rows whose catalog has not loaded.
    defaults: ComposerDefaults,
    _poll: Task<()>,
    _observe: Subscription,
}
impl EventEmitter<AutomationsEvent> for AutomationsPage {}

fn text(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
fn timestamp(value: &Value) -> String {
    value
        .as_i64()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%b %d, %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "Not scheduled".into())
}
fn next_run(value: &Value) -> Option<String> {
    let at = chrono::DateTime::from_timestamp_millis(value.as_i64()?)?;
    Some(schedule::next_run_words(
        at.with_timezone(&chrono::Local).naive_local(),
        chrono::Local::now().naive_local(),
    ))
}
/// A stable seed from an automation id, for the fallback face.
fn id_seed(id: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    hash
}
fn is_active_status(status: &str) -> bool {
    matches!(status, "running" | "awaitingInput")
}
fn parse_result_settings(
    manifest: &str,
    review_url: &str,
    command: &str,
) -> Result<Option<Vec<String>>, &'static str> {
    if !manifest.is_empty()
        && (std::path::Path::new(manifest).is_absolute()
            || !manifest.contains("{{run_id}}")
            || std::path::Path::new(manifest).components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir
                        | std::path::Component::Prefix(_)
                        | std::path::Component::RootDir
                )
            }))
    {
        return Err(
            "Use a path inside the working folder that contains {{run_id}}, for example results/{{run_id}}.json.",
        );
    }
    if !review_url.is_empty() {
        let url = url::Url::parse(&review_url.replace("{{run_id}}", "run"))
            .map_err(|_| "Enter a full address, for example http://localhost:8793.")?;
        if !matches!(url.scheme(), "http" | "https")
            || !matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            )
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(
                "The review page must be on this computer (http://localhost or 127.0.0.1).",
            );
        }
    }
    if command.is_empty() {
        return Ok(None);
    }
    if review_url.is_empty() {
        return Err(
            "Add a review page URL so Zeron knows what to open after starting the command.",
        );
    }
    let args: Vec<String> = serde_json::from_str(command).map_err(
        |_| "Write the command as a JSON list, for example [\"npm\", \"run\", \"review\"].",
    )?;
    if args.is_empty() || args[0].trim().is_empty() || args.iter().any(|arg| arg.contains('\0')) {
        return Err("The command needs a program name as its first item.");
    }
    Ok(Some(args))
}
impl AutomationsPage {
    pub(super) fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let poll = cx.spawn(async move |this, cx| {
            loop {
                let engine = this
                    .update(cx, |page, cx| {
                        if page.busy {
                            None
                        } else {
                            page.state
                                .read(cx)
                                .engine()
                                .cloned()
                                .map(|engine| (engine, page.save_generation))
                        }
                    })
                    .ok()
                    .flatten();
                if let Some((engine, generation)) = engine {
                    let result = engine
                        .client()
                        .call(methods::LIST_AUTOMATIONS, json!({}))
                        .await;
                    if this
                        .update(cx, |page, cx| {
                            // A save invalidates any list request started before it.
                            if page.busy || generation != page.save_generation {
                                return;
                            }
                            match result {
                                Ok(value) => {
                                    if let Some(rows) = value.as_array() {
                                        page.rows = rows.clone();
                                        page.loaded = true;
                                        page.load_error = None;
                                    } else {
                                        page.load_error = Some(
                                            "Unexpected automation response from engine".into(),
                                        );
                                    }
                                }
                                Err(error) => {
                                    page.load_error =
                                        Some(format!("Could not load automations: {error}").into())
                                }
                            }
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(5))
                    .await;
            }
        });
        let defaults = state
            .read(cx)
            .data_dir
            .as_deref()
            .map(ComposerDefaults::load)
            .unwrap_or_default();
        Self {
            state,
            rows: vec![],
            editor: None,
            menu: Popup::default(),
            error: None,
            load_error: None,
            save_generation: 0,
            loaded: false,
            busy: false,
            expanded: None,
            delete_confirm: None,
            notice: None,
            pending_test_run: false,
            row_motion: crate::agent_avatar::AvatarMotion::default(),
            task: None,
            harnesses: Loadable::Idle,
            models: HashMap::new(),
            catalog_task: None,
            defaults,
            _poll: poll,
            _observe: observe,
        }
    }

    /// The connected engine's device: the only device automations run on.
    fn local_device_id(&self, cx: &gpui::App) -> Option<String> {
        self.state
            .read(cx)
            .engine()
            .map(|engine| engine.engine_info().device_id.clone())
    }

    fn ensure_harnesses(&mut self, force: bool, cx: &mut Context<Self>) {
        let reload = match self.harnesses {
            Loadable::Idle => true,
            Loadable::Loading => false,
            Loadable::Ready(_) | Loadable::Error(_) => force,
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if !reload {
            return;
        }
        if !matches!(self.harnesses, Loadable::Ready(_)) {
            self.harnesses = Loadable::Loading;
        }
        self.catalog_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_HARNESSES, json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.harnesses = match result {
                    Ok(value) => match serde_json::from_value::<Vec<HarnessDescriptor>>(value) {
                        Ok(list) => Loadable::Ready(list),
                        Err(error) => Loadable::Error(error.to_string()),
                    },
                    Err(error) => Loadable::Error(error.to_string()),
                };
                page.adopt_offered_harness(cx);
                cx.notify();
            })
            .ok();
        }));
    }

    fn ensure_models(&mut self, harness: HarnessId, force: bool, cx: &mut Context<Self>) {
        let reload = match self.models.get(&harness) {
            None | Some(Loadable::Idle) => true,
            Some(Loadable::Loading) => false,
            Some(Loadable::Ready(_)) | Some(Loadable::Error(_)) => force,
        };
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        if !reload {
            return;
        }
        if !matches!(self.models.get(&harness), Some(Loadable::Ready(_))) {
            self.models.insert(harness, Loadable::Loading);
        }
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_MODELS, json!({ "harness": harness }))
                .await;
            this.update(cx, |page, cx| {
                let loaded = match result {
                    Ok(value) => match serde_json::from_value::<Vec<Model>>(value) {
                        Ok(models) => {
                            Loadable::Ready(crate::pickers::normalize_model_rows(harness, models))
                        }
                        Err(error) => Loadable::Error(error.to_string()),
                    },
                    Err(error) => Loadable::Error(error.to_string()),
                };
                page.models.insert(harness, loaded);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The agents this device offers (enabled in Settings > Agents and installed).
    fn offered_harnesses(&self) -> Option<Vec<HarnessDescriptor>> {
        self.harnesses
            .ready()
            .map(|list| crate::pickers::offered_harnesses(list))
    }

    fn agent_label(&self, harness: HarnessId) -> String {
        self.harnesses
            .ready()
            .and_then(|list| list.iter().find(|d| d.id == harness))
            .map(|d| d.name.clone())
            .unwrap_or_else(|| schedule::agent_name(harness).to_string())
    }

    /// A model's short display name: the loaded catalog's label, else the
    /// label the composer last saw for that id, else the id itself, shortened
    /// for routed catalogs.
    fn model_label(&self, harness: HarnessId, id: &str) -> String {
        let label = self
            .models
            .get(&harness)
            .and_then(Loadable::ready)
            .and_then(|models| models.iter().find(|m| m.id == id))
            .map(|m| m.label.clone())
            .or_else(|| self.defaults.model_labels.get(id).cloned())
            .unwrap_or_else(|| id.to_string());
        crate::model_display::display_model(harness, id, &label).name
    }

    fn persist(
        &mut self,
        id: Option<String>,
        mut spec: Value,
        close_editor: bool,
        refresh_roots: bool,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Zeron's engine is not connected. Reconnect to save.".into());
            cx.notify();
            return;
        };
        // Pause/activate (and a name-only edit during a run) must preserve the
        // exact execution request, including its instruction roots, even if
        // the workspace configuration has since changed.
        if refresh_roots {
            let state = self.state.read(cx);
            let device_id = &engine.engine_info().device_id;
            let root = crate::settings::current(cx)
                .workspace_space_id
                .as_deref()
                .and_then(|id| state.space_row(id))
                .filter(|space| &space.device_id == device_id)
                .map(|space| space.path.clone());
            let cwd = text(&spec["request"], "cwd");
            let project = state
                .spaces
                .iter()
                .filter(|space| {
                    &space.device_id == device_id
                        && std::path::Path::new(&cwd).starts_with(&space.path)
                })
                .max_by_key(|space| space.path.len())
                .map(|space| space.path.clone());
            let mut options = spec["request"]["modelOptions"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            options.remove("instructionRoot");
            options.remove("originalProjectPath");
            if let Some(root) = root {
                options.insert("instructionRoot".into(), json!(root));
            }
            if let Some(project) = project {
                options.insert("originalProjectPath".into(), json!(project));
            }
            spec["request"]["modelOptions"] = Value::Object(options);
        }
        // Check the UI-produced payload against the same wire type the engine reads.
        if let Err(error) = serde_json::from_value::<zeron_proto::AutomationSpec>(spec.clone()) {
            self.error = Some(format!("Could not prepare automation: {error}").into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.save_generation = self.save_generation.wrapping_add(1);
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::SAVE_AUTOMATION, json!({"id":id,"spec":spec}))
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(row) => {
                        let id = text(&row, "id");
                        let name = text(&row["spec"], "name");
                        let paused = row["spec"]["paused"].as_bool().unwrap_or(true);
                        if let Some(at) = page.rows.iter().position(|v| text(v, "id") == id) {
                            page.rows[at] = row;
                        } else {
                            page.rows.push(row);
                        }
                        if close_editor {
                            page.editor = None;
                            page.menu = Popup::default();
                            page.notice = (paused && !name.is_empty()).then(|| {
                                SharedString::from(format!(
                                    "{name} is ready, paused until you turn it on."
                                ))
                            });
                        }
                        // A Test run saves the draft first, then starts once.
                        if std::mem::take(&mut page.pending_test_run) {
                            page.run_now(id, cx);
                        }
                    }
                    Err(error) => {
                        page.pending_test_run = false;
                        page.error = Some(format!("Could not save automation: {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Save the open draft, then start one run of it right away.
    pub(super) fn test_run(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.pending_test_run = true;
        self.notice = None;
        self.save_editor(cx);
        // save_editor refuses an invalid form; do not leave the flag armed.
        if !self.busy {
            self.pending_test_run = false;
        }
    }

    /// Ask the engine for one immediate run, whatever the schedule says.
    fn run_now(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Zeron's engine is not connected. Reconnect to run.".into());
            return;
        };
        self.notice = Some("Test run started in a new chat.".into());
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::RUN_AUTOMATION_NOW, json!({ "id": id }))
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(row) => {
                        let id = text(&row, "id");
                        if let Some(at) = page.rows.iter().position(|v| text(v, "id") == id) {
                            page.rows[at] = row;
                        }
                    }
                    Err(error) => {
                        page.notice = None;
                        page.error = Some(format!("Could not start the test run: {error}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The face of one automation: its saved avatar, or a face derived from its
    /// id so rows saved before avatars existed still have one.
    fn delete_automation(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy { return; }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Reconnect to delete this automation.".into()); cx.notify(); return;
        };
        self.busy = true;
        self.save_generation = self.save_generation.wrapping_add(1);
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::DELETE_AUTOMATION, json!({"id": id})).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(_) => { page.rows.retain(|row| text(row, "id") != id); page.delete_confirm = None; page.notice = None; }
                    Err(error) => page.error = Some(format!("Could not delete automation: {error}").into()),
                }
                cx.notify();
            }).ok();
        }));
        cx.notify();
    }

    fn row_avatar(&self, id: &str, spec: &Value, size: f32, phase: f32, cx: &mut Context<Self>) -> AnyElement {
        let saved = spec["avatar"].as_object();
        let fallback = crate::agent_avatar::random_pick(id_seed(id));
        let shape = saved
            .and_then(|a| a.get("shape"))
            .and_then(Value::as_str)
            .and_then(crate::agent_avatar::shape)
            .map(|s| s.key)
            .unwrap_or(fallback.0.key);
        let color = saved
            .and_then(|a| a.get("color"))
            .and_then(Value::as_str)
            .map(crate::agent_avatar::hex_color)
            .unwrap_or_else(|| fallback.1.hsla());
        let seconds = if crate::motion::reduced_motion(cx) {
            None
        } else {
            crate::motion::pulse_lease(cx.entity_id(), cx);
            Some(self.row_motion.seconds())
        };
        crate::agent_avatar::Avatar::new(shape, color, size)
            .motion(fallback.2)
            .time(seconds)
            .phase(phase)
            .render()
    }

    fn render_empty(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // No frame around this: the figure, the line and the button sit
        // directly on the page's glass.
        div()
            .mt(px(64.0))
            .mb(px(64.0))
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .child(
                crate::agent_avatar::Avatar::new(
                    "cloud",
                    theme.text_muted.opacity(0.75),
                    64.0,
                )
                .render(),
            )
            .child(
                div()
                    .mt(px(4.0))
                    .text_size(crate::typography::ui_rems(16.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child("No automations yet"),
            )
            .child(
                div()
                    .max_w(px(420.0))
                    .text_center()
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.text_muted)
                    .child(
                        "Have an agent do recurring work for you.",
                    ),
            )
            .child(
                div().mt(px(14.0)).child(
                    popover::btn_primary(theme, "New automation")
                        .id("new-automation-empty")
                        .on_click(cx.listener(|page, _, window, cx| {
                            if !page.busy {
                                page.open_editor(None, window, cx);
                            }
                        })),
                ),
            )
            .into_any_element()
    }

    fn render_row(
        &self,
        theme: &Theme,
        row: &Value,
        first: bool,
        index: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = text(row, "id");
        let spec = &row["spec"];
        let request = &spec["request"];
        let paused = spec["paused"].as_bool().unwrap_or(true);
        let harness = serde_json::from_value::<HarnessId>(request["harness"].clone()).ok();
        let mut history: Vec<Value> = row["history"].as_array().cloned().unwrap_or_default();
        history.sort_by_key(|run| std::cmp::Reverse(run["startedAt"].as_i64().unwrap_or(0)));
        let running = history
            .iter()
            .any(|run| is_active_status(&text(run, "status")));

        // Identity: "Name | Role", the way the agent introduces itself.
        let role = text(spec, "role");
        let identity = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .min_w_0()
            .child(
                div()
                    .flex_none()
                    .text_size(crate::typography::ui_rems(15.0))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(SharedString::from(text(spec, "name"))),
            )
            .when(!role.is_empty(), |el| {
                el.child(div().flex_none().w(px(1.0)).h(px(14.0)).bg(theme.border))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(crate::typography::ui_rems(13.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text.opacity(0.48))
                            .child(SharedString::from(role)),
                    )
            });

        let fragment = |copy: String| div().child(SharedString::from(copy)).into_any_element();
        let mut meta = Vec::new();
        meta.push(fragment(if let Some(days) = spec["weekdays"].as_array() {
            let days: Vec<u8> = days.iter().filter_map(|d| d.as_u64().map(|d| d as u8)).collect();
            schedule::describe_days(&days, spec["timeOfDay"].as_u64().unwrap_or(540) as u16)
        } else { schedule::describe_schedule(
            spec["intervalMinutes"].as_u64().unwrap_or(0) as u32,
            spec["timeOfDay"].as_u64().map(|time| time as u16),
            spec["weekday"].as_u64().map(|day| day as u8),
        ) }));
        meta.push(fragment(if paused {
            "Paused, not scheduled".into()
        } else if running {
            "Running now".into()
        } else {
            next_run(&row["nextRunAt"])
                .map(|at| format!("Next {at}"))
                .unwrap_or_else(|| "Next run soon".into())
        }));
        if let Some(harness) = harness {
            let model = request["model"]
                .as_str()
                .map(|model| self.model_label(harness, model))
                .unwrap_or_else(|| "Default model".into());
            meta.push(fragment(format!("{} · {model}", self.agent_label(harness))));
        }
        let cwd = text(request, "cwd");
        if !cwd.is_empty() {
            meta.push(
                div()
                    .min_w_0()
                    .truncate()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        crate::icons::icon(crate::icons::FOLDER)
                            .size(px(12.0))
                            .text_color(theme.text_muted.opacity(0.65)),
                    )
                    .child(SharedString::from(schedule::folder_name(
                        &schedule::display_path(&cwd),
                    )))
                    .into_any_element(),
            );
        }
        if let Some(run) = history.first() {
            let status = text(run, "status");
            let problem = schedule::run_status_is_problem(&status);
            meta.push(
                div()
                    .when(problem, |el| el.text_color(theme.danger_muted.opacity(0.9)))
                    .child(SharedString::from(format!(
                        "Last run {} · {}",
                        timestamp(&run["startedAt"]),
                        schedule::run_status_label(&status)
                    )))
                    .into_any_element(),
            );
        }

        // Plain status text with the same steady emerald glow as Devices.
        let status_chip = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .text_size(crate::typography::ui_rems(12.0))
            .text_color(theme.text_muted)
            .child(
                div()
                    .flex_none()
                    .size(px(6.0))
                    .rounded_full()
                    .bg(if paused {
                        theme.text_muted.opacity(0.6)
                    } else {
                        theme.success
                    })
                    .when(!paused, |el| {
                        el.shadow(vec![gpui::BoxShadow {
                            color: theme.success.opacity(0.55),
                            offset: gpui::point(px(0.0), px(0.0)),
                            blur_radius: px(6.0),
                            spread_radius: px(0.0),
                            inset: false,
                        }])
                    }),
            )
            .child(if paused {
                "Paused"
            } else if running {
                "Running"
            } else {
                "Active"
            });

        let edit_row = row.clone();
        let toggle_id = id.clone();
        let mut toggle_spec = spec.clone();
        toggle_spec["paused"] = json!(!paused);
        let history_id = id.clone();
        let action = |label: String, key: String| {
            widgets::ghost_action(theme)
                .id(SharedString::from(key))
                .hover(|s| widgets::ghost_hover(theme, s))
                .child(SharedString::from(label))
        };
        let delete_id = id.clone();
        let confirming_delete = self.delete_confirm.as_deref() == Some(id.as_str());
        let actions = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(action(if confirming_delete { "Confirm delete" } else { "Delete" }.into(), format!("delete-{id}"))
                .text_color(theme.danger).when(self.busy || running, |el| el.opacity(0.4))
                .on_click(cx.listener(move |page, _, _, cx| {
                    if page.busy || running { return; }
                    if page.delete_confirm.as_ref() == Some(&delete_id) { page.delete_automation(delete_id.clone(), cx); }
                    else { page.delete_confirm = Some(delete_id.clone()); cx.notify(); }
                })))
            .when(confirming_delete, |el| el.child(action("Cancel".into(), format!("cancel-delete-{id}"))
                .on_click(cx.listener(|page, _, _, cx| { page.delete_confirm = None; cx.notify(); }))))
            .when(!history.is_empty(), |el| {
                el.child(
                    action(
                        format!("History ({})", history.len()),
                        format!("history-{id}"),
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.expanded = if page.expanded.as_ref() == Some(&history_id) {
                            None
                        } else {
                            Some(history_id.clone())
                        };
                        cx.notify();
                    })),
                )
            })
            .child(
                action("Edit".into(), format!("edit-{id}")).on_click(cx.listener(
                    move |page, _, window, cx| {
                        if !page.busy {
                            page.open_editor(Some(edit_row.clone()), window, cx);
                        }
                    },
                )),
            );

        // The switch glides between the two states off a stable key, never the
        // row index (the list is rebuilt on every poll).
        let progress = widgets::switch_progress(&format!("automation-{id}"), !paused, cx);
        let toggle = div()
            .id(SharedString::from(format!("switch-{id}")))
            .flex_none()
            .cursor_pointer()
            .child(widgets::toggle_switch_t(theme, progress))
            .on_click(cx.listener(move |page, _, _, cx| {
                page.notice = None;
                page.persist(
                    Some(toggle_id.clone()),
                    toggle_spec.clone(),
                    false,
                    false,
                    cx,
                )
            }));

        let main = widgets::card_row(theme, true)
            .gap(px(18.0))
            .child(self.row_avatar(&id, spec, 48.0, index as f32 * -0.7, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(identity)
                    .child(widgets::meta_line(theme, meta)),
            )
            .child(actions)
            .child(status_chip)
            .child(toggle);

        let mut block = div()
            .flex()
            .flex_col()
            .when(!first, |el| el.border_t_1().border_color(theme.border))
            .child(main);
        if self.expanded.as_ref() == Some(&id) {
            let mut list = div()
                .px(px(20.0))
                .pb(px(14.0))
                .pl(px(82.0))
                .flex()
                .flex_col()
                .gap(px(2.0));
            for (ix, run) in history.iter().enumerate() {
                let chat = text(run, "chatId");
                let status = text(run, "status");
                let problem = schedule::run_status_is_problem(&status);
                list = list.child(
                    div()
                        .id(SharedString::from(format!("run-{id}-{ix}")))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(8.0))
                        .py(px(5.0))
                        .rounded(px(6.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted)
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::ink(0.05)).text_color(theme.text))
                        .child(SharedString::from(timestamp(&run["startedAt"])))
                        .child(
                            div()
                                .when(problem, |el| el.text_color(theme.danger_muted))
                                .child(schedule::run_status_label(&status)),
                        )
                        .child(div().flex_1())
                        .child("Open chat")
                        .on_click(cx.listener(move |_, _, _, cx| {
                            if !chat.is_empty() {
                                cx.emit(AutomationsEvent::OpenChat(chat.clone()));
                            }
                        })),
                );
                if let Some(error) = run["error"].as_str() {
                    list = list.child(
                        div()
                            .px(px(8.0))
                            .pb(px(4.0))
                            .text_size(crate::typography::ui_rems(11.5))
                            .text_color(theme.danger_muted.opacity(0.85))
                            .child(SharedString::from(error.to_string())),
                    );
                }
            }
            block = block.child(list);
        }
        block.into_any_element()
    }
}

impl Render for AutomationsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let state = self.state.read(cx);
        let device_id = self.local_device_id(cx).unwrap_or_default();
        let location = state
            .devices
            .iter()
            .find(|device| device.id == device_id)
            .map(|device| device.name.clone())
            .unwrap_or(device_id);
        let mut column = widgets::page_column()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(widgets::page_header(
                        &theme,
                        "Automations",
                        Some(self.rows.len()),
                    ))
                    .child(
                        popover::btn_primary(&theme, "New automation")
                            .id("new-automation")
                            .on_click(cx.listener(|page, _, window, cx| {
                                if !page.busy {
                                    page.open_editor(None, window, cx);
                                }
                            })),
                    ),
            )
            .child(widgets::page_subtitle(
                &theme,
                "Run an agent on a schedule. Each run opens as its own chat.",
            ))
            .child(
                div()
                    .mt(px(8.0))
                    .flex()
                    .items_start()
                    .gap(px(6.0))
                    .text_size(crate::typography::ui_rems(12.0))
                    .text_color(theme.text_muted.opacity(0.8))
                    .child(
                        div().flex_none().mt(px(2.0)).child(
                            crate::icons::icon(crate::icons::INFO_CIRCLE)
                                .size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.8)),
                        ),
                    )
                    .child(div().min_w_0().child(SharedString::from(format!(
                        "Runs only while the engine is running on this device ({location}). \
                         Missed runs are skipped; runs never overlap."
                    )))),
            );
        // The wizard's confirmation lives here: the page has no toast layer.
        if let Some(notice) = &self.notice {
            column = column.child(
                div()
                    .mt(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.success.opacity(0.2))
                    .bg(theme.success.opacity(0.06))
                    .text_size(crate::typography::ui_rems(12.5))
                    .text_color(theme.success_muted.opacity(0.95))
                    .child(div().flex_none().size(px(6.0)).rounded_full().bg(theme.success))
                    .child(div().min_w_0().child(notice.clone())),
            );
        }
        if let Some(error) = &self.load_error {
            column = column.child(widgets::error_strip(&theme, error.clone()));
        }
        if self.editor.is_none()
            && let Some(error) = &self.error
        {
            column = column.child(widgets::error_strip(&theme, error.clone()));
        }
        if self.rows.is_empty() {
            column = if self.loaded {
                column.child(self.render_empty(&theme, cx))
            } else {
                column.child(
                    div()
                        .mt(px(24.0))
                        .child(widgets::page_subtitle(&theme, "Loading automations...")),
                )
            };
        } else {
            let mut card = widgets::section_card(&theme);
            for (ix, row) in self.rows.clone().iter().enumerate() {
                card = card.child(self.render_row(&theme, row, ix == 0, ix, cx));
            }
            column = column.child(card);
        }
        let overlay = self.render_editor_overlay(window, cx);
        div()
            .id("automations-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
            .children(overlay)
    }
}

#[cfg(test)]
mod result_settings_tests {
    use super::parse_result_settings;
    #[test]
    fn optional_settings_and_explicit_local_review() {
        assert_eq!(parse_result_settings("", "", ""), Ok(None));
        assert_eq!(
            parse_result_settings(
                "results/{{run_id}}.json",
                "http://localhost:8793/?run={{run_id}}",
                r#"["python", "review/server.py"]"#
            ),
            Ok(Some(vec!["python".into(), "review/server.py".into()]))
        );
    }
    #[test]
    fn rejects_unscoped_paths_remote_urls_and_ambiguous_commands() {
        assert!(parse_result_settings("../{{run_id}}.json", "", "").is_err());
        assert!(parse_result_settings("results/latest.json", "", "").is_err());
        assert!(parse_result_settings("", "https://example.com", "").is_err());
        assert!(parse_result_settings("", "http://localhost:8793", "python server.py").is_err());
        assert!(parse_result_settings("", "", r#"["python"]"#).is_err());
    }
}
