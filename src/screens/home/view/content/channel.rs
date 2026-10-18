//! The guild-channel conversation: its header, messages, and composer.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, divider::Divider, h_flex, v_flex,
};

use crate::discord::Channel;
use crate::screens::home::HomeScreen;
use crate::screens::home::channels::channel_icon_path;
use crate::ui::button::Button;
use crate::ui::theme;

use super::super::markdown::MarkdownOptions;
use super::{header, header_content, message_list, pane};

impl HomeScreen {
    pub(super) fn render_channel_content(&self, cx: &Context<Self>) -> AnyElement {
        let Some(channel) = self.selected_channel_info().cloned() else {
            return pane(message_list::skeleton().into_any_element(), cx);
        };

        let pane = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.render_channel_header(&channel, cx));

        // A voice channel's pane is the call itself: no history, no composer.
        if channel.kind.is_voice() {
            return pane.child(self.render_voice_stage(cx)).into_any_element();
        }
        if channel.kind == crate::discord::ChannelKind::Forum || self.threads_open {
            return pane.child(self.render_threads_panel(cx)).into_any_element();
        }

        // The member list runs from under the header to the bottom of the
        // pane, beside the messages and the composer both.
        let conversation = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(self.render_messages(cx))
            .child(self.render_message_bar(self.can_send_to_selected(), cx));

        pane.child(
            h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(conversation)
                .children(self.render_member_list(cx)),
        )
        .into_any_element()
    }

    fn render_channel_header(&self, channel: &Channel, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        header(
            header_content()
                .child(
                    Icon::default()
                        .path(channel_icon_path(channel.kind))
                        .size_5()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(channel.name.clone()),
                )
                .when_some(channel.topic.as_ref(), |this, topic| {
                    this.child(Divider::vertical().h(px(24.))).child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(theme::secondary_text(cx))
                            .child(self.render_inline_markdown(
                                topic,
                                MarkdownOptions::new(format!("topic-{}", channel.id), &[]),
                                cx,
                            )),
                    )
                })
                // Pushes the controls to the right when there's no topic to
                // fill the space.
                .when(channel.topic.is_none(), |this| this.child(div().flex_1()))
                .when(!channel.kind.is_voice(), |this| {
                    let hidden = self.member_list_hidden;
                    this.child(
                        Button::new("toggle-member-list")
                            .icon(Icon::default().path("icons/users.svg"))
                            .ghost()
                            .small()
                            .selected(!hidden)
                            .tooltip(if hidden {
                                "Show Member List"
                            } else {
                                "Hide Member List"
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_member_list(cx))),
                    )
                    .child(self.render_inbox_button(cx))
                    .child(
                        Button::new("channel-pins")
                            .icon(Icon::default().path("icons/pin.svg"))
                            .small()
                            .ghost()
                            .tooltip("Pinned Messages")
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_pin_panel(cx))),
                    )
                    .child(
                        Button::new("channel-threads")
                            .icon(Icon::default().path("icons/messages-square.svg"))
                            .small()
                            .ghost()
                            .tooltip("Threads")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_threads_panel(window, cx)
                            })),
                    )
                    .child(self.render_search_box(cx))
                }),
            cx,
        )
    }
}
