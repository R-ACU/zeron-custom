//! Settings → General: how Zeron treats this machine while agents run
//! (the keep-awake sleep veto, [`crate::power`]) and what starts when the
//! user signs in ([`crate::autostart`]).
//!
//! Keep awake is the one persisted preference (`keepAwakeWhileRunning` in
//! `ui-settings.json`): every flip emits [`GeneralEvent::KeepAwakeChanged`]
//! and the shell persists it and applies it to the power thread. The
//! autostart rows persist nothing: the OS registration is the source of
//! truth, read fresh every time the page opens (the shell recreates the page
//! per visit) and again after every change. All registration work runs on
//! the background executor because it touches the registry and may spawn
//! `schtasks`.

use gpui::{Context, EventEmitter, SharedString, Task, Window, div, prelude::*, px};

use crate::autostart::{self, AutostartStatus, ItemStatus, LoginItem};
use crate::icons;
use crate::settings::widgets;
use crate::theme::Theme;

#[derive(Debug, Clone)]
pub enum GeneralEvent {
    /// The keep-awake switch flipped — persist and apply the new value.
    KeepAwakeChanged(bool),
}

pub struct GeneralPage {
    /// `keepAwakeWhileRunning`: sleep veto while an agent works (`power.rs`).
    keep_awake: bool,
    /// `None` until the first read lands.
    status: Option<AutostartStatus>,
    /// The item whose change is being applied; its switch is inert meanwhile.
    pending: Option<LoginItem>,
    error: Option<SharedString>,
    task: Option<Task<()>>,
}

fn is_switch_activation(key: &str, is_held: bool) -> bool {
    !is_held && matches!(key, "enter" | "space")
}

/// What a click on the engine row has to apply. The switch reads on when
/// EITHER mechanism starts the engine
/// ([`AutostartStatus::engine_starts_at_login`]), so switching it off must
/// clear both — the login entry and the `daemon install` task — while
/// switching it on only ever writes the login entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct EngineAction {
    /// The login registration to write, or `None` to leave it alone.
    login_item: Option<bool>,
    /// Remove the `daemon install` service registration.
    remove_service: bool,
}

fn engine_action(status: &AutostartStatus) -> EngineAction {
    if status.engine_starts_at_login() {
        EngineAction {
            // A registration the OS switched off still counts as present:
            // switching the row off deletes it instead of rewriting it.
            login_item: status.engine.registered.then_some(false),
            remove_service: status.engine_service_installed,
        }
    } else {
        EngineAction {
            login_item: Some(true),
            remove_service: false,
        }
    }
}

/// What the switch shows for `item`.
fn switch_on(status: &AutostartStatus, item: LoginItem) -> bool {
    match item {
        LoginItem::App => status.app.enabled(),
        LoginItem::Engine => status.engine_starts_at_login(),
    }
}

/// The row's target state when clicked: whatever the switch does not show.
/// A registration the OS has switched off (Task Manager) reads as off, so a
/// click re-writes it; one that launches another executable reads as on, so
/// a click removes it and the next click registers this executable.
fn toggle_target(item: &ItemStatus) -> bool {
    !item.enabled()
}

fn service_note() -> &'static str {
    if cfg!(windows) {
        "Started by the Zeron background task."
    } else {
        "Started by the zeron.service unit."
    }
}

fn system_disabled_note() -> &'static str {
    if cfg!(windows) {
        "Turned off in Task Manager under Startup apps. Switch on to allow it again."
    } else {
        "Turned off in the desktop session's startup settings. Switch on to allow it again."
    }
}

impl EventEmitter<GeneralEvent> for GeneralPage {}

impl GeneralPage {
    pub fn new(keep_awake: bool, cx: &mut Context<Self>) -> Self {
        let mut page = Self {
            keep_awake,
            status: None,
            pending: None,
            error: None,
            task: None,
        };
        if autostart::is_supported() {
            page.refresh(cx);
        }
        page
    }

