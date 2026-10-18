use std::time::{Duration, Instant};

const KEYFRAME_RETRY: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct KeyframeRequests {
    last: Option<(u32, Instant)>,
}

impl KeyframeRequests {
    pub(super) fn request(
        &mut self,
        ssrc: Option<u32>,
        needed: bool,
        now: Instant,
        send: impl FnOnce(u32),
    ) {
        let Some(ssrc) = ssrc.filter(|&ssrc| ssrc != 0 && needed) else {
            return;
        };
        if self.last.is_some_and(|(previous, at)| {
            previous == ssrc && now.saturating_duration_since(at) < KEYFRAME_RETRY
        }) {
            return;
        }
        self.last = Some((ssrc, now));
        send(ssrc);
    }
}

#[derive(Default)]
pub(super) struct CaptureReadiness {
    ready: bool,
}

impl CaptureReadiness {
    pub(super) fn update(&mut self, ready: bool, request_capture: impl FnOnce()) {
        let previous = std::mem::replace(&mut self.ready, ready);
        if ready && !previous {
            request_capture();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_handshake_requests_a_keyframe_without_receiving_any_rtp() {
        let now = Instant::now();
        let mut requests = KeyframeRequests::default();
        let mut sent = Vec::new();
        requests.request(Some(42), true, now, |ssrc| sent.push(ssrc));
        assert_eq!(sent, [42]);
        requests.request(Some(42), true, now + Duration::from_millis(499), |ssrc| {
            sent.push(ssrc)
        });
        assert_eq!(sent, [42]);
        requests.request(Some(42), true, now + KEYFRAME_RETRY, |ssrc| sent.push(ssrc));
        assert_eq!(sent, [42, 42]);
        requests.request(Some(42), false, now + Duration::from_secs(2), |ssrc| {
            sent.push(ssrc)
        });
        assert_eq!(sent, [42, 42]);
    }

    #[test]
    fn late_video_metadata_and_replaced_ssrc_do_not_wait_for_rtp_or_retry_budget() {
        let now = Instant::now();
        let mut requests = KeyframeRequests::default();
        let mut sent = Vec::new();
        requests.request(None, true, now, |ssrc| sent.push(ssrc));
        requests.request(Some(0), true, now, |ssrc| sent.push(ssrc));
        assert!(sent.is_empty());
        requests.request(Some(7), true, now, |ssrc| sent.push(ssrc));
        requests.request(Some(8), true, now, |ssrc| sent.push(ssrc));
        assert_eq!(sent, [7, 8]);
    }

    #[test]
    fn dave_ready_transition_wakes_static_capture_without_a_frame_event() {
        let mut readiness = CaptureReadiness::default();
        let mut wakes = 0;
        readiness.update(false, || wakes += 1);
        readiness.update(false, || wakes += 1);
        assert_eq!(wakes, 0);
        readiness.update(true, || wakes += 1);
        assert_eq!(wakes, 1);
        readiness.update(true, || wakes += 1);
        assert_eq!(wakes, 1);
        readiness.update(false, || wakes += 1);
        readiness.update(true, || wakes += 1);
        assert_eq!(wakes, 2);
    }
}
