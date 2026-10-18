//! Every sender summed and laid out for the speakers.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::{FromSample, SizedSample};
use songbird::constants::SAMPLE_RATE;

use super::CHANNELS;
use super::ring::SampleRing;
use super::settings::{AudioControl, GainRamp};
use crate::platform::audio::sanitize;

/// Eases a mix that has run past full scale back inside it.
///
/// Several people talking at once sum past the ceiling, and clipping there
/// turns a loud moment into a burst of harmonics — which is the artefact that
/// actually gets noticed. A knee above `THRESHOLD` bends the peaks instead;
/// `tanh` is chosen for meeting the untouched signal at the same slope, so
/// ordinary speech passes through unaltered and nothing steps as it crosses
/// over.
fn limit(sample: f32) -> f32 {
    // High enough that one person talking, however loudly, is never touched:
    // the knee is there for overlapping speakers, and a waveshaper that
    // reaches into ordinary speech colours it for no gain.
    const THRESHOLD: f32 = 0.85;
    const HEADROOM: f32 = 1. - THRESHOLD;

    if !sample.is_finite() {
        return 0.;
    }
    if sample.abs() <= THRESHOLD {
        return sample;
    }
    let over = (sample.abs() - THRESHOLD) / HEADROOM;
    (THRESHOLD + HEADROOM * over.tanh()).copysign(sample)
}

/// Duration of the gain ramp applied to each peer when their volume or mute state changes.
pub(super) const PEER_GAIN_RAMP_MS: f32 = 0.005;
/// Ramp step per sample at 48kHz stereo (~5ms duration).
pub(super) const PEER_GAIN_STEP: f32 =
    1. / (SAMPLE_RATE as f32 * CHANNELS as f32 * PEER_GAIN_RAMP_MS);

pub(super) struct StreamEntry {
    pub(super) ssrc: u32,
    pub(super) ring: Arc<SampleRing>,
    pub(super) target_gain: Arc<AtomicU32>,
    pub(super) muted: Arc<AtomicBool>,
    pub(super) ramp: GainRamp,
    pub(super) was_muted: bool,
}

/// Everyone audible in the call, one ring per sender, summed on the way out.
///
/// Each sender's packets keep their own clock and their own jitter, so each
/// gets a ring to prime, starve and ramp on its own: one person's connection
/// stalling doesn't cut out everyone else.
pub(super) struct Mixer {
    /// Keyed by SSRC. A list rather than a map because the audio callback
    /// walks all of it every time and looks nothing up.
    streams: Mutex<Vec<StreamEntry>>,
    control: Arc<AudioControl>,
}

impl Mixer {
    pub(super) fn new(control: Arc<AudioControl>) -> Self {
        Self {
            streams: Mutex::default(),
            control,
        }
    }

    /// Fills `out` with every sender summed. `stream` is scratch space for
    /// one sender's share, held by the caller so the audio thread doesn't
    /// allocate.
    fn fill(&self, out: &mut [f32], stream: &mut Vec<f32>) {
        out.fill(0.);
        // Resized to length only: [`SampleRing::fill`] rewrites every slot of
        // `stream` before the sum reads it, so zero-filling the buffer on
        // each callback was traffic the first write overwrote anyway. Slots
        // left past the length by a smaller callback stay unread — the length
        // is the exact window the sum walks, so no old audio can leak back.
        stream.resize(out.len(), 0.);

        if let Ok(mut streams) = self.streams.lock() {
            for entry in streams.iter_mut() {
                let is_muted = entry.muted.load(Ordering::Relaxed);
                let target_gain = if is_muted {
                    0.0
                } else {
                    f32::from_bits(entry.target_gain.load(Ordering::Relaxed))
                };

                // While locally muted, discard incoming audio.
                // When transitioning from muted to unmuted, also clear the ring
                // so old stale PCM queued while muted does not resurface.
                if is_muted || entry.was_muted {
                    entry.ring.clear();
                }
                entry.was_muted = is_muted;

                entry.ring.fill(stream);
                for (slot, sample) in out.iter_mut().zip(stream.iter()) {
                    let scaled = entry.ramp.apply(*sample, target_gain, PEER_GAIN_STEP);
                    *slot += scaled;
                }
            }
        }

        for slot in out.iter_mut() {
            *slot = limit(*slot);
        }
    }

