//! Durable engine inbox. Results link to their source instead of duplicating artifacts.
use crate::{settings::widgets, state::AppState, theme::Theme};
use gpui::{
    Context, Entity, EventEmitter, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use serde_json::{Value, json};

pub(super) enum InboxEvent {
    OpenChat(String),
    OpenFile {
        chat_id: String,
        device_id: String,
        path: String,
    },
}
#[derive(Clone, Copy, PartialEq)]
enum Filter {
    Open,
    Unread,
    Done,
    All,
}

pub(super) struct InboxPage {
    state: Entity<AppState>,
    items: Vec<Value>,
    selected: Option<String>,
    filter: Filter,
    loaded: bool,
    load_error: Option<SharedString>,
    error: Option<SharedString>,
    busy: bool,
    generation: u64,
    task: Option<Task<()>>,
    _poll: Task<()>,
    _observe: Subscription,
}
impl EventEmitter<InboxEvent> for InboxPage {}
fn text(item: &Value, key: &str) -> String {
    item[key].as_str().unwrap_or_default().to_owned()
}
fn flag(item: &Value, key: &str) -> bool {
    item[key].as_bool().unwrap_or(false)
}
fn status(item: &Value) -> &str {
    match item["status"].as_str().unwrap_or_default() {
        "running" => "Running",
        "awaitingInput" => "Needs input",
        "succeeded" => "Completed",
        "failed" => "Failed",
        "interrupted" => "Interrupted",
        "resolved" => "Resolved",
        _ => "",
    }
}
fn timestamp(item: &Value) -> String {
    item["updatedAt"]
        .as_i64()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|at| {
            at.with_timezone(&chrono::Local)
                .format("%b %d, %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}
fn button(theme: &Theme, label: impl Into<SharedString>) -> gpui::Div {
    div()
        .px(px(10.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .cursor_pointer()
        .text_size(crate::typography::ui_rems(12.0))
        .text_color(theme.text)
        .hover(|s| s.bg(crate::theme::ink(0.06)))
        .child(label.into())
}
fn action_button(theme: &Theme, label: &str, mark: &'static str, primary: bool) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(7.0))
        .px(px(12.0))
        .h(px(34.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(if primary { theme.text } else { theme.border })
        .bg(if primary {
            theme.text
        } else {
            crate::theme::ink(0.04)
        })
        .text_color(if primary { theme.surface } else { theme.text })
        .text_size(crate::typography::ui_rems(12.0))
        .cursor_pointer()
        .hover(|s| s.opacity(0.8))
        .child(
            crate::icons::icon(mark)
                .size(px(15.0))
                .flex_none()
                .text_color(if primary { theme.surface } else { theme.text }),
        )
        .child(label.to_owned())
}
fn web_link(target: &str) -> bool {
    url::Url::parse(target).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
    })
}
impl InboxPage {
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
                                .map(|engine| (engine, page.generation))
                        }
                    })
                    .ok()
                    .flatten();
                if let Some((engine, generation)) = engine {
                    let result = engine.client().call("ListInbox", json!({})).await;
                    if this
                        .update(cx, |page, cx| {
                            if page.busy || generation != page.generation {
                                return;
                            }
                            match result {
                                Ok(value) => {
                                    if let Some(items) = value.as_array() {
                                        page.items = items.clone();
                                        page.loaded = true;
                                        page.load_error = None;
                                    } else {
                                        page.load_error =
                                            Some("Could not read inbox results.".into());
                                    }
                                }
                                Err(error) => {
                                    page.load_error =
                                        Some(format!("Could not load inbox: {error}").into())
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
        Self {
            state,
            items: vec![],
            selected: None,
            filter: Filter::Open,
            loaded: false,
            load_error: None,
            error: None,
            busy: false,
            generation: 0,
            task: None,
            _poll: poll,
            _observe: observe,
        }
    }
    fn update_item(
        &mut self,
        id: String,
        read: Option<bool>,
        done: Option<bool>,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Reconnect to update this item.".into());
            cx.notify();
            return;
        };
        let mut params = json!({"id":id});
        if let Some(read) = read {
            params["read"] = json!(read);
        }
        if let Some(done) = done {
            params["done"] = json!(done);
        }
        self.busy = true;
        self.generation = self.generation.wrapping_add(1);
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call("UpdateInbox", params).await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(item) => {
                        let id = text(&item, "id");
                        if let Some(existing) =
                            page.items.iter_mut().find(|old| text(old, "id") == id)
                        {
                            *existing = item;
                        } else {
                            page.items.push(item);
                        }
                    }
                    Err(error) => {
                        page.error = Some(format!("Could not update item: {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
    fn open_review(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Reconnect to open this review.".into());
            cx.notify();
            return;
        };
        self.busy = true;
        self.error = None;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call("OpenInboxReview", json!({"id":id}))
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(result) => {
                        if let Some(url) = result["url"].as_str().filter(|url| web_link(url)) {
                            cx.open_url(url);
                        } else {
                            page.error = Some("The review did not return a valid URL.".into());
                        }
                    }
                    Err(error) => {
                        page.error = Some(format!("Could not open review: {error}").into())
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
    fn select(&mut self, id: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.selected = Some(id.clone());
        if self
            .items
            .iter()
            .any(|item| text(item, "id") == id && !flag(item, "read"))
        {
            self.update_item(id, Some(true), None, cx);
        }
        cx.notify();
    }
}
impl Render for InboxPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let unread = self
            .items
            .iter()
            .filter(|item| !flag(item, "read") && !flag(item, "done"))
            .count();
        let mut column = widgets::page_column()
            .child(widgets::page_header(&theme, "Inbox", Some(unread)))
            .child(widgets::page_subtitle(
                &theme,
                "Results and updates from your tasks.",
            ));
        for error in [&self.load_error, &self.error].into_iter().flatten() {
            column = column.child(widgets::error_strip(&theme, error.clone()));
        }
        if let Some(item) = self
            .selected
            .as_ref()
            .and_then(|id| self.items.iter().find(|item| text(item, "id") == *id))
            .cloned()
        {
            let id = text(&item, "id");
            let read_id = id.clone();
            let done_id = id.clone();
            let read = flag(&item, "read");
            let done = flag(&item, "done");
            column = column.child(
                div()
                    .mt(px(20.0))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        action_button(&theme, "Back to inbox", crate::icons::ARROW_LEFT, false)
                            .id("inbox-back")
                            .on_click(cx.listener(|page, _, _, cx| {
                                page.selected = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .child(
                                action_button(
                                    &theme,
                                    if read { "Mark unread" } else { "Mark read" },
                                    crate::icons::CHAT_ROUND_LINE,
                                    false,
                                )
                                .id("inbox-read")
                                .on_click(cx.listener(
                                    move |page, _, _, cx| {
                                        page.update_item(read_id.clone(), Some(!read), None, cx)
                                    },
                                )),
                            )
                            .child(
                                action_button(
                                    &theme,
                                    if done { "Reopen" } else { "Mark done" },
                                    crate::icons::CHECK,
                                    false,
                                )
                                .id("inbox-done")
                                .on_click(cx.listener(
                                    move |page, _, _, cx| {
                                        page.update_item(done_id.clone(), None, Some(!done), cx)
                                    },
                                )),
                            ),
                    ),
            );
            column = column
                .child(div().mt(px(24.0)).child(widgets::page_header(
                    &theme,
                    &text(&item, "title"),
                    None,
                )))
                .child(widgets::page_subtitle(
                    &theme,
                    format!(
                        "{} · {}{}",
                        status(&item),
                        timestamp(&item),
                        if done { " · Done" } else { "" }
                    ),
                ));
            if let Some(summary) = item["summary"].as_str().filter(|s| !s.is_empty()) {
                column = column.child(
                    div()
                        .mt(px(18.0))
                        .text_size(crate::typography::ui_rems(13.0))
                        .text_color(theme.text)
                        .child(summary.to_owned()),
                );
            }
            if let Some(error) = item["error"].as_str().filter(|s| !s.is_empty()) {
                column = column.child(widgets::error_strip(&theme, error.to_owned()));
            }
            let chat = text(&item, "chatId");
            if !chat.is_empty() {
                column = column.child(
                    div().flex().child(
                        action_button(&theme, "Open in chat", crate::icons::ARROW_UP_RIGHT, true)
                            .id("inbox-chat")
                            .mt(px(16.0))
                            .on_click(cx.listener(move |_, _, _, cx| {
                                cx.emit(InboxEvent::OpenChat(chat.clone()))
                            })),
                    ),
                );
            }
            if item["reviewUrl"].as_str().is_some_and(web_link) {
                let review_id = id.clone();
                column = column.child(
                    button(
                        &theme,
                        if self.busy {
                            "Working..."
                        } else {
                            "Open review"
                        },
                    )
                    .id("inbox-review")
                    .on_click(
                        cx.listener(move |page, _, _, cx| page.open_review(review_id.clone(), cx)),
                    ),
                );
            }
            if let Some(links) = item["links"].as_array().filter(|links| !links.is_empty()) {
                column = column.child(
                    div()
                        .mt(px(20.0))
                        .child(widgets::field_label(&theme, "Results")),
                );
                for (ix, link) in links.iter().enumerate() {
                    let target = text(link, "target");
                    let kind = text(link, "type");
                    let label = link["label"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&target)
                        .to_owned();
                    let device = text(&item, "deviceId");
                    let chat_id = text(&item, "chatId");
                    let valid = (kind == "url" && web_link(&target))
                        || (kind == "file"
                            && !device.is_empty()
                            && !chat_id.is_empty()
                            && std::path::Path::new(&target).is_absolute());
                    if valid {
                        column =
                            column.child(button(&theme, label).id(("inbox-result", ix)).on_click(
                                cx.listener(move |_, _, _, cx| {
                                    if kind == "url" {
                                        cx.open_url(&target);
                                    } else {
                                        cx.emit(InboxEvent::OpenFile {
                                            chat_id: chat_id.clone(),
                                            device_id: device.clone(),
                                            path: target.clone(),
                                        });
                                    }
                                }),
                            ));
                    }
                }
            }
        } else {
            let mut filters = div().mt(px(20.0)).mb(px(12.0)).flex().gap(px(4.0));
            for (ix, (filter, label)) in [
                (Filter::Open, "Open"),
                (Filter::Unread, "Unread"),
                (Filter::Done, "Done"),
                (Filter::All, "All"),
            ]
            .into_iter()
            .enumerate()
            {
                filters = filters.child(
                    button(&theme, label)
                        .id(("inbox-filter", ix))
                        .when(self.filter == filter, |s| {
                            s.bg(theme.accent.opacity(0.12)).text_color(theme.accent)
                        })
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.filter = filter;
                            cx.notify();
                        })),
                );
            }
            column = column.child(filters);
            let mut items: Vec<_> = self
                .items
                .iter()
                .filter(|item| match self.filter {
                    Filter::Open => !flag(item, "done"),
                    Filter::Unread => !flag(item, "done") && !flag(item, "read"),
                    Filter::Done => flag(item, "done"),
                    Filter::All => true,
                })
                .cloned()
                .collect();
            items.sort_by_key(|item| std::cmp::Reverse(item["updatedAt"].as_i64().unwrap_or(0)));
            if items.is_empty() {
                column = column.child(widgets::page_subtitle(
                    &theme,
                    if self.state.read(cx).engine().is_none() {
                        "Reconnect to load your inbox."
                    } else if !self.loaded {
                        "Loading inbox..."
                    } else if self.filter == Filter::Done {
                        "Completed items will appear here."
                    } else {
                        "You're all caught up. New task results will appear here."
                    },
                ));
            }
            for item in items {
                let id = text(&item, "id");
                let read = flag(&item, "read");
                column = column.child(
                    div()
                        .id(SharedString::from(format!("inbox-item-{id}")))
                        .relative()
                        .when(!read, |row| {
                            row.child(
                                div()
                                    .absolute()
                                    .left(px(-5.0))
                                    .top(px(21.0))
                                    .size(px(6.0))
                                    .rounded_full()
                                    .bg(gpui::rgb(0x4b9dff)),
                            )
                        })
                        .py(px(14.0))
                        .px(px(10.0))
                        .rounded(px(8.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(crate::theme::ink(0.03)))
                        .on_click(cx.listener(move |page, _, _, cx| page.select(id.clone(), cx)))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .gap(px(12.0))
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .text_size(crate::typography::ui_rems(13.0))
                                        .text_color(theme.text)
                                        .font_weight(if read {
                                            gpui::FontWeight::NORMAL
                                        } else {
                                            gpui::FontWeight::SEMIBOLD
                                        })
                                        .child(text(&item, "title")),
                                )
                                .child(
                                    div()
                                        .text_size(crate::typography::ui_rems(11.0))
                                        .text_color(theme.text_muted)
                                        .child(timestamp(&item)),
                                ),
                        )
                        .child(widgets::page_subtitle(&theme, status(&item).to_owned()))
                        .when_some(
                            item["summary"].as_str().map(str::to_owned),
                            |row, summary| {
                                row.child(widgets::page_subtitle(&theme, summary).truncate())
                            },
                        ),
                );
            }
        }
        div()
            .id("inbox-page")
            .size_full()
            .overflow_y_scroll()
            .child(column)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[gpui::test]
    fn opening_an_offline_result_never_acknowledges_it_locally(cx: &mut gpui::TestAppContext) {
        let state = cx.new(|_| AppState::new());
        let page = cx.new(|cx| InboxPage::new(state, cx));
        page.update(cx, |page, cx| {
            page.items
                .push(json!({"id":"result-1", "read":false, "done":false}));
            page.select("result-1".into(), cx);
            assert_eq!(page.selected.as_deref(), Some("result-1"));
            assert!(!flag(&page.items[0], "read"));
            assert!(page.error.is_some());
            assert!(!page.busy);
            page.update_item("result-1".into(), None, Some(true), cx);
            assert!(!flag(&page.items[0], "done"));
        });
    }

    #[test]
    fn result_urls_require_a_web_origin_and_no_embedded_credentials() {
        assert!(web_link("https://example.com/result"));
        assert!(web_link("http://localhost:8793/?run=123"));
        assert!(!web_link("http://"));
        assert!(!web_link("https://name:secret@example.com"));
        assert!(!web_link("file:///C:/results.json"));
        assert!(!web_link("javascript:alert(1)"));
    }
}
