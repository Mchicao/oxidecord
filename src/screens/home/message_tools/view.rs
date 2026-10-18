//! The message tools' docked panel: search results on one side, pins on the
//! other. Every state it can be in — loading, empty, error, index-pending —
//! is drawn as what it is; nothing pretends to have results it doesn't.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Selectable as _, Sizable as _, h_flex,
    spinner::Spinner, v_flex,
};

use crate::screens::home::HomeScreen;
use crate::screens::home::state::View;
use crate::ui::button::Button;
use crate::ui::theme;

use super::{MAX_SEARCH_RETRIES, SearchScope, ToolsPanel};

const PANEL_WIDTH: f32 = 320.;

impl HomeScreen {
    /// The docked panel beside the chat, when it's open at all.
    pub(in crate::screens::home) fn render_message_tools(
        &self,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let panel = match self.message_tools.panel? {
            ToolsPanel::Search => self.render_search_panel(cx).into_any_element(),
            ToolsPanel::Pins => self.render_pins_panel(cx).into_any_element(),
        };
        Some(
            v_flex()
                .w(px(PANEL_WIDTH))
                .flex_shrink_0()
                .h_full()
                .min_h_0()
                .border_l_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_search_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let search = &self.message_tools.search;
        let guild_search = self.view != View::DirectMessages;

        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .h(px(44.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .when(guild_search, |this| {
                        this.child(
                            Button::new("search-scope-channel")
                                .label("This channel")
                                .ghost()
                                .xsmall()
                                .selected(search.scope == SearchScope::Channel)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_message_search_scope(SearchScope::Channel, cx)
                                })),
                        )
                        .child(
                            Button::new("search-scope-guild")
                                .label("This server")
                                .ghost()
                                .xsmall()
                                .selected(search.scope == SearchScope::Guild)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_message_search_scope(SearchScope::Guild, cx)
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("close-search")
                            .icon(IconName::Close)
                            .ghost()
                            .xsmall()
                            .tooltip("Close")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.close_message_tools_panel(cx)),
                            ),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(theme::secondary_text(cx))
                    .child("from: in: has: before: after:"),
            )
            .when_some(search.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .when(search.indexing, |this| {
                this.child(self.render_indexing_notice(cx))
            })
            .when(search.deep_indexing, |this| {
                this.child(
                    div()
                        .px_3()
                        .pb_2()
                        .text_xs()
                        .text_color(theme::secondary_text(cx))
                        .child(
                            "This server's history is being indexed, so results may be incomplete.",
                        ),
                )
            })
            .child(if search.loading {
                centered(Spinner::new().small().color(theme::secondary_text(cx))).into_any_element()
            } else if search.hits.is_empty() {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .px_4()
                    .text_sm()
                    .text_color(theme::secondary_text(cx))
                    .child(if search.error.is_some() || search.indexing {
                        "No results shown."
                    } else {
                        "Nothing to show yet."
                    })
                    .into_any_element()
            } else {
                let entity = cx.entity();
                list(search.list.clone(), move |ix, _window, cx| {
                    entity.update(cx, |this, cx| this.render_search_item(ix, cx))
                })
                .flex_1()
                .into_any_element()
            })
            .when(search.capped, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(theme::secondary_text(cx))
                        .child(
                            "Only the first 500 results are kept here — refine the search to see more.",
                        ),
                )
            })
            .when(
                !search.hits.is_empty() || search.total_results > 0,
                |this| {
                    this.child(
                        h_flex()
                            .flex_shrink_0()
                            .items_center()
                            .justify_between()
                            .px_3()
                            .py_2()
                            .border_t_1()
                            .border_color(cx.theme().border)
                            .child(div().text_xs().text_color(theme::secondary_text(cx)).child(
                                format!("{} of {}", search.hits.len(), search.total_results),
                            ))
                            .when(!search.complete, |this| {
                                this.child(
                                    Button::new("load-more-results")
                                        .label("Load more")
                                        .ghost()
                                        .xsmall()
                                        .loading(search.loading_more)
                                        .disabled(search.loading_more)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.load_more_message_results(cx)
                                        })),
                                )
                            }),
                    )
                },
            )
    }

    fn render_indexing_notice(&self, cx: &Context<Self>) -> impl IntoElement {
        let search = &self.message_tools.search;
        let retries_left = MAX_SEARCH_RETRIES.saturating_sub(search.retries_used);
        h_flex()
            .items_center()
            .justify_between()
            .px_3()
            .py_2()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme::secondary_text(cx))
                    .child(format!(
                        "The search index isn't ready yet.{}",
                        if retries_left > 0 {
                            format!(
                                " It can be retried {retries_left} more time{}.",
                                if retries_left == 1 { "" } else { "s" }
                            )
                        } else {
                            String::new()
                        }
                    )),
            )
            .child(
                Button::new("retry-search")
                    .label("Retry")
                    .outline()
                    .xsmall()
                    .loading(search.loading)
                    .disabled(search.loading || retries_left == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.retry_message_search(cx))),
            )
    }

    /// One virtualized search-result row, resolved out of the entity so the
    /// list closure can run against `&mut HomeScreen`.
    fn render_search_item(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let Some(hit) = self.message_tools.search.hits.get(index) else {
            return div().into_any_element();
        };
        self.render_search_hit(index, hit, cx).into_any_element()
    }

    fn render_search_hit(
        &self,
        index: usize,
        hit: &crate::discord::message_tools::SearchHit,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let channel = self
            .channel_info(hit.channel_id)
            .map(|channel| channel.name.clone())
            .unwrap_or_else(|| "unknown channel".into());
        let in_this_conversation = self.selected_channel == Some(hit.channel_id);

        v_flex()
            .id(SharedString::from(format!("search-hit-{}", hit.message.id)))
            .px_3()
            .py_2()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().border)
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().muted))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_message_search_result(index, window, cx)
            }))
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(hit.message.author_name.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme::secondary_text(cx))
                            .child(hit.message.timestamp_label()),
                    ),
            )
            .when(!in_this_conversation, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme::secondary_text(cx))
                        .child(format!("#{channel}")),
                )
            })
            .child(
                div()
                    .text_sm()
                    .text_color(theme::secondary_text(cx))
                    .child(hit_preview(&hit.message)),
            )
    }

    fn render_pins_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let pins = &self.message_tools.pins;

        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .items_center()
                    .gap_1()
                    .px_3()
                    .h(px(44.))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Pinned Messages"),
                    )
                    .child(
                        Button::new("close-pins")
                            .icon(IconName::Close)
                            .ghost()
                            .xsmall()
                            .tooltip("Close")
                            .on_click(
                                cx.listener(|this, _, _, cx| this.close_message_tools_panel(cx)),
                            ),
                    ),
            )
            .when_some(pins.error.clone(), |this, error| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(cx.theme().danger)
                        .child(error),
                )
            })
            .child(if pins.loading {
                centered(Spinner::new().small().color(theme::secondary_text(cx))).into_any_element()
            } else if pins.entries.is_empty() {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .px_4()
                    .text_sm()
                    .text_color(theme::secondary_text(cx))
                    .child(if pins.error.is_some() {
                        "The pin list couldn't be loaded."
                    } else {
                        "No pinned messages in this conversation."
                    })
                    .into_any_element()
            } else {
                let entity = cx.entity();
                list(pins.list.clone(), move |ix, _window, cx| {
                    entity.update(cx, |this, cx| this.render_pin_item(ix, cx))
                })
                .flex_1()
                .into_any_element()
            })
            .when(pins.capped, |this| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_xs()
                        .text_color(theme::secondary_text(cx))
                        .child("Only the newest 500 pins are kept here — older pins aren't shown."),
                )
            })
            .when(!pins.entries.is_empty(), |this| {
                this.child(
                    h_flex()
                        .flex_shrink_0()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme::secondary_text(cx))
                                .child(format!("{} pinned", pins.entries.len())),
                        )
                        .when(pins.has_more, |this| {
                            this.child(
                                Button::new("load-more-pins")
                                    .label("Load older")
                                    .ghost()
                                    .xsmall()
                                    .loading(pins.loading_more)
                                    .disabled(pins.loading_more)
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.load_more_pins(cx)),
                                    ),
                            )
                        }),
                )
            })
    }

    /// One virtualized pin row, resolved out of the entity like the search
    /// rows above.
    fn render_pin_item(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let Some(entry) = self.message_tools.pins.entries.get(index) else {
            return div().into_any_element();
        };
        self.render_pin_entry(index, entry, cx).into_any_element()
    }

    fn render_pin_entry(
        &self,
        index: usize,
        entry: &crate::discord::message_tools::PinEntry,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let message_id = entry.message.id;
        let pinned = pinned_date(entry.pinned_at);
        let busy = self.message_tools.pins.busy.contains(&message_id);

        v_flex()
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .child(entry.message.author_name.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme::secondary_text(cx))
                            .child(pinned),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(format!("pin-row-{}", message_id)))
                    .w_full()
                    .cursor_pointer()
                    .hover(|style| style.bg(cx.theme().muted))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_pinned_message(index, window, cx)
                    }))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::secondary_text(cx))
                            .child(hit_preview(&entry.message)),
                    ),
            )
            .child(
                h_flex().justify_end().child(
                    Button::new(SharedString::from(format!("unpin-{}", message_id)))
                        .label("Unpin")
                        .ghost()
                        .xsmall()
                        .loading(busy)
                        .disabled(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.toggle_pin_message(message_id, window, cx)
                        })),
                ),
            )
    }
}

/// One row's worth of text: the content's first line, or what was sent instead
/// of text.
fn hit_preview(message: &crate::discord::Message) -> String {
    let content = message.content.trim();
    let text = if content.is_empty() {
        if !message.images.is_empty() {
            "sent an image"
        } else if !message.videos.is_empty() {
            "sent a video"
        } else if !message.embeds.is_empty() {
            "sent an embed"
        } else {
            "sent a message"
        }
    } else {
        content
    };
    let first_line = text.lines().next().unwrap_or_default();
    let mut preview: String = first_line.chars().take(160).collect();
    if preview.chars().count() < first_line.chars().count() {
        preview.push('…');
    }
    preview
}

fn pinned_date(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%b %e, %Y %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn centered(child: impl IntoElement) -> Div {
    v_flex()
        .flex_1()
        .items_center()
        .justify_center()
        .child(child)
}
