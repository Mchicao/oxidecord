use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use opus2::{Application, Channels, Encoder};
use tokio::sync::{mpsc, oneshot, watch};

use super::processing::resample::Resampler;
use super::{AudioIo, MicSource};
use crate::platform::capture::clock::MediaClock;
use crate::voice::stream::{AudioPacket, CallMedia};

pub(in crate::voice) enum TransmitEvent {
    Speaking(bool),
    Failed(String),
}

pub(in crate::voice) struct Transmitter {
    gate: watch::Sender<bool>,
    _stop: oneshot::Sender<()>,
}

impl Transmitter {
    pub(in crate::voice) fn start(
        audio: Arc<AudioIo>,
        media: Arc<CallMedia>,
        events: mpsc::Sender<TransmitEvent>,
    ) -> Self {
        let (gate, gate_rx) = watch::channel(false);
        let (stop, stopped) = oneshot::channel();
        crate::platform::runtime::handle().spawn(async move {
            let result = tokio::select! {
                _ = stopped => return,
                result = run(audio, media, gate_rx, &events) => result,
            };
            if let Err(error) = result {
                let _ = events.send(TransmitEvent::Failed(error)).await;
            }
        });
        Self { gate, _stop: stop }
    }

    pub(in crate::voice) fn set_open(&self, open: bool) {
        self.gate.send_if_modified(|current| {
            if *current == open {
                return false;
            }
            *current = open;
            true
        });
    }
}

struct FrameSizer {
    rate: u32,
    ticks: u64,
}
impl FrameSizer {
    fn next(&mut self) -> usize {
        let before = self.ticks * u64::from(self.rate) / 100;
        self.ticks += 1;
        (self.ticks * u64::from(self.rate) / 100 - before) as usize
    }
}

struct PcmEncoder {
    left: Resampler,
    right: Resampler,
    channels: usize,
    opus: Encoder,
    scratch_left: Vec<f32>,
    scratch_right: Vec<f32>,
    pcm: [f32; 1920],
    used: usize,
    at: Instant,
}

impl PcmEncoder {
    fn new(rate: u32, channels: usize) -> Result<Self, String> {
        if !(1..=2).contains(&channels) {
            return Err("Microphone must provide mono or stereo PCM".into());
        }
        let mut opus =
            Encoder::new(48_000, Channels::Stereo, Application::Voip).map_err(|e| e.to_string())?;
        opus.set_bitrate(opus2::Bitrate::Bits(96_000))
            .map_err(|e| e.to_string())?;
        Ok(Self {
            left: Resampler::new(rate, 48_000)?,
            right: Resampler::new(rate, 48_000)?,
            channels,
            opus,
            scratch_left: Vec::with_capacity(rate as usize / 100 + 1),
            scratch_right: Vec::with_capacity(rate as usize / 100 + 1),
            pcm: [0.; 1920],
            used: 0,
            at: Instant::now(),
        })
    }

    fn reset(&mut self) -> Result<(), String> {
        self.left.reset();
        self.right.reset();
        self.used = 0;
        self.pcm.fill(0.);
        self.opus.reset_state().map_err(|e| e.to_string())
    }

    fn push(
        &mut self,
        pcm: &[f32],
        at: Instant,
        generation: u64,
        clock: MediaClock,
        mut emit: impl FnMut(AudioPacket),
    ) -> Result<(), String> {
        self.scratch_left.clear();
        self.scratch_right.clear();
        for frame in pcm.chunks_exact(self.channels) {
            self.scratch_left.push(frame[0]);
            self.scratch_right.push(frame[self.channels - 1]);
        }
        self.left.push(&self.scratch_left, at)?;
        self.right.push(&self.scratch_right, at)?;
        let (mut left, mut right, mut compressed) = ([0.; 480], [0.; 480], [0u8; 1275]);
        loop {
            let left_at = self.left.pull(&mut left)?;
            let right_at = self.right.pull(&mut right)?;
            match (left_at, right_at) {
                (None, None) => break,
                (Some(time), Some(_)) => {
                    if self.used == 0 {
                        self.at = time;
                    }
                    for (index, (left, right)) in left.into_iter().zip(right).enumerate() {
                        self.pcm[self.used + index * 2] = left;
                        self.pcm[self.used + index * 2 + 1] = right;
                    }
                    self.used += 960;
                    if self.used == self.pcm.len() {
                        let length = self
                            .opus
                            .encode_float(&self.pcm, &mut compressed)
                            .map_err(|e| e.to_string())?;
                        emit(AudioPacket {
                            generation,
                            capture_epoch: 0,
                            opus: compressed[..length].to_vec(),
                            timestamp: clock.at(self.at),
                        });
                        self.used = 0;
                    }
                }
                _ => return Err("Microphone stereo resampler clocks diverged".into()),
            }
        }
        Ok(())
    }
}

