mod engine;
pub(in crate::voice::audio) mod resample;

pub const LICENSE: &str = include_str!("SONORA-LICENSE");

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod tests;

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::{FromSample, Sample as _};
use serde::{Deserialize, Serialize};

use super::settings::AudioControl;
use super::{Capture, GainRamp};
use crate::platform::audio::sanitize;
use engine::Engine;
use resample::{FRAME, MAX_RATE, RATE, Resampler};

const QUEUE: usize = 64;
const MAX_AGE: Duration = Duration::from_millis(100);
const MAX_REFERENCE_GAP: Duration = Duration::from_millis(80);
const MAX_OUTPUT: usize = (MAX_RATE / 100) as usize + 1;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Options {
    pub noise_suppression: bool,
    pub echo_cancellation: bool,
    pub automatic_gain_control: bool,
}

impl Options {
    pub(super) fn bits(self) -> u8 {
        u8::from(self.noise_suppression)
            | u8::from(self.echo_cancellation) << 1
            | u8::from(self.automatic_gain_control) << 2
    }

    pub(super) fn from_bits(bits: u8) -> Self {
        Self {
            noise_suppression: bits & 1 != 0,
            echo_cancellation: bits & 2 != 0,
            automatic_gain_control: bits & 4 != 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(super) enum Status {
    Bypass,
    Starting,
    Active,
    WaitingReference,
    Failed,
}

#[derive(Clone, Copy)]
struct Packet {
    samples: [f32; FRAME],
    len: usize,
    rate: u32,
    at: Instant,
    epoch: u64,
    render: bool,
}

struct Worker {
    stopped: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

pub(super) struct Processing {
    sender: RwLock<Option<mpsc::SyncSender<Packet>>>,
    worker: Mutex<Option<Worker>>,
    fault: AtomicU64,
    configured: AtomicU8,
    pub(super) control: Arc<AudioControl>,
    session: u64,
    owner: u64,
}

impl Processing {
    pub(super) fn new(control: Arc<AudioControl>, session: u64) -> Arc<Self> {
        let owner = control
            .processing_owner
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let this = Arc::new(Self {
            sender: RwLock::new(None),
            worker: Mutex::new(None),
            fault: AtomicU64::new(0),
            configured: AtomicU8::new(0),
            control: control.clone(),
            session,
            owner,
        });
        *control
            .active_processing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::downgrade(&this);
        this.state(Status::Starting, None, control.snapshot().epoch());
        this
    }

    fn state(&self, status: Status, error: Option<&'static str>, epoch: u64) {
        self.control
            .processing_state(status, error, self.session, epoch, self.owner);
    }

    pub(super) fn is_current(&self) -> bool {
        self.control.processing_owner.load(Ordering::Acquire) == self.owner
    }

    pub(super) fn configure(
        self: &Arc<Self>,
        capture: Weak<Capture>,
        options: Options,
        rate: u32,
        channels: usize,
    ) {
        self.stop();
        if options.bits() == 0 {
            return;
        }
        self.fault.store(0, Ordering::Release);
        self.state(Status::Starting, None, self.control.snapshot().epoch());
        let stopped = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(QUEUE);
        let processing = self.clone();
        let end = stopped.clone();
        let thread = std::thread::Builder::new()
            .name("voice-sonora".into())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(
                        &processing,
                        &capture,
                        &receiver,
                        &end,
                        (options, rate, channels),
                        |_| Instant::now(),
                    )
                }));
                match outcome {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => processing.fail(error),
                    Err(_) => processing
                        .fail("Sonora initialization or processing failed; no bypass was applied."),
                }
            });
        match thread {
            Ok(thread) => {
                let (Ok(mut slot), Ok(mut worker)) = (self.sender.write(), self.worker.lock())
                else {
                    stopped.store(true, Ordering::Release);
                    drop(sender);
                    let _ = thread.join();
                    self.fail("Sonora worker state is unavailable.");
                    return;
                };
                *slot = Some(sender);
                *worker = Some(Worker { stopped, thread });
                self.configured.store(options.bits(), Ordering::Release);
                drop(slot);
                drop(worker);
                if self.control.processing_bits.load(Ordering::Acquire) != options.bits()
                    || !self.is_current()
                {
                    self.stop();
                }
            }
            Err(_) => self.fail("Could not start the bounded Sonora worker."),
        }
    }

    pub(super) fn stop(&self) {
        self.configured.store(0, Ordering::Release);
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(worker) = &worker {
            worker.stopped.store(true, Ordering::Release);
        }
        *self
            .sender
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if let Some(worker) = worker
            && worker.thread.thread().id() != std::thread::current().id()
        {
            let _ = worker.thread.join();
        }
    }

    pub(super) fn fail(&self, error: &'static str) {
        self.fail_at(error, self.control.snapshot().epoch());
    }

    fn has_fault(&self) -> bool {
        self.fault.load(Ordering::Acquire) == (self.control.snapshot().epoch() << 1) | 1
    }

    fn fail_at(&self, error: &'static str, epoch: u64) {
        let tag = (epoch << 1) | 1;
        if self.fault.fetch_max(tag, Ordering::AcqRel) >= tag {
            return;
        }
        if let Ok(mut slot) = self.sender.try_write()
            && self.control.snapshot().epoch() == epoch
        {
            *slot = None;
        }
        self.state(Status::Failed, Some(error), epoch);
    }

    fn sender(&self) -> Option<mpsc::SyncSender<Packet>> {
        if self.has_fault() {
            return None;
        }
        match self.sender.try_read() {
            Ok(slot) => slot.clone(),
            Err(std::sync::TryLockError::WouldBlock) => None,
            Err(std::sync::TryLockError::Poisoned(_)) => {
                self.fail("Sonora callback queue is unavailable; microphone blocked.");
                None
            }
        }
    }
}

