//! The pinned-messages popout, opened from the conversation header's pin
//! button.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{ActiveTheme as _, Icon, Sizable as _, h_flex, spinner::Spinner, v_flex};

use crate::discord;
use crate::screens::home::HomeScreen;
use crate::screens::home::view::avatar;
use crate::ui::depth::{self, Level, radius};

use super::super::markdown::MarkdownOptions;
use super::MEDIA_MAX_WIDTH;

const MAX_LIST_HEIGHT: f32 = 480.;
const AVATAR_SIZE: f32 = 32.;
const PIN_PADDING: f32 = 12.;
/// Where a pin's content starts, clear of the avatar in its gutter.
const CONTENT_INSET: f32 = PIN_PADDING + AVATAR_SIZE + 12.;
/// Images and videos are laid out at a fixed size up to [`MEDIA_MAX_WIDTH`],
/// and don't shrink to their column, so the popout is as wide as it takes for
/// that column to hold them: the borders, the list's padding, and the pin's
/// padding and avatar gutter.
const WIDTH: f32 = MEDIA_MAX_WIDTH + 2. * 1. + 2. * 8. + 2. * 1. + CONTENT_INSET + PIN_PADDING;

impl HomeScreen {
    /// The open pins, over the whole app like the emoji picker: a layer that
    /// closes them on a click outside, with the card hanging below the header
    /// from the pin button.
    pub(in crate::screens::home) fn render_pins_popup(
        &self,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        let popup = self.pins_popup.as_ref()?;
        let theme = cx.theme();

        let body = if popup.loading {
            h_flex()
                .justify_center()
                .py_6()
                .child(Spinner::new().color(theme.muted_foreground))
                .into_any_element()
        } else if let Some(error) = &popup.error {
            div()
                .p_4()
                .text_sm()
                .text_color(theme.danger)
                .child(error.clone())
                .into_any_element()
        } else if popup.messages.is_empty() {
            v_flex()
                .items_center()
                .gap_2()
                .py_6()
                .px_4()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(
                    Icon::default()
                        .path("icons/pin.svg")
                        .size_8()
                        .text_color(theme.muted_foreground),
                )
                .child("This conversation doesn't have any pinned messages... yet.")
                .into_any_element()
        } else {
            v_flex()
                .id("pins-list")
                .max_h(px(MAX_LIST_HEIGHT))
                .overflow_y_scroll()
                .p_2()
                .gap_2()
                .children(
                    popup
                        .messages
                        .iter()
                        .map(|message| self.render_pin(message, cx)),
                )
                .into_any_element()
        };

        let card = v_flex()
            .occlude()
            .w(px(WIDTH))
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(depth::ring(cx))
            .rounded(radius::CARD)
            .shadow(depth::shadow(Level::Overlay, cx))
            .overflow_hidden()
            // Clicks inside the card are for the card, not the dismiss layer
            // it sits on.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                h_flex()
                    .h(px(44.))
                    .px_4()
                    .gap_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .bg(theme.muted.opacity(0.4))
                    .child(Icon::default().path("icons/pin.svg").small())
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Pinned Messages"),
                    ),
            )
            .child(body);

        Some(
            deferred(
                div()
                    .absolute()
                    .inset_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            // Consumed, so a click on the pin button closes
                            // the popout instead of closing and reopening it.
                            cx.stop_propagation();
                            this.close_pins(cx);
                        }),
                    )
                    .child(
                        anchored()
                            .anchor(Corner::TopRight)
                            .position(popup.position)
                            .snap_to_window_with_margin(px(8.))
                            .child(card),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// One pinned message: who sent it and when, over what it says.
    fn render_pin(&self, message: &discord::Message, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let has_media =
            !message.images.is_empty() || !message.videos.is_empty() || !message.embeds.is_empty();

        // The avatar is laid over a padded gutter rather than set beside the
        // content in a row: inside a row, the content's text is measured as
        // if it had no width — a one-line caption wraps a character per line
        // — and the pin keeps the height of all those lines, empty, below it.
        div()
            .relative()
            .flex_shrink_0()
            .w_full()
            .p(px(PIN_PADDING))
            .pl(px(CONTENT_INSET))
            .rounded(radius::ITEM)
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .child(
                div()
                    .absolute()
                    .top(px(PIN_PADDING))
                    .left(px(PIN_PADDING))
                    .child(avatar(
                        message.author_name.clone(),
                        message.author_avatar_url.clone(),
                        px(AVATAR_SIZE),
                    )),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .when_some(self.role_color(message.author_id), |this, color| {
                                        this.text_color(rgb(color))
                                    })
                                    .child(message.author_name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(message.timestamp_label()),
                            ),
                    )
                    .when(!message.content.is_empty(), |this| {
                        this.child(
                            div().w_full().min_w_0().text_sm().child(
                                self.render_markdown(
                                    &message.markdown,
                                    // Apart from the list's, so a spoiler
                                    // revealed here stays hidden there.
                                    MarkdownOptions::new(
                                        format!("pin-{}", message.id),
                                        &message.mentions,
                                    )
                                    .edited(message.edited_label()),
                                    cx,
                                ),
                            ),
                        )
                    })
                    .when_some(message.forward.as_ref(), |this, forward| {
                        this.child(self.render_forward(message.id.get(), forward, cx))
                    })
                    .when(has_media, |this| {
                        this.child(self.render_media(
                            message.id.get(),
                            &message.images,
                            &message.videos,
                            &message.embeds,
                            &message.mentions,
                            cx,
                        ))
                    }),
            )
    }
}