async fn run(
    audio: Arc<AudioIo>,
    media: Arc<CallMedia>,
    mut gate: watch::Receiver<bool>,
    events: &mpsc::Sender<TransmitEvent>,
) -> Result<(), String> {
    let (rate, channels) = audio.capture_format;
    let mut encoder = PcmEncoder::new(rate, channels as usize)?;
    let mut source = MicSource {
        capture: audio.capture.clone(),
        shutdown: audio.shutdown.clone(),
        samples: Vec::new(),
    };
    let mut sizer = FrameSizer { rate, ticks: 0 };
    let mut bytes = vec![0; (rate as usize / 100 + 1) * channels as usize * 4];
    let mut samples = vec![0.; bytes.len() / 4];
    let mut timer = tokio::time::interval(Duration::from_millis(10));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = audio.capture.control.snapshot();
    let mut generation = media.audio_generation();
    let mut speaking = false;
    loop {
        let open = *gate.borrow_and_update();
        tokio::select! {
            biased;
            changed = gate.changed() => {
                if changed.is_err() { return Ok(()); }
                encoder.reset()?;
                generation = media.audio_generation();
                timer.reset();
                if speaking {
                    speaking = false;
                    if events.send(TransmitEvent::Speaking(false)).await.is_err() { return Ok(()); }
                }
            },
            _ = timer.tick(), if open => {
                let state = audio.capture.control.snapshot();
                let now_generation = media.audio_generation();
                if previous != state || generation != now_generation {
                    encoder.reset()?;
                    previous = state; generation = now_generation;
                }
                if !audio.can_transmit() { continue; }
                let count = sizer.next() * channels as usize;
                let at = Instant::now().checked_sub(Duration::from_secs_f64(count as f64 / channels as f64 / f64::from(rate))).unwrap_or_else(Instant::now);
                source.read_exact(&mut bytes[..count * 4]).map_err(|e| e.to_string())?;
                for (slot, encoded) in samples[..count].iter_mut().zip(bytes[..count * 4].as_chunks::<4>().0) {
                    *slot = f32::from_le_bytes(*encoded);
                }
                encoder.push(&samples[..count], at, generation, media.clock(), |mut packet| {
                    packet.capture_epoch = state.epoch();
                    if audio.capture.control.snapshot() == state && audio.can_transmit() && media.audio_generation() == generation {
                        let _ = media.send_opus(packet);
                    }
                })?;
                if audio.capture.control.snapshot() != state || !audio.can_transmit() { encoder.reset()?; }
                let active = audio.is_speaking();
                if speaking != active {
                    speaking = active;
                    if events.send(TransmitEvent::Speaking(active)).await.is_err() { return Ok(()); }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_gate_updates_do_not_reset_a_running_microphone() {
        let (gate, mut observed) = watch::channel(false);
        let (stop, _stopped) = oneshot::channel();
        let transmitter = Transmitter { gate, _stop: stop };
        transmitter.set_open(false);
        assert!(!observed.has_changed().unwrap());
        transmitter.set_open(true);
        assert!(observed.has_changed().unwrap());
        assert!(*observed.borrow_and_update());
        for _ in 0..50 {
            transmitter.set_open(true);
        }
        assert!(!observed.has_changed().unwrap());
        transmitter.set_open(false);
        assert!(observed.has_changed().unwrap());
        assert!(!*observed.borrow_and_update());
    }

    #[test]
    fn rational_ten_ms_chunks_do_not_drift_at_fractional_rates() {
        for rate in [8_000, 11_025, 22_050, 44_100, 48_000, 96_000, 192_000] {
            let mut sizer = FrameSizer { rate, ticks: 0 };
            assert_eq!(
                (0..1000).map(|_| sizer.next()).sum::<usize>(),
                rate as usize * 10
            );
        }
    }

    #[test]
    fn native_rates_produce_decodable_twenty_ms_stereo_opus() {
        for rate in [8_000, 22_050, 44_100, 48_000, 96_000] {
            for channels in [1, 2] {
                let mut encoder = PcmEncoder::new(rate, channels).unwrap();
                let start = Instant::now();
                let clock = MediaClock::default();
                let mut packets = Vec::new();
                let mut sizer = FrameSizer { rate, ticks: 0 };
                let mut offset = 0;
                for _ in 0..110 {
                    let frames = sizer.next();
                    let pcm: Vec<_> = (0..frames)
                        .flat_map(|n| {
                            let sample = ((offset + n) as f32 * 440. * std::f32::consts::TAU
                                / rate as f32)
                                .sin()
                                * 0.2;
                            std::iter::repeat_n(sample, channels)
                        })
                        .collect();
                    encoder
                        .push(
                            &pcm,
                            start + Duration::from_secs_f64(offset as f64 / rate as f64),
                            7,
                            clock,
                            |packet| packets.push(packet),
                        )
                        .unwrap();
                    offset += frames;
                }
                assert!(
                    (53..=55).contains(&packets.len()),
                    "rate {rate}: {} packets",
                    packets.len()
                );
                let mut decoder = opus2::Decoder::new(48_000, Channels::Stereo).unwrap();
                let mut pcm = [0.; 1920];
                for packet in &packets {
                    assert_eq!(packet.generation, 7);
                    assert_eq!(
                        decoder.decode_float(&packet.opus, &mut pcm, false).unwrap(),
                        960
                    );
                    assert!(pcm.iter().all(|sample| sample.is_finite()));
                }
                encoder.reset().unwrap();
                assert_eq!(encoder.used, 0);
            }
        }
    }
}
