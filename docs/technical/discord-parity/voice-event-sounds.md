# Voice event sounds

**NOT TESTED YET in running app.** No app launch, live account, physical audio,
microphone, or foreground desktop interaction was used for this implementation.

## Scope and provenance

This implements the requested audible voice-membership behavior natively. It is
not a recovered port of Discord's frontend sound policy, original sound assets,
or proprietary DSP. Decompiled native wrappers alone do not establish the
frontend's event-to-sound decisions or original waveforms; those frontend/assets
were not recovered for this work. No extracted code, bundles, downloaded audio,
or new dependencies are included.

The existing gateway model and songbird connection remain in use. The gateway
[Connecting to Voice reference](https://docs.discord.com/developers/topics/voice-connections#connecting-to-voice)
was consulted: joining is a command answered by state/server updates, not proof
that the voice transport connected. CPAL 0.18.2's installed traits and timestamp
API were checked directly.

## Transition policy

- A join request and its gateway acknowledgement are silent. Only `Connected`
  for the current local attempt confirms a join. Duplicate confirmations and
  same-channel transport reconnects are silent.
- Local leave, remote disconnect, or a dropped confirmed call emits one leave
  cue. Local teardown consumes the confirmation before the gateway/engine
  acknowledgement can repeat it. Cancelling/failing an initial connection
  emits neither a successful join nor a leave.
- Explicit channel switches and moderator moves emit one destination join after
  confirmation, not a leave/join pair at request time. Peer cues are suspended
  while switching/reconnecting. If a switch fails, no destination join is
  emitted; subsequently ending the call can still announce leaving the last
  previously confirmed channel.
- Only live peer updates crossing the currently connected channel boundary
  produce cues: entry uses `PeerJoin`, departure uses `Leave`. Moves between
  unrelated channels, repeated same-channel updates, mute, deafen, streaming,
  and speaking updates do not produce membership cues.
- `READY`, `GUILD_CREATE`, and `GUILD_DELETE` participant replacements are silent.
  Snapshots seed the state map without a live cue policy. A following identical
  live state is therefore silent too.
- Engine connection, failure, and disconnect events carry a local attempt ID.
  A result from an older/cancelled attempt cannot confirm or end a newer call.
  This does not add request IDs to Discord's gateway protocol or redesign its
  existing server/state pairing and cross-session reconciliation. It also does
  not abort the engine's already-running serialized connection handshake.

## Playback and preferences

Settings → **Voice → Sounds → Voice event sounds** applies immediately and is
persisted as `voice_event_sounds_disabled` in `prefs.json` (missing means enabled).
Microphone mute does not silence cues. Self-deafen suppresses new cues and
invalidates queued/current ones; undeafening does not replay them. Leaving while
deafened retains that local flag instead of interpreting an empty-channel
acknowledgement as an instruction to undeafen. Channel requests/teardown also
invalidate older queued cues.

`voice::cues` owns one worker per home screen, a four-slot peer/wakeup queue, and
two atomic own-transition slots. Own join/leave take priority over queued peers,
including when that queue is full. Epoch invalidation means only the latest own
transition remains eligible. Enqueue uses nonblocking `try_send`; peer overload
drops new peer cues rather than growing threads or memory. Playback is best
effort: rapid superseding own transitions, peer overload, no device, unsupported
format, initialization failure, or driver errors can omit a cue. One cue is
rendered at a time, so cues do not sum or clip each other.

The worker sleeps on the receiver when idle, opens only an output stream, and
closes it after each cue drains. It survives call-engine teardown, allowing a
local leave cue independently of the voice stream. Call audio, jitter buffers,
and microphone capture are not modified. Dropping the home screen cancels cues
and closes the worker rather than keeping notification audio alive after logout.
No I/O, logging, allocation, or explicit
locks are added to the cue audio callback; foreground event handling only updates
policy/atomics and attempts bounded enqueue. Preference persistence uses the
existing discrete-settings write pattern.

Three original 180 ms two-note sine motifs distinguish self join, leave, and peer
entry; peer departure shares the leave motif. Each note has 12 ms attack/release ramps, peak amplitude 0.08, finite output,
and the existing sample sanitizer before device-format conversion. Rendering uses
the device rate and duplicates mono across its channels. Completion uses CPAL's
predicted playback timestamps; the worker parks until callback notification,
with a two-second playback watchdog for a stalled driver. Stream initialization
requests a one-second timeout, which CPAL notes not all backends honor.

### Output-device integration

Standalone, **all cues use the system default output**, including local leave.
PR #11 (`feat/native-voice-output-device`, `c411c91`) is deliberately not integrated:
this branch adds no `output_device` preference, chooser, or competing getter.
`CuePlayer::start` accepts an output-selector closure executed on its worker for
each cue. At integration, wire that closure to the same shared selection used by
PR #11, retaining it after engine teardown. Merely merging the branches does not
automatically route cues to PR #11's selected device. Device opening is separate
from the voice callback and does not change its buffer policy.

## Validation

Deterministic tests exercise the production transition policy, snapshot/live
state recorder, cancellation/failure/stale attempts, reconnects/moves, bounded
enqueue and own-cue priority under peer saturation, deafen/disable invalidation,
and renderer termination/layout at 8, 44.1,
48, 96, and 192 kHz with 1, 2, 6, and 8 channels. They never start the cue worker
or open an audio device. Existing hardware capture tests remain ignored.

Commands (MSVC environment and isolated Cargo target supplied by the parent):

```powershell
cargo fmt --all -- --check
cargo check --locked --tests
cargo test --locked
cargo clippy --locked --tests
```

Verified on this branch: all four commands passed; tests reported **69 passed,
0 failed, 2 ignored**. Check reports three existing atomic-deprecation warnings;
Clippy reports twelve warnings in unchanged code. The existing
`proc-macro-error2` future-incompatibility warning also remains.

Logs belong in `logs/tests/discord-parity/voice-event-sounds-*.log`, not the PR's
source payload. Runtime sound quality, device compatibility, output latency,
settings interaction, and real multi-user gateway timing still require manual
app testing. No measured voice-performance or perceptual-parity claim is made.