    pub(super) fn add_with_user(&self, ssrc: u32, user_id: Option<u64>, ring: Arc<SampleRing>) {
        let (target_gain, muted) = if let Some(uid) = user_id {
            let ctrl = self.control.peer_settings.get_or_create(uid);
            (ctrl.target_gain, ctrl.muted)
        } else {
            (
                Arc::new(AtomicU32::new(1.0f32.to_bits())),
                Arc::new(AtomicBool::new(false)),
            )
        };

        let initial_gain = if muted.load(Ordering::Relaxed) {
            0.0
        } else {
            f32::from_bits(target_gain.load(Ordering::Relaxed))
        };

        if let Ok(mut streams) = self.streams.lock() {
            streams.retain(|e| e.ssrc != ssrc);
            streams.push(StreamEntry {
                ssrc,
                ring,
                target_gain,
                muted,
                ramp: GainRamp::new(initial_gain),
                was_muted: initial_gain == 0.0,
            });
        }
    }

    pub(super) fn remove(&self, ssrc: u32) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.retain(|e| e.ssrc != ssrc);
        }
    }

    pub(super) fn clear(&self) {
        if let Ok(mut streams) = self.streams.lock() {
            streams.clear();
        }
    }
}

/// Lays the call's 48kHz stereo out in the output device's rate and channel
/// count.
///
/// The position between source frames is carried across callbacks, and so are
/// the two frames it sits between. Resampling each callback from scratch put a
/// step in the waveform at every buffer boundary, and a step that repeats at
/// the callback rate is heard as a tone over the speech rather than as the
/// glitches it is.
pub(super) struct Playback {
    gain: GainRamp,
    gain_step: f32,
    mixer: Arc<Mixer>,
    /// The source frames the next output frame falls between.
    previous: [f32; CHANNELS],
    next: [f32; CHANNELS],
    /// Where between them it falls, in `[0, 1)`.
    phase: f64,
    /// Source frames advanced per output frame.
    step: f64,
    /// The source frames one callback needs. Held across callbacks so the
    /// audio thread stops allocating once the buffer size has settled.
    scratch: Vec<f32>,
    /// One sender's share of `scratch`, for the mixer to sum from.
    stream: Vec<f32>,
    channels: usize,
}

impl Playback {
    pub(super) fn new(mixer: Arc<Mixer>, sample_rate: u32, channels: usize) -> Self {
        Self {
            gain: GainRamp::new(f32::from_bits(
                mixer
                    .control
                    .output_gain
                    .load(std::sync::atomic::Ordering::Relaxed),
            )),
            gain_step: 1. / (sample_rate.max(1) as f32 * channels.max(1) as f32 * 0.005),
            mixer,
            previous: [0.; CHANNELS],
            next: [0.; CHANNELS],
            phase: 0.,
            step: f64::from(SAMPLE_RATE) / f64::from(sample_rate.max(1)),
            scratch: Vec::new(),
            stream: Vec::new(),
            channels: channels.max(1),
        }
    }

