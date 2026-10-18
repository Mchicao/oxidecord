//! The ring of samples between an audio callback and the call.

use std::collections::VecDeque;
use std::sync::Mutex;

use crate::platform::audio::sanitize;

/// Ceiling on each ring: enough to ride out a scheduling hiccup, short enough
/// that recovery isn't audible as a growing delay.
const MAX_BUFFERED_MS: u32 = 400;

/// How long the ramp either side of a gap lasts.
///
/// Long enough to take the edge off the step, short enough not to swallow a
/// consonant.
const FADE_MS: u32 = 3;

/// A ring of interleaved samples shared between an audio callback and the
/// call.
///
/// Neither side ever blocks: a starved reader gets silence and an overrun
/// writer drops the oldest audio, since stalling either thread would be heard
/// as a stutter across the whole call.
///
/// Both of those are steps in the waveform, and a step is a click — the louder
/// the audio it interrupts, the louder the click. So the ring hides them: it
/// ramps down into a gap and back up out of one, and after a stall it waits
/// for `prime` samples to build up rather than tearing another gap on the very
/// next callback.
pub(super) struct SampleRing {
    state: Mutex<RingState>,
    /// Interleaved channel count. Every drain and every drop is a whole number
    /// of frames, because losing a single sample would rotate every later one
    /// onto the wrong channel for the rest of the call.
    pub(super) channels: usize,
    /// Samples the reader waits for after a stall before it drains again.
    prime: usize,
    capacity: usize,
    /// Length of the ramp either side of a gap, in frames.
    fade: usize,
}

#[derive(Default)]
struct RingState {
    epoch: u64,
    samples: VecDeque<f32>,
    /// While set the reader emits silence: the writer hasn't built the buffer
    /// back up since the last stall.
    priming: bool,
    /// The last sample emitted on each channel. A gap ramps down from here
    /// rather than cutting straight to zero.
    last: Vec<f32>,
    /// Frames left of the ramp back in after a gap.
    fade_in: usize,
}

impl SampleRing {
    pub(super) fn new(sample_rate: u32, channels: usize, prime_ms: u32) -> Self {
        let channels = channels.max(1);
        let frames = |ms: u32| (u64::from(sample_rate) * u64::from(ms) / 1000) as usize;
        let prime = frames(prime_ms) * channels;
        Self {
            state: Mutex::new(RingState {
                priming: true,
                last: vec![0.; channels],
                ..RingState::default()
            }),
            channels,
            prime,
            // A ring that couldn't hold what the reader waits for would never
            // finish priming.
            capacity: (frames(MAX_BUFFERED_MS) * channels).max(prime * 2),
            fade: frames(FADE_MS).max(1),
        }
    }

    pub(super) fn push(&self, samples: impl Iterator<Item = f32>) {
        self.push_gated(samples, 0, || true);
    }

    pub(super) fn push_gated(
        &self,
        samples: impl Iterator<Item = f32>,
        epoch: u64,
        open: impl FnOnce() -> bool,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if epoch < state.epoch {
            return;
        }
        if epoch != state.epoch {
            Self::reset(&mut state);
            state.epoch = epoch;
        }
        // A driver that hands back a NaN, or a mix that ran past full scale,
        // becomes a full-scale spike once it's converted to the device's
        // format. Neither belongs in a call, and neither is worth finding out
        // about through the speakers.
        state.samples.extend(samples.map(sanitize));
        if !open() {
            Self::reset(&mut state);
            return;
        }

        if state.samples.len() > self.capacity {
            // Keep the newest audio: the listener would rather skip forward
            // than fall further behind the conversation.
            let excess = state.samples.len() - self.capacity;
            let excess = (excess.div_ceil(self.channels) * self.channels).min(state.samples.len());
            state.samples.drain(..excess);
            // The reader is about to jump over whatever was dropped, so ramp
            // it back in on the far side of the jump.
            state.fade_in = self.fade;
        }
    }

    /// Fills `out` with what's buffered, ramping into silence when the writer
    /// hasn't kept up. `out` holds a whole number of frames.
    pub(super) fn fill(&self, out: &mut [f32]) {
        self.fill_gated(out, 0, true);
    }

    pub(super) fn fill_gated(&self, out: &mut [f32], epoch: u64, open: bool) {
        if out.is_empty() {
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            out.fill(0.);
            return;
        };
        if epoch != state.epoch || !open {
            Self::reset(&mut state);
            state.epoch = state.epoch.max(epoch);
            out.fill(0.);
            return;
        }

        if state.priming {
            if state.samples.len() < self.prime {
                self.ramp_from_last(&state, out);
                state.last.fill(0.);
                return;
            }
            state.priming = false;
            state.fade_in = self.fade;
        }

        let available = (state.samples.len() / self.channels) * self.channels;
        let take = available.min(out.len());
        for slot in out[..take].iter_mut() {
            *slot = state.samples.pop_front().unwrap_or(0.);
        }

        if take < out.len() {
            // Underrun. Ease out of the audio there was and wait for the
            // writer to get ahead again, rather than cutting it dead here and
            // tearing another gap on the next callback.
            self.fade_out(&mut out[..take]);
            out[take..].fill(0.);
            state.priming = true;
        }

        if state.fade_in > 0 {
            self.fade_in(out, &mut state.fade_in);
        }

        // Remember where the waveform ended, so the next gap knows what to
        // ramp down from.
        let frames = out.len() / self.channels;
        let last_frame = &out[(frames - 1) * self.channels..][..self.channels];
        state.last.copy_from_slice(last_frame);
    }

    /// Ramps `out` down from the last sample emitted, then holds silence.
    ///
    /// Used when there is nothing at all to play: cutting from the previous
    /// callback's final sample straight to zero is the click.
    fn ramp_from_last(&self, state: &RingState, out: &mut [f32]) {
        out.fill(0.);
        let span = self.fade.min(out.len() / self.channels);
        for frame in 0..span {
            let gain = 1. - (frame + 1) as f32 / span as f32;
            for (channel, slot) in out[frame * self.channels..][..self.channels]
                .iter_mut()
                .enumerate()
            {
                *slot = state.last[channel] * gain;
            }
        }
    }

    /// Ramps the end of what was filled down to silence, so the gap that
    /// follows isn't a step.
    fn fade_out(&self, filled: &mut [f32]) {
        let frames = filled.len() / self.channels;
        let span = self.fade.min(frames);
        for frame in 0..span {
            let gain = 1. - (frame + 1) as f32 / span as f32;
            let start = (frames - span + frame) * self.channels;
            for slot in filled[start..][..self.channels].iter_mut() {
                *slot *= gain;
            }
        }
    }

    /// Ramps the start of `out` up out of a gap, carrying on across callbacks
    /// when the ramp outlasts one of them.
    fn fade_in(&self, out: &mut [f32], remaining: &mut usize) {
        let span = (*remaining).min(out.len() / self.channels);
        for frame in 0..span {
            let done = self.fade - *remaining + frame;
            let gain = (done + 1) as f32 / self.fade as f32;
            for slot in out[frame * self.channels..][..self.channels].iter_mut() {
                *slot *= gain;
            }
        }
        *remaining -= span;
    }

    pub(super) fn clear(&self) {
        if let Ok(mut state) = self.state.lock() {
            Self::reset(&mut state);
        }
    }

    fn reset(state: &mut RingState) {
        state.samples.clear();
        state.priming = true;
        state.fade_in = 0;
        state.last.fill(0.);
    }
}
