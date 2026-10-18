//! The microphone and the speakers.
//!
//! Rings of interleaved `f32` sit between the audio devices and the call:
//! one for capture, which the mixer drains through [`MicSource`], and one per
//! sender for playback, which [`Incoming`] fills from the packets as they
//! arrive and the output device drains through [`Mixer`].
//!
//! Capture stays at the device's own sample rate; [`transmit`] resamples it
//! to 48kHz stereo and encodes Opus. Received packets arrive here without
//! transport encryption or DAVE framing and are decoded for the output device.
//!
//! Both directions do convert sample formats, to `f32` and back: the format a
//! device reports is whichever one its driver prefers, and a call with no
//! microphone is worse than a handful of conversions.
//!
//! Neither side of a ring shares a clock with the other, so a ring is either
//! running dry or running over most of the time. The rings answer for that
//! rather than the callbacks: see [`SampleRing`](ring::SampleRing) for the
//! priming and the ramps that keep a stall from being heard, and
//! [`Playback`](playback::Playback) for the phase the resampler has to carry
//! between callbacks.
//!
//! cpal streams are not `Send` on every platform, so both live on a thread of
//! their own that does nothing but hold them open until the call ends.

mod activity;
mod devices;
mod incoming;
#[cfg(test)]
mod microphone;
mod playback;
pub(crate) mod processing;
mod ring;
pub(crate) mod settings;
pub(super) mod transmit;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io::{Read, Result as IoResult};
#[cfg(test)]
use std::io::{Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use songbird::constants::SAMPLE_RATE;
#[cfg(test)]
use songbird::input::{Input, core::io::MediaSource};

use activity::CaptureActivity;
pub use devices::{AudioDevice, input_devices, output_devices};
use devices::{chosen_input, chosen_output, ring_channels, run_devices};
use incoming::Incoming;
#[cfg(test)]
use microphone::microphone;
use playback::Mixer;
use ring::SampleRing;
use settings::{AudioControl, GainRamp};

/// Packets are decoded to stereo, which is the layout the playback rings carry.
const CHANNELS: usize = 2;

/// How much the capture ring builds up before it starts draining again.
/// Shallower than a sender's ring (`incoming::PLAYBACK_PRIME_MS`): the
/// microphone only has device jitter to absorb rather than a network as well.
const CAPTURE_PRIME_MS: u32 = 30;

/// Ends the call for the device thread and everything it holds open.
///
/// The flag is what the callbacks read, on the audio threads, where taking a
/// lock is the one thing that must not happen. The condition variable is how
/// the device thread hears about it: polling for the flag left the microphone
/// light on for up to a poll interval after the user hung up.
#[derive(Default)]
struct Shutdown {
    stopped: AtomicBool,
    guard: Mutex<()>,
    signal: Condvar,
}

impl Shutdown {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // Taken and dropped purely to pair with the waiter: without it a stop
        // landing between the waiter's check and its wait would be missed.
        let _guard = self.guard.lock();
        self.signal.notify_all();
    }

    fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn wait(&self) {
        let Ok(mut guard) = self.guard.lock() else {
            return;
        };
        while !self.is_stopped() {
            let Ok(next) = self.signal.wait(guard) else {
                return;
            };
            guard = next;
        }
    }
}

pub struct AudioIo {
    pub(super) capture: Arc<Capture>,
    /// What the speakers play: every sender's ring.
    mixer: Arc<Mixer>,
    /// Every sender's decoder, keyed by SSRC. Only the call's event handlers
    /// touch these, so they're kept apart from the rings the audio callback
    /// locks.
    incoming: Mutex<HashMap<u32, Incoming>>,
    /// Set when the call ends, which stops the device thread and every
    /// callback still running on it.
    shutdown: Arc<Shutdown>,
    /// While deafened, incoming audio is dropped rather than played.
    deafened: Arc<AtomicBool>,
    /// The sample rate and channel count the capture ring is filled at, which
    /// the mixer is told so it can resample. Falls back to the call's own
    /// format when there's no microphone to ask.
    capture_format: (u32, u32),
}