    pub(super) fn render<T>(&mut self, out: &mut [T])
    where
        T: SizedSample + FromSample<f32>,
    {
        let frames = out.len() / self.channels;
        if frames == 0 {
            out.fill(T::EQUILIBRIUM);
            return;
        }

        let mut phase = self.phase;
        let mut pulls = 0;
        for _ in 0..frames {
            phase += self.step;
            while phase >= 1. {
                pulls += 1;
                phase -= 1.;
            }
        }

        self.scratch.resize(pulls * CHANNELS, 0.);
        self.mixer.fill(&mut self.scratch, &mut self.stream);

        let mut pulled = 0;
        let gain = f32::from_bits(
            self.mixer
                .control
                .output_gain
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        for frame in 0..frames {
            let blend = self.phase as f32;
            for (channel, slot) in out[frame * self.channels..][..self.channels]
                .iter_mut()
                .enumerate()
            {
                let (previous, next) = match channel {
                    0 | 1 => (self.previous[channel], self.next[channel]),
                    _ => (
                        (self.previous[0] + self.previous[1]) / 2.,
                        (self.next[0] + self.next[1]) / 2.,
                    ),
                };
                *slot = T::from_sample(self.gain.apply(
                    sanitize(previous + (next - previous) * blend),
                    gain,
                    self.gain_step,
                ));
            }

            self.phase += self.step;
            while self.phase >= 1. {
                self.previous = self.next;
                self.next = self
                    .scratch
                    .get(pulled * CHANNELS..(pulled + 1) * CHANNELS)
                    .map_or([0.; CHANNELS], |frame| [frame[0], frame[1]]);
                pulled += 1;
                self.phase -= 1.;
            }
        }

        out[frames * self.channels..].fill(T::EQUILIBRIUM);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
    use std::time::Instant;

    use super::super::settings::AudioPreferences;
    use super::*;

    #[test]
    fn global_output_gain_changes_rendered_pcm_live_without_changing_senders() {
        let control = Arc::new(AudioControl::new(&AudioPreferences::default()));
        let mixer = Arc::new(Mixer::new(control.clone()));
        let ring = Arc::new(SampleRing::new(48_000, 2, 0));
        mixer.add_with_user(1, None, ring.clone());
        let mut playback = Playback::new(mixer, 48_000, 2);
        for gain in [1., 0., 0.5, 2.] {
            control.configure(
                &AudioPreferences {
                    output_gain: gain,
                    ..AudioPreferences::default()
                },
                false,
            );
            ring.push(std::iter::repeat_n(0.25, 4096));
            let mut out = [0f32; 2048];
            playback.render(&mut out);
            assert!(
                (out[2047] - 0.25 * gain).abs() < 0.00001,
                "gain {gain}: got {}",
                out[2047]
            );
            assert!(
                out.iter()
                    .all(|sample| sample.is_finite() && sample.abs() <= 1.)
            );
        }
        control.configure(
            &AudioPreferences {
                output_gain: 2.,
                ..AudioPreferences::default()
            },
            false,
        );
        ring.clear();
        ring.push(std::iter::repeat_n(0.8, 4096));
        let mut out = [0f32; 2048];
        playback.render(&mut out);
        assert_eq!(out[2047], 1.);
        ring.clear();
        ring.push([f32::NAN, f32::INFINITY].into_iter().cycle().take(4096));
        playback.render(&mut out);
        assert!(
            out.iter()
                .all(|sample| sample.is_finite() && sample.abs() <= 1.)
        );
        assert_eq!(out[2047], 0.);
    }

    #[test]
    fn peer_gain_and_local_mute_per_user_isolation_and_reconnect() {
        let control = Arc::new(AudioControl::new(&AudioPreferences::default()));
        let mixer = Arc::new(Mixer::new(control.clone()));
        let ring_a = Arc::new(SampleRing::new(48_000, 2, 0));
        let ring_b = Arc::new(SampleRing::new(48_000, 2, 0));

        let user_a = 111111u64;
        let user_b = 222222u64;

        // Configure user A to 150% volume, user B to 50% volume and locally muted
        control.peer_settings.set_volume(user_a, 1.5).unwrap();
        control.peer_settings.set_volume(user_b, 0.5).unwrap();
        control.peer_settings.set_muted(user_b, true);

        mixer.add_with_user(10, Some(user_a), ring_a.clone());
        mixer.add_with_user(20, Some(user_b), ring_b.clone());

        // Check mixer fill
        let mut out = [0f32; 2048];
        let mut stream = Vec::new();
        for _ in 0..8 {
            ring_a.push(std::iter::repeat_n(0.2, out.len()));
            ring_b.push(std::iter::repeat_n(0.4, out.len()));
            mixer.fill(&mut out, &mut stream);
        }

        // User B is muted, so its samples contribute 0. User A is 1.5x, so 0.2 * 1.5 = 0.3
        assert!((out[2047] - 0.3).abs() < 0.001);

        // Now unmute User B
        control.peer_settings.set_muted(user_b, false);
        for _ in 0..8 {
            ring_a.push(std::iter::repeat_n(0.2, out.len()));
            ring_b.push(std::iter::repeat_n(0.4, out.len()));
            mixer.fill(&mut out, &mut stream);
        }
        // User A (0.3) + User B (0.4 * 0.5 = 0.2) = 0.5
        assert!((out[2047] - 0.5).abs() < 0.001);

        // Reconnect user A with new SSRC 30
        mixer.remove(10);
        let ring_a_new = Arc::new(SampleRing::new(48_000, 2, 0));
        mixer.add_with_user(30, Some(user_a), ring_a_new.clone());
        for _ in 0..8 {
            ring_a_new.push(std::iter::repeat_n(0.2, out.len()));
            ring_b.push(std::iter::repeat_n(0.4, out.len()));
            mixer.fill(&mut out, &mut stream);
        }
        // User A's 1.5x setting was preserved without leak
        assert!((out[2047] - 0.5).abs() < 0.001);
    }

    struct CountingAlloc;

    thread_local! {
        static CALLBACK_ALLOCS: Cell<u64> = const { Cell::new(0) };
    }

    static TOTAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
    static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);
    static PEAK_BYTES: AtomicI64 = AtomicI64::new(0);

    fn account(delta: i64) {
        let live = LIVE_BYTES.fetch_add(delta, Ordering::Relaxed) + delta;
        loop {
            let peak = PEAK_BYTES.load(Ordering::Relaxed);
            if live <= peak
                || PEAK_BYTES
                    .compare_exchange(peak, live, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                break;
            }
        }
    }

    unsafe impl GlobalAlloc for CountingAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let _ = CALLBACK_ALLOCS.try_with(|count| count.set(count.get() + 1));
            TOTAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                account(layout.size() as i64);
            }
            ptr
        }
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            LIVE_BYTES.fetch_sub(layout.size() as i64, Ordering::Relaxed);
            unsafe { System.dealloc(ptr, layout) }
        }
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let _ = CALLBACK_ALLOCS.try_with(|count| count.set(count.get() + 1));
            TOTAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
            let ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !ptr.is_null() {
                account(new_size as i64 - layout.size() as i64);
            }
            ptr
        }
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let _ = CALLBACK_ALLOCS.try_with(|count| count.set(count.get() + 1));
            TOTAL_ALLOCS.fetch_add(1, Ordering::Relaxed);
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                account(layout.size() as i64);
            }
            ptr
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAlloc = CountingAlloc;

    pub(in crate::voice::audio) fn allocations() -> u64 {
        TOTAL_ALLOCS.load(Ordering::Relaxed)
    }

    pub(in crate::voice::audio) fn allocations_here() -> u64 {
        CALLBACK_ALLOCS.with(Cell::get)
    }

    pub(in crate::voice::audio) fn live_bytes() -> u64 {
        LIVE_BYTES.load(Ordering::Relaxed).max(0) as u64
    }

    pub(in crate::voice::audio) fn peak_bytes() -> u64 {
        PEAK_BYTES.load(Ordering::Relaxed).max(0) as u64
    }

    struct Rig {
        mixer: Arc<Mixer>,
        ring: Arc<SampleRing>,
        playback: Playback,
    }

    fn rig(sample_rate: u32, channels: usize) -> Rig {
        let control = Arc::new(AudioControl::new(&AudioPreferences::default()));
        let mixer = Arc::new(Mixer::new(control));
        let ring = Arc::new(SampleRing::new(48_000, 2, 0));
        mixer.add_with_user(1, None, ring.clone());
        Rig {
            mixer: mixer.clone(),
            ring,
            playback: Playback::new(mixer, sample_rate, channels),
        }
    }

    struct Noise(u64);

    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 32) as u32 as f32 / u32::MAX as f32 * 2. - 1.
        }
    }

    #[test]
    fn rendered_pcm_is_unchanged_by_the_reused_scratch() {
        for (rate, channels) in [
            (48_000u32, 2usize),
            (44_100, 2),
            (96_000, 2),
            (8_000, 2),
            (48_000, 6),
        ] {
            let mut live = rig(rate, channels);
            let mut zeroed = rig(rate, channels);
            let mut noise = Noise(0x5EED);
            let sizes = [
                1024 * channels,
                2048 * channels,
                1023 * channels + 1,
                256 * channels,
                3072 * channels,
                3 * channels,
                1536 * channels,
                2047 * channels + 1,
            ];
            for (round, size) in sizes.into_iter().enumerate() {
                match round {
                    0 | 1 => {
                        let chunk: Vec<f32> = (0..8192).map(|_| noise.next() * 0.4).collect();
                        live.ring.push(chunk.iter().copied());
                        zeroed.ring.push(chunk.iter().copied());
                    }
                    2 => {}
                    3 => {
                        let junk = [f32::NAN, f32::INFINITY, -1.5, 0.75];
                        let chunk: Vec<f32> = junk.iter().cycle().take(4096).copied().collect();
                        live.ring.push(chunk.iter().copied());
                        zeroed.ring.push(chunk.iter().copied());
                    }
                    4 => {
                        let live_ring = Arc::new(SampleRing::new(48_000, 2, 0));
                        let zeroed_ring = Arc::new(SampleRing::new(48_000, 2, 0));
                        live.mixer.add_with_user(2, None, live_ring.clone());
                        zeroed.mixer.add_with_user(2, None, zeroed_ring.clone());
                        let chunk: Vec<f32> = (0..8192).map(|_| noise.next() * 0.4).collect();
                        live.ring.push(chunk.iter().copied());
                        zeroed.ring.push(chunk.iter().copied());
                        live_ring.push(chunk.iter().rev().copied());
                        zeroed_ring.push(chunk.iter().rev().copied());
                    }
                    5 => {
                        live.ring.clear();
                        zeroed.ring.clear();
                        live.mixer.remove(2);
                        zeroed.mixer.remove(2);
                    }
                    _ => {}
                }
                let mut fresh = vec![0f32; size];
                let mut reused = vec![0f32; size];
                live.playback.render(&mut reused);
                zeroed.playback.render(&mut fresh);
                zeroed.playback.scratch.fill(0.);
                zeroed.playback.stream.fill(0.);
                let difference = fresh
                    .iter()
                    .zip(&reused)
                    .position(|(fresh, reused)| fresh.to_bits() != reused.to_bits());
                assert!(
                    difference.is_none(),
                    "rate {rate}, channels {channels}, round {round}, first difference: {:?}",
                    difference.map(|index| (index, fresh[index], reused[index]))
                );
                assert!(reused.iter().all(|sample| sample.is_finite()));
            }
        }
    }

    #[test]
    fn shrunken_callbacks_after_loud_audio_never_replay_the_scratch_tail() {
        let mut rig = rig(48_000, 2);
        rig.ring.push(std::iter::repeat_n(0.8, 48_000));
        let mut loud = [0f32; 8192];
        rig.playback.render(&mut loud);
        assert!(loud.iter().any(|sample| *sample > 0.5));

        rig.ring.clear();
        let mut quiet = [0f32; 1024];
        rig.playback.render(&mut quiet);
        assert!((quiet[0] - 0.8).abs() < 0.00001);
        assert!(quiet.iter().take(4).all(|sample| *sample <= 0.8));
        assert!(
            quiet[4..].iter().all(|sample| *sample == 0.),
            "silence must be exact once the carry-over is spent, got {:?}",
            &quiet[4..20]
        );

        let mut grown = [0f32; 8192];
        rig.playback.render(&mut grown);
        assert!(grown.iter().all(|sample| *sample == 0.));

        rig.ring.push(std::iter::repeat_n(0.05, 48_000));
        let mut resumed = [0f32; 2048];
        rig.playback.render(&mut resumed);
        assert!(
            resumed.iter().all(|sample| *sample <= 0.05 + 0.00001),
            "old loud samples must not bleed into fresh audio"
        );
        assert!((resumed[2047] - 0.05).abs() < 0.0001);
    }

    #[test]
    fn callbacks_stop_allocating_once_buffer_sizes_settle() {
        let mut rig = rig(8_000, 2);
        let second = Arc::new(SampleRing::new(48_000, 2, 0));
        rig.mixer.add_with_user(2, None, second.clone());
        let mut noise = Noise(99);
        let chunk: Vec<f32> = (0..8192).map(|_| noise.next() * 0.3).collect();
        rig.ring.push(chunk.iter().copied());
        second.push(chunk.iter().rev().copied());

        let mut warm = [0f32; 2048];
        for _ in 0..4 {
            rig.playback.render(&mut warm);
        }

        let before = allocations_here();
        let mut out = [0f32; 2048];
        let mut none: [f32; 0] = [];
        for size in [
            2048usize, 1920, 2047, 512, 6, 1536, 2048, 1023, 2048, 128, 2046, 2048,
        ] {
            rig.playback.render(&mut out[..size]);
        }
        rig.playback.render(&mut none);
        assert_eq!(
            allocations_here(),
            before,
            "the settled callback path must allocate nothing"
        );
    }

    #[test]
    #[ignore = "a measurement, not a check: run with --release --ignored --nocapture"]
    fn render_benchmark_report() {
        let mut rig = rig(48_000, 2);
        let mut noise = Noise(7);
        let chunk: Vec<f32> = (0..8192).map(|_| noise.next() * 0.3).collect();
        let mut out = [0f32; 2048];

        for _ in 0..64 {
            rig.ring.push(chunk.iter().copied());
            rig.playback.render(&mut out);
        }
        let before = allocations_here();
        let started = Instant::now();
        const CALLBACKS: u32 = 20_000;
        for i in 0..CALLBACKS {
            if i % 2 == 0 {
                rig.ring.push(chunk.iter().copied());
            }
            rig.playback.render(&mut out);
        }
        let elapsed = started.elapsed();
        assert_eq!(
            allocations_here(),
            before,
            "the steady path must not allocate"
        );
        println!(
            "{CALLBACKS} callbacks of {} samples: {} ns per callback, {:.2} ns per output sample",
            out.len(),
            elapsed.as_nanos() / u128::from(CALLBACKS),
            elapsed.as_nanos() as f64 / f64::from(CALLBACKS) / out.len() as f64,
        );
        println!(
            "allocator: {} events process-wide (parallel tests included), {} bytes live, {} bytes peak",
            allocations(),
            live_bytes(),
            peak_bytes(),
        );
    }
}
