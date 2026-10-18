use std::collections::VecDeque;
use std::time::{Duration, Instant};

use sonora_common_audio::sinc_resampler::{KERNEL_SIZE, SincResampler, SincResamplerCallback};

pub(super) const RATE: u32 = 48_000;
pub(super) const FRAME: usize = 480;
pub(super) const MAX_RATE: u32 = 384_000;
const REQUEST: usize = 64;
const CAPACITY: usize = 16_384;

struct Source {
    samples: VecDeque<f32>,
    underflow: bool,
    pulled: u64,
}

impl SincResamplerCallback for Source {
    fn run(&mut self, frames: usize, destination: &mut [f32]) {
        self.pulled += frames as u64;
        for slot in &mut destination[..frames] {
            if let Some(sample) = self.samples.pop_front() {
                *slot = sample;
            } else {
                self.underflow = true;
                *slot = 0.;
            }
        }
    }
}

pub(super) struct Resampler {
    sinc: Option<SincResampler>,
    source: Source,
    input_rate: u32,
    output_rate: u32,
    first: Option<Instant>,
    output_frames: u64,
    input_frames: u64,
}

impl Resampler {
    pub(super) fn new(input_rate: u32, output_rate: u32) -> Result<Self, &'static str> {
        if !(8_000..=MAX_RATE).contains(&input_rate) || !(8_000..=MAX_RATE).contains(&output_rate) {
            return Err("Sonora requires device rates between 8 kHz and 384 kHz.");
        }
        Ok(Self {
            sinc: (input_rate != output_rate).then(|| {
                SincResampler::new(f64::from(input_rate) / f64::from(output_rate), REQUEST)
            }),
            source: Source {
                samples: VecDeque::with_capacity(CAPACITY),
                underflow: false,
                pulled: 0,
            },
            input_rate,
            output_rate,
            first: None,
            output_frames: 0,
            input_frames: 0,
        })
    }

    pub(super) fn push(&mut self, samples: &[f32], at: Instant) -> Result<(), &'static str> {
        if self.source.samples.len() + samples.len() > CAPACITY {
            return Err("Audio conversion backlog exceeded its fixed capacity.");
        }
        let elapsed =
            Duration::from_secs_f64(self.input_frames as f64 / f64::from(self.input_rate));
        self.first = Some(at.checked_sub(elapsed).unwrap_or(at));
        self.input_frames += samples.len() as u64;
        self.source.samples.extend(samples.iter().copied());
        Ok(())
    }

    pub(super) fn pull(&mut self, output: &mut [f32]) -> Result<Option<Instant>, &'static str> {
        if output.is_empty() {
            return Ok(None);
        }
        let required = if self.sinc.is_some() {
            // Include the sinc's retained input and its shorter first block, not a fresh margin per pull.
            let last = ((u128::from(self.output_frames) + output.len() as u128 - 1)
                * u128::from(self.input_rate))
            .div_ceil(u128::from(self.output_rate));
            let initial = (REQUEST - KERNEL_SIZE / 2) as u128;
            let requests = 1 + if last < initial {
                0
            } else {
                1 + (last - initial) / REQUEST as u128
            };
            usize::try_from(
                (requests * REQUEST as u128).saturating_sub(u128::from(self.source.pulled)),
            )
            .map_err(|_| "Audio conversion input demand overflowed.")?
        } else {
            output.len()
        };
        if self.source.samples.len() < required {
            return Ok(None);
        }
        let first = self.first.ok_or("Audio conversion has no timestamp.")?;
        let delay = if self.sinc.is_some() {
            Duration::from_secs_f64(16. / f64::from(self.input_rate))
        } else {
            Duration::ZERO
        };
        let at = (first
            + Duration::from_secs_f64(self.output_frames as f64 / f64::from(self.output_rate)))
        .checked_sub(delay)
        .unwrap_or(first);
        if let Some(sinc) = &mut self.sinc {
            sinc.resample(output.len(), output, &mut self.source);
        } else {
            for slot in output.iter_mut() {
                *slot = self
                    .source
                    .samples
                    .pop_front()
                    .ok_or("Audio conversion underflow.")?;
            }
        }
        if self.source.underflow {
            return Err("Audio conversion underflow; microphone blocked.");
        }
        self.output_frames += output.len() as u64;
        Ok(Some(at))
    }

    pub(super) fn reset(&mut self) {
        self.source.samples.clear();
        self.source.underflow = false;
        self.source.pulled = 0;
        self.first = None;
        self.output_frames = 0;
        self.input_frames = 0;
        if let Some(sinc) = &mut self.sinc {
            sinc.flush();
        }
    }
}
