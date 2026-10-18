//! View rendering for the Friends / Relationships management screen.
//!
//! Provides the top header with tab switching, badge counters, inbox access button,
//! the Add Friend panel, search filtering, and virtualized user rows with per-row actions and confirmation modals.

use std::ops::Range;

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::ButtonVariant;
use gpui_component::dialog::DialogButtonProps;
use gpui_component::input::Input;
use gpui_component::spinner::Spinner;
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _,
    WindowExt as _, h_flex, v_flex,
};
use twilight_model::id::{Id, marker::UserMarker};

use crate::discord::PresenceStatus;
use crate::discord::relationships::*;
use crate::screens::home::HomeScreen;
use crate::screens::home::relationships::data::*;
use crate::screens::home::view::avatar;
use crate::screens::home::view::content::{header, header_content};
use crate::screens::home::view::user_panel::status_indicator;
use crate::ui::button::Button;
use crate::ui::depth::{self, radius};

impl HomeScreen {
    /// Renders the complete Friends screen in the main content pane.
    pub fn render_friends_pane(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();

        if self.relationships.loading && !self.relationships.loaded {
            return header(
                header_content()
                    .child(self.render_friends_header_tabs(cx))
                    .child(div().flex_1())
                    .child(self.render_inbox_button(cx)),
                cx,
            )
            .into_any_element();
        }

        if let Some(error) = &self.relationships.error
            && !self.relationships.loaded
        {
            return v_flex()
                .size_full()
                .child(header(
                    header_content()
                        .child(self.render_friends_header_tabs(cx))
                        .child(div().flex_1())
                        .child(self.render_inbox_button(cx)),
                    cx,
                ))
                .child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .child(div().text_color(theme.danger).child(error.clone()))
                        .child(
                            Button::new("retry-friends-load")
                                .outline()
                                .small()
                                .label("Retry")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.relationships.load(cx);
                                })),
                        ),
                )
                .into_any_element();
        }

        v_flex()
            .size_full()
            .child(header(
                header_content()
                    .child(self.render_friends_header_tabs(cx))
                    .child(div().flex_1())
                    .child(self.render_inbox_button(cx)),
                cx,
            ))
            .child(self.render_friends_body(cx))
            .into_any_element()
    }

    fn render_friends_header_tabs(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let active = self.relationships.active_tab;

        let online_count = self.relationships.online_count(&self.user_presences);
        let all_count = self.relationships.all_friends_count();
        let pending_count = self.relationships.pending_count();
        let blocked_count = self.relationships.blocked_count();

        h_flex()
            .gap_2()
            .items_center()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .pr_2()
                    .border_r_1()
                    .border_color(theme.border)
                    .child(
                        Icon::new(IconName::User)
                            .size_4()
                            .text_color(theme.muted_foreground),
                    )
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Friends")),
            )
            .child(self.render_tab_button(
                "tab-online",
                format!("Online ({online_count})"),
                active == FriendsTab::Online,
                FriendsTab::Online,
                None,
                cx,
            ))
            .child(self.render_tab_button(
                "tab-all",
                format!("All ({all_count})"),
                active == FriendsTab::All,
                FriendsTab::All,
                None,
                cx,
            ))
            .child(self.render_tab_button(
                "tab-pending",
                "Pending".to_string(),
                active == FriendsTab::Pending,
                FriendsTab::Pending,
                (pending_count > 0).then_some(pending_count),
                cx,
            ))
            .child(self.render_tab_button(
                "tab-blocked",
                format!("Blocked ({blocked_count})"),
                active == FriendsTab::Blocked,
                FriendsTab::Blocked,
                None,
                cx,
            ))
            .child(
                Button::new("tab-add-friend")
                    .small()
                    .label("Add Friend")
                    .when(active == FriendsTab::AddFriend, |b| b.primary())
                    .when(active != FriendsTab::AddFriend, |b| b.outline())
                    .on_click(cx.listener(|this, _, _, cx| {
                        let presences = this.user_presences.clone();
                        this.relationships
                            .set_tab(FriendsTab::AddFriend, &presences, cx);
                    })),
            )
    }

    fn render_tab_button(
        &self,
        id: &'static str,
        label: String,
        is_selected: bool,
        target_tab: FriendsTab,
        badge_count: Option<usize>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();

        let btn = Button::new(id)
            .small()
            .label(label)
            .ghost()
            .selected(is_selected)
            .on_click(cx.listener(move |this, _, _, cx| {
                let presences = this.user_presences.clone();
                this.relationships.set_tab(target_tab, &presences, cx);
            }));

        h_flex()
            .items_center()
            .gap_1()
            .child(btn)
            .when_some(badge_count, |this, count| {
                this.child(
                    div()
                        .px_1p5()
                        .py(px(1.))
                        .rounded_full()
                        .bg(theme.danger)
                        .text_xs()
                        .font_weight(FontWeight::BOLD)
                        .text_color(theme.danger_foreground)
                        .child(count.to_string()),
                )
            })
    }

    fn render_friends_body(&self, cx: &Context<Self>) -> impl IntoElement {
        if self.relationships.active_tab == FriendsTab::AddFriend {
            return self.render_add_friend_view(cx).into_any_element();
        }

        self.render_relationships_list_view(cx).into_any_element()
    }

    fn render_add_friend_view(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let is_submitting = self.relationships.add_friend_submitting;
        let input_text = self
            .relationships
            .add_friend_input
            .read(cx)
            .value()
            .to_string();
        let is_empty = input_text.trim().is_empty();

        v_flex()
            .flex_1()
            .p_6()
            .gap_4()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme.foreground)
                            .child("ADD FRIEND"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("You can add friends with their Discord username."),
                    ),
            )
            .child(
                h_flex()
                    .p_3()
                    .gap_3()
                    .bg(theme.input)
                    .border_1()
                    .border_color(theme.border)
                    .rounded(radius::CARD)
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.relationships.add_friend_input)),
                    )
                    .child(
                        Button::new("submit-friend-request")
                            .primary()
                            .label("Send Friend Request")
                            .disabled(is_empty || is_submitting)
                            .loading(is_submitting)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.relationships.send_friend_request(window, cx);
                            })),
                    ),
            )
            .when_some(self.relationships.add_friend_status.clone(), |this, status| {
                match status {
                    AddFriendStatus::Success(msg) => this.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_sm()
                            .text_color(theme.success)
                            .child(Icon::new(IconName::Check).size_4())
                            .child(msg.clone()),
                    ),
                    AddFriendStatus::Error(err) => this.child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .text_sm()
                            .text_color(theme.danger)
                            .child(Icon::new(IconName::TriangleAlert).size_4())
                            .child(err.clone()),
                    ),
                }
            })
            .child(div().h(px(16.)))
            .child(
                v_flex()
                    .items_center()
                    .justify_center()
                    .p_8()
                    .gap_2()
                    .text_center()
                    .child(
                        Icon::new(IconName::User)
                            .size_12()
                            .text_color(theme.muted_foreground.opacity(0.4)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Wumpus is waiting on friends. You can add them with either modern usernames or classic tags."),
                    ),
            )
    }

    fn render_relationships_list_view(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let items_count = self.relationships.filtered_ids.len();

        let section_title = match self.relationships.active_tab {
            FriendsTab::Online => format!("ONLINE \u{2014} {items_count}"),
            FriendsTab::All => format!("ALL FRIENDS \u{2014} {items_count}"),
            FriendsTab::Pending => format!("PENDING \u{2014} {items_count}"),
            FriendsTab::Blocked => format!("BLOCKED \u{2014} {items_count}"),
            FriendsTab::AddFriend => String::new(),
        };

        let empty_message = match self.relationships.active_tab {
            FriendsTab::Online => "No one's around to play with Wumpus.",
            FriendsTab::All => "You don't have any friends added yet.",
            FriendsTab::Pending => "There are no pending friend requests.",
            FriendsTab::Blocked => "You haven't blocked anyone.",
            FriendsTab::AddFriend => "",
        };

        v_flex()
            .flex_1()
            .p_4()
            .gap_3()
            .child(
                div().w_full().child(
                    Input::new(&self.relationships.search_input).small().suffix(
                        Icon::new(IconName::Search)
                            .small()
                            .text_color(theme.muted_foreground),
                    ),
                ),
            )
            .child(
                div()
                    .pt_2()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.muted_foreground)
                    .child(section_title),
            )
            .child(if items_count == 0 {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .child(
                        Icon::new(IconName::User)
                            .size_10()
                            .text_color(theme.muted_foreground.opacity(0.3)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(empty_message),
                    )
                    .into_any_element()
            } else {
                uniform_list(
                    "friends-uniform-list",
                    items_count,
                    cx.processor(move |this, range: Range<usize>, _, cx| {
                        this.render_friends_virtual_range(range, cx)
                    }),
                )
                .track_scroll(self.relationships.scroll.clone())
                .size_full()
                .pb_2()
                .into_any_element()
            })
    }

    fn render_friends_virtual_range(
        &self,
        range: Range<usize>,
        cx: &Context<Self>,
    ) -> Vec<AnyElement> {
        range
            .filter_map(|idx| {
                let user_id = self.relationships.filtered_ids.get(idx)?;
                let rel = self
                    .relationships
                    .relationships
                    .iter()
                    .find(|r| r.id == *user_id)?;
                Some(
                    div()
                        .w_full()
                        .child(self.render_relationship_row(rel, cx))
                        .into_any_element(),
                )
            })
            .collect()
    }

    fn render_relationship_row(&self, rel: &Relationship, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let user_id = rel.id;
        let user_name = rel.display_name().to_string();
        let user_tag = rel.user.tag();
        let is_busy = self.relationships.busy_users.contains(&user_id);
        let row_error = self.relationships.row_errors.get(&user_id).cloned();

        let presence = self
            .user_presences
            .get(&user_id)
            .copied()
            .unwrap_or(PresenceStatus::Invisible);

        let show_status_dot = rel.kind.is_friend() && presence != PresenceStatus::Invisible;

        let subtext = match rel.kind {
            RelationshipType::Friend => user_tag,
            RelationshipType::IncomingRequest => "Incoming Friend Request".to_string(),
            RelationshipType::OutgoingRequest => "Outgoing Friend Request".to_string(),
            RelationshipType::Blocked => "Blocked".to_string(),
            _ => user_tag,
        };

        let weak = cx.entity().downgrade();
        let user_name_clone = user_name.clone();

        h_flex()
            .id(("rel-row", user_id.get()))
            .px_3()
            .py_2()
            .items_center()
            .justify_between()
            .rounded(radius::ITEM)
            .hover(|this| this.bg(theme.sidebar_accent))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(
                        div()
                            .relative()
                            .child(avatar(user_name.clone(), Some(rel.avatar_url()), px(36.)))
                            .when(show_status_dot, |this| {
                                this.child(div().absolute().right(px(-2.)).bottom(px(-2.)).child(
                                    status_indicator(
                                        presence,
                                        px(10.),
                                        theme.background,
                                        None,
                                        theme,
                                    ),
                                ))
                            }),
                    )
                    .child(
                        v_flex()
                            .gap(px(1.))
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_sm()
                                    .text_color(theme.foreground)
                                    .child(user_name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(subtext),
                            )
                            .when_some(row_error, |this, err| {
                                this.child(div().text_xs().text_color(theme.danger).child(err))
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(is_busy, |this| this.child(Spinner::new().small()))
                    .when(!is_busy, |this| match rel.kind {
                        RelationshipType::Friend => {
                            let weak_remove = weak.clone();
                            let name_remove = user_name_clone.clone();
                            let weak_block = weak.clone();
                            let name_block = user_name_clone.clone();

                            this.child(
                                Button::new(("msg", user_id.get()))
                                    .ghost()
                                    .small()
                                    .label("Message")
                                    .tooltip("Message")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.open_or_create_direct_message(user_id, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(("rem", user_id.get()))
                                    .ghost()
                                    .small()
                                    .label("Remove")
                                    .tooltip("Remove Friend")
                                    .on_click(cx.listener(move |_this, _, window, cx| {
                                        confirm_remove_friend(
                                            user_id,
                                            name_remove.clone(),
                                            window,
                                            cx,
                                            weak_remove.clone(),
                                        );
                                    })),
                            )
                            .child(
                                Button::new(("blk", user_id.get()))
                                    .ghost()
                                    .small()
                                    .label("Block")
                                    .tooltip("Block User")
                                    .on_click(cx.listener(move |_this, _, window, cx| {
                                        confirm_block_user(
                                            user_id,
                                            name_block.clone(),
                                            window,
                                            cx,
                                            weak_block.clone(),
                                        );
                                    })),
                            )
                        }
                        RelationshipType::IncomingRequest => this
                            .child(
                                Button::new(("accept", user_id.get()))
                                    .primary()
                                    .small()
                                    .label("Accept")
                                    .tooltip("Accept Friend Request")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.relationships.accept_request(user_id, cx);
                                    })),
                            )
                            .child(
                                Button::new(("ignore", user_id.get()))
                                    .ghost()
                                    .small()
                                    .label("Ignore")
                                    .tooltip("Ignore Friend Request")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.relationships.decline_request(user_id, cx);
                                    })),
                            ),
                        RelationshipType::OutgoingRequest => this.child(
                            Button::new(("cancel", user_id.get()))
                                .ghost()
                                .small()
                                .label("Cancel")
                                .tooltip("Cancel Outgoing Request")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.relationships.cancel_request(user_id, cx);
                                })),
                        ),
                        RelationshipType::Blocked => this.child(
                            Button::new(("unblock", user_id.get()))
                                .outline()
                                .small()
                                .label("Unblock")
                                .tooltip("Unblock User")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.relationships.unblock_user(user_id, cx);
                                })),
                        ),
                        _ => this,
                    }),
            )
    }

    /// Opens an existing DM or creates one via REST if it does not exist yet.
    pub fn open_or_create_direct_message(
        &mut self,
        user_id: Id<UserMarker>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(dm) = self.dms.iter().find(|dm| dm.recipient_id == Some(user_id)) {
            let channel_id = dm.id;
            self.select_channel(channel_id, window, cx);
            return;
        }

        if self.relationships.busy_users.contains(&user_id) {
            return;
        }

        self.relationships.busy_users.insert(user_id);
        self.relationships.row_errors.remove(&user_id);
        cx.notify();

        let session_gen = self.relationships.session_generation;

        cx.spawn(async move |this, cx| {
            let result = create_or_open_dm(user_id).await;
            let _ = this.update(cx, |this, cx| {
                if this.relationships.session_generation != session_gen {
                    return;
                }
                this.relationships.busy_users.remove(&user_id);
                match result {
                    Ok(channel_id) => {
                        this.selected_channel = Some(channel_id);
                        this.load_dms(cx);
                    }
                    Err(err) => {
                        this.relationships.row_errors.insert(user_id, err);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

fn confirm_remove_friend(
    user_id: Id<UserMarker>,
    name: String,
    window: &mut Window,
    cx: &mut App,
    weak: WeakEntity<HomeScreen>,
) {
    let title = format!("Remove '{name}'");
    window.open_dialog(cx, move |dialog, _window, cx| {
        let weak = weak.clone();
        dialog
            .confirm()
            .overlay_closable(true)
            .border_color(depth::ring(cx))
            .w(px(440.))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Remove Friend")
                    .ok_variant(ButtonVariant::Danger)
                    .cancel_text("Cancel"),
            )
            .title(title.clone())
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Are you sure you want to remove {name} from your friends?"
                    )),
            )
            .on_ok(move |_, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.relationships.remove_friend(user_id, cx);
                });
                true
            })
    });
}

fn confirm_block_user(
    user_id: Id<UserMarker>,
    name: String,
    window: &mut Window,
    cx: &mut App,
    weak: WeakEntity<HomeScreen>,
) {
    let title = format!("Block '{name}'");
    window.open_dialog(cx, move |dialog, _window, cx| {
        let weak = weak.clone();
        dialog
            .confirm()
            .overlay_closable(true)
            .border_color(depth::ring(cx))
            .w(px(440.))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Block")
                    .ok_variant(ButtonVariant::Danger)
                    .cancel_text("Cancel"),
            )
            .title(title.clone())
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Are you sure you want to block {name}? They will not be able to message you or see your profile."
                    )),
            )
            .on_ok(move |_, _, cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.relationships.block_user(user_id, cx);
                });
                true
            })
    });
}
