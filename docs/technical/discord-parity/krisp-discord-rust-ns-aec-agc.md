# DRAFT — Krisp as Discord uses it, and Rust paths to NS/AEC/AGC in Oxidecord

> **RESEARCH ONLY — NOT IMPLEMENTED — NOT TESTED IN APP.** No dependency added, no code
> changed, no SDK downloaded, no vendor contacted. Base: `origin/main` `e580fb3`, branch
> `docs/krisp-rust-audio-processing`. Evidence retrieved 2026-10-17.

## Answer up front

1. **Discord's Krisp usage is knowable but not free to reproduce.** Discord embeds Krisp's
   native module plus `.kef` models for **noise cancellation (NC), background voice
   cancellation (BVC), and VAD** [L2][W1]. The reviewed public RTC SDK documentation
   describes these capabilities; it does not establish an AEC/AGC integration [K6].
   Reproducing the observed native integration requires a commercial SDK license [K1].
   AEC/AGC must be evaluated separately; absence from an index is not proof that the
   vendor has no such capability in any product or private agreement.
2. **Discord's AEC/AGC implementation remains unknown.** Discord exposes Echo Cancellation,
   Standard Noise Suppression, Krisp Noise Cancellation, and Automatic Gain Control as four
   separate settings [W1][D2]. Desktop voice processing is delegated to `discord_voice.node`
   (inventoried, not decompiled) [L0][L1]; separate settings alone do not identify algorithms
   or their vendor. Only the JS/settings call contract is recovered —
   do **not** claim WebRTC APM or any other engine inside Discord without evidence.
3. **No license for reusing Discord's bundled Krisp artifacts was established.**
   Public SDK integration requires a commercial license [K1]. This investigation neither
   executes nor redistributes Discord's native binaries or model files.
4. Realistic stack today: **pure-Rust NS first (`nnnoiseless` 0.5.2)**; for AEC/AGC later,
   **WebRTC APM via the maintained `webrtc-audio-processing` wrapper**, whose open gate is a
   Windows/MSVC build (CI covers Ubuntu/macOS only) [C3]. None of the evaluated open
   alternatives establishes equivalent BVC behavior; this is not an exhaustive absence claim.

## Terms, kept distinct

| Feature | Removes/does | Needs |
| --- | --- | --- |
| Noise suppression (NS/NC) | non-voice noise from the mic stream | capture stream only |
| Background voice cancellation (BVC) | *other people's voices*, mic side | capture + device allow-list rules [K3] |
| Acoustic echo cancellation (AEC) | loudspeaker audio re-entering the mic | synchronized far-end render reference + delay/timing |
| Automatic gain control (AGC) | levels voice into a usable range | capture stream; can amplify residual noise |
| Voice activity detection (VAD) | speech-present score | any stream; does not clean audio |

## What Discord actually ships (verified evidence)

Desktop 1.0.9242 extraction, read-only [L0]:

- `discord_voice-1/discord_voice/index.js:545-551` — `setupKrispPath()` resolves
  `nativeModules.getModulePath("discord_krisp")` → native `VoiceEngine.setKrispPath(path)`
  (`:552-556` mirrors it for `discord_ml`); `:2` loads `discord_voice.node`; `:164` declares
  feature `krisp_native_error`; `:240` forwards an opaque `setTransportOptions` — AEC/AGC
  wiring is native-side and not exposed in this wrapper.
- `discord_krisp-1/discord_krisp/index.js:2,15-31` — the whole JS surface of
  `discord_krisp.node` is `_initialize({logDirectory, logLevel})`, `_getNcModels`,
  `_getVadModels`, `_getNcModelFilename` (promise-wrapped).
- `discord_krisp-1/discord_krisp/manifest.json:3-8` — ships `discord_krisp.node` +
  `krisp-vad-o-v2.kef`, `krisp-nc-o-nb-v2.kef`, `krisp-nc-o-lite-v1.kef`,
  `krisp-nc-o-med-v7.kef`, `krisp-bvc-o-pro-v3.kef`. **No model named AEC/AGC appears
  in this manifest.** That does not identify algorithms compiled into native binaries.
  The names match Krisp's public outbound catalog (med-v7 32 k, lite-v1 16 k,
  nb-v2 8 k on-demand, bvc-o-pro-v3 32 k) [K3].