pub(super) struct Capture {
    processing: Arc<processing::Processing>,
    rate: u32,
    ring: SampleRing,
    activity: CaptureActivity,
    pub(super) control: Arc<AudioControl>,
    pub(super) session: u64,
}

impl Capture {
    fn open(&self, state: settings::CaptureState) -> bool {
        f32::from_bits(self.control.input_gain.load(Ordering::Relaxed)) > 0.
            && self.processing.is_current()
            && self.control.processing_ready(state.epoch())
            && state.open(
                self.activity
                    .active(std::time::Instant::now(), state.epoch()),
                self.session,
            )
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.processing.stop();
    }
}

impl AudioIo {
    /// Opens the microphone and the speakers. Either name is a device id, as
    /// [`input_devices`] or [`output_devices`] reported it; `None`, or one
    /// without a supported configuration in its direction, falls back to
    /// the system default before streams are built.
    ///
    /// Either device may fail to open — no microphone, no output device, a
    /// driver that refuses the format — and the call carries on with the half
    /// that worked.
    pub fn start(
        input_device: Option<&str>,
        output_device: Option<&str>,
        control: Arc<AudioControl>,
        session: u64,
    ) -> Self {
        let host = cpal::default_host();
        // The devices are picked here, on the caller's thread, because the
        // capture format has to be known before the mixer is handed the
        // microphone — and before the capture ring can be sized in it. Only
        // the streams themselves are `!Send`.
        let input = chosen_input(&host, input_device);
        let output = chosen_output(&host, output_device);

        let capture_format =
            input
                .as_ref()
                .map_or((SAMPLE_RATE, CHANNELS as u32), |(_, config)| {
                    (
                        config.sample_rate(),
                        ring_channels(config.channels()) as u32,
                    )
                });

        let this = Self {
            capture: Arc::new(Capture {
                processing: processing::Processing::new(control.clone(), session),
                rate: capture_format.0,
                ring: SampleRing::new(
                    capture_format.0,
                    capture_format.1 as usize,
                    CAPTURE_PRIME_MS,
                ),
                activity: CaptureActivity::default(),
                control: control.clone(),
                session,
            }),
            mixer: Arc::new(Mixer::new(control)),
            incoming: Mutex::default(),
            shutdown: Arc::default(),
            deafened: Arc::new(AtomicBool::new(false)),
            capture_format,
        };
        this.configure_processing();

        let capture = this.capture.clone();
        let mixer = this.mixer.clone();
        let shutdown = this.shutdown.clone();
        std::thread::Builder::new()
            .name("voice-audio".into())
            .spawn(move || run_devices(input, output, &capture, &mixer, &shutdown))
            .ok();

        this
    }

    /// The parsed PCM adapter exercises capture without decoder read-ahead.
    #[cfg(test)]
    pub fn mic_input(&self) -> Input {
        let (sample_rate, channels) = self.capture_format;
        microphone(
            MicSource {
                capture: self.capture.clone(),
                shutdown: self.shutdown.clone(),
                samples: Vec::new(),
            },
            sample_rate,
            channels,
        )
    }

    pub(super) fn configure_processing(&self) {
        self.capture.ring.clear();
        self.capture.activity.clear();
        self.capture.processing.configure(
            Arc::downgrade(&self.capture),
            processing::Options::from_bits(
                self.capture.control.processing_bits.load(Ordering::Acquire),
            ),
            self.capture.rate,
            self.capture.ring.channels,
        );
    }

