//! Device-local default workspace and project navigation. Spaces remain the
//! engine's source of truth for cwd; organizing them never moves a directory.
use super::*;
use zeron_proto::Space;

impl Shell {
    pub(super) fn choose_workspace(&mut self, cx: &mut Context<Self>) {
        self.choose_local_project(true, cx);
    }

    pub(super) fn choose_local_project(&mut self, workspace: bool, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(
                if workspace {
                    "Choose workspace"
                } else {
                    "Add project"
                }
                .into(),
            ),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = receiver.await {
                if let Some(path) = paths.pop() {
                    let _ = this.update(cx, |this, cx| {
                        this.register_local_project(path, workspace, cx)
                    });
                }
            }
        })
        .detach();
    }

    pub(super) fn create_local_project(&mut self, cx: &mut Context<Self>) {
        self.open_rename_space("new-folder".into(), cx);
    }

    pub(super) fn open_chat_folder(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(folder) = self.settings.chat_folders.iter().find(|f| f.id == id).cloned() else { return; };
        if let Some(workspace) = folder.workspace {
            self.land_in_space(workspace, cx);
        } else {
            self.open_new_session(cx);
        }
        self.settings.active_chat_folder = Some(id);
        self.schedule_save(cx);
    }

    fn register_local_project(&mut self, path: PathBuf, workspace: bool, cx: &mut Context<Self>) {
        let path = path.to_string_lossy().into_owned();
        let (device, engine, existing) = {
            let state = self.state.read(cx);
            let Some(device) = state.local_device_id.clone() else {
                return;
            };
            let Some(engine) = state.engine().cloned() else {
                return;
            };
            let existing = state
                .spaces
                .iter()
                .find(|s| s.device_id == device && same_folder(&s.path, &path))
                .map(|s| s.id.clone());
            (device, engine, existing)
        };
        if let Some(id) = existing {
            self.finish_project_selection(id, workspace, cx);
            return;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let space = Space {
            id: id.clone(),
            device_id: device.clone(),
            path: path.clone(),
            name: None,
            git_detected: false,
            git_checked_at: None,
            checkout_id: None,
            created_at: Utc::now(),
        };
        cx.spawn(async move |this, cx| {
            let result = engine.client().call(methods::MUTATE, serde_json::json!({
                "op":"createSpace", "spaceId":id, "deviceId":device, "path":path, "gitDetected":false
            })).await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(_) => {
                        this.state.update(cx, |state, cx| {
                            if !state.spaces.iter().any(|s| s.id == space.id) { state.spaces.push(space); }
                            cx.notify();
                        });
                        this.finish_project_selection(id, workspace, cx);
                    }
                    Err(error) => { this.sidebar_notice = Some(error.to_string().into()); cx.notify(); }
                }
            });
        }).detach();
    }

    fn finish_project_selection(&mut self, id: String, workspace: bool, cx: &mut Context<Self>) {
        if workspace {
            self.settings.workspace_space_id = Some(id.clone());
        }
        self.settings.workspace_initialized = true;
        self.settings.space_filter = None;
        self.land_in_space(id, cx);
        self.schedule_save(cx);
    }

    pub(super) fn workspace_groups(
        &self,
        cx: &App,
    ) -> Vec<(Option<String>, String, Vec<zeron_proto::Chat>)> {
        let state = self.state.read(cx);
        let mut chats: Vec<_> = state
            .sidebar_chats(Utc::now(), None)
            .into_iter()
            .map(|(_, c)| c.clone())
            .collect();
        chats.sort_by(|a, b| spaces::compare_sidebar_chats(self.settings.sidebar_sort, a, b));
        let workspace = self.settings.workspace_space_id.as_deref();
        let mut groups = vec![(None, "Chats".to_string(), Vec::new())];
        for folder in &self.settings.chat_folders {
            if folder.workspace.as_deref() == workspace {
                groups.push((Some(folder.id.clone()), folder.name.clone(), Vec::new()));
            }
        }
        for space in state.spaces_sorted() {
            if Some(space.id.as_str()) == workspace {
                continue;
            }
            let mut name = space.display_name().to_string();
            if Some(&space.device_id) != state.local_device_id.as_ref() {
                name = format!(
                    "{name} @ {}",
                    state
                        .device_name(&space.device_id)
                        .unwrap_or("Unknown device")
                );
            }
            groups.push((Some(space.id.clone()), name, Vec::new()));
        }
        let assignments = crate::settings::current(cx).chat_folder_assignments;
        for chat in chats {
            let assigned = assignments.get(&chat.id).cloned();
            let folder_index = assigned.and_then(|id| groups.iter().position(|(key, _, _)| key.as_ref() == Some(&id)));
            let index = if let Some(index) = folder_index { index } else if chat.space_id.as_deref() == workspace || chat.space_id.is_none() {
                0
            } else {
                groups
                    .iter()
                    .position(|(id, _, _)| id.as_ref() == chat.space_id.as_ref())
                    .unwrap_or(0)
            };
            groups[index].2.push(chat);
        }
        groups
    }

    pub(super) fn render_workspace_rows(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<(String, f32, AnyElement)> {
        let groups = self.workspace_groups(cx);
        let mut result = Vec::new();
        let mut slot = 0usize;
        for (id, label, chats) in groups {
            let key = format!("project:{}", id.as_deref().unwrap_or("workspace"));
            let collapsed = self.sidebar_collapsed_groups.contains(&key);
            let toggle = key.clone();
            let target = id.clone();
            let menu_id = id.clone();
            let header = div()
                .id(SharedString::from(format!("header-{key}")))
                .h(px(30.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(Theme::SPACE_SM))
                .text_size(crate::typography::ui_rems(12.0))
                .text_color(theme.text_muted)
                .child(
                    div()
                        .id(SharedString::from(format!("toggle-{key}")))
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.sidebar_collapsed_groups.remove(&toggle) {
                                this.sidebar_collapsed_groups.insert(toggle.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            icon(if collapsed {
                                icons::ALT_ARROW_RIGHT
                            } else {
                                icons::ALT_ARROW_DOWN
                            })
                            .size(px(12.0)).text_color(theme.text_muted),
                        )
                        .child(icon(match id.as_deref() {
                            None => icons::CHAT_ROUND_LINE,
                            Some(id) if id.starts_with("folder:") => icons::FOLDER,
                            Some(_) => icons::MONITOR,
                        }).size(px(14.0))
                            .text_color(id.as_ref().and_then(|id| self.settings.project_colors.get(id)).map(|c| gpui::rgb(*c).into()).unwrap_or(theme.text_muted)))
                        .child(div().truncate().child(label)),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("new-{key}")))
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .hover(|s| s.bg(theme.glass_hover()))
                        .cursor_pointer()
                        .role(gpui::Role::Button)
                        .aria_label("New chat")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(id) = target.clone() {
                                if id.starts_with("folder:") { this.open_chat_folder(id, cx); }
                                else { this.settings.active_chat_folder = None; this.land_in_space(id, cx); }
                            } else {
                                this.open_new_session(cx);
                            }
                        }))
                        .child(icon(icons::PLUS).size(px(14.0)).text_color(theme.text_muted)),
                );
            let header = header.when_some(menu_id, |header, id| {
                header.on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.space_menu.open((id.clone(), event.position));
                        cx.notify();
                    }),
                )
            });
            result.push((format!("h:{key}"), 30.0, header.into_any_element()));
            if collapsed {
                continue;
            }
            for chat in chats {
                let state = self.state.read(cx);
                let status = state
                    .sidebar_chats(Utc::now(), None)
                    .into_iter()
                    .find(|(_, c)| c.id == chat.id)
                    .map(|(s, _)| s)
                    .unwrap();
                let context = if Some(&chat.device_id) != state.local_device_id.as_ref() {
                    state
                        .device_name(&chat.device_id)
                        .unwrap_or("Unknown device")
                        .to_string()
                } else if chat.space_id.is_none() && self.settings.workspace_space_id.is_some() {
                    chat.cwd.clone().unwrap_or_else(|| "Home".into())
                } else {
                    String::new()
                };
                let branch = self
                    .settings
                    .sidebar_show_branch
                    .then(|| {
                        crate::change_requests::conversation_branch(&chat, &state.spaces)
                            .map(str::to_string)
                    })
                    .flatten();
                let pr = self
                    .settings
                    .sidebar_show_pull_request
                    .then(|| state.change_request_for_chat(&chat).cloned())
                    .flatten();
                let selected = state.selected_chat.as_deref() == Some(chat.id.as_str());
                let harness = self
                    .settings
                    .sidebar_show_harness
                    .then(|| chat.config.as_ref().map(|c| c.harness))
                    .flatten();
                let height = super::chat_row_height(branch.is_some(), pr.is_some())
                    - if context.is_empty() { 16.0 } else { 0.0 };
                let jump_label =
                    if self.jump_hints && !self.overlay_owns_keyboard(cx) && slot < JUMP_SLOTS {
                        let combo = self.settings.keymap.get(ShortcutId::JumpSession(slot));
                        (!combo.is_empty()).then(|| badge_combo(combo).into())
                    } else {
                        None
                    };
                slot += 1;
                let row = self.render_chat_row(
                    chat.id.clone(),
                    transcript::single_line(chat.title.as_deref().unwrap_or("New chat")).into(),
                    format_time_ago(chat.last_message_at.unwrap_or(chat.created_at), Utc::now())
                        .into(),
                    context.into(),
                    branch.map(Into::into),
                    pr,
                    harness,
                    status,
                    selected,
                    false,
                    jump_label,
                    theme,
                    cx,
                );
                result.push((format!("c:{}", chat.id), height, row));
            }
        }
        result
    }
}

