//! Stub notification backend for non-Windows platforms.

use futures::channel::mpsc::UnboundedSender;

use super::{NotificationTarget, ToastPayload};

pub(super) struct PlatformNotifier;

impl PlatformNotifier {
    pub fn new() -> Self {
        Self
    }

    pub fn is_supported(&self) -> bool {
        false
    }

    pub fn ensure_opt_in_ready(&mut self) -> Result<(), String> {
        Err("Desktop notifications aren't supported on this platform yet.".into())
    }

    pub fn show(
        &mut self,
        _payload: &ToastPayload,
        _click_tx: UnboundedSender<NotificationTarget>,
    ) -> Result<(), String> {
        Err("Desktop notifications aren't supported on this platform yet.".into())
    }

    pub fn clear(&mut self) {}
}
