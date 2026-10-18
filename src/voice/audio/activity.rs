use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const HOLD: Duration = Duration::from_millis(200);

pub(super) struct CaptureActivity {
    started: Instant,
    last_active_ms: AtomicU64,
    epoch: AtomicU64,
}

impl Default for CaptureActivity {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            last_active_ms: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
        }
    }
}

impl CaptureActivity {
    pub(super) fn observe(
        &self,
        power: f64,
        samples: usize,
        now: Instant,
        threshold: f64,
        epoch: u64,
    ) {
        if self.epoch.load(Ordering::Relaxed) != epoch {
            self.clear();
            self.epoch.store(epoch, Ordering::Release);
        }
        // ponytail: energy also detects loud noise; use the shared VAD when voice processing exists.
        if samples > 0
            && power.is_finite()
            && threshold.is_finite()
            && threshold > 0.
            && power / samples as f64 >= threshold
        {
            self.last_active_ms
                .store(self.timestamp(now), Ordering::Relaxed);
        }
    }

    pub(super) fn active(&self, now: Instant, epoch: u64) -> bool {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return false;
        }
        let last = self.last_active_ms.load(Ordering::Relaxed);
        self.epoch.load(Ordering::Acquire) == epoch
            && last != 0
            && self.timestamp(now).saturating_sub(last) < HOLD.as_millis() as u64
    }

    pub(super) fn clear(&self) {
        self.last_active_ms.store(0, Ordering::Relaxed);
    }

    fn timestamp(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.started).as_millis() as u64 + 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_noise_and_invalid_samples_do_not_activate_or_extend_the_hold() {
        let activity = CaptureActivity::default();
        let start = activity.started;
        assert!(!activity.active(start, 0));
        for (power, samples) in [(0., 960), (0.000001 * 960., 960), (f64::NAN, 960), (1., 0)] {
            activity.observe(power, samples, start, 0.0001, 0);
            assert!(!activity.active(start, 0));
        }
        activity.observe(0.01 * 960., 960, start, 0.0001, 0);
        assert!(activity.active(start, 0));
        let quiet = start + Duration::from_millis(199);
        activity.observe(0., 960, quiet, 0.0001, 0);
        assert!(activity.active(quiet, 0));
        assert!(!activity.active(start + HOLD, 0));
        activity.observe(0.01 * 960., 960, start + HOLD, 0.0001, 0);
        assert!(activity.active(start + HOLD, 0));
        activity.clear();
        assert!(!activity.active(start + HOLD, 0));
    }

    #[test]
    fn manual_sensitivity_and_new_epochs_reset_the_hold() {
        let activity = CaptureActivity::default();
        let start = activity.started;
        activity.observe(0.0001 * 960., 960, start, 0.001, 1);
        assert!(!activity.active(start, 1));
        activity.observe(0.0001 * 960., 960, start, 0.00001, 2);
        assert!(activity.active(start, 2));
        assert!(!activity.active(start, 3));
        activity.observe(0., 960, start, 0.00001, 3);
        assert!(!activity.active(start, 3));
        activity.observe(f64::INFINITY, 960, start, 0.00001, 3);
        assert!(!activity.active(start, 3));
        activity.observe(1., 960, start, 0.00001, 3);
        assert!(activity.active(start + Duration::from_millis(199), 3));
        assert!(!activity.active(start + HOLD, 3));
    }
}
