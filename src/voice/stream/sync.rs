use std::collections::HashMap;
use std::time::{Duration, Instant};

pub(super) struct SenderReport {
    pub ssrc: u32,
    pub timestamp: u32,
    pub ntp: u64,
}

struct Mapping {
    rtp: u32,
    ntp: u64,
    rate: u32,
}
impl Mapping {
    fn time(&self, timestamp: u32) -> i128 {
        let ntp_ns = ((u128::from(self.ntp) * 1_000_000_000) >> 32) as i128;
        ntp_ns
            + i128::from(timestamp.wrapping_sub(self.rtp) as i32) * 1_000_000_000
                / i128::from(self.rate)
    }
}

#[derive(Default)]
pub(super) struct Timeline {
    mappings: HashMap<u32, Mapping>,
    audio: Option<(i128, Instant)>,
}

impl Timeline {
    pub(super) fn report(&mut self, report: SenderReport, rate: u32) {
        if self.mappings.len() >= 4 && !self.mappings.contains_key(&report.ssrc) {
            self.mappings.clear();
            self.audio = None;
        }
        self.mappings.insert(
            report.ssrc,
            Mapping {
                rtp: report.timestamp,
                ntp: report.ntp,
                rate,
            },
        );
    }

    pub(super) fn audio(&mut self, ssrc: u32, timestamp: u32, received: Instant) {
        if let Some(mapping) = self.mappings.get(&ssrc) {
            self.audio = Some((
                mapping.time(timestamp),
                received + Duration::from_millis(60),
            ));
        }
    }

    pub(super) fn video(&self, ssrc: u32, timestamp: u32, now: Instant) -> Instant {
        let Some((audio_time, audio_at)) = self.audio else {
            return now;
        };
        let Some(mapping) = self.mappings.get(&ssrc) else {
            return now;
        };
        let delta = mapping.time(timestamp) - audio_time;
        let deadline = if delta >= 0 {
            audio_at.checked_add(Duration::from_nanos(delta.min(500_000_000) as u64))
        } else {
            audio_at.checked_sub(Duration::from_nanos((-delta).min(500_000_000) as u64))
        };
        deadline
            .unwrap_or(now)
            .clamp(now, now + Duration::from_millis(200))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn independent_rtp_clocks_align_against_the_same_ntp_epoch_across_wrap() {
        let now = Instant::now();
        let ntp = 100u64 << 32;
        let mut timeline = Timeline::default();
        timeline.report(
            SenderReport {
                ssrc: 1,
                timestamp: u32::MAX - 47_999,
                ntp,
            },
            48_000,
        );
        timeline.report(
            SenderReport {
                ssrc: 2,
                timestamp: 700,
                ntp,
            },
            90_000,
        );
        timeline.audio(1, 0, now);
        assert_eq!(
            timeline.video(2, 90_700, now),
            now + Duration::from_millis(60)
        );
        assert_eq!(
            timeline.video(2, 92_500, now),
            now + Duration::from_millis(80)
        );
    }

    #[test]
    fn absent_and_hostile_reports_never_create_unbounded_waits() {
        let now = Instant::now();
        let mut timeline = Timeline::default();
        assert_eq!(timeline.video(9, 4, now), now);
        timeline.report(
            SenderReport {
                ssrc: 1,
                timestamp: 0,
                ntp: 0,
            },
            48_000,
        );
        timeline.report(
            SenderReport {
                ssrc: 2,
                timestamp: 0,
                ntp: u64::MAX,
            },
            90_000,
        );
        timeline.audio(1, 0, now);
        assert!(timeline.video(2, 0, now) <= now + Duration::from_millis(200));
    }
}
