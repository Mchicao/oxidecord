# Guild voice presence on the server rail

**DRAFT — NOT TESTED YET in the running app.** Badge placement, tooltip layout,
and live interaction still require manual QA.

Discord's web client marks every guild icon that has people in voice with a
small speaker badge in its upper-right corner, and hovering it opens a tooltip
naming the guild above a row of overlapping avatars capped at five with a
"+N" bubble for the rest. This document covers how Oxidecord reproduces both
parts of that behaviour.

**Provenance.** This parity work is based on the user's screenshots of Discord
web only. Discord's remotely loaded frontend was not recovered, so no Discord
code, bundles, or extracted assets were consulted or copied — the layout,
sizes, and text below are judgement calls matched to the screenshots, not
ported constants.

## Data source

Everything shown comes from state the gateway already maintains:

- `HomeScreen::voice_states` — one `discord::VoiceUserState` per connected
  user, keyed by user id, carrying `guild_id`, `channel_id`, and whatever
  profile the dispatch included. It is filled from `READY`,
  `GUILD_CREATE`, and `GUILD_DELETE` snapshots and kept current by
  `VOICE_STATE_UPDATE`; `handle_voice_state` and `replace_voice_states`
  drop disconnected states (`channel_id: None`) and notify the screen, which
  is what makes an open tooltip re-render.
- No HTTP calls and no polling: joining, moving, and leaving update the rail
  and any open tooltip through the same notify the rest of the app uses.

DM calls (voice states with no `guild_id`) never reach the rail, and a guild
only counts while at least one of its voice states is connected.

## Rail badge

`render_server_rail` folds the voice states into a
`HashSet<Id<GuildMarker>>` in one pass, without cloning anything, because the
rail renders on every frame of a scroll glide. Each guild icon then adds
`voice_badge` — an 18px circle anchored to the icon frame's upper-right corner,
holding the existing `icons/volume-2.svg` speaker glyph. It uses the same
foreground/background theme pair, so it reads as chrome
over any icon in either theme.

Collapsed folders deliberately get no aggregated badge: folding several
servers' presence into one icon would read as the folder being in a call,
in this implementation. An expanded folder's guilds are
rendered through the same `render_rail_guild`, so they carry their own
badges, like their unread pills already do.

## Tooltip

A guild with the badge swaps its plain name tooltip for
`GuildVoiceTooltip`, built the same way as the reaction tooltip: a view
holding the screen entity with an `observe` subscription, so it re-renders
live while joins, moves, and leaves land under an open pointer instead of
going stale.

On each render it calls `guild_voice_presence(guild_id, 5)`:

1. `preview_guild_voice` filters the screen's voice states to connected
   states of that guild, orders them signed-in user first then by user id,
   and truncates to `max` references — a deterministic preview that holds
   still while the tooltip is open, with no per-element allocation.
2. Only the selected references are resolved into `VoiceParticipant`s, so
   profiles are read and strings cloned for at most the five drawn users.

The tooltip then draws the guild name, the speaker glyph, the overlapping
avatars (a 2px ring of the tooltip's own fill separates them), and a "+N"
bubble once the guild has more than five people in voice.

## Accessibility

The tooltip includes a visible text alternative — "3 in voice: Amy, Bea,
Cid" — naming the count and previewed members. This is a visible deviation
from the screenshot. Screen-reader behavior and keyboard access to this
hover preview have not been validated.

## Tests

`preview_guild_voice` is unit-tested for the behaviour the rail depends on:
other guilds and disconnected states are excluded, an empty guild previews
nothing, the signed-in user sorts first and the rest by id, and everything
past the cap folds into the overflow count. Rendering itself (badge
placement, tooltip layout) is not unit-tested, per the no-GPU-tests rule;
it needs a manual pass in the running app, which this branch has not had.
