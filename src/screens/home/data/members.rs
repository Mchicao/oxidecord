//! Guild members' roles: the signed-in user's, which decide what pings them
//! and which emoji they can use, and everyone else's, which colour their names.

use std::cmp::Reverse;
use std::iter;

use twilight_model::id::{
    Id,
    marker::{GuildMarker, RoleMarker, UserMarker},
};

use crate::screens::home::{HomeScreen, View};

impl HomeScreen {
    /// The roles the signed-in user holds in a guild, once the gateway has
    /// said.
    pub(in crate::screens::home) fn self_roles(
        &self,
        guild_id: Id<GuildMarker>,
    ) -> Option<&[Id<RoleMarker>]> {
        let self_id = self.self_user_id?;
        Some(self.member_roles.get(&guild_id)?.get(&self_id)?)
    }

    /// The colour a user's name takes in the open guild, as packed `0xRRGGBB`:
    /// that of the highest of their roles that has one, ties going to the
    /// older role as in Discord's own list. `None` in a DM, or while their
    /// roles aren't known.
    pub(in crate::screens::home) fn role_color(&self, user_id: Id<UserMarker>) -> Option<u32> {
        let guild_id = self.selected_guild.filter(|_| self.view == View::Guild)?;
        let roles = self.guild_roles.get(&guild_id)?;
        self.member_roles
            .get(&guild_id)?
            .get(&user_id)?
            .iter()
            .filter_map(|id| roles.get(id))
            .filter(|role| role.color.is_some())
            .max_by_key(|role| (role.position, Reverse(role.id)))?
            .color
    }

    /// Asks the gateway for the members behind the open conversation's
    /// authors, and the authors its replies quote, whose roles aren't known
    /// yet. Fetched history carries no members, and a user session is only
    /// sent a few of a guild's up front, so without this most names would go
    /// uncoloured. Called as each page of history lands.
    pub(in crate::screens::home) fn request_author_members(&mut self) {
        let Some(guild_id) = self.selected_guild.filter(|_| self.view == View::Guild) else {
            return;
        };
        let Some(gateway) = &self.gateway else {
            return;
        };
        let known = self.member_roles.get(&guild_id);
        let mut wanted = Vec::new();
        let authors = self.messages.iter().flat_map(|message| {
            iter::once(message.author_id).chain(message.reply.as_ref().map(|reply| reply.author_id))
        });
        for user_id in authors {
            if known.is_some_and(|known| known.contains_key(&user_id)) {
                continue;
            }
            if self.requested_members.insert((guild_id, user_id)) {
                wanted.push(user_id);
            }
        }
        if !wanted.is_empty() {
            gateway.request_members(guild_id, &wanted);
        }
    }
}
