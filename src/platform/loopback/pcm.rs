use std::time::Duration;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
pub const FRAMES: usize = 960;
pub const SAMPLES: usize = FRAMES * CHANNELS;

pub struct Packetizer {
    samples: [i16; SAMPLES],
    used: usize,
    start: Duration,
    expected: Option<Duration>,
}

impl Default for Packetizer {
    fn default() -> Self {
        Self {
            samples: [0; SAMPLES],
            used: 0,
            start: Duration::ZERO,
            expected: None,
        }
    }
}

impl Packetizer {
    pub fn push(
        &mut self,
        samples: &[i16],
        timestamp: Duration,
        mut packet: impl FnMut(&[i16], Duration),
    ) {
        if self
            .expected
            .is_some_and(|expected| expected.abs_diff(timestamp) > Duration::from_millis(5))
        {
            self.used = 0;
        }
        let samples = &samples[..samples.len() / CHANNELS * CHANNELS];
        for (index, &sample) in samples.iter().enumerate() {
            if self.used == 0 {
                self.start = timestamp + duration(index / CHANNELS);
            }
            self.samples[self.used] = sample;
            self.used += 1;
            if self.used == SAMPLES {
                packet(&self.samples, self.start);
                self.used = 0;
            }
        }
        self.expected = Some(timestamp + duration(samples.len() / CHANNELS));
    }
}

pub fn duration(frames: usize) -> Duration {
    Duration::from_nanos((frames as u128 * 1_000_000_000 / u128::from(RATE)) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_wasapi_chunks_make_exact_stereo_twenty_ms_packets() {
        let input: Vec<i16> = (0..SAMPLES * 3).map(|n| n as i16).collect();
        let mut packetizer = Packetizer::default();
        let mut output = Vec::new();
        let mut timestamps = Vec::new();
        let mut offset = 0;
        for length in [160, 800, 1920, 2880] {
            packetizer.push(
                &input[offset..offset + length],
                duration(offset / 2),
                |pcm, timestamp| {
                    output.extend_from_slice(pcm);
                    timestamps.push(timestamp);
                },
            );
            offset += length;
        }
        assert_eq!(output, input);
        assert_eq!(
            timestamps,
            [
                Duration::ZERO,
                Duration::from_millis(20),
                Duration::from_millis(40)
            ]
        );
    }

    #[test]
    fn discontinuity_discards_partial_old_audio_instead_of_joining_epochs() {
        let mut packetizer = Packetizer::default();
        packetizer.push(&[9; 20], Duration::ZERO, |_, _| panic!("partial"));
        let mut packets = 0;
        packetizer.push(&[2; SAMPLES], Duration::from_secs(2), |pcm, timestamp| {
            assert!(pcm.iter().all(|sample| *sample == 2));
            assert_eq!(timestamp, Duration::from_secs(2));
            packets += 1;
        });
        assert_eq!(packets, 1);
    }
}
