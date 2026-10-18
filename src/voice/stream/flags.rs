#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoDescription {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallMediaFlags {
    pub microphone: bool,
    pub speaking: bool,
    pub deafened: bool,
    pub video: Option<VideoDescription>,
}

impl CallMediaFlags {
    pub(super) fn sending(self) -> bool {
        self.microphone && self.speaking && !self.deafened
    }

    pub(super) fn speaking_change(self, previous: Self) -> Option<bool> {
        (self.sending() != previous.sending()).then_some(self.sending())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_on_announces_activity_not_permission_and_coalesces_unchanged_frames() {
        let quiet = CallMediaFlags {
            microphone: true,
            ..CallMediaFlags::default()
        };
        let active = CallMediaFlags {
            speaking: true,
            ..quiet
        };
        let muted = CallMediaFlags {
            microphone: false,
            ..active
        };
        let deafened = CallMediaFlags {
            deafened: true,
            ..active
        };
        assert_eq!(quiet.speaking_change(CallMediaFlags::default()), None);
        assert!(!quiet.sending());
        assert!(!muted.sending());
        assert!(!deafened.sending());
        let mut previous = quiet;
        let mut announcements = Vec::new();
        for next in std::iter::repeat_n(active, 50).chain([quiet, muted, quiet, active, deafened]) {
            if let Some(speaking) = next.speaking_change(previous) {
                announcements.push(speaking);
            }
            previous = next;
        }
        assert_eq!(announcements, [true, false, true, false]);
    }
}
