//! Rendering for [`super::InboxState`]: the header Inbox button with unread
//! badge, the virtualized Mentions & Unreads popover, and the desktop
//! notifications opt-in controls.

use std::ops::Range;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    alert::Alert,
    avatar::Avatar,
    badge::Badge,
    checkbox::Checkbox,
    divider::Divider,
    h_flex,
    setting::{RenderOptions, SettingGroup, SettingItem, SettingPage},
    skeleton::Skeleton,
    switch::Switch,
    tab::{Tab, TabBar},
    tag::Tag,
    v_flex,
};

use crate::platform::notifications::NotificationPreferences;
use crate::screens::home::HomeScreen;
use crate::ui::button::Button;
use crate::ui::depth::{self, Level, radius};
use crate::ui::theme;

use super::{InboxTab, MentionEntry, UnreadChannelEntry, UnreadPreviewState};

const POPOVER_WIDTH: f32 = 460.;
const POPOVER_LIST_HEIGHT: f32 = 420.;
const MENTION_ROW_HEIGHT: f32 = 112.;
const UNREAD_ROW_HEIGHT: f32 = 116.;
const HEADER_TOP_OFFSET: f32 = 52.;
const HEADER_RIGHT_OFFSET: f32 = 16.;

impl HomeScreen {
    /// Header button that toggles the Inbox popover and displays a badge with
    /// the current unread mentions count.
    pub(in crate::screens::home) fn render_inbox_button(
        &self,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let open = self.inbox.open;
        let badge_count = self.inbox.badge_count();
        let tooltip = if open {
            "Close Inbox".to_string()
        } else if badge_count > 0 {
            format!("Inbox ({badge_count} unread mentions)")
        } else {
            "Inbox (mentions & unreads)".to_string()
        };

        let button = Button::new("toggle-inbox")
            .icon(Icon::new(IconName::Inbox))
            .ghost()
            .small()
            .selected(open)
            .tooltip(tooltip)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_inbox(cx)));

        div().flex_shrink_0().map(|this| {
            if badge_count > 0 {
                this.child(Badge::new().count(badge_count).max(99).child(button))
            } else {
                this.child(button)
            }
        })
    }

    /// Alias for [`Self::render_inbox_popover`] so parent view wiring can call
    /// `.children(self.render_inbox(cx))` or `.children(self.render_inbox_popover(cx))`.
    pub(in crate::screens::home) fn render_inbox(&self, cx: &Context<Self>) -> Option<AnyElement> {
        self.render_inbox_popover(cx)
    }

    /// Renders the floating Inbox popover when open, anchored below the top-right
    /// conversation header.
    pub(in crate::screens::home) fn render_inbox_popover(
        &self,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !self.inbox.open {
            return None;
        }

        let theme = cx.theme();
        let active_tab = self.inbox.active_tab;
        let tab_ix = match active_tab {
            InboxTab::Mentions => 0,
            InboxTab::Unreads => 1,
        };
        let unread_count = self.inbox.unreads.len();
        let mentions_label = if self.inbox.mentions_loaded && !self.inbox.mentions.is_empty() {
            format!("Mentions ({})", self.inbox.mentions.len())
        } else {
            "Mentions".to_string()
        };
        let unreads_label = if unread_count > 0 {
            format!("Unreads ({unread_count})")
        } else {
            "Unreads".to_string()
        };

        let header = h_flex()
            .px_3()
            .py_2p5()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(
                Icon::new(IconName::Inbox)
                    .size_4()
                    .text_color(theme.foreground),
            )
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_sm()
                    .child("Inbox"),
            )
            .child(div().flex_1())
            .child(
                TabBar::new("inbox-tabs")
                    .segmented()
                    .small()
                    .selected_index(tab_ix)
                    .on_click(cx.listener(|this, ix: &usize, _, cx| {
                        let next = if *ix == 0 {
                            InboxTab::Mentions
                        } else {
                            InboxTab::Unreads
                        };
                        this.set_inbox_tab(next, cx);
                    }))
                    .child(Tab::new().label(mentions_label))
                    .child(Tab::new().label(unreads_label)),
            )
            .child(
                Button::new("inbox-close")
                    .icon(Icon::new(IconName::Close))
                    .ghost()
                    .xsmall()
                    .tooltip("Close")
                    .on_click(cx.listener(|this, _, _, cx| this.close_inbox(cx))),
            );

        let toolbar = match active_tab {
            InboxTab::Mentions => self.render_mentions_toolbar(cx).into_any_element(),
            InboxTab::Unreads => self.render_unreads_toolbar(cx).into_any_element(),
        };

        let body = match active_tab {
            InboxTab::Mentions => self.render_mentions_body(cx),
            InboxTab::Unreads => self.render_unreads_body(cx),
        };

        let action_error = self.inbox.action_error.clone();
        let notif_error = self
            .inbox
            .notifier
            .last_error()
            .map(std::string::ToString::to_string);

        let card = v_flex()
            .id("inbox-popover-card")
            .occlude()
            .w(px(POPOVER_WIDTH))
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .border_1()
            .border_color(depth::ring(cx))
            .rounded(radius::CARD)
            .shadow(depth::shadow(Level::Overlay, cx))
            .overflow_hidden()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(header)
            .child(toolbar)
            .when_some(action_error, |this, err| {
                this.child(
                    div()
                        .px_3()
                        .pt_2()
                        .child(Alert::error("inbox-action-error", err).small()),
                )
            })
            .when_some(notif_error, |this, err| {
                this.child(
                    div()
                        .px_3()
                        .pt_2()
                        .child(Alert::warning("inbox-notif-error", err).small()),
                )
            })
            .child(
                div()
                    .h(px(POPOVER_LIST_HEIGHT))
                    .w_full()
                    .px_2()
                    .py_1()
                    .child(body),
            )
            .child(self.render_inbox_footer(cx));

        Some(
            deferred(
                div()
                    .id("inbox-backdrop")
                    .absolute()
                    .inset_0()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_inbox(cx);
                        }),
                    )
                    .child(
                        div()
                            .absolute()
                            .top(px(HEADER_TOP_OFFSET))
                            .right(px(HEADER_RIGHT_OFFSET))
                            .child(card),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn render_mentions_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let filter = self.inbox.filter;
        let has_selected_guild = self.selected_guild.is_some();

        h_flex()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.25))
            .text_xs()
            .child(
                Checkbox::new("inbox-filter-everyone")
                    .label("@everyone")
                    .checked(filter.include_everyone)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_mentions_everyone(cx))),
            )
            .child(
                Checkbox::new("inbox-filter-roles")
                    .label("@roles")
                    .checked(filter.include_roles)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_mentions_roles(cx))),
            )
            .child(
                Checkbox::new("inbox-filter-guild")
                    .label("This server")
                    .checked(filter.current_guild_only)
                    .disabled(!has_selected_guild && !filter.current_guild_only)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_mentions_current_guild(cx);
                    })),
            )
            .child(div().flex_1())
            .child(
                Button::new("inbox-refresh-mentions")
                    .label("Refresh")
                    .ghost()
                    .xsmall()
                    .loading(self.inbox.mentions_loading)
                    .on_click(cx.listener(|this, _, _, cx| this.load_mentions(false, cx))),
            )
    }

    fn render_unreads_toolbar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let unread_count = self.inbox.unreads.len();

        h_flex()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.25))
            .text_xs()
            .child(
                div()
                    .text_color(theme.muted_foreground)
                    .child(if unread_count == 0 {
                        "All conversations are read".to_string()
                    } else if unread_count == 1 {
                        "1 unread conversation".to_string()
                    } else {
                        format!("{unread_count} unread conversations")
                    }),
            )
            .child(div().flex_1())
            .child(
                Button::new("inbox-mark-all-read")
                    .icon(Icon::new(IconName::Check))
                    .label("Mark All Read")
                    .outline()
                    .xsmall()
                    .loading(self.inbox.bulk_acking)
                    .disabled(unread_count == 0 || self.inbox.bulk_acking)
                    .on_click(cx.listener(|this, _, _, cx| this.ack_all_unreads(cx))),
            )
    }

    fn render_mentions_body(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        // When the mentions endpoint fails and no cached items exist, display an
        // explicit error view with a retry action rather than a blank list.
        if let Some(error) = &self.inbox.mentions_error
            && self.inbox.mentions.is_empty()
        {
            return v_flex()
                .size_full()
                .p_4()
                .gap_3()
                .items_center()
                .justify_center()
                .child(
                    Alert::error("mentions-api-error", error.clone())
                        .title("Failed to load mentions from Discord"),
                )
                .child(
                    Button::new("retry-mentions")
                        .label("Try Again")
                        .primary()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| this.load_mentions(false, cx))),
                )
                .into_any_element();
        }

        if self.inbox.mentions_loading && self.inbox.mentions.is_empty() {
            return render_inbox_skeleton().into_any_element();
        }

        if self.inbox.mentions.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1p5()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::Inbox).size_6())
                .child(
                    div()
                        .text_sm()
                        .child("No recent mentions match your filters."),
                )
                .into_any_element();
        }

        let item_count = self.inbox.mentions.len();
        v_flex()
            .size_full()
            .gap_1()
            .when_some(self.inbox.mentions_error.clone(), |this, err| {
                this.child(
                    div()
                        .px_1()
                        .child(Alert::error("mentions-page-error", err).small()),
                )
            })
            .child(
                div().flex_1().min_h_0().w_full().child(
                    uniform_list(
                        "inbox-mentions-list",
                        item_count,
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            this.render_mention_rows(range, cx)
                        }),
                    )
                    .track_scroll(self.inbox.mentions_scroll.clone())
                    .size_full(),
                ),
            )
            .when(!self.inbox.mentions_complete, |this| {
                this.child(
                    h_flex().pt_1().justify_center().child(
                        Button::new("inbox-load-more-mentions")
                            .label("Load Older Mentions")
                            .outline()
                            .xsmall()
                            .loading(self.inbox.mentions_loading_more)
                            .on_click(cx.listener(|this, _, _, cx| this.load_mentions(true, cx))),
                    ),
                )
            })
            .into_any_element()
    }

    fn render_unreads_body(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        if self.inbox.unreads.is_empty() {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_1p5()
                .text_color(theme.muted_foreground)
                .child(Icon::new(IconName::CircleCheck).size_6())
                .child(div().text_sm().child("You're all caught up!"))
                .into_any_element();
        }

        let item_count = self.inbox.unreads.len();
        uniform_list(
            "inbox-unreads-list",
            item_count,
            cx.processor(|this, range: Range<usize>, _, cx| this.render_unread_rows(range, cx)),
        )
        .track_scroll(self.inbox.unreads_scroll.clone())
        .size_full()
        .into_any_element()
    }

    fn render_mention_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::with_capacity(range.len());
        for ix in range {
            let Some(entry) = self.inbox.mentions.get(ix).cloned() else {
                continue;
            };
            rows.push(self.render_mention_row(ix, &entry, cx).into_any_element());
        }
        rows
    }

    fn render_mention_row(
        &self,
        ix: usize,
        entry: &MentionEntry,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let guild_id = entry.guild_id;
        let channel_id = entry.channel_id;
        let message_id = entry.message.id;
        let busy = entry.busy;

        let context_label = match &entry.guild_name {
            Some(guild_name) => format!("#{} \u{2022} {}", entry.channel_name, guild_name),
            None => entry.channel_name.clone(),
        };
        let guild_badge_avatar = entry.guild_name.as_ref().map(|guild_name| {
            inbox_avatar(guild_name.clone(), entry.guild_icon_url.clone(), px(16.))
        });

        let preview_text = format_message_preview(&entry.message);
        let badge_tag = if entry.message.mention_everyone {
            Some("@everyone")
        } else if !entry.message.mention_roles.is_empty() {
            Some("@role")
        } else {
            None
        };

        div()
            .id(("inbox-mention-row", ix))
            .h(px(MENTION_ROW_HEIGHT))
            .w_full()
            .py_1()
            .px_1()
            .child(
                v_flex()
                    .id(("inbox-mention-card", ix))
                    .size_full()
                    .p_2p5()
                    .gap_1p5()
                    .rounded(radius::ITEM)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background.opacity(0.6))
                    .hover(|s| s.bg(theme.list_hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.jump_to_inbox_target(guild_id, channel_id, message_id, window, cx);
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .when_some(guild_badge_avatar, |this, avatar| this.child(avatar))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::secondary_text(cx))
                                    .child(context_label),
                            )
                            .when_some(badge_tag, |this, tag| {
                                this.child(Tag::secondary().small().child(tag))
                            })
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(entry.message.timestamp_label()),
                            )
                            .child(
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        Button::new(("mention-jump", message_id.get()))
                                            .icon(Icon::new(IconName::ExternalLink))
                                            .ghost()
                                            .xsmall()
                                            .tooltip("Jump to Message")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.jump_to_inbox_target(
                                                    guild_id, channel_id, message_id, window, cx,
                                                );
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        Button::new(("mention-dismiss", message_id.get()))
                                            .icon(Icon::new(IconName::Close))
                                            .ghost()
                                            .xsmall()
                                            .loading(busy)
                                            .disabled(busy)
                                            .tooltip("Remove from Mentions")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.dismiss_mention(channel_id, message_id, cx);
                                            })),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2p5()
                            .items_start()
                            .child(inbox_avatar(
                                entry.message.author_name.clone(),
                                entry.message.author_avatar_url.clone(),
                                px(30.),
                            ))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_0p5()
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .truncate()
                                            .child(entry.message.author_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.foreground)
                                            .line_clamp(2)
                                            .child(preview_text),
                                    ),
                            ),
                    ),
            )
    }

    fn render_unread_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows = Vec::with_capacity(range.len());
        for ix in range {
            let Some(entry) = self.inbox.unreads.get(ix).cloned() else {
                continue;
            };
            rows.push(self.render_unread_row(ix, &entry, cx).into_any_element());
        }
        rows
    }

    fn render_unread_row(
        &self,
        ix: usize,
        entry: &UnreadChannelEntry,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let guild_id = entry.guild_id;
        let channel_id = entry.channel_id;
        let acked_id = entry.acked_message_id;
        let jump_message_id = match &entry.preview {
            UnreadPreviewState::Loaded(msgs) => msgs
                .iter()
                .find(|m| acked_id.is_none_or(|acked| m.id > acked))
                .or_else(|| msgs.first())
                .map_or(entry.last_message_id, |m| m.id),
            _ => entry.last_message_id,
        };
        let last_message_id = entry.last_message_id;
        let acking = entry.acking;

        let title = if entry.is_dm {
            entry.channel_name.clone()
        } else {
            format!("#{}", entry.channel_name)
        };
        let context_subtitle = match (&entry.guild_name, entry.parent_id, entry.recipient_id) {
            (Some(guild_name), Some(_), _) => Some(format!("\u{2022} {guild_name}")),
            (Some(guild_name), None, _) => Some(format!("\u{2022} {guild_name}")),
            (None, _, Some(_)) if entry.is_dm => Some("\u{2022} Direct Message".to_string()),
            _ => None,
        };
        let header_avatar = if entry.is_dm {
            Some(inbox_avatar(
                entry.channel_name.clone(),
                entry.dm_avatar_url.clone(),
                px(18.),
            ))
        } else {
            entry.guild_name.as_ref().map(|guild_name| {
                inbox_avatar(guild_name.clone(), entry.guild_icon_url.clone(), px(18.))
            })
        };

        let preview_element = match &entry.preview {
            UnreadPreviewState::NotLoaded | UnreadPreviewState::Loading => h_flex()
                .gap_2()
                .items_center()
                .child(Skeleton::new().size_6().rounded_full())
                .child(
                    v_flex()
                        .gap_1()
                        .child(Skeleton::new().w(px(140.)).h_3())
                        .child(Skeleton::new().secondary().w(px(220.)).h_3()),
                )
                .into_any_element(),
            UnreadPreviewState::Unavailable(reason) => div()
                .text_xs()
                .text_color(theme.danger)
                .line_clamp(2)
                .child(reason.clone())
                .into_any_element(),
            UnreadPreviewState::Loaded(messages) => {
                if let Some(latest) = messages.last() {
                    let count_badge =
                        (messages.len() > 1).then(|| format!("+{} more", messages.len() - 1));
                    h_flex()
                        .w_full()
                        .gap_2()
                        .items_start()
                        .child(inbox_avatar(
                            latest.author_name.clone(),
                            latest.author_avatar_url.clone(),
                            px(28.),
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .gap_0p5()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_xs()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .truncate()
                                                .child(latest.author_name.clone()),
                                        )
                                        .child(
                                            div()
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(latest.timestamp_label()),
                                        )
                                        .when_some(count_badge, |this, extra| {
                                            this.child(Tag::secondary().small().child(extra))
                                        }),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.foreground)
                                        .line_clamp(2)
                                        .child(format_message_preview(latest)),
                                ),
                        )
                        .into_any_element()
                } else {
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Unread activity in this conversation \u{2014} click to jump.")
                        .into_any_element()
                }
            }
        };

        div()
            .id(("inbox-unread-row", ix))
            .h(px(UNREAD_ROW_HEIGHT))
            .w_full()
            .py_1()
            .px_1()
            .child(
                v_flex()
                    .id(("inbox-unread-card", ix))
                    .size_full()
                    .p_2p5()
                    .gap_1p5()
                    .rounded(radius::ITEM)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background.opacity(0.6))
                    .hover(|s| s.bg(theme.list_hover))
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.jump_to_inbox_target(
                            guild_id,
                            channel_id,
                            jump_message_id,
                            window,
                            cx,
                        );
                    }))
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .items_center()
                            .when_some(header_avatar, |this, avatar| this.child(avatar))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_xs()
                                    .truncate()
                                    .child(title),
                            )
                            .when_some(context_subtitle.clone(), |this, subtitle| {
                                this.child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(subtitle),
                                )
                            })
                            .when(context_subtitle.is_none(), |this| {
                                this.child(div().flex_1())
                            })
                            .child(
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        Button::new(("unread-jump", channel_id.get()))
                                            .icon(Icon::new(IconName::ExternalLink))
                                            .ghost()
                                            .xsmall()
                                            .tooltip("Jump to Unread")
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.jump_to_inbox_target(
                                                    guild_id,
                                                    channel_id,
                                                    jump_message_id,
                                                    window,
                                                    cx,
                                                );
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation()
                                    })
                                    .child(
                                        Button::new(("unread-ack", channel_id.get()))
                                            .icon(Icon::new(IconName::Check))
                                            .ghost()
                                            .xsmall()
                                            .loading(acking)
                                            .disabled(acking)
                                            .tooltip("Mark as Read")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.ack_unread_channel(
                                                    channel_id,
                                                    last_message_id,
                                                    cx,
                                                );
                                            })),
                                    ),
                            ),
                    )
                    .child(Divider::horizontal())
                    .child(preview_element),
            )
    }

    fn render_inbox_footer(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let prefs = self.notification_preferences();
        let supported = self.inbox.notifier.is_supported();

        h_flex()
            .px_3()
            .py_2()
            .gap_3()
            .items_center()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.35))
            .text_xs()
            .child(
                Icon::new(IconName::Bell)
                    .size_3p5()
                    .text_color(theme.muted_foreground),
            )
            .child(
                Switch::new("inbox-desktop-toasts-opt-in")
                    .label("Desktop Toasts")
                    .small()
                    .checked(prefs.enabled)
                    .disabled(!supported)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let mut next = this.notification_preferences();
                        next.enabled = *checked;
                        this.set_notification_preferences(next, cx);
                    })),
            )
            .child(
                Checkbox::new("inbox-toast-privacy")
                    .label("Show preview")
                    .checked(prefs.show_content)
                    .disabled(!prefs.enabled || !supported)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let mut next = this.notification_preferences();
                        next.show_content = *checked;
                        this.set_notification_preferences(next, cx);
                    })),
            )
            .child(
                Checkbox::new("inbox-toast-mentions-only")
                    .label("Guild @mentions only")
                    .checked(prefs.only_mentions_in_guilds)
                    .disabled(!prefs.enabled || !supported)
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let mut next = this.notification_preferences();
                        next.only_mentions_in_guilds = *checked;
                        this.set_notification_preferences(next, cx);
                    })),
            )
    }
}