Current **web** bundle `https://discord.com/assets/web.4d4b2f75fdea5470.js`, SHA-256
`667a771c…4a12d084ae`, 12,410,287 bytes, fetched 2026-10-17 without auth; **current-web
build, may differ from desktop 1.0.9242** [W1]:

- Settings actions (char offsets ≈546677/547516/547696): `setEchoCancellation` →
  `AUDIO_SET_ECHO_CANCELLATION`; `setNoiseSuppression` → `AUDIO_SET_NOISE_SUPPRESSION`
  (Standard); `setNoiseCancellation` → `AUDIO_SET_NOISE_CANCELLATION` **plus inverted
  `AUDIO_SET_NOISE_SUPPRESSION`** (Krisp and Standard are mutually exclusive);
  `setAutomaticGainControl` → `AUDIO_SET_AUTOMATIC_GAIN_CONTROL`.
- Krisp knobs (≈550097-550383): `AUDIO_SET_KRISP_SUPPRESSION_LEVEL` (matches the SDK's
  0–100 % `noiseSuppressionLevel` [K5]), `AUDIO_SET_KRISP_MODEL_OVERRIDE` (restarts NC),
  `AUDIO_SET_NOISE_CANCELLATION_ENABLE_STATS`.
- Web NC model table (offset 9151793): `small_NC_8k`→`krisp-nc-o-nb-v2.kef` (≤8 kHz),
  `small_NC`→`krisp-nc-o-lite-v1.kef` (≤16 kHz), `full_NC`→`krisp-nc-o-med-v7-fp16.kef`
  (≤96 kHz): model auto-chosen by device rate; an `-fp16` web variant exists that the
  public catalog does not list.
- Module load (offset 9172554): `ensureModule("discord_krisp")` → `getSdkVersion()`,
  `getSuppressionLevel() ?? 100`, `getNcModels()`, then `ensureModule("discord_voice")` →
  `setupKrispPath()` — the frontend drives the desktop wrapper above.
- Telemetry/state: `noise_canceller_process_time_ms`, `noise_canceller_frame_count`,
  `noise_canceller_error` (≈2448953, 2487876); certified devices carry per-device
  `echo_cancellation`/`noise_suppression`/`automatic_gain_control`/`hardware_mute`
  capability flags (≈1949665); RPC voice settings expose all four toggles separately
  (≈1991208). **No `bvc` strings occur (0 matches)** — desktop bundles a BVC model [L2]
  but no user-facing BVC control is evidenced; whether BVC ships enabled anywhere is unknown.

Public docs corroborate scope: Krisp on Discord is on-device, outbound-mic-only, free
inside Discord, auto-disables under high CPU, and can hurt quality on clean mics [D1];
Echo Cancellation and Automatic Gain Control are Discord's own toggles [D2]. Native
internals stay proprietary: binaries hash-inventoried, not decompiled [L0].

## Krisp official SDK — verified facts and unknowns

- **Licensing:** commercial; license via the developer portal; consumer subscriptions are
  not SDK licenses [K1]. SDK + models are delivered through the licensed portal
  (programmatic downloads return short-lived links) [K8]. No price or OSS-client terms
  public — a business gate, not a technical one.
- **Platforms:** desktop Windows `x86_64`/`armv8a`, macOS, Linux; CPU-only C-library core,
  explicitly integrable into WebRTC/Electron stacks [K2] — a thin C-ABI shim for Rust is
  plausible **after** licensing; no official Rust binding was identified in the reviewed docs.
- **C++ API (Audio SDK v9 docs):** `globalInit`/`globalDestroy`; `Nc<FrameDataType>::
  create(NcSessionConfig{inRate, frameDurationMillis, outRate, modelInfo(.kef path),
  enableStats…})`; `process(in, numIn, out, numOut, noiseSuppressionLevel[0..100],
  frameStats*)`; frame size = `frameDuration × rate / 1000` (10 ms → 480 @ 48 kHz);
  per-stream sessions, preload via dummy session; errors are C++ exceptions a shim must
  catch [K4][K5]. `FrameDataType` is `int16` or `float`; **float amplitude scale and
  multichannel interleaving are undocumented publicly** — confirm in the licensed package.
