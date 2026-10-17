# Guild voice hovercard parity

Status: **DRAFT — NOT TESTED YET in the running app.** This change combines the
guild hovercard correction and participant-avatar correction into one focused
feature, dependent on PR #13.

## Provenance and scope

The user-supplied Discord crop shows a compact dark card, bold guild name, speaker, overlapping
face avatars (including Discord defaults), and no count/name caption. The Oxi
crop instead shows a pale inverted tooltip, small initial placeholders, and a
long visible `5 in voice: ...` caption. These images establish the target, not
evidence that the changed app has been rendered. No redesign or global tooltip
theme change is included.

This is part of the feature-porting effort informed by the inspected/decompiled
Discord desktop app. This particular correction uses the user's visual reference
and official CDN documentation; it does not copy recovered frontend UI code or
claim that proprietary native implementations were recovered.

## Hovercard

- Use `theme.popover` and `theme.popover_foreground`, not foreground as the fill.
  Dark themes now retain a dark surface; light themes retain a light surface
  with their corresponding readable foreground.
- Keep existing paper-depth helpers, with `Level::Overlay`, `Finish::Matte`, and
  the existing 8 px item radius. The height estimate for lighting is 72 px.
- Use the existing font family, 14 px bold guild title, 12 px horizontal and
  8 px vertical padding, and an 8 px title/row gap.
- Use a 20 px speaker and 28 px avatar discs: 24 px image plus a 2 px surface-color
  ring on each edge. Each subsequent disc overlaps by 8 px. Five avatars and a
  speaker need about 160 px including padding, rather than stretching for names.
  Width is content-driven, with a 240 px ceiling for long guild names.
- Remove the visible count/name caption. GPUI 0.2.2 exposes `hoverable_tooltip`
  and per-element `tooltip`; use those to retain names on individual avatars.
  Pointer behavior and screen-reader access have not been tested.
- Preserve the upper-right circular guild voice badge, five-person cap, `+N`
  overflow, self-first/then-user-ID ordering, and screen-observation subscription.
  Join/move/leave/snapshot handlers and their notifications are unchanged.

## Avatar root cause and fallback

`RawMember::avatar_url` previously returned `None` for a known user whose avatar
hash was absent. The shared UI avatar then correctly rendered initials for that
`None`, but that was the wrong model result for an account with no custom avatar.
Resolve the actual Discord default-face CDN URL in the model instead.

Official source, retrieved 2026-10-17:
[Discord API reference, Image Formatting / CDN Endpoints](https://docs.discord.com/developers/reference#image-formatting-cdn-endpoints).
It specifies `https://cdn.discordapp.com/embed/avatars/{index}.png`, with:

- Migrated username (`discriminator == 0`): `(user_id >> 22) % 6`.
- Legacy username: `discriminator % 5`.
- Default-avatar endpoint sizes are fixed; `size` is ignored.

Preserve the discriminator string from raw member/profile payloads and the
numeric discriminator already available on Twilight users. A missing or
unparseable raw discriminator falls back to the migrated formula; the exact
legacy color cannot be inferred without a discriminator. Existing custom-avatar
URLs, sizes, and animated-profile selection are unchanged.

Callers traced:

- Raw members feed voice snapshots/live states and member-list conversion.
- The small-user resolver feeds message authors, quoted messages, DM recipients,
  and reactors; those surfaces now receive real default faces too.
- Current-user and both profile-conversion paths use the same default formula,
  so cached-profile/self fallbacks do not reintroduce initials.
- Voice participant resolution is shared by the rail, sidebar voice rows, and
  voice tiles. It now also borrows an already-synced member row from the same
  guild before current-user/profile fallback. It does not clone the member list
  or whole cached profiles. The rail still resolves at most five participants.
- The generic UI avatar helper is unchanged: guild initials and genuinely
  unknown users retain their existing placeholder behavior.

An absent member/user payload is **unknown**, not proof that an account has no
custom avatar. If state, synced member row, self data, and cached profile all
lack an image, no custom image is claimed recovered and no synthetic default
image is assigned merely from the voice user ID. No new profile/member requests
are introduced. The existing `request_author_members`/`requested_members` gateway
deduplication path was inspected and left unchanged; render and hover do not
request members, fetch profiles, poll, or add locks. This deliberately does not
promise to recover uncached custom avatars from member-less snapshots.

## Validation

- Formatting and diff checks passed.
- Forced primary-crate Clippy compilation passed with 12 warnings in untouched
  files. Shared-cache freshness was not accepted as compilation evidence.
- A freshly compiled test executable from this branch passed **68 tests** with
  **0 failures and 2 ignored**. Four new behavioral checks cover migrated and
  legacy default avatars, custom/missing user data, cached profiles, and gateway
  snapshot/live conversion.
- Test executable SHA-256:
  `31138CF19F1CDE932D31082F8A3377FCED98539F1421938E8A16D6F4B04A13ED`.
- The mechanical UI detector returned `[]`; Rust coverage is unconfirmed, so
  that result does not establish visual correctness.

Reproduce with the repository's existing Rust/MSVC toolchain:

```powershell
cargo fmt --check
cargo clippy --locked --tests
cargo test --locked
```

When sharing a Cargo target directory across checkouts, compile the primary
crate with a unique `--cfg` through `cargo rustc --locked --tests`, then copy and
run the executable identified in Cargo's JSON output. This avoids accidentally
running another checkout's cached primary artifact.

## Remaining manual validation

**NOT TESTED YET IN THE RUNNING APP:** dark/light geometry and contrast,
long-name truncation, nested avatar-name tooltips, CDN loading, overflow with
more than five people, and live join/move/leave/snapshot changes. Screen-reader
behavior has not been checked.

The existing QA application was inspected read-only and remains open with its
original binary. This patch has not been loaded into that process. No app,
account, microphone, call, or foreground interaction was used for validation.