/// Builds a [`SettingPage`] for Desktop Notifications so the parent can mount
/// it in `src/ui/settings.rs` alongside Themes, Voice, and Updates.
pub fn notifications_settings_page(
    initial: NotificationPreferences,
    on_change: impl Fn(NotificationPreferences, &mut Window, &mut App) + Clone + 'static,
) -> SettingPage {
    let on_change_group = on_change.clone();
    SettingPage::new("Notifications")
        .description("Configure native desktop toast alerts and lockscreen privacy.")
        .resettable(false)
        .groups([SettingGroup::new()
            .title("Desktop Toast Notifications")
            .item(SettingItem::render(
                move |_: &RenderOptions, window, cx| {
                    let state = window.use_keyed_state(
                        SharedString::from("settings-notification-prefs"),
                        cx,
                        |_, _| initial,
                    );
                    let current = *state.read(cx);
                    let on_toggle_enabled = on_change_group.clone();
                    let on_toggle_preview = on_change_group.clone();
                    let on_toggle_mentions = on_change_group.clone();
                    let state_enabled = state.clone();
                    let state_preview = state.clone();
                    let state_mentions = state.clone();

                    v_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            Switch::new("settings-notif-enabled")
                                .label("Enable native desktop notifications (opt-in)")
                                .checked(current.enabled)
                                .on_click(move |checked, window, cx| {
                                    let mut next = *state_enabled.read(cx);
                                    next.enabled = *checked;
                                    state_enabled.update(cx, |s, cx| {
                                        *s = next;
                                        cx.notify();
                                    });
                                    on_toggle_enabled(next, window, cx);
                                }),
                        )
                        .child(
                            Checkbox::new("settings-notif-preview")
                                .label("Show message author and preview text (disable to hide private content on lockscreen)")
                                .checked(current.show_content)
                                .disabled(!current.enabled)
                                .on_click(move |checked, window, cx| {
                                    let mut next = *state_preview.read(cx);
                                    next.show_content = *checked;
                                    state_preview.update(cx, |s, cx| {
                                        *s = next;
                                        cx.notify();
                                    });
                                    on_toggle_preview(next, window, cx);
                                }),
                        )
                        .child(
                            Checkbox::new("settings-notif-mentions-only")
                                .label("Only notify for @mentions in servers (DMs always notify unless muted)")
                                .checked(current.only_mentions_in_guilds)
                                .disabled(!current.enabled)
                                .on_click(move |checked, window, cx| {
                                    let mut next = *state_mentions.read(cx);
                                    next.only_mentions_in_guilds = *checked;
                                    state_mentions.update(cx, |s, cx| {
                                        *s = next;
                                        cx.notify();
                                    });
                                    on_toggle_mentions(next, window, cx);
                                }),
                        )
                },
            ))])
}

fn format_message_preview(message: &crate::discord::Message) -> String {
    let trimmed = message.content.trim();
    if !trimmed.is_empty() {
        trimmed.replace('\n', " ")
    } else if !message.images.is_empty() || !message.videos.is_empty() {
        "[Attachment]".to_string()
    } else if !message.embeds.is_empty() {
        "[Embed]".to_string()
    } else if message.forward.is_some() {
        "[Forwarded Message]".to_string()
    } else {
        "[Message]".to_string()
    }
}

fn inbox_avatar(name: String, url: Option<String>, size: Pixels) -> Avatar {
    let avatar = Avatar::new().name(name).with_size(size);
    match url {
        Some(url) => avatar.src(url),
        None => avatar,
    }
}

fn render_inbox_skeleton() -> Div {
    v_flex().size_full().p_2().gap_2().children((0..3).map(|_| {
        h_flex()
            .w_full()
            .p_3()
            .gap_3()
            .items_start()
            .rounded(radius::ITEM)
            .child(Skeleton::new().size_8().rounded_full())
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(Skeleton::new().w(px(140.)).h_3())
                    .child(Skeleton::new().w(px(260.)).h_3())
                    .child(Skeleton::new().secondary().w(px(180.)).h_3()),
            )
    }))
}
