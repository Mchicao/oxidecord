//! Every sender summed and laid out for the speakers.

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

/// Everyone audible in the call, one ring per sender, summed on the way out.
///
/// Each sender's packets keep their own clock and their own jitter, so each
/// gets a ring to prime, starve and ramp on its own: one person's connection
/// stalling doesn't cut out everyone else.
pub(super) struct Mixer {
    /// Keyed by SSRC. A list rather than a map because the audio callback
    /// walks all of it every time and looks nothing up.
    rings: Mutex<Vec<(u32, Arc<SampleRing>)>>,
    control: Arc<AudioControl>,
}

impl Mixer {
    pub(super) fn new(control: Arc<AudioControl>) -> Self {
        Self {
            rings: Mutex::default(),
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

        if let Ok(rings) = self.rings.lock() {
            for (_, ring) in rings.iter() {
                ring.fill(stream);
                for (slot, sample) in out.iter_mut().zip(stream.iter()) {
                    *slot += sample;
                }
            }
        }

        for slot in out.iter_mut() {
            *slot = limit(*slot);
        }
    }

    pub(super) fn add(&self, ssrc: u32, ring: Arc<SampleRing>) {
        if let Ok(mut rings) = self.rings.lock() {
            rings.push((ssrc, ring));
        }
    }

    pub(super) fn remove(&self, ssrc: u32) {
        if let Ok(mut rings) = self.rings.lock() {
            rings.retain(|(id, _)| *id != ssrc);
        }
    }

    pub(super) fn clear(&self) {
        if let Ok(mut rings) = self.rings.lock() {
            rings.clear();
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

        // How many source frames the loop below will step through. Counted by
        // walking the same additions in the same order rather than by the
        // closed form, which rounds differently often enough to leave the
        // resampler a frame out of step for the rest of the call.
        let mut phase = self.phase;
        let mut pulls = 0;
        for _ in 0..frames {
            phase += self.step;
            while phase >= 1. {
                pulls += 1;
                phase -= 1.;
            }
        }

        // Resized to the exact window this callback pulls rather than
        // cleared: a shrink leaves the surplus slots unread past the length,
        // a grow zero-fills only the new few, and the mixer's accumulator
        // pass overwrites the whole buffer before it is read. Once the
        // device's buffer size has settled this is a no-op.
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
                    // Anything past stereo (a surround device) gets the mix,
                    // which is better than leaving those speakers silent.
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

        // Whatever the device asked for beyond a whole number of frames.
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
        mixer.add(1, ring.clone());
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

    /// The test binary's allocator, instrumented so tests can prove things
    /// stopped allocating without asserting on private structure.
    ///
    /// Events are counted twice: per thread (see [`allocations_here`]) and
    /// process-wide (see [`allocations`]); bytes handed out are tracked
    /// process-wide only. Deallocations are not events, but they do return
    /// bytes to [`live_bytes`].
    struct CountingAlloc;

    thread_local! {
        static CALLBACK_ALLOCS: Cell<u64> = const { Cell::new(0) };
    }

    static TOTAL_ALLOCS: AtomicU64 = AtomicU64::new(0);
    static LIVE_BYTES: AtomicI64 = AtomicI64::new(0);
    static PEAK_BYTES: AtomicI64 = AtomicI64::new(0);

    /// Adds to the live-byte count and raises the high-water mark if the
    /// addition passed it.
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

    /// Allocation events (`alloc`, `realloc` and `alloc_zeroed` calls;
    /// deallocations are not events) on every thread of the test binary,
    /// counted from process start. This is the counter for work done on
    /// threads of their own — a worker's allocations land here even when the
    /// test drives it from another thread. Other tests running in parallel
    /// are counted too, so a before/after window is only meaningful when the
    /// measured work out-allocates that noise or the binary runs
    /// single-threaded.
    pub(in crate::voice::audio) fn allocations() -> u64 {
        TOTAL_ALLOCS.load(Ordering::Relaxed)
    }

    /// [`allocations`] restricted to the calling thread: immune to tests
    /// running in parallel, and blind to anything the code under test does
    /// on its own threads.
    pub(in crate::voice::audio) fn allocations_here() -> u64 {
        CALLBACK_ALLOCS.with(Cell::get)
    }

    /// Bytes currently handed out, across every thread. A snapshot taken
    /// while other threads are mid-allocation is approximate, never torn.
    /// This is book-keeping inside the test binary; it says nothing about
    /// the process's working set or private bytes.
    pub(in crate::voice::audio) fn live_bytes() -> u64 {
        LIVE_BYTES.load(Ordering::Relaxed).max(0) as u64
    }

    /// The high-water mark of [`live_bytes`] since process start.
    pub(in crate::voice::audio) fn peak_bytes() -> u64 {
        PEAK_BYTES.load(Ordering::Relaxed).max(0) as u64
    }

    /// One playback chain — a mixer, a sender's ring, a renderer — driven by
    /// hand, with a second sender that can join mid-run.
    struct Rig {
        mixer: Arc<Mixer>,
        ring: Arc<SampleRing>,
        playback: Playback,
    }

    fn rig(sample_rate: u32, channels: usize) -> Rig {
        let control = Arc::new(AudioControl::new(&AudioPreferences::default()));
        let mixer = Arc::new(Mixer::new(control));
        let ring = Arc::new(SampleRing::new(48_000, 2, 0));
        mixer.add(1, ring.clone());
        Rig {
            mixer: mixer.clone(),
            ring,
            playback: Playback::new(mixer, sample_rate, channels),
        }
    }

    /// Deterministic noise: same seed, same waveform, in any number of rigs.
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
            // Re-zeroed between callbacks, exactly as the scratch behaved
            // before it was reused: the reference the reuse must match.
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
                    // Fresh audio, identical in both rigs.
                    0 | 1 => {
                        let chunk: Vec<f32> = (0..8192).map(|_| noise.next() * 0.4).collect();
                        live.ring.push(chunk.iter().copied());
                        zeroed.ring.push(chunk.iter().copied());
                    }
                    // Nothing new: the rings run dry into underruns and priming.
                    2 => {}
                    // A driver handing back garbage.
                    3 => {
                        let junk = [f32::NAN, f32::INFINITY, -1.5, 0.75];
                        let chunk: Vec<f32> = junk.iter().cycle().take(4096).copied().collect();
                        live.ring.push(chunk.iter().copied());
                        zeroed.ring.push(chunk.iter().copied());
                    }
                    // A second sender joins, then both get fresh audio.
                    4 => {
                        let live_ring = Arc::new(SampleRing::new(48_000, 2, 0));
                        let zeroed_ring = Arc::new(SampleRing::new(48_000, 2, 0));
                        live.mixer.add(2, live_ring.clone());
                        zeroed.mixer.add(2, zeroed_ring.clone());
                        let chunk: Vec<f32> = (0..8192).map(|_| noise.next() * 0.4).collect();
                        live.ring.push(chunk.iter().copied());
                        zeroed.ring.push(chunk.iter().copied());
                        live_ring.push(chunk.iter().rev().copied());
                        zeroed_ring.push(chunk.iter().rev().copied());
                    }
                    // Everyone leaves; the queues are empty.
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

        // The queue empties and the callback shrinks: only the resampler's
        // carry-over may sound, then exact silence — never the loud samples
        // the scratch still holds past its new length.
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

        // Growing back over the stale region with nothing queued must stay
        // silent too.
        let mut grown = [0f32; 8192];
        rig.playback.render(&mut grown);
        assert!(grown.iter().all(|sample| *sample == 0.));

        // And fresh quiet audio over the same region must arrive clean,
        // without the loud era bleeding through.
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
        // Six source frames per output frame: the deepest scratch and stream
        // the sizes below can ask for.
        let mut rig = rig(8_000, 2);
        let second = Arc::new(SampleRing::new(48_000, 2, 0));
        rig.mixer.add(2, second.clone());
        let mut noise = Noise(99);
        let chunk: Vec<f32> = (0..8192).map(|_| noise.next() * 0.3).collect();
        rig.ring.push(chunk.iter().copied());
        second.push(chunk.iter().rev().copied());

        // Settle the scratch, the stream and both rings at the largest
        // buffer the window below asks for.
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