- **Models:** outbound NC `pro-v1`(32 k)/`med-v7`(32 k)/`lite-v1`(16 k), 8 k `nb-v2`
  on-demand, BVC `pro-v3`(32 k)/`lite-v2`(16 k; BVC includes NC — never stack them;
  headset/allow-list required), plus inbound speaker-side `nc-i-*` [K3]. Discord bundles
  the outbound set + VAD [L2][W1].
- **AEC/AGC scope unconfirmed:** the reviewed public index documents NC/BVC/VAD,
  accent conversion and voice translation [K6]. No documented AEC/AGC API was located;
  this cannot establish the contents of private SDK agreements or every vendor product.
- **Quality evidence:** vendor-commissioned 3QUEST testing (ETSI TS 103 281,
  SigmaConnectivity, Q4-2022) claiming Krisp ahead of Zoom/Teams/WebEx/Meet on
  N-MOS/S-MOS; near-field (<50 cm) design scope [K7]. Vendor-published, not independently
  reproduced; no comparison vs RNNoise/DeepFilterNet/WebRTC APM.

## Rust/open alternatives — actual coverage

| Option | NS | BVC | AEC | AGC | VAD | License | Windows-Rust status | Quality evidence | Limits |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Krisp SDK | ✔ | ✔ | unconfirmed | unconfirmed | ✔ | commercial [K1] | Win x64 C++ API [K2]; C-ABI shim plausible, unverified | vendor 3QUEST [K7] | license gate; float scale/channels unpublished |
| `nnnoiseless` 0.5.2 | ✔ | ✖ | ✖ | ✖ | ✔(prob) | BSD-3-Clause [C1] | pure Rust; our own Windows compile/runtime check remains | RNNoise paper (original C impl) [P1] | old RNNoise arch; 48 kHz/480-frame; s16-magnitude floats; port itself unbenchmarked |
| `webrtc-audio-processing` 2.1.0 wrapper | ✔ | ✖ | ✔(AEC3) | ✔(1/2) | ✔ | BSD-3-style + patent grant [C3] | **CI Ubuntu/macOS only**; bundled build needs clang/meson/ninja → Windows unverified [C3] | ships in Chromium/Meet at scale; no controlled public benchmark | AEC needs render reference + delays; `~2.x` tilde-versioning, not semver |
| DeepFilterNet (`deep_filter` 0.2.5) | ✔ | ✖ | ✖ | ✖ | not evaluated | MIT OR Apache-2.0 (code) [C4] | older published crate; build/integration not checked here | Interspeech papers w/ metrics [P2][P3] | model provenance separate; no Windows/quality/CPU result here |
| SpeexDSP C engine / Rust bindings | ✔ | not established | ✔ | ✔ | ✔ | audit selected engine + bindings [C5] | `speexdsp` 0.1.2 package is old; API coverage/build not checked | upstream API documents features [S1] | older DSP approach; binding age alone does not prove abandonment or absent functionality |

Freshness vs prior research (re-verified 2026-10-17): nnnoiseless still 0.5.2 (2025-12-18),
repo pushed 2026-09-28 [C1][C2]; wrapper 2.1.0 (2026-05-13), repo pushed 2026-07-16, CI
still lacks Windows [C3]; DeepFilterNet repo untouched ~2 years **and** its crates.io
release is 4 years old [C4]; RNNoise upstream last commit observed 2025-02-22 [C6].
These are retrieval-time metadata snapshots, not proof of maintenance status,
algorithm quality, or what an unpublished/newer branch supports.

## Quality evidence discipline

Published/reproducible-ish: RNNoise (MMSP 2017) [P1] and DeepFilterNet 1/2 [P2][P3] report
objective metrics **for the original implementations**, not for nnnoiseless-on-Windows or
our pipeline. Vendor claims: Krisp 3QUEST [K7]. Deployment evidence: WebRTC APM ships at
Chromium-scale — robustness, not superiority. **Not yet benchmarked: everything on
Windows-Rust in Oxidecord**; no performance or Krisp-parity claim is allowed before the
offline experiment below runs. No public Krisp-vs-RNNoise benchmark was found.

