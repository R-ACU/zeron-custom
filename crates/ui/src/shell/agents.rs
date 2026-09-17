//! Persistent, chat-scoped access to the agents indexed by spawn parts.
use super::*;
use crate::agent_avatar::{Avatar, PALETTE, SHAPES};
use zeron_doc::{MessagePart, SessionMessageEntry, SubagentStatus};
use zeron_proto::ToolCall;

#[derive(Clone)]
struct Agent {
    key: String,
    doc: Option<String>,
    name: String,
    task: String,
    tail: Option<String>,
    status: &'static str,
    running: bool,
    seed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, status: SubagentStatus) -> SessionMessageEntry {
        SessionMessageEntry {
            id: id.into(),
            role: zeron_doc::MessageRole::Assistant,
            created_at: 0,
            device_id: "test".into(),
            status: None,
            continuation_of: None,
            parts: vec![MessagePart::Tool {
                id: "spawn".into(),
                call: ToolCall::Unknown {
                    name: "Agent: Review".into(),
                    input: Some(serde_json::json!({"name":"Ada", "description":"Review changes"})),
                },
                resolved: true,
                is_error: false,
                output: None,
                diff: None,
                output_ref: None,
                output_bytes: None,
                diff_ref: None,
                diff_stats: None,
                subagent_ref: Some(format!("{id}--sub--spawn")),
                subagent_status: Some(status),
                subagent_tail: None,
            }],
        }
    }

    #[test]
    fn eager_spawn_result_does_not_finish_child_and_identity_survives_sorting() {
        let done = entry("first", SubagentStatus::Done);
        let running = entry("second", SubagentStatus::Running);
        let before = agents(&[done.clone(), running.clone()]);
        assert_eq!(before[0].doc.as_deref(), Some("second--sub--spawn"));
        assert!(before[0].running);
        assert_eq!(before[0].name, "Ada");
        assert_eq!(before[0].task, "Review changes");
        let after = agents(&[done, entry("second", SubagentStatus::Done)]);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].seed, agents(&[running])[0].seed);
        assert!(after.is_empty());
        assert!(agents(&[entry("failed", SubagentStatus::Failed)]).is_empty());
    }

    #[test]
    fn stray_reference_on_regular_tool_is_not_an_agent() {
        let mut message = entry("first", SubagentStatus::Running);
        if let MessagePart::Tool { call, .. } = &mut message.parts[0] {
            *call = ToolCall::Unknown {
                name: "Read".into(),
                input: None,
            };
        }
        assert!(agents(&[message]).is_empty());
        assert!(agents(&[]).is_empty());
    }
}

fn agents(entries: &[SessionMessageEntry]) -> Vec<Agent> {
    let mut agents = Vec::new();
    for entry in entries {
        for part in &entry.parts {
            let MessagePart::Tool {
                id,
                call,
                resolved,
                is_error,
                subagent_ref,
                subagent_status,
                subagent_tail,
                ..
            } = part
            else {
                continue;
            };
            if !call.is_subagent_spawn() {
                continue;
            }
            let (label, input) = match call {
                ToolCall::Unknown { name, input } => (name.as_str(), input.as_ref()),
                ToolCall::Mcp { tool, input, .. } => (tool.as_str(), input.as_ref()),
                _ => continue,
            };
            let field = |key: &str| {
                input
                    .and_then(|v| v.get(key))
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            };
            let key = format!("{}:{id}", entry.id);
            // Explicit algorithm, independent of process-random HashMap seeds.
            let seed = crate::agent_avatar::spawn_seed(call);
            let name = field("name")
                .or_else(|| field("agent_name"))
                .or_else(|| field("nickname"))
                .or_else(|| field("subagent_type"))
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Agent {}", agents.len() + 1));
            let task = field("description")
                .or_else(|| field("prompt"))
                .unwrap_or(label.strip_prefix("Agent: ").unwrap_or(label))
                .to_owned();
            let (status, running) = match subagent_status {
                Some(SubagentStatus::Running) => ("Working", true),
                Some(SubagentStatus::Done) => ("Completed", false),
                Some(SubagentStatus::Failed) => ("Failed", false),
                None if *is_error => ("Failed", false),
                None if *resolved => ("Finished", false),
                None => ("Starting", true),
            };
            agents.push(Agent {
                key,
                doc: subagent_ref.clone(),
                name,
                task,
                tail: subagent_tail.clone(),
                status,
                running,
                seed,
            });
        }
    }
    // Completion removes the bubble item, while the transcript keeps its history.
    agents.retain(|a| a.running);
    agents
}

fn avatar(agent: &Agent, size: f32, time: Option<f32>) -> AnyElement {
    Avatar::new(
        SHAPES[agent.seed % SHAPES.len()].key,
        PALETTE[(agent.seed / SHAPES.len()) % PALETTE.len()].hsla(),
        size,
    )
    .motion(crate::agent_avatar::IdleMotion::Breathe)
    .time(time)
    .phase((agent.seed % 100) as f32 / 17.)
    .render()
}

