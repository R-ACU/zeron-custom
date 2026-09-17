//! The settings outlet while the sidebar search box holds a query: the
//! matching settings themselves, grouped by the page they live on, in the
//! same page rhythm and card/row look as the pages (`widgets`). Pure
//! rendering — the shell supplies the hits, the section icons and the click
//! handlers, and re-renders on every keystroke.

use gpui::{AnyElement, App, ClickEvent, SharedString, Window, div, prelude::*, px};

use crate::settings::search::{BoolSetting, Control, SettingEntry};
use crate::settings::widgets;
use crate::shell::SettingsSection;
use crate::theme::Theme;

/// A click handler for one result row, as the shell's `cx.listener` hands
/// it out.
pub type OpenHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// The results page for `query`. `entries` are the hits, best first (the view
/// regroups them by section in nav order and keeps that rank within a
/// group); `section_icon` is the nav's icon map; `open` yields the handler
/// that opens a section and clears the query. `switch_t` is the painted
/// progress (0..1) of a plain boolean setting's switch — the shell owns the
/// glide so it survives this view's rebuild on every keystroke — and
/// `on_toggle` yields the handler that flips it in place, so a switch hit is
/// usable without opening its page.
pub fn render(
    theme: &Theme,
    query: &str,
    entries: &[&'static SettingEntry],
    section_icon: impl Fn(SettingsSection) -> &'static str,
    mut open: impl FnMut(SettingsSection) -> OpenHandler,
    switch_t: impl Fn(BoolSetting) -> f32,
    mut on_toggle: impl FnMut(BoolSetting) -> OpenHandler,
) -> AnyElement {
    let query = query.trim();
    let count = entries.len();
    let subtitle = match count {
        0 => format!("No settings match \"{query}\""),
        1 => format!("1 setting matches \"{query}\""),
        n => format!("{n} settings match \"{query}\""),
    };
    let mut column = widgets::page_column()
        .child(widgets::page_header(theme, "Search results", (count > 0).then_some(count)))
        .child(widgets::page_subtitle(theme, subtitle).line_height(px(20.0)));

    if count == 0 {
        column = column.child(
            div()
                .mt(px(24.0))
                .text_size(crate::typography::ui_rems(13.0))
                .text_color(theme.text_muted.opacity(0.7))
                .child(SharedString::from(
                    "Try another word, or pick a page from the list on the left.",
                )),
        );
    }

    // Groups in nav order; rows inside a group keep their search rank.
    let mut row_ix = 0usize;
    for section in SettingsSection::ALL {
        let hits: Vec<&&SettingEntry> =
            entries.iter().filter(|entry| entry.section == section).collect();
        if hits.is_empty() {
            continue;
        }
        let icon_path = section_icon(section);
        let mut card = widgets::section_card(theme).mt(px(8.0));
        for (gx, entry) in hits.into_iter().enumerate() {
            let handler = open(section);
            // A plain on/off setting is operated right here; everything else
            // keeps the chevron and only opens its page.
            let toggle = match entry.control {
                Control::Toggle(setting) => Some((switch_t(setting), on_toggle(setting))),
                Control::OpenPage => None,
            };
            card = card.child(
                // `widgets::card_row` metrics. No hover wash: no settings card
                // row in the app tints on hover, and a hit row is no exception.
                div()
                    .id(("settings-search-hit", row_ix))
                    .px(px(20.0))
                    .py(px(14.0))
                    .when(gx > 0, |el| el.border_t_1().border_color(theme.border))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(14.0))
                    .child(
                        // Opening the page hangs off the tile/text half alone,
                        // so a click on the switch flips the setting and
                        // nothing else.
                        div()
                            .id(("settings-search-hit-open", row_ix))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(14.0))
                            .cursor_pointer()
                            .on_click(handler)
                            .child(widgets::row_tile(theme, icon_path))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(widgets::row_title(theme, entry.title))
                                    .child(widgets::meta_line(
                                        theme,
                                        vec![
                                            div()
                                                .child(SharedString::from(entry.description))
                                                .into_any_element(),
                                        ],
                                    )),
                            ),
                    )
                    .child(match toggle {
                        Some((t, on_click)) => div()
                            .id(("settings-search-hit-toggle", row_ix))
                            .flex_none()
                            .cursor_pointer()
                            .on_click(on_click)
                            .child(widgets::toggle_switch_t(theme, t))
                            .into_any_element(),
                        None => crate::icons::icon(crate::icons::ALT_ARROW_RIGHT)
                            .size(px(14.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .into_any_element(),
                    }),
            );
            row_ix += 1;
        }
        column = column.child(
            div()
                .mt(px(24.0))
                .flex()
                .flex_col()
                .child(
                    div()
                        .px(px(2.0))
                        .text_size(crate::typography::ui_rems(11.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.text_muted.opacity(0.6))
                        .child(SharedString::from(section.label())),
                )
                .child(card),
        );
    }

    div()
        .id("settings-search-results")
        .size_full()
        .overflow_y_scroll()
        .child(column)
        .into_any_element()
}