## Fit with Oxidecord's audio graph (verified in source)

Baseline `e580fb3` (QA worktree adds PR11 output-device selection, read-only):

- Capture runs at the **device's native rate/format**; `push_capture` normalizes to f32 and
  clamps to ≤2 channels inside the CPAL callback; `RawAdapter` declares the native rate and
  songbird resamples (`devices.rs:105-144`, `audio/mod.rs:130-186`).
- `SampleRing` is a `Mutex<VecDeque>` (locks both sides), 400 ms cap, drop-oldest, **30 ms
  capture prime**, 3 ms gap ramps (`ring.rs:10-96`, `audio/mod.rs:62`); per-sender rings
  prime 60 ms with Opus FEC/concealment at 48 kHz stereo (`incoming.rs:21-103`).
- Playback: `Mixer::fill` sums senders + tanh limiter; `Playback::render` carries resampler
  phase across callbacks (`playback.rs:43-191`). **Both callbacks discard cpal timestamps**
  (`devices.rs:118,175`) — capture and render clocks are independent; a future AEC must add
  its own clocking and a reference tap at the 48 kHz mix feeding `render` (post-mix,
  pre-device-resample), following the **selected** output (PR11).
- Lifecycle: base `AudioIo::start(input)`; QA adds `start(input, output)`,
  `Command::OutputDevice`, `swap_devices`, `chosen_output` (QA `voice/mod.rs:90,286,345`,
  `devices.rs:59`). Mute = `driver.mute`; deafen drops and clears rings (`voice/mod.rs:197-209`,
  `audio/mod.rs:227-246`). DSP state must reset on swap/mute/deafen epochs and never
  outlive the device pair.

## Long-term minimal architecture (direction, not implemented)

One DSP worker owned by `AudioIo`; CPAL callbacks only convert+copy into bounded SPSC
queues (never lock, allocate, or wait on DSP); the worker owns resampling to 48 kHz,
480-sample block framing, per-channel NS state, and later APM with the far-end reference
enqueued from the output path. Mute/deafen/swap become epoch boundaries that flush and
reset state. No plugin framework, no per-effect threads, no new blocking in callbacks —
a design direction, not a change to the current graph.

## Recommendation: one first prototype

**Opt-in `nnnoiseless` 0.5.2 NS on the capture worker.** Pure Rust (no Windows FFI gate),
BSD-3, small API, per-channel state, free VAD probability, measurable quickly. Material
tradeoffs: older RNNoise architecture; no AEC/AGC/BVC; s16-magnitude scaling
(`clamp(x*32768, -32768, 32767)` in, `sanitize(out/32768)` out); Windows quality unproven
until acceptance runs. Krisp-first deferred (license gate and AEC/AGC scope unconfirmed);
APM-first deferred *for now* (Windows build unverified; AEC reference plumbing is a
separate, larger front). When AEC/AGC become requirements: a **Windows build-only spike**
of `webrtc-audio-processing ~2.1` (`bundled`) in a throwaway harness precedes any
architecture commitment.

## Offline acceptance experiment (defined; NOT run now)

Fixed offline vectors (licensed speech + noise mixes at SNR 0/5/15 dB; synthetic echo case
mixing far-end speech back with delay; clean-speech control). Arms: bypass vs nnnoiseless
(vs upstream RNNoise C as reference if available). Metrics: intrusive quality (PESQ-WB /
POLQA / DNSMOS as tooling permits) + SI-SDRi for NS; ERLE + double-talk speech damage for
any future AEC; real-time factor (processing time per 10 ms block, well under 1× on target
hardware); added algorithmic latency (block + lookahead, measured); artifact checks
(finite output, channel alignment, startup fade-in, no muted-audio replay). Pass thresholds
must be agreed before comparing results, using target hardware budgets and speech
preservation requirements. Nothing ships default-on before acceptance.

## Gates