impl Shell {
    pub(super) fn render_agent_dock(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.state.read(cx);
        let chat = state.selected_chat.clone()?;
        let agents = agents(&state.transcript);
        if agents.is_empty() {
            self.agents_expanded_chat = None;
            return None;
        }
        let theme = Theme::of(cx).clone();
        let time = if self.reduced_motion {
            None
        } else {
            motion::pulse_lease(cx.entity_id(), cx);
            Some(self.agents_motion.seconds())
        };
        let working = agents.iter().filter(|a| a.running).count();
        let names = agents
            .iter()
            .take(2)
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let summary = if agents.len() <= 2 {
            format!("{names} · working")
        } else {
            format!("{names} +{} · working", agents.len() - 2)
        };
        let expanded = self.agents_expanded_chat.as_ref() == Some(&chat);
        let toggle_chat = chat.clone();
        let header = div()
            .id("agent-dock-toggle")
            .flex()
            .items_center()
            .h(px(32.))
            .max_w(px(420.))
            .gap_2()
            .px_3()
            .rounded_full()
            .border_1()
            .border_color(theme.border)
            .bg(theme.element_hover)
            .cursor_pointer()
            .hover(|s| s.bg(theme.element_hover))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |shell, _, _, cx| {
                    shell.agents_expanded_chat = if expanded {
                        None
                    } else {
                        Some(toggle_chat.clone())
                    };
                    cx.notify();
                }),
            )
            .child(div().flex().items_center().flex_shrink_0().children(
                agents.iter().take(5).enumerate().map(|(ix, a)| {
                    div()
                        .size(px(24.))
                        .when(ix > 0, |el| el.ml(px(-7.)))
                        .child(avatar(a, 22., time))
                }),
            ))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .text_color(theme.text)
                    .child(summary),
            )
            .child(
                icon(if expanded {
                    icons::ALT_ARROW_UP
                } else {
                    icons::ALT_ARROW_DOWN
                })
                .size(px(12.))
                .text_color(theme.text_muted),
            );
        let mut dock = div()
            .id("agent-dock")
            .relative()
            .flex()
            .flex_col()
            .items_center()
            .mx(px(Theme::SPACE_LG))
            .mb_2()
            .child(header);
        if expanded {
            let mut panel = div()
                .id("agent-dock-popup")
                .w(px(340.))
                .max_w_full()
                .rounded_xl()
                .p_1()
                .bg(theme.surface_overlay)
                .border_1()
                .border_color(theme.border)
                .shadow_lg()
                .occlude()
                .on_mouse_down_out(cx.listener(|shell, _, _, cx| {
                    if shell.agents_expanded_chat.take().is_some() {
                        cx.notify();
                    }
                }));
            let mut list = div()
                .id("agent-dock-list")
                .max_h(px(250.))
                .overflow_y_scroll()
                .px_2()
                .pb_2()
                .flex()
                .flex_col()
                .gap_1();
            for agent in agents {
                let selected = agent.doc.as_ref().is_some_and(|doc| {
                    matches!(self.resolved_right_active(cx), RightSurface::Subagent(id)
                        if self.subagent_tabs.get(&id).is_some_and(|tab| &tab.doc_id == doc))
                });
                let mut row = div()
                    .id(SharedString::from(format!("agent-dock-{}", agent.key)))
                    .flex()
                    .items_center()
                    .gap_3()
                    .p_2()
                    .rounded_lg()
                    .when(selected, |el| el.bg(theme.accent_wash))
                    .child(avatar(&agent, 34., time))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(theme.text)
                                            .child(agent.name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(10.))
                                            .text_color(if agent.running {
                                                theme.success
                                            } else {
                                                theme.text_muted
                                            })
                                            .child(agent.status),
                                    ),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme.text_muted)
                                    .truncate()
                                    .child(agent.task.clone()),
                            )
                            .when_some(agent.tail.clone(), |el, tail| {
                                el.child(
                                    div()
                                        .text_size(px(10.))
                                        .text_color(theme.text_faint)
                                        .truncate()
                                        .child(tail),
                                )
                            }),
                    );
                if let Some(doc) = agent.doc {
                    let chat = chat.clone();
                    row = row
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.element_hover))
                        .child(
                            icon(icons::ARROW_UP_RIGHT)
                                .size(px(14.))
                                .text_color(theme.text_muted),
                        )
                        .on_click(cx.listener(move |shell, _, _, cx| {
                            shell.add_subagent_surface(
                                chat.clone(),
                                doc.clone(),
                                agent.name.clone(),
                                !agent.running,
                                cx,
                            );
                        }));
                } else {
                    row = row.child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.text_faint)
                            .child("No transcript yet"),
                    );
                }
                list = list.child(row);
            }
            panel = panel.child(list);
            if working > 0 {
                let stopping = self.composer.read(cx).is_interrupting(&chat);
                panel = panel.child(
                    div()
                        .id("agent-dock-stop-run")
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_2()
                        .h(px(32.))
                        .mx_2()
                        .rounded_lg()
                        .bg(theme.element_hover)
                        .text_size(px(12.))
                        .text_color(theme.text)
                        .child(icon(icons::STOP).size(px(14.)).text_color(theme.text))
                        .child(if stopping { "Stopping…" } else { "Stop run" })
                        .when(!stopping, |el| {
                            el.cursor_pointer()
                                .hover(|s| s.bg(theme.element_hover))
                                .on_click(cx.listener(move |shell, _, _, cx| {
                                    shell.composer.update(cx, |composer, cx| {
                                        composer.interrupt_chat(chat.clone(), cx)
                                    });
                                }))
                        }),
                );
                panel = panel.child(
                    div()
                        .w_full()
                        .py_2()
                        .text_center()
                        .text_size(px(10.))
                        .text_color(theme.text_faint)
                        .child("Stops all agents, including the main agent"),
                );
            }
            dock = dock.child(
                gpui::deferred(
                    div()
                        .absolute()
                        .bottom(px(40.))
                        .left_0()
                        .w_full()
                        .flex()
                        .justify_center()
                        .child(panel),
                )
                .priority(1),
            );
        }
        Some(dock.into_any_element())
    }
}
