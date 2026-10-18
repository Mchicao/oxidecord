//! Keeping track of what's been read: acking the open conversation as the
//! user reads it, marking a message unread, and the "new" divider that shows
//! where they'd read up to.

use std::time::Duration;

use gpui::*;
use twilight_model::id::{
    Id,
    marker::{ChannelMarker, MessageMarker},
};

use crate::discord;
use crate::screens::home::HomeScreen;

/// How long acks wait to be sent. Following a busy channel live would
/// otherwise send one per message; this way it's one every so often.
const ACK_DELAY: Duration = Duration::from_secs(2);

impl HomeScreen {
    /// Acks the open conversation up to its newest message, once the user can
    /// see it: it has loaded, they're scrolled to the bottom, and they haven't
    /// marked it unread — which holds until they leave it.
    pub(in crate::screens::home) fn ack_open_conversation(&mut self, cx: &mut Context<Self>) {
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        if !self.inbox.window_active
            || self.messages_loading
            || self.messages_error.is_some()
            || !self.at_bottom
            || !self.reached_newest
            || self.marked_unread
        {
            return;
        }
        let Some(newest) = self.newest_sent_message() else {
            return;
        };
        self.unreads.note_latest(channel_id, newest);
        self.queue_ack(channel_id, newest, cx);
    }

    /// Moves a channel's ack forward at once, and sends it after a moment
    /// along with any others that pile up meanwhile.
    fn queue_ack(
        &mut self,
        channel_id: Id<ChannelMarker>,
        message_id: Id<MessageMarker>,
        cx: &mut Context<Self>,
    ) {
        if !self.unreads.ack(channel_id, message_id) {
            return;
        }
        self.rebuild_inbox_unreads(cx);
        cx.notify();
        self.pending_acks.insert(channel_id, message_id);
        if self.ack_flush.is_some() {
            return;
        }
        self.ack_flush = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(ACK_DELAY).await;
            let Ok(acks) = this.update(cx, |this, _| {
                this.ack_flush = None;
                std::mem::take(&mut this.pending_acks)
            }) else {
                return;
            };
            for (channel_id, message_id) in acks {
                if let Err(err) = discord::ack_message(channel_id, message_id, false).await {
                    eprintln!("acking channel {channel_id} failed: {err}");
                }
            }
        }));
    }

    /// Marks the open conversation unread from `message_id` on, as alt-click
    /// does in Discord. The divider moves above the message, and the
    /// conversation isn't acked again until the user leaves it.
    pub(in crate::screens::home) fn mark_unread(
        &mut self,
        message_id: Id<MessageMarker>,
        cx: &mut Context<Self>,
    ) {
        let Some(channel_id) = self.selected_channel else {
            return;
        };
        // An ack needn't name a real message, so the one just before is
        // simply the snowflake before it.
        let Some(before) = Id::new_checked(message_id.get() - 1) else {
            return;
        };
        if let Some(newest) = self.newest_sent_message() {
            self.unreads.note_latest(channel_id, newest);
        }
        self.unreads.set_acked(channel_id, before);
        self.rebuild_inbox_unreads(cx);
        self.pending_acks.remove(&channel_id);
        self.marked_unread = true;
        self.set_unread_marker(Some(before));
        cx.notify();

        cx.spawn(async move |_, _| {
            if let Err(err) = discord::ack_message(channel_id, before, true).await {
                eprintln!("marking channel {channel_id} unread failed: {err}");
            }
        })
        .detach();
    }

    /// Moves the "new" divider, re-measuring the rows it leaves and lands on:
    /// the list caches each row's height, and the divider is part of its row.
    pub(in crate::screens::home) fn set_unread_marker(
        &mut self,
        marker: Option<Id<MessageMarker>>,
    ) {
        let old = self.unread_divider_index();
        self.unread_marker = marker;
        let new = self.unread_divider_index();
        for ix in [old, new].into_iter().flatten() {
            self.messages_list.splice(ix..ix + 1, 1);
        }
    }

    /// The row the "new" divider sits above: the first message past the
    /// marker. Only when the message before it is loaded too, or there is
    /// none — otherwise the marker is further back than the list goes.
    pub(in crate::screens::home) fn unread_divider_index(&self) -> Option<usize> {
        let marker = self.unread_marker?;
        let ix = self
            .messages
            .partition_point(|message| message.id <= marker);
        (ix < self.messages.len() && (ix > 0 || self.reached_oldest)).then_some(ix)
    }

    /// The newest message in the open conversation that the server has; a
    /// pending one's id is only its nonce.
    fn newest_sent_message(&self) -> Option<Id<MessageMarker>> {
        self.messages
            .iter()
            .rev()
            .find(|message| message.delivery == discord::Delivery::Sent)
            .map(|message| message.id)
    }
}