pub(super) struct Tap {
    processing: Arc<Processing>,
    rate: u32,
    channels: usize,
    partial: f64,
    channel: usize,
    frame_at: Instant,
    epoch: u64,
}

impl Tap {
    pub(super) fn new(processing: Arc<Processing>, rate: u32, channels: usize) -> Self {
        Self {
            processing,
            rate,
            channels: channels.max(1),
            partial: 0.,
            channel: 0,
            frame_at: Instant::now(),
            epoch: u64::MAX,
        }
    }

    pub(super) fn push<T>(&mut self, data: &[T], at: Instant, render: bool)
    where
        T: Copy,
        f32: FromSample<T>,
    {
        let control = &self.processing.control;
        let bits = control.processing_bits.load(Ordering::Acquire);
        if bits == 0 || (render && bits & 2 == 0) {
            return;
        }
        if bits != self.processing.configured.load(Ordering::Acquire) {
            return;
        }
        if !(8_000..=MAX_RATE).contains(&self.rate) {
            self.processing
                .fail("Unsupported device sample rate; Sonora microphone blocked.");
            return;
        }
        let state = control.snapshot();
        if self.epoch != state.epoch() {
            self.channel = 0;
            self.partial = 0.;
            self.epoch = state.epoch();
        }
        if !render
            && (!state.open(true, self.processing.session)
                || f32::from_bits(control.input_gain.load(Ordering::Relaxed)) == 0.)
        {
            return;
        }
        let Some(sender) = self.processing.sender() else {
            return;
        };
        let mut packet = Packet {
            samples: [0.; FRAME],
            len: 0,
            rate: self.rate,
            at,
            epoch: self.epoch,
            render,
        };
        for (index, &sample) in data.iter().enumerate() {
            if self.channel == 0 && packet.len == 0 {
                self.frame_at = at
                    + Duration::from_secs_f64(
                        index as f64 / (f64::from(self.rate) * self.channels as f64),
                    );
            }
            self.partial += f64::from(sanitize(f32::from_sample(sample)));
            self.channel += 1;
            if self.channel == self.channels {
                if packet.len == 0 {
                    packet.at = self.frame_at;
                }
                packet.samples[packet.len] = (self.partial / self.channels as f64) as f32;
                packet.len += 1;
                self.partial = 0.;
                self.channel = 0;
                if packet.len == FRAME {
                    if sender.try_send(packet).is_err() {
                        self.processing.fail_at("Sonora input queue overran; microphone blocked. Change processing toggles to retry.", self.epoch);
                        return;
                    }
                    packet.len = 0;
                }
            }
        }
        if packet.len > 0 && sender.try_send(packet).is_err() {
            self.processing.fail_at("Sonora input queue overran; microphone blocked. Change processing toggles to retry.", self.epoch);
        }
    }
}

