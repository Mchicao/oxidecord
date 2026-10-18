//! A message's menu of actions, opened from its toolbar's "More" button or by
//! right-clicking the message.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{
    ActiveTheme as _, Icon, IconName,
    menu::{PopupMenu, PopupMenuItem},
};
use twilight_model::id::{Id, marker::MessageMarker};

use crate::discord;
use crate::screens::home::{EmojiTarget, HomeScreen, ReplyTarget};

/// What a message's menu offers, decided as the message renders.
#[derive(Clone)]
pub(super) struct MessageMenu {
    pub message_id: Id<MessageMarker>,
    pub reply: ReplyTarget,
    pub can_edit: bool,
    pub can_delete: bool,
    pub has_reactions: bool,
    pub pin: Option<crate::screens::home::message_tools::PinMenuAction>,
    pub can_create_thread: bool,
}

impl MessageMenu {
    pub(super) fn new(screen: &HomeScreen, message: &discord::Message) -> Self {
        Self {
            message_id: message.id,
            reply: ReplyTarget {
                message_id: message.id,
                author_name: message.author_name.clone(),
            },
            // Already open for editing, the action would have nothing to do.
            can_edit: screen.is_own_message(message)
                && screen
                    .editing
                    .as_ref()
                    .is_none_or(|editing| editing.message_id != message.id),
            can_delete: screen.can_delete_message(message),
            has_reactions: !message.reactions.is_empty(),
            pin: screen.pin_menu_action(message),
            can_create_thread: screen.selected_channel_info().is_some_and(|channel| {
                !channel.kind.is_thread()
                    && channel
                        .permissions
                        .contains(discord::Permissions::CREATE_PUBLIC_THREADS)
            }),
        }
    }

    /// Fills `menu`, in Discord's order. `full` adds the actions the toolbar
    /// already has buttons for — reacting and replying — which the right-click
    /// menu needs and the toolbar's own "More" menu doesn't.
    pub(super) fn build(
        &self,
        menu: PopupMenu,
        full: bool,
        screen: WeakEntity<HomeScreen>,
        cx: &App,
    ) -> PopupMenu {
        let message_id = self.message_id;
        let danger = cx.theme().danger;
        // Each item runs its action against the screen, if it's still there.
        let action =
            |run: fn(&mut HomeScreen, Id<MessageMarker>, &mut Window, &mut Context<HomeScreen>)| {
                let screen = screen.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    if let Some(screen) = screen.upgrade() {
                        screen.update(cx, |this, cx| run(this, message_id, window, cx));
                    }
                }
            };

        let menu = menu
            .when(full, |menu| {
                menu.item(
                    PopupMenuItem::new("Add Reaction")
                        .icon(Icon::default().path("icons/smile-plus.svg"))
                        .on_click(action(|this, message_id, window, cx| {
                            // The picker opens beside the item that was picked,
                            // where the pointer still is.
                            this.toggle_emoji_picker(
                                window.mouse_position(),
                                EmojiTarget::Reaction(message_id),
                                window,
                                cx,
                            );
                        })),
                )
            })
            .when(self.has_reactions, |menu| {
                menu.item(
                    PopupMenuItem::new("View Reactions")
                        .icon(Icon::default().path("icons/smile.svg"))
                        .on_click(action(|this, message_id, window, cx| {
                            this.view_reactions(message_id, None, window, cx)
                        })),
                )
            })
            .when(full && self.has_reactions, |menu| menu.separator())
            .when(self.can_edit, |menu| {
                menu.item(
                    PopupMenuItem::new("Edit Message")
                        .icon(Icon::default().path("icons/pencil.svg"))
                        .on_click(action(|this, message_id, window, cx| {
                            this.start_editing(message_id, window, cx)
                        })),
                )
            })
            .when(full, |menu| {
                let screen = screen.clone();
                let reply = self.reply.clone();
                menu.item(PopupMenuItem::new("Reply").icon(IconName::Undo2).on_click(
                    move |_, window, cx| {
                        if let Some(screen) = screen.upgrade() {
                            screen
                                .update(cx, |this, cx| this.start_reply(reply.clone(), window, cx));
                        }
                    },
                ))
            })
            .item(
                PopupMenuItem::new("Mark Unread")
                    .icon(Icon::default().path("icons/mail.svg"))
                    .on_click(action(|this, message_id, _, cx| {
                        this.mark_unread(message_id, cx)
                    })),
            )
            .item(
                PopupMenuItem::new("Copy Message Link")
                    .icon(Icon::default().path("icons/link.svg"))
                    .on_click(action(|this, message_id, _, cx| {
                        this.copy_message_link(message_id, cx)
                    })),
            );
        let menu = menu
            .when_some(self.pin, |menu, pin| {
                menu.item(
                    PopupMenuItem::new(
                        if pin == crate::screens::home::message_tools::PinMenuAction::Pin {
                            "Pin Message"
                        } else {
                            "Unpin Message"
                        },
                    )
                    .icon(Icon::default().path("icons/pin.svg"))
                    .on_click(action(|this, id, window, cx| {
                        this.toggle_pin_message(id, window, cx)
                    })),
                )
            })
            .when(self.can_create_thread, |menu| {
                menu.item(
                    PopupMenuItem::new("Create Thread")
                        .icon(Icon::default().path("icons/messages-square.svg"))
                        .on_click(action(|this, id, window, cx| {
                            this.threads_open = true;
                            this.sync_thread_parent(window, cx);
                            if let Some(channel) = this.selected_channel {
                                this.threads
                                    .begin_create_from_message(channel, id, window, cx);
                            }
                        })),
                )
            });
        if !self.can_delete {
            return menu;
        }

        menu.separator().item(
            PopupMenuItem::element(move |_, _| div().text_color(danger).child("Delete Message"))
                .icon(Icon::default().path("icons/trash-2.svg").text_color(danger))
                .on_click(action(|this, message_id, _, cx| {
                    this.delete_message(message_id, cx)
                })),
        )
    }
}

impl HomeScreen {
    /// Starts a reply to a message and jumps straight to composing it, like
    /// Discord.
    pub(super) fn start_reply(
        &mut self,
        target: ReplyTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replying_to = Some(target);
        self.message_input.focus_handle(cx).focus(window);
        cx.notify();
    }

    /// Opens a message's menu where it was right-clicked, keeping the message
    /// lit while the menu is up, as the toolbar's "More" menu does.
    pub(super) fn message_context_menu(
        &self,
        row: Stateful<Div>,
        menu: MessageMenu,
        cx: &Context<Self>,
    ) -> impl IntoElement + use<> {
        use gpui_component::menu::ContextMenuExt as _;

        let screen = cx.entity().downgrade();
        row.context_menu(move |popup, _, cx| {
            let message_id = menu.message_id;
            if let Some(screen) = screen.upgrade() {
                screen.update(cx, |this, cx| {
                    this.message_menu = Some(message_id);
                    cx.notify();
                });
                cx.subscribe_self(move |_, _: &DismissEvent, cx| {
                    screen.update(cx, |this, cx| {
                        if this.message_menu == Some(message_id) {
                            this.message_menu = None;
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
            menu.build(popup, true, screen.clone(), cx)
        })
    }
}