    /// Binds the sender before the first decoded sample reaches playback.
    pub(super) fn receive_from(&self, user_id: u64, ssrc: u32, sequence: u16, opus: &[u8]) {
        if self.deafened.load(Ordering::Relaxed) || self.shutdown.is_stopped() {
            return;
        }
        let Ok(mut incoming) = self.incoming.lock() else {
            return;
        };
        let stream = match incoming.entry(ssrc) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let Some(stream) = Incoming::new() else {
                    return;
                };
                self.mixer
                    .add_with_user(ssrc, Some(user_id), stream.ring.clone());
                entry.insert(stream)
            }
        };
        stream.receive(sequence, opus);
    }

    /// Stops playing a sender who has left the call.
    pub fn forget(&self, ssrc: u32) {
        if let Ok(mut incoming) = self.incoming.lock() {
            incoming.remove(&ssrc);
        }
        self.mixer.remove(ssrc);
    }

    /// Stops playing everyone. Whoever is still talking starts over with a
    /// fresh decoder at their next packet.
    pub(super) fn forget_all(&self) {
        if let Ok(mut incoming) = self.incoming.lock() {
            incoming.clear();
        }
        self.mixer.clear();
    }

    #[cfg(test)]
    pub fn set_deafened(&self, deafened: bool) {
        self.capture.control.set_deafened(deafened);
        self.set_playback_deafened(deafened);
    }

    pub(super) fn set_playback_deafened(&self, deafened: bool) {
        self.deafened.store(deafened, Ordering::Relaxed);
        self.capture.activity.clear();
        if deafened {
            // Drop what's already queued, so undeafening doesn't replay a
            // burst of stale audio.
            self.forget_all();
        }
    }

    #[cfg(test)]
    pub fn set_muted(&self, muted: bool) {
        self.capture.control.set_muted(muted);
        self.capture.activity.clear();
    }

    pub fn is_speaking(&self) -> bool {
        let state = self.capture.control.snapshot();
        self.capture.open(state)
            && !self.shutdown.is_stopped()
            && self
                .capture
                .activity
                .active(std::time::Instant::now(), state.epoch())
            && self.capture.control.snapshot() == state
    }

    pub(super) fn can_transmit(&self) -> bool {
        !self.shutdown.is_stopped() && self.capture.open(self.capture.control.snapshot())
    }

    pub(super) fn accepts_capture_epoch(&self, epoch: u64) -> bool {
        self.capture.control.snapshot().epoch() == epoch && self.can_transmit()
    }

    pub(super) fn end_call(&self) {
        self.capture.control.end_session(self.capture.session);
        self.stop();
    }

    /// Closes devices immediately, even while media tasks retain references.
    pub fn stop(&self) {
        self.shutdown.stop();
        self.capture.processing.stop();
        self.capture.ring.clear();
        self.forget_all();
    }
}

impl Drop for AudioIo {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The transmitter's view of the microphone: raw little-endian `f32`
/// that returns silence rather than ending when the device falls behind.
///
/// Shutdown ends the source; temporary capture starvation produces silence.
struct MicSource {
    capture: Arc<Capture>,
    shutdown: Arc<Shutdown>,
    samples: Vec<f32>,
}

impl Read for MicSource {
    fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
        if self.shutdown.is_stopped() {
            return Ok(0);
        }

        // Whole frames only: handing back a partial one would put every later
        // sample on the wrong channel. A buffer too small to hold one is the
        // only case with nothing to hand back, and `Ok(0)` there would read as
        // the end of the track.
        let channels = self.capture.ring.channels;
        let count = (buf.len() / size_of::<f32>() / channels) * channels;
        if count == 0 {
            return Err(std::io::ErrorKind::Interrupted.into());
        }

        self.samples.resize(count, 0.);
        let samples = &mut self.samples;
        let state = self.capture.control.snapshot();
        self.capture
            .ring
            .fill_gated(samples, state.epoch(), self.capture.open(state));
        if self.capture.control.snapshot() != state || !self.capture.open(state) {
            samples.fill(0.);
        }
        for (slot, sample) in buf.as_chunks_mut::<4>().0.iter_mut().zip(samples.iter()) {
            slot.copy_from_slice(&sample.to_le_bytes());
        }
        if self.capture.control.snapshot() != state || !self.capture.open(state) {
            buf[..count * size_of::<f32>()].fill(0);
        }
        self.capture.control.transmission(
            self.capture.open(self.capture.control.snapshot()),
            self.capture.session,
        );

        Ok(count * size_of::<f32>())
    }
}

#[cfg(test)]
impl Seek for MicSource {
    fn seek(&mut self, _pos: SeekFrom) -> IoResult<u64> {
        Err(std::io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
impl MediaSource for MicSource {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}