struct Pipeline {
    engine: Engine,
    capture: Resampler,
    render: Option<Resampler>,
    output: Resampler,
    render_rate: Option<u32>,
    reference: Option<(Instant, Instant)>,
    epoch: u64,
    input_end: Option<Instant>,
    render_end: Option<Instant>,
    output_blocks: u64,
    gain: GainRamp,
}

impl Pipeline {
    fn new(options: Options, rate: u32, epoch: u64, gain: f32) -> Result<Self, &'static str> {
        Ok(Self {
            engine: Engine::new(options),
            capture: Resampler::new(rate, RATE)?,
            render: None,
            output: Resampler::new(RATE, rate)?,
            render_rate: None,
            reference: None,
            epoch,
            input_end: None,
            render_end: None,
            output_blocks: 0,
            gain: GainRamp::new(gain),
        })
    }
}

fn run(
    processing: &Processing,
    capture: &Weak<Capture>,
    receiver: &mpsc::Receiver<Packet>,
    stopped: &AtomicBool,
    format: (Options, u32, usize),
    clock: impl Fn(Instant) -> Instant,
) -> Result<(), &'static str> {
    let (options, rate, channels) = format;
    let mut pipeline: Option<Pipeline> = None;
    let mut input = [0.; FRAME];
    let mut processed = [0.; FRAME];
    let mut native = [0.; MAX_OUTPUT];
    while !stopped.load(Ordering::Acquire) {
        let Ok(packet) = receiver.recv() else {
            break;
        };
        if stopped.load(Ordering::Acquire) || processing.has_fault() {
            break;
        }
        let Some(capture) = capture.upgrade() else {
            break;
        };
        let state = capture.control.snapshot();
        if packet.epoch != state.epoch()
            || capture.control.processing_bits.load(Ordering::Acquire) != options.bits()
        {
            continue;
        }
        if !packet.render && packet.at > clock(packet.at) + Duration::from_millis(10) {
            return Err("Microphone clock is ahead of callback time; microphone blocked.");
        }
        if clock(packet.at).saturating_duration_since(packet.at) > MAX_AGE {
            return Err(
                "Sonora audio is more than 100 ms late; microphone blocked. Change processing toggles to retry.",
            );
        }
        if pipeline
            .as_ref()
            .is_none_or(|pipeline| pipeline.epoch != packet.epoch)
        {
            pipeline = Some(Pipeline::new(
                options,
                rate,
                packet.epoch,
                f32::from_bits(capture.control.input_gain.load(Ordering::Relaxed)),
            )?);
            processing.state(
                if options.echo_cancellation {
                    Status::WaitingReference
                } else {
                    Status::Starting
                },
                None,
                state.epoch(),
            );
        }
        let pipeline = pipeline.as_mut().ok_or("Sonora pipeline is unavailable.")?;
        let end = if packet.render {
            &mut pipeline.render_end
        } else {
            &mut pipeline.input_end
        };
        if end.is_some_and(|last| distance(last, packet.at) > Duration::from_millis(30)) {
            return Err(
                "Audio device timing changed or a stream stalled; microphone blocked. Change processing toggles to retry.",
            );
        }
        *end =
            Some(packet.at + Duration::from_secs_f64(packet.len as f64 / f64::from(packet.rate)));
        if packet.render {
            if !options.echo_cancellation {
                continue;
            }
            if pipeline.render_rate != Some(packet.rate) {
                pipeline.render = Some(Resampler::new(packet.rate, RATE)?);
                pipeline.render_rate = Some(packet.rate);
            }
            let render = pipeline
                .render
                .as_mut()
                .ok_or("Echo cancellation has no playback converter.")?;
            render.push(&packet.samples[..packet.len], packet.at)?;
            while let Some(at) = render.pull(&mut input)? {
                pipeline.engine.render(&input)?;
                pipeline.reference = Some((at, clock(packet.at)));
            }
            continue;
        }
        pipeline
            .capture
            .push(&packet.samples[..packet.len], packet.at)?;
        while let Some(at) = pipeline.capture.pull(&mut input)? {
            let now = clock(packet.at);
            let delay = if options.echo_cancellation {
                let Some((render_at, analyzed_at)) = pipeline.reference else {
                    processing.state(Status::WaitingReference, None, state.epoch());
                    continue;
                };
                if now.saturating_duration_since(analyzed_at) > MAX_REFERENCE_GAP
                    || distance(render_at, at) > Duration::from_millis(500)
                {
                    processing.state(Status::WaitingReference, None, state.epoch());
                    pipeline.output.reset();
                    pipeline.capture.reset();
                    if let Some(render) = &mut pipeline.render {
                        render.reset();
                    }
                    pipeline.reference = None;
                    pipeline.output_blocks = 0;
                    pipeline.render_end = None;
                    pipeline.engine = Engine::new(options);
                    continue;
                }
                stream_delay_ms(render_at, analyzed_at, at, now)?
            } else {
                0
            };
            pipeline.engine.capture(&input, &mut processed, delay)?;
            if clock(packet.at).saturating_duration_since(at) > MAX_AGE {
                return Err("Sonora processing missed its 100 ms deadline; microphone blocked.");
            }
            let gain = f32::from_bits(capture.control.input_gain.load(Ordering::Relaxed));
            for sample in &mut processed {
                *sample = pipeline
                    .gain
                    .apply(*sample, gain, 1. / (RATE as f32 * 0.005));
            }
            if capture.control.snapshot() != state
                || capture.control.processing_bits.load(Ordering::Acquire) != options.bits()
                || stopped.load(Ordering::Acquire)
            {
                break;
            }
            processing.state(Status::Active, None, state.epoch());
            let power = processed
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum();
            capture.activity.observe(
                power,
                FRAME,
                clock(packet.at),
                state.activity_threshold(capture.control.threshold()),
                state.epoch(),
            );
            pipeline.output.push(&processed, at)?;
            loop {
                let next = ((pipeline.output_blocks + 1) * u64::from(rate) / 100
                    - pipeline.output_blocks * u64::from(rate) / 100)
                    as usize;
                if pipeline.output.pull(&mut native[..next])?.is_none() {
                    break;
                }
                pipeline.output_blocks += 1;
                capture.ring.push_gated(
                    native[..next]
                        .iter()
                        .flat_map(|sample| std::iter::repeat_n(*sample, channels)),
                    state.epoch(),
                    || {
                        let open = capture.control.snapshot() == state
                            && capture.open(state)
                            && !stopped.load(Ordering::Acquire);
                        capture.control.transmission(open, capture.session);
                        open
                    },
                );
            }
        }
    }
    Ok(())
}

fn distance(a: Instant, b: Instant) -> Duration {
    a.saturating_duration_since(b)
        .max(b.saturating_duration_since(a))
}

fn stream_delay_ms(
    render: Instant,
    analyzed: Instant,
    captured: Instant,
    processed: Instant,
) -> Result<i32, &'static str> {
    let render_delay = if render >= analyzed {
        render.duration_since(analyzed).as_micros() as i128
    } else {
        -(analyzed.duration_since(render).as_micros() as i128)
    };
    let delay = render_delay + processed.saturating_duration_since(captured).as_micros() as i128;
    if !(-10_000..=500_000).contains(&delay) {
        return Err("Playback/capture clocks are desynchronized; microphone blocked.");
    }
    Ok((delay.max(0) / 1000) as i32)
}