fn same_folder(a: &str, b: &str) -> bool {
    let normalize = |s: &str| {
        let s = s.replace('\\', "/").trim_end_matches('/').to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    normalize(a) == normalize(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn folder_identity_preserves_segment_boundaries() {
        assert!(same_folder("/projects/app/", "/projects/app"));
        assert!(!same_folder("/projects/app", "/projects/apple"));
        #[cfg(windows)]
        assert!(same_folder("D:\\Work\\App", "d:/work/app/"));
    }

    #[gpui::test]
    fn workspace_default_is_independent_of_project_and_preserves_existing_chats(
        cx: &mut gpui::TestAppContext,
    ) {
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            gpui_base::init(cx);
            cx.set_global(Theme::default());
            crate::app_menus::init(cx);
            crate::history::init(
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                cx,
            );
            settings::init(UiSettings::default(), dir.path(), cx);
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| AppState::new());
            Shell::new(
                state,
                EngineBootConfig {
                    data_dir: dir.path().into(),
                    ipc_port: 0,
                    edge_url: "http://127.0.0.1:1".into(),
                    edge_token: None,
                    org_id: None,
                    workos_client_id: None,
                    default_harness: zeron_proto::HarnessId::Mock,
                },
                cx,
            )
        });
        window.update(cx,|shell,window,cx| {
            shell.state.update(cx,|state,cx| {
                state.local_device_id=Some("local".into());
                state.spaces=[("root","/workspace"),("project","/workspace/project"),("empty","/workspace/empty")].into_iter()
                    .map(|(id,path)| serde_json::from_value(serde_json::json!({"id":id,"deviceId":"local","path":path,"gitDetected":false,"createdAt":Utc::now()})).unwrap()).collect();
                state.chats=[("general",Some("root")),("child",Some("project")),("legacy",None)].into_iter()
                    .map(|(id,space)| serde_json::from_value(serde_json::json!({"id":id,"deviceId":"local","spaceId":space,"archived":false,"createdAt":Utc::now()})).unwrap()).collect();
                state.select_chat(Some("child".into()),cx);
            });
            shell.settings.workspace_space_id=Some("root".into());
            shell.settings.workspace_initialized=true;
            shell.open_new_session(cx);
            assert_eq!(shell.state.read(cx).selected_space.as_deref(),Some("root"));
            shell.land_in_space("project".into(),cx);
            assert_eq!(shell.state.read(cx).selected_space.as_deref(),Some("project"));
            shell.open_new_session(cx);
            assert_eq!(shell.state.read(cx).selected_space.as_deref(),Some("root"));
            assert_eq!(shell.state.read(cx).chats.len(),3);
            // A result from another chat must not attach to the previously active chat.
            shell.open_chat("general".into(), cx);
            shell.on_state_changed(&shell.state.clone(), cx);
            let artifact = dir.path().join("result.json");
            std::fs::write(&artifact, "{}").unwrap();
            let artifact = artifact.to_string_lossy().into_owned();
            shell.open_inbox_file("child", "different-device", &artifact, window, cx);
            assert_eq!(shell.active_chat, "general");
            assert!(shell.file_surfaces.is_empty());
            shell.open_inbox_file("child", "local", &artifact, window, cx);
            assert_eq!(shell.active_chat, "child");
            assert_eq!(shell.state.read(cx).selected_chat.as_deref(), Some("child"));
            assert!(shell.file_surface_keys.contains_key(&(shell.panel_key(cx), artifact)));
            let groups=shell.workspace_groups(cx);
            assert_eq!(groups[0].2.len(),2);
            assert!(groups.iter().any(|(id,_,chats)|id.as_deref()==Some("empty") && chats.is_empty()));
            shell.sidebar_collapsed_groups.insert("project:project".into());
            assert!(!shell.sidebar_visible_order(cx).contains(&"child".into()));
            shell.settings.chat_folders.push(crate::settings::ChatFolder {
                id: "folder:images".into(), name: "Images".into(), workspace: Some("root".into()),
            });
            shell.open_chat_folder("folder:images".into(), cx);
            assert_eq!(shell.state.read(cx).selected_space.as_deref(), Some("root"));
            assert_eq!(settings::current(cx).active_chat_folder.as_deref(), Some("folder:images"));
            settings::update(SavePolicy::Immediate, cx, |s| {
                s.chat_folder_assignments.insert("general".into(), "folder:images".into());
            });
            let groups = shell.workspace_groups(cx);
            assert_eq!(groups[0].2.len(), 1);
            assert!(groups.iter().any(|(id, _, chats)| id.as_deref() == Some("folder:images") && chats[0].id == "general"));
            shell.open_new_session(cx);
            assert!(settings::current(cx).active_chat_folder.is_none());
            assert_eq!(settings::current(cx).chat_folder_assignments.get("general").map(String::as_str), Some("folder:images"));
            shell.settings.workspace_space_id=None;
            shell.open_new_session(cx);
            assert!(shell.state.read(cx).selected_space.is_none());
        }).unwrap();
    }
}
