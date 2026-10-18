//! Rendering the threads panel: the tab header over the active list and the
//! archive, the thread rows, and the create forms for threads and forum
//! posts.
//!
//! The panel is one function the screen's content hook drops below the
//! conversation header when the open channel carries threads; the floating
//! form for a thread started from a message is a second one the content hook
//! draws over the conversation while it is open.

use std::ops::Range;

use gpui::prelude::*;
use gpui::{AnyElement, FontWeight, SharedString, div, px, uniform_list};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex,
    input::Input, v_flex,
};

use crate::discord::{ArchivedScope, ForumTag, Permissions, ThreadSummary};
use crate::screens::home::HomeScreen;
use crate::ui::button::Button;
use crate::ui::theme;

use super::{
    CreateSource, Tab,
    data::{can_archive, can_create_thread, can_join, can_list_archived, can_view},
};

/// Every row is the same height so the list virtualizes: a name line and a
/// meta line, whatever the preview would like to say.
const ROW_HEIGHT: f32 = 60.;

/// How many tag chips a row shows before it stops.
const ROW_TAG_LIMIT: usize = 3;

impl HomeScreen {
    /// The whole panel body: tabs, list, and the inline create form. The
    /// screen's content hook places it for a channel that carries threads —
    /// a forum/media channel, or the "Threads" browser of a text channel.
    pub(in crate::screens::home) fn render_threads_panel(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        if !can_view(self.threads.permissions) {
            return empty_notice("You don't have access to this channel.", cx);
        }

        let panel = v_flex()
            .flex_1()
            .min_h_0()
            .child(self.render_threads_header(cx))
            .children(
                self.threads
                    .error
                    .as_ref()
                    .map(|error| error_banner("threads-error", error.clone(), cx)),
            )
            .child(self.render_threads_list(cx));

        let may_create = self.threads.parent.is_some()
            && (self.threads.forum && can_create_thread(self.threads.permissions, true, false)
                || !self.threads.forum
                    && (can_create_thread(self.threads.permissions, false, false)
                        || can_create_thread(self.threads.permissions, false, true)));

        let panel = if self.threads.creating == Some(CreateSource::Channel) {
            panel.child(self.render_create_card(cx))
        } else if may_create {
            panel.child(
                h_flex()
                    .flex_shrink_0()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("threads-new")
                            .primary()
                            .small()
                            .icon(Icon::new(IconName::Plus))
                            .label(if self.threads.forum {
                                "New Post"
                            } else {
                                "New Thread"
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.threads.begin_create(window, cx)
                            })),
                    ),
            )
        } else {
            panel
        };

        panel.into_any_element()
    }

    /// The tab strip, with the archive's scope picker beside it while the
    /// archive is showing.
    fn render_threads_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        let tab = |id: &'static str, tab: Tab, label: &'static str| {
            Button::new(id)
                .ghost()
                .xsmall()
                .label(label)
                .selected(self.threads.tab == tab)
                .on_click(cx.listener(move |this, _, _, cx| this.threads.select_tab(tab, cx)))
        };

        h_flex()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(tab("threads-tab-active", Tab::Active, "Active"))
            .child(tab("threads-tab-archived", Tab::Archived, "Archived"))
            .when(self.threads.tab == Tab::Archived, |strip| {
                strip
                    .child(div().flex_1())
                    .child(self.render_scope_picker(cx))
            })
    }

    /// The three archive scopes, each disabled while the current user lacks
    /// the permission its route asks for.
    fn render_scope_picker(&self, cx: &Context<Self>) -> impl IntoElement {
        let scopes = [
            (ArchivedScope::Public, "Public"),
            (ArchivedScope::Private, "Private"),
            (ArchivedScope::JoinedPrivate, "Joined"),
        ];
        h_flex().gap_1().children(scopes.map(|(scope, label)| {
            Button::new(SharedString::from(format!(
                "threads-scope-{}",
                label.to_lowercase()
            )))
            .ghost()
            .xsmall()
            .label(label)
            .selected(self.threads.scope == scope)
            .disabled(!can_list_archived(self.threads.permissions, scope))
            .tooltip(match scope {
                ArchivedScope::Public => "Archived public threads",
                ArchivedScope::Private => "All archived private threads (moderator)",
                ArchivedScope::JoinedPrivate => "Archived private threads you joined",
            })
            .on_click(cx.listener(move |this, _, _, cx| this.threads.select_scope(scope, cx)))
        }))
    }

    /// The list itself: skeleton while the first page loads, a notice when
    /// there is nothing, and "load more" while the server has more.
    fn render_threads_list(&self, cx: &Context<Self>) -> AnyElement {
        let threads = self.threads.visible();
        let first_page = threads.is_empty()
            && match self.threads.tab {
                Tab::Active => self.threads.active_loading,
                Tab::Archived => self.threads.archived_loading,
            };

        let body = if first_page {
            v_flex()
                .size_full()
                .pt_2()
                .children((0..6).map(|_| skeleton_row(cx)))
                .into_any_element()
        } else if threads.is_empty() {
            let notice = match self.threads.tab {
                Tab::Active => "No threads yet.",
                Tab::Archived => "Nothing archived here.",
            };
            empty_notice(notice, cx)
        } else {
            let count = threads.len();
            v_flex()
                .size_full()
                .child(
                    uniform_list(
                        "threads-list",
                        count,
                        cx.processor(|this, range: Range<usize>, _, cx| {
                            range
                                .map(|index| this.render_thread_row(index, cx))
                                .collect()
                        }),
                    )
                    .flex_1()
                    .min_h_0(),
                )
                .children(self.render_load_more(cx))
                .into_any_element()
        };

        div().flex_1().min_h_0().child(body).into_any_element()
    }

    /// The "load more" row under a list with another page coming, replaced
    /// by a quiet "Loading…" while the next page flies.
    fn render_load_more(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let loading = match self.threads.tab {
            Tab::Active => self.threads.active_loading,
            Tab::Archived => self.threads.archived_loading,
        };
        if self.threads.loading_more && loading {
            return Some(
                h_flex().flex_shrink_0().justify_center().p_2().child(
                    div()
                        .text_xs()
                        .text_color(theme::secondary_text(cx))
                        .child("Loading…"),
                ),
            );
        }
        let has_more = match self.threads.tab {
            Tab::Active => self.threads.has_more_active,
            Tab::Archived => self.threads.has_more_archived,
        };
        if !has_more || loading {
            return None;
        }
        Some(
            h_flex().flex_shrink_0().justify_center().p_2().child(
                Button::new("threads-load-more")
                    .ghost()
                    .small()
                    .label("Load more")
                    .on_click(cx.listener(|this, _, _, cx| match this.threads.tab {
                        Tab::Active => this.threads.load_more_active(cx),
                        Tab::Archived => this.threads.load_more_archived(cx),
                    })),
            ),
        )
    }

    fn render_thread_row(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(thread) = self.threads.visible().get(index) else {
            return div().into_any_element();
        };
        let thread_id = thread.id;
        let selected = self.selected_channel == Some(thread_id);
        let archived = thread.archived;
        let locked = thread.locked;
        let joined = thread.joined;
        let is_owner = self.self_user_id == thread.owner_id;
        let busy = self.threads.mutating.is_some();

        let row = h_flex()
            .id(SharedString::from(format!("threads-row-{thread_id}")))
            .h(px(ROW_HEIGHT))
            .flex_1()
            .min_w_0()
            .px_3()
            .gap_2()
            .items_center()
            .rounded(px(6.))
            .when(selected, |row| row.bg(theme.secondary))
            .hover(|row| row.bg(theme.secondary).cursor_pointer())
            .on_click(
                cx.listener(move |this, _, window, cx| this.select_channel(thread_id, window, cx)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(if archived {
                                        theme.muted_foreground
                                    } else {
                                        theme.foreground
                                    })
                                    .child(thread.name.clone()),
                            )
                            .when(thread.pinned, |line| {
                                line.child(
                                    Icon::new(IconName::Star)
                                        .xsmall()
                                        .text_color(theme.muted_foreground),
                                )
                            })
                            .when(locked, |line| line.child(badge("Locked", cx))),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .min_w_0()
                            .overflow_hidden()
                            .child(self.render_row_tags(thread, cx))
                            .when_some(row_meta(thread), |line, meta| {
                                line.child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(meta),
                                )
                            }),
                    ),
            )
            // The row actions ride on the right: follow, then archive. A
            // busy panel dims them until the server answers.
            .when(can_join(thread) && !busy, |row| {
                row.child(
                    Button::new(SharedString::from(format!("threads-join-{thread_id}")))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Bell))
                        .selected(joined)
                        .tooltip(if joined { "Unfollow" } else { "Follow" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.threads.toggle_membership(thread_id, cx)
                        })),
                )
            })
            .when(
                !busy && can_archive(self.threads.permissions, thread, !archived, is_owner),
                |row| {
                    row.child(
                        Button::new(SharedString::from(format!("threads-archive-{thread_id}")))
                            .ghost()
                            .xsmall()
                            .label(if archived { "Unarchive" } else { "Archive" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.threads.set_archived(thread_id, !archived, cx)
                            })),
                    )
                },
            );

        div().w_full().px_2().py_0p5().child(row).into_any_element()
    }

    /// A row's tag chips: the applied tags named from the parent channel's
    /// own list. A tag the channel no longer offers shows without a name,
    /// since a thread's id alone says nothing.
    fn render_row_tags(&self, thread: &ThreadSummary, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let tags: Vec<&ForumTag> = thread
            .applied_tags
            .iter()
            .take(ROW_TAG_LIMIT)
            .filter_map(|id| self.threads.forum_tags.iter().find(|tag| &tag.id == id))
            .collect();
        let overflow = thread.applied_tags.len().saturating_sub(ROW_TAG_LIMIT);

        h_flex()
            .flex_shrink_0()
            .gap_1()
            .children(tags.into_iter().map(|tag| {
                div()
                    .flex_shrink_0()
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(4.))
                    .text_xs()
                    .bg(theme.secondary)
                    .text_color(if tag.moderated {
                        theme.muted_foreground
                    } else {
                        theme.foreground
                    })
                    .child(tag_label(tag))
            }))
            .when(overflow > 0, |chips| {
                chips.child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("+{overflow}")),
                )
            })
    }

    /// The create card: name, a post's first message, tags or the
    /// public/private pick, its error line, and the two buttons. It stands
    /// at the panel's footer for the channel source, and floats over the
    /// conversation for a message source.
    fn render_create_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let forum = self.threads.forum;
        let public_allowed = forum || can_create_thread(self.threads.permissions, false, false);
        let private_allowed = !forum && can_create_thread(self.threads.permissions, false, true);
        let can_submit = if forum {
            public_allowed
        } else if self.threads.private_thread {
            private_allowed
        } else {
            public_allowed
        };

        v_flex()
            .flex_shrink_0()
            .gap_2()
            .p_3()
            .border_t_1()
            .border_color(theme.border)
            .child(Input::new(&self.threads.name_input).small())
            .when(forum, |card| {
                card.child(Input::new(&self.threads.body_input).small())
            })
            .when(forum && !self.threads.forum_tags.is_empty(), |card| {
                card.child(h_flex().flex_wrap().gap_1().children(
                    self.threads.forum_tags.iter().map(|tag| {
                        let tag_id = tag.id;
                        // A moderated tag answers to MANAGE_THREADS
                        // before the server ever sees the request.
                        let pickable = !tag.moderated
                            || self
                                .threads
                                .permissions
                                .contains(Permissions::MANAGE_THREADS);
                        Button::new(SharedString::from(format!("threads-tag-{tag_id}")))
                            .ghost()
                            .xsmall()
                            .label(tag_label(tag))
                            .selected(self.threads.selected_tags.contains(&tag_id))
                            .disabled(!pickable)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.threads.toggle_tag(tag_id, cx)
                            }))
                    }),
                ))
            })
            .when(!forum, |card| {
                card.child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("threads-kind-public")
                                .ghost()
                                .xsmall()
                                .label("Public")
                                .selected(!self.threads.private_thread)
                                .disabled(!public_allowed)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.threads.set_private(false, cx)
                                })),
                        )
                        .child(
                            Button::new("threads-kind-private")
                                .ghost()
                                .xsmall()
                                .label("Private")
                                .selected(self.threads.private_thread)
                                .disabled(!private_allowed)
                                .on_click(
                                    cx.listener(|this, _, _, cx| {
                                        this.threads.set_private(true, cx)
                                    }),
                                ),
                        ),
                )
            })
            .children(self.threads.create_error.as_ref().map(|error| {
                div()
                    .text_xs()
                    .text_color(theme.danger)
                    .child(error.clone())
            }))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("threads-create-cancel")
                            .ghost()
                            .small()
                            .label("Cancel")
                            .disabled(self.threads.submitting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.threads.close_create(window, cx)
                            })),
                    )
                    .child(
                        Button::new("threads-create-submit")
                            .primary()
                            .small()
                            .label(if forum { "Post" } else { "Create" })
                            .disabled(!can_submit || self.threads.submitting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.threads.submit_create(window, cx)
                            })),
                    ),
            )
    }

    /// The floating create card for a thread started from a message. The
    /// content hook renders this over the conversation while the message
    /// source is open; it hides itself once the form closes.
    pub(in crate::screens::home) fn render_thread_create_form(
        &self,
        cx: &Context<Self>,
    ) -> Option<AnyElement> {
        if !matches!(self.threads.creating, Some(CreateSource::Message(_, _))) {
            return None;
        }
        Some(
            div()
                .absolute()
                .top_4()
                .left_4()
                .w(px(420.))
                .max_w_full()
                .rounded(px(8.))
                .border_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .shadow_md()
                .overflow_hidden()
                .child(self.render_create_card(cx))
                .into_any_element(),
        )
    }
}

