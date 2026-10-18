#[cfg(not(windows))]
use std::sync::Arc;

use crate::voice::AudioControl;

pub fn valid_key(key: u16) -> bool {
    matches!(key, 0x20 | 0x30..=0x39 | 0x41..=0x5a | 0x70..=0x87 | 0xa0..=0xa5)
}

pub fn keys() -> Vec<(u16, String)> {
    [
        (0x20, "Space"),
        (0xa0, "Left Shift"),
        (0xa1, "Right Shift"),
        (0xa2, "Left Ctrl"),
        (0xa3, "Right Ctrl"),
        (0xa4, "Left Alt"),
        (0xa5, "Right Alt"),
    ]
    .into_iter()
    .map(|(key, label)| (key, label.to_owned()))
    .chain((0x70..=0x87).map(|key| (key, format!("F{}", key - 0x6f))))
    .chain((0x30..=0x39).chain(0x41..=0x5a).map(|key| {
        (
            key,
            char::from_u32(u32::from(key)).unwrap_or('?').to_string(),
        )
    }))
    .collect()
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Listener;

#[cfg(not(windows))]
pub struct Listener;

#[cfg(not(windows))]
impl Listener {
    pub fn rearm(&self) {}

    pub fn start(control: Arc<AudioControl>, _: u16) -> Result<Self, String> {
        control.ptt_failed();
        Err("Background push to talk is only supported on Windows.".into())
    }
}
