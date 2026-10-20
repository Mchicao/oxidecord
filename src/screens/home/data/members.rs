//! Guild members' roles and names: the signed-in user's roles, which decide
//! what pings them and which emoji they can use, and everyone else's, which
//! colour their names, along with the names the guild knows them by.

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

    /// The name a user goes by in the open guild — their nickname there, where
    /// they have one — once the gateway has named them. `None` in a DM.
    ///
    /// Messages are drawn by this rather than their own `author_name`, which
    /// is only the nickname for those that arrived live: fetched history
    /// carries no members, so the same author would otherwise flip between
    /// names as cached and fetched messages are merged.
    pub(in crate::screens::home) fn member_name(&self, user_id: Id<UserMarker>) -> Option<&str> {
        let guild_id = self.selected_guild.filter(|_| self.view == View::Guild)?;
        let member = self.guild_members.get(&guild_id)?.get(&user_id)?;
        Some(&member.name)
    }

    /// What to call a message's author: by [`Self::member_name`] where it's
    /// known, else the name the message came with.
    pub(in crate::screens::home) fn author_name<'a>(
        &'a self,
        user_id: Id<UserMarker>,
        fallback: &'a str,
    ) -> &'a str {
        self.member_name(user_id).unwrap_or(fallback)
    }

    /// Asks the gateway for the members behind the open conversation's
    /// authors, and the authors its replies quote, who haven't been named
    /// yet. Fetched history carries no members, and a user session is only
    /// sent a few of a guild's up front, so without this most names would go
    /// uncoloured, and show global names in place of nicknames. Called as
    /// each page of history lands.
    pub(in crate::screens::home) fn request_author_members(&mut self) {
        let Some(guild_id) = self.selected_guild.filter(|_| self.view == View::Guild) else {
            return;
        };
        let Some(gateway) = &self.gateway else {
            return;
        };
        // Roles alone come from places that don't name the member, like the
        // member list, so it's the name that says whether to ask.
        let known = self.guild_members.get(&guild_id);
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