    fn toggle_keep_awake(&mut self, cx: &mut Context<Self>) {
        self.keep_awake = !self.keep_awake;
        cx.emit(GeneralEvent::KeepAwakeChanged(self.keep_awake));
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let read = cx.background_spawn(async move { autostart::read_status() });
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = read.await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(status) => page.status = Some(status),
                    Err(err) => page.error = Some(err.into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn toggle(&mut self, item: LoginItem, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            return;
        }
        let Some(status) = self.status.as_ref() else {
            return;
        };
        let action = match item {
            LoginItem::App => EngineAction {
                login_item: Some(toggle_target(status.item(item))),
                remove_service: false,
            },
            LoginItem::Engine => engine_action(status),
        };
        self.pending = Some(item);
        self.error = None;
        let apply = cx.background_spawn(async move {
            // The task goes first: its uninstall also drops the fallback login
            // entry, so the write below is what decides the final state.
            let mut applied = if action.remove_service {
                autostart::remove_engine_service()
            } else {
                Ok(())
            };
            if let Some(enable) = action.login_item {
                let wrote = autostart::set_enabled(item, enable);
                if applied.is_ok() {
                    applied = wrote;
                }
            }
            // Re-read either way so the page shows what is really registered.
            (applied, autostart::read_status())
        });
        self.task = Some(cx.spawn(async move |this, cx| {
            let (applied, status) = apply.await;
            this.update(cx, |page, cx| {
                page.pending = None;
                if let Err(err) = applied {
                    page.error = Some(err.into());
                }
                match status {
                    Ok(status) => page.status = Some(status),
                    Err(err) => {
                        if page.error.is_none() {
                            page.error = Some(err.into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for GeneralPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let accent = theme.accent;
        let keep_awake = self.keep_awake;

        // Device-local power behaviour; only Windows acts on it (`crate::power`).
        let keep_awake_title = "Keep laptop awake while agents are running";
        let power_card = widgets::section_card(&theme).child(
            widgets::card_row(&theme, true)
                .child(widgets::row_tile(&theme, icons::LAPTOP))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(widgets::row_title(&theme, keep_awake_title))
                        .child(widgets::meta_line(
                            &theme,
                            vec![
                                div()
                                    .child(SharedString::from(
                                        "Prevents sleep and lid-close sleep while an agent is \
                                         working.",
                                    ))
                                    .into_any_element(),
                            ],
                        )),
                )
                .child(
                    div()
                        .id("general-keep-awake-toggle")
                        .flex_none()
                        .size(px(40.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .role(gpui::Role::Switch)
                        .aria_label(keep_awake_title)
                        .aria_toggled(if keep_awake {
                            gpui::Toggled::True
                        } else {
                            gpui::Toggled::False
                        })
                        .child(widgets::toggle_switch_t(
                            &theme,
                            widgets::switch_progress("general-keep-awake", keep_awake, cx),
                        ))
                        .tab_index(0)
                        .focus_visible(move |style| style.border_2().border_color(accent))
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_keep_awake(cx)))
                        .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                            if is_switch_activation(&event.keystroke.key, event.is_held) {
                                cx.stop_propagation();
                                this.toggle_keep_awake(cx);
                            }
                        })),
                ),
        );

        let mut column = widgets::page_column()
            .child(widgets::page_header(&theme, "General", None))
            .child(
                widgets::page_subtitle(
                    &theme,
                    "Startup behaviour and how Zeron treats this machine while agents run.",
                )
                .max_w(px(512.0))
                .line_height(px(20.0)),
            )
            .child(power_card);

        // The login rows only exist where an OS registration can be managed.
        if !autostart::is_supported() {
            return div()
                .id("general-page")
                .size_full()
                .overflow_y_scroll()
                .child(column);
        }

        let status = self.status.clone();
        let pending = self.pending;
        let loaded = status.is_some();

        let row = |item: LoginItem, first: bool, cx: &mut Context<Self>| -> gpui::Div {
            let (icon, title, description, id, motion_key) = match item {
                LoginItem::App => (
                    icons::LAPTOP,
                    "Open Zeron at login",
                    "Opens the Zeron window when you sign in.",
                    "startup-app-toggle",
                    "general-startup-app",
                ),
                LoginItem::Engine => (
                    icons::CLOCK_CIRCLE,
                    "Run engine in background at login",
                    "Starts the engine hidden when you sign in, so automations keep running \
                     after the window is closed.",
                    "startup-engine-toggle",
                    "general-startup-engine",
                ),
            };
            let on = status.as_ref().is_some_and(|s| switch_on(s, item));
            // Never greyed out: a switch the user cannot move is a dead end.
            // The engine row switches off by removing whatever starts the
            // engine, the background task included (`engine_action`); only an
            // in-flight change or a status that has not landed yet holds it.
            let interactive = pending.is_none() && status.is_some();

            let mut notes: Vec<SharedString> = Vec::new();
            if let Some(status) = status.as_ref() {
                let entry = status.item(item);
                if entry.registered && entry.disabled_by_system {
                    notes.push(system_disabled_note().into());
                }
                if let Some(other) = entry.other_executable.as_ref()
                    && entry.registered
                {
                    notes.push(
                        format!(
                            "Registered for a different Zeron executable ({other}). Switch off \
                             and on to use this one."
                        )
                        .into(),
                    );
                }
                if item == LoginItem::Engine && status.engine_service_installed {
                    notes.push(service_note().into());
                }
                if item == LoginItem::App && entry.enabled() && !status.engine_starts_at_login() {
                    notes.push(
                        "Without the background engine, automations stop when the window is \
                         closed."
                            .into(),
                    );
                }
            }
            if pending == Some(item) {
                notes.push("Applying...".into());
            }

            let mut text = div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(widgets::row_title(&theme, title))
                .child(widgets::meta_line(
                    &theme,
                    vec![
                        div()
                            .child(SharedString::from(description))
                            .into_any_element(),
                    ],
                ));
            for note in notes {
                text = text.child(
                    div()
                        .mt(px(2.0))
                        .text_size(crate::typography::ui_rems(widgets::ROW_DESCRIPTION_SIZE))
                        .text_color(theme.text_muted.opacity(0.85))
                        .child(note),
                );
            }

            let switch = div()
                .id(id)
                .flex_none()
                .size(px(40.0))
                .flex()
                .items_center()
                .justify_center()
                .role(gpui::Role::Switch)
                .aria_label(title)
                .aria_toggled(if on {
                    gpui::Toggled::True
                } else {
                    gpui::Toggled::False
                })
                .child(widgets::toggle_switch_t(
                    &theme,
                    widgets::switch_progress(motion_key, on, cx),
                ))
                .when(interactive, |el| {
                    el.tab_index(0)
                        .focus_visible(move |style| style.border_2().border_color(accent))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle(item, cx)))
                        .on_key_down(cx.listener(move |this, event: &gpui::KeyDownEvent, _, cx| {
                            if is_switch_activation(&event.keystroke.key, event.is_held) {
                                cx.stop_propagation();
                                this.toggle(item, cx);
                            }
                        }))
                });

            widgets::card_row(&theme, first)
                .when(!loaded, |el| el.opacity(0.55))
                .child(widgets::row_tile(&theme, icon))
                .child(text)
                .child(switch)
        };

        let card = widgets::section_card(&theme)
            .child(row(LoginItem::App, true, cx))
            .child(row(LoginItem::Engine, false, cx));
        column = column.child(card);

        if let Some(error) = self.error.clone() {
            column = column.child(widgets::error_strip(&theme, error));
        }

        div()
            .id("general-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registered(other: Option<&str>, disabled: bool) -> ItemStatus {
        ItemStatus {
            registered: true,
            disabled_by_system: disabled,
            other_executable: other.map(str::to_string),
        }
    }

    #[test]
    fn switches_accept_enter_or_space_once_per_press() {
        assert!(is_switch_activation("enter", false));
        assert!(is_switch_activation("space", false));
        assert!(!is_switch_activation("space", true));
        assert!(!is_switch_activation("escape", false));
    }

    #[test]
    fn toggle_turns_off_only_a_working_registration() {
        assert!(toggle_target(&ItemStatus::default()));
        assert!(!toggle_target(&registered(None, false)));
        // Disabled in Task Manager reads as off, so a click re-enables it.
        assert!(toggle_target(&registered(None, true)));
        // Registered for another executable still reads as on: click removes.
        assert!(!toggle_target(&registered(
            Some(r"C:\old\zeron.exe"),
            false
        )));
    }

    #[test]
    fn engine_row_switches_the_daemon_task_off_too() {
        let mut status = AutostartStatus {
            engine_service_installed: true,
            ..Default::default()
        };
        // The task alone makes the row read on; switching off removes it and
        // touches no login entry that does not exist.
        assert!(switch_on(&status, LoginItem::Engine));
        assert_eq!(
            engine_action(&status),
            EngineAction {
                login_item: None,
                remove_service: true,
            }
        );
        // A duplicate login entry next to the task goes with it.
        status.engine = registered(None, false);
        assert_eq!(
            engine_action(&status),
            EngineAction {
                login_item: Some(false),
                remove_service: true,
            }
        );
        assert!(!switch_on(&status, LoginItem::App));
    }

    #[test]
    fn engine_row_registers_the_login_item_when_nothing_starts_the_engine() {
        let mut status = AutostartStatus::default();
        assert!(!switch_on(&status, LoginItem::Engine));
        assert_eq!(
            engine_action(&status),
            EngineAction {
                login_item: Some(true),
                remove_service: false,
            }
        );
        // Switched off in Task Manager reads as off, so a click rewrites it.
        status.engine = registered(None, true);
        assert_eq!(
            engine_action(&status),
            EngineAction {
                login_item: Some(true),
                remove_service: false,
            }
        );
        // A working registration alone is switched off without any task work.
        status.engine = registered(None, false);
        assert_eq!(
            engine_action(&status),
            EngineAction {
                login_item: Some(false),
                remove_service: false,
            }
        );
    }
}
