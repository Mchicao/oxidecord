use std::time::{Duration, Instant, SystemTime};

#[derive(Clone, Copy)]
pub struct MediaClock {
    started: Instant,
    wall: SystemTime,
}

impl Default for MediaClock {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            wall: SystemTime::now(),
        }
    }
}

impl MediaClock {
    pub fn at(self, instant: Instant) -> Duration {
        instant.saturating_duration_since(self.started)
    }
    pub fn elapsed(self) -> Duration {
        self.started.elapsed()
    }
    pub fn wall_origin(self) -> SystemTime {
        self.wall
    }

    pub fn captured_at(self, captured_hns: u64, now_hns: u64) -> Duration {
        self.elapsed().saturating_sub(Duration::from_nanos(
            now_hns.saturating_sub(captured_hns).saturating_mul(100),
        ))
    }
}

pub fn rtp_timestamp(base: u32, elapsed: Duration, rate: u32) -> u32 {
    base.wrapping_add((elapsed.as_nanos() * u128::from(rate) / 1_000_000_000) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_and_video_clocks_cover_the_same_real_second_and_wrap() {
        let elapsed = Duration::from_millis(1020);
        assert_eq!(rtp_timestamp(0, elapsed, 48_000), 48_960);
        assert_eq!(rtp_timestamp(0, elapsed, 90_000), 91_800);
        assert_eq!(
            rtp_timestamp(u32::MAX - 959, Duration::from_millis(20), 48_000),
            0
        );
    }
}
