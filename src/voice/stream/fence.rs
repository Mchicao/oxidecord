use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub(super) struct AudioFence(AtomicU64);

impl AudioFence {
    pub(super) fn generation(&self) -> u64 {
        self.0.load(Ordering::Acquire) >> 1
    }

    pub(super) fn accepts(&self, generation: u64) -> bool {
        self.0.load(Ordering::Acquire) == (generation << 1) | 1
    }

    pub(super) fn set_open(&self, open: bool) {
        let _ = self
            .0
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state & 1 != u64::from(open))
                    .then_some(((state & !1).wrapping_add(2)) | u64::from(open))
            });
    }

    pub(super) fn invalidate(&self) {
        self.0.fetch_add(2, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_and_reopening_never_accepts_an_old_packet() {
        let fence = AudioFence::default();
        assert!(!fence.accepts(fence.generation()));
        fence.set_open(true);
        let captured = fence.generation();
        assert!(fence.accepts(captured));
        fence.set_open(false);
        assert!(!fence.accepts(captured));
        fence.set_open(true);
        assert!(!fence.accepts(captured));
        assert!(fence.accepts(fence.generation()));
        fence.invalidate();
        assert!(!fence.accepts(captured));
    }

    #[test]
    fn bounded_queued_packets_are_discarded_across_a_gate_epoch() {
        let fence = AudioFence::default();
        let (tx, rx) = std::sync::mpsc::sync_channel(8);
        fence.set_open(true);
        for sample in 0..8 {
            tx.try_send((fence.generation(), sample)).unwrap();
        }
        assert!(tx.try_send((fence.generation(), 99)).is_err());
        fence.set_open(false);
        fence.set_open(true);
        let played: Vec<_> = rx
            .try_iter()
            .filter(|(generation, _)| fence.accepts(*generation))
            .collect();
        assert!(played.is_empty());
        tx.try_send((fence.generation(), 42)).unwrap();
        let played: Vec<_> = rx
            .try_iter()
            .filter(|(generation, _)| fence.accepts(*generation))
            .map(|(_, sample)| sample)
            .collect();
        assert_eq!(played, [42]);
    }
}