| Gate | Blocks | Owner |
| --- | --- | --- |
| Krisp commercial license + delivered SDK/models | observed Krisp NS/BVC/VAD integration | vendor + budget |
| Windows/MSVC build of the APM C++ stack | WebRTC APM (AEC/AGC/NS/VAD) path | us (build spike) |
| Offline acceptance numbers | any quality/CPU/latency claim; default-on | us |
| Model/weights provenance audit (nnnoiseless weights, any DFN models) | shipping binaries | us |
| Real-device validation (later, authorized) | live capture rollout | us + user |

## Conclusion — what can match Discord and what is unknown

- **Matchable now, pending our own validation:** the Discord-parity settings surface
  (NS on/off + level) and RNNoise-class suppression in pure Rust. A combined open
  NS+AEC+AGC+VAD stack is documented in WebRTC APM FFI — credible, pending a Windows
  build spike, and its AEC needs reference/timestamp work in our graph.
- **Not matchable without the vendor:** Krisp-grade NC/BVC quality claims (unverifiable
  independently) and Krisp model redistribution.
- **Unknown:** Discord's internal AEC/AGC implementation and its quality; BVC exposure in
  shipping clients; relative quality of every candidate on Windows-Rust until the offline
  experiment runs.

## Sources (accessed 2026-10-17)

Local evidence is not redistributed in this research PR:

- [L0] `output/discord-desktop-1.0.9242/`: extracted desktop package and hash manifest;
  29 native binaries inventoried, not decompiled or executed.
- [L1] `readable/modules/discord_voice-1/discord_voice/index.js`, lines 2, 164,
  240, 545–556. [L2] `readable/modules/discord_krisp-1/discord_krisp/`:
  `index.js` lines 2, 15–31 and `manifest.json` lines 3–8.
- [S*] Oxidecord base `e580fb3`, `src/voice/audio/*`, `src/voice/mod.rs`,
  `src/platform/audio.rs`; separate QA source inspected read-only for PR #11.
- Public frontend bundle [W1], SHA-256
  `667a771c61ff709e2f0400d395a324f0a5b1b5aa89e11e263289144a12d084ae`;
  offsets above refer to the cached UTF-8 text. JavaScript was not executed.

[W1]: https://discord.com/assets/web.4d4b2f75fdea5470.js
[K1]: https://sdk-docs.krisp.ai/docs/licensing-information
[K2]: https://sdk-docs.krisp.ai/docs/supported-platforms
[K3]: https://sdk-docs.krisp.ai/docs/rtc-model-guide-bvc-nc
[K4]: https://sdk-docs.krisp.ai/docs/desktop-sdk-bvc
[K5]: https://cdn.krisp.ai/sdk/doc_v9/class_krisp_1_1_audio_sdk_1_1_nc.html
[K6]: https://sdk-docs.krisp.ai/
[K7]: https://sdk-docs.krisp.ai/docs/krisp-audio-quality-testings
[K8]: https://sdk-docs.krisp.ai/docs/programmatic-sdk-model-downloads-api
[D1]: https://support.discord.com/hc/en-us/articles/360040843952
[D2]: https://support.discord.com/hc/en-us/articles/360045138471
[C1]: https://crates.io/api/v1/crates/nnnoiseless
[C2]: https://api.github.com/repos/jneem/nnnoiseless
[C3]: https://github.com/tonarino/webrtc-audio-processing
[C4]: https://api.github.com/repos/Rikorose/DeepFilterNet
[C5]: https://crates.io/api/v1/crates/speexdsp
[C6]: https://gitlab.xiph.org/api/v4/projects/xiph%2Frnnoise/repository/commits?per_page=1
[P1]: https://arxiv.org/abs/1709.08243
[P2]: https://arxiv.org/abs/2110.05588
[P3]: https://arxiv.org/abs/2205.05474
[S1]: https://www.speex.org/docs/manual/speex-manual/node7.html

Package and build details: [nnnoiseless 0.5.2 README](https://github.com/jneem/nnnoiseless/blob/v0.5.2/README.md),
[APM crate versions](https://crates.io/api/v1/crates/webrtc-audio-processing),
[APM CI matrix](https://github.com/tonarino/webrtc-audio-processing/blob/main/.github/workflows/rust.yml),
and [DeepFilterNet crate versions](https://crates.io/api/v1/crates/deep_filter).
Moving repository links describe the retrieval-time snapshot, not a future guarantee.