/// The small caps a row annotates itself with.
fn badge(label: &'static str, cx: &Context<HomeScreen>) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .px_1p5()
        .py_0p5()
        .rounded(px(4.))
        .text_xs()
        .bg(cx.theme().secondary)
        .text_color(cx.theme().muted_foreground)
        .child(label)
}

/// A forum tag as its chip reads: the unicode emoji it carries, if any,
/// before its name. Custom-emoji tags show their name alone; the id alone
/// can't be drawn here without the guild's emoji, which the picker owns.
fn tag_label(tag: &ForumTag) -> String {
    match tag.emoji_name.as_deref() {
        Some(emoji) if !emoji.is_empty() => format!("{emoji} {}", tag.name),
        _ => tag.name.clone(),
    }
}

/// The line under a thread's name: the post's first message when the search
/// supplied one, else the counters Discord shows. `None` when neither is
/// known, which leaves the row to the tags alone.
fn row_meta(thread: &ThreadSummary) -> Option<String> {
    if let Some(preview) = &thread.preview {
        return Some(preview.clone());
    }
    let mut parts = Vec::new();
    if let Some(count) = thread.message_count {
        parts.push(format!(
            "{count} message{}",
            if count == 1 { "" } else { "s" }
        ));
    }
    if let Some(count) = thread.member_count {
        parts.push(format!(
            "{count} member{}",
            if count == 1 { "" } else { "s" }
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// The quiet placeholder rows of a first load.
fn skeleton_row(cx: &Context<HomeScreen>) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .h(px(ROW_HEIGHT))
        .w_full()
        .px_3()
        .items_center()
        .child(
            v_flex()
                .gap_2()
                .child(
                    div()
                        .h(px(12.))
                        .w(px(180.))
                        .rounded(px(4.))
                        .bg(theme.secondary),
                )
                .child(
                    div()
                        .h(px(10.))
                        .w(px(120.))
                        .rounded(px(4.))
                        .bg(theme.secondary),
                ),
        )
}

/// A centered quiet notice, for empty lists and missing access.
fn empty_notice(notice: &str, cx: &Context<HomeScreen>) -> AnyElement {
    div()
        .flex_1()
        .items_center()
        .justify_center()
        .p_4()
        .text_sm()
        .text_color(theme::secondary_text(cx))
        .child(notice.to_string())
        .into_any_element()
}

/// The banner under the tabs: what went wrong, and the way back in.
fn error_banner(id: &'static str, error: String, cx: &Context<HomeScreen>) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .flex_shrink_0()
        .items_center()
        .gap_2()
        .px_3()
        .py_1p5()
        .bg(theme.danger.opacity(0.08))
        .text_color(theme.danger)
        .child(div().min_w_0().flex_1().truncate().text_xs().child(error))
        .child(
            Button::new(id)
                .ghost()
                .xsmall()
                .label("Retry")
                .on_click(cx.listener(|this, _, _, cx| this.threads.retry(cx))),
        )
}
