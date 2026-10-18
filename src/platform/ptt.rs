#[cfg(not(target_os = "windows"))]
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::voice::AudioControl;

pub fn valid_key(key: u16) -> bool {
    matches!(key, 0x20 | 0x30..=0x39 | 0x41..=0x5a | 0x70..=0x87 | 0xa0..=0xa5)
}

#[cfg(test)]
pub fn key_label(key: u16) -> String {
    match key {
        0x20 => "Space".into(),
        0xa0 => "Left Shift".into(),
        0xa1 => "Right Shift".into(),
        0xa2 => "Left Ctrl".into(),
        0xa3 => "Right Ctrl".into(),
        0xa4 => "Left Alt".into(),
        0xa5 => "Right Alt".into(),
        0x70..=0x87 => format!("F{}", key - 0x6f),
        0x30..=0x39 | 0x41..=0x5a => char::from_u32(u32::from(key)).unwrap_or('?').to_string(),
        _ => format!("Key(0x{key:02X})"),
    }
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

pub const MOD_CTRL: u8 = 1 << 0;
pub const MOD_SHIFT: u8 = 1 << 1;
pub const MOD_ALT: u8 = 1 << 2;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Modifiers {
    #[serde(default)]
    pub ctrl: bool,
    #[serde(default)]
    pub shift: bool,
    #[serde(default)]
    pub alt: bool,
}

impl Modifiers {
    #[cfg(test)]
    pub const NONE: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
    };

    pub const CTRL_SHIFT: Self = Self {
        ctrl: true,
        shift: true,
        alt: false,
    };

    pub fn bits(self) -> u8 {
        (if self.ctrl { MOD_CTRL } else { 0 })
            | (if self.shift { MOD_SHIFT } else { 0 })
            | (if self.alt { MOD_ALT } else { 0 })
    }

    pub fn from_bits(bits: u8) -> Self {
        Self {
            ctrl: bits & MOD_CTRL != 0,
            shift: bits & MOD_SHIFT != 0,
            alt: bits & MOD_ALT != 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleMute,
    ToggleDeafen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    pub key: u16,
    #[serde(default)]
    pub modifiers: Modifiers,
    #[serde(default)]
    pub enabled: bool,
}

impl Hotkey {
    pub fn default_mute() -> Self {
        Self {
            key: 0x4d, // 'M'
            modifiers: Modifiers::CTRL_SHIFT,
            enabled: false,
        }
    }

    pub fn default_deafen() -> Self {
        Self {
            key: 0x44, // 'D'
            modifiers: Modifiers::CTRL_SHIFT,
            enabled: false,
        }
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.enabled {
            return Ok(());
        }
        if !valid_key(self.key) {
            return Err(
                "Shortcut key must be a supported key (letters, numbers, function keys, Space).",
            );
        }
        if self.modifiers.bits() == 0 {
            return Err(
                "Global shortcuts require at least one modifier (Ctrl, Shift, or Alt) to avoid hijacking normal typing.",
            );
        }
        if matches!(self.key, 0xa0..=0xa5) {
            return Err("Shortcut key cannot be a standalone modifier key.");
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn format_display(&self) -> String {
        if !self.enabled {
            return "Disabled".to_string();
        }
        let mut parts = Vec::new();
        if self.modifiers.ctrl {
            parts.push("Ctrl");
        }
        if self.modifiers.shift {
            parts.push("Shift");
        }
        if self.modifiers.alt {
            parts.push("Alt");
        }
        let label = key_label(self.key);
        parts.push(&label);
        parts.join(" + ")
    }
}

/// Minimal chord state tracking physical key hold to prevent auto-repeat triggers
/// even when modifiers cycle while the main key is held down.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyState {
    pub key_down: bool,
}

impl HotkeyState {
    pub const fn new() -> Self {
        Self { key_down: false }
    }

    pub fn reset(&mut self) {
        self.key_down = false;
    }

    pub fn key_event(
        &mut self,
        config: &Hotkey,
        canonical: u16,
        is_down: bool,
        current_mods: Modifiers,
    ) -> bool {
        if !config.enabled || canonical != config.key {
            return false;
        }

        if is_down {
            if !self.key_down {
                self.key_down = true;
                config.modifiers == current_mods
            } else {
                false
            }
        } else {
            self.key_down = false;
            false
        }
    }
}

pub fn validate_hotkeys_conflict(
    ptt_key: u16,
    ptt_mode_is_push: bool,
    mute: &Hotkey,
    deafen: &Hotkey,
) -> Result<(), &'static str> {
    mute.validate()?;
    deafen.validate()?;

    if mute.enabled
        && deafen.enabled
        && mute.key == deafen.key
        && mute.modifiers == deafen.modifiers
    {
        return Err("Mute and Deafen shortcuts cannot use the exact same key combination.");
    }

    if ptt_mode_is_push {
        if mute.enabled && mute.key == ptt_key {
            return Err("Mute shortcut key cannot be the same as the push-to-talk key.");
        }
        if deafen.enabled && deafen.key == ptt_key {
            return Err("Deafen shortcut key cannot be the same as the push-to-talk key.");
        }
    }

    Ok(())
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

    pub fn start_with_hotkeys(
        control: Arc<AudioControl>,
        _: u16,
        _: bool,
        _: Hotkey,
        _: Hotkey,
    ) -> Result<Self, String> {
        control.ptt_failed();
        Err("Background push to talk and global shortcuts are only supported on Windows.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hotkey_validation_and_display() {
        let mute = Hotkey::default_mute();
        assert!(!mute.enabled);
        assert!(mute.validate().is_ok());
        assert_eq!(mute.format_display(), "Disabled");

        let mut enabled_mute = mute;
        enabled_mute.enabled = true;
        assert!(enabled_mute.validate().is_ok());
        assert_eq!(enabled_mute.format_display(), "Ctrl + Shift + M");

        let mut invalid_no_mods = enabled_mute;
        invalid_no_mods.modifiers = Modifiers::NONE;
        assert!(invalid_no_mods.validate().is_err());

        let mut invalid_mod_key = enabled_mute;
        invalid_mod_key.key = 0xa2;
        assert!(invalid_mod_key.validate().is_err());
    }

    #[test]
    fn hotkey_conflicts_detected() {
        let mut mute = Hotkey::default_mute();
        mute.enabled = true;
        let mut deafen = Hotkey::default_deafen();
        deafen.enabled = true;

        assert!(validate_hotkeys_conflict(0x77, true, &mute, &deafen).is_ok());

        deafen.key = mute.key;
        deafen.modifiers = mute.modifiers;
        assert!(validate_hotkeys_conflict(0x77, true, &mute, &deafen).is_err());

        let mut ptt_match = Hotkey::default_mute();
        ptt_match.enabled = true;
        ptt_match.key = 0x77;
        let other_deafen = Hotkey::default_deafen();
        assert!(validate_hotkeys_conflict(0x77, true, &ptt_match, &other_deafen).is_err());
    }

    #[test]
    fn hotkey_state_tracks_physical_held_across_modifier_release_and_repress() {
        let mut mute = Hotkey::default_mute();
        mute.enabled = true;
        let mut state = HotkeyState::new();

        let mods_ctrl_shift = Modifiers::CTRL_SHIFT;
        let mods_shift_only = Modifiers {
            ctrl: false,
            shift: true,
            alt: false,
        };

        // 1. Initial press with exact modifiers -> triggers
        assert!(state.key_event(&mute, mute.key, true, mods_ctrl_shift));
        assert!(state.key_down);

        // 2. Hardware auto-repeat while still held -> must not re-trigger
        assert!(!state.key_event(&mute, mute.key, true, mods_ctrl_shift));
        assert!(!state.key_event(&mute, mute.key, true, mods_ctrl_shift));

        // 3. User releases Ctrl modifier while M is still physically held
        assert!(state.key_down);

        // 4. User re-presses Ctrl modifier while M is still physically held
        assert!(state.key_down);

        // 5. Subsequent auto-repeat of M after modifier repress -> MUST NOT TRIGGER
        assert!(!state.key_event(&mute, mute.key, true, mods_ctrl_shift));
        assert!(!state.key_event(&mute, mute.key, true, mods_shift_only));

        // 6. Physical release of M
        assert!(!state.key_event(&mute, mute.key, false, mods_ctrl_shift));
        assert!(!state.key_down);

        // 7. Fresh physical press of M with valid modifiers -> triggers second time
        assert!(state.key_event(&mute, mute.key, true, mods_ctrl_shift));
        assert!(state.key_down);
    }

    #[test]
    fn hotkey_state_mismatch_initial_keydown_does_not_open_on_subsequent_repeat() {
        let mut mute = Hotkey::default_mute();
        mute.enabled = true;
        let mut state = HotkeyState::new();

        // 1. User presses M without modifiers (mismatch initial keydown)
        assert!(!state.key_event(&mute, mute.key, true, Modifiers::NONE));
        assert!(state.key_down);

        // 2. While M is held down, user presses Ctrl+Shift, and M produces auto-repeat
        // Must NOT trigger because the key was already down before the chord matched!
        assert!(!state.key_event(&mute, mute.key, true, Modifiers::CTRL_SHIFT));
        assert!(!state.key_event(&mute, mute.key, true, Modifiers::CTRL_SHIFT));

        // 3. Physical release
        assert!(!state.key_event(&mute, mute.key, false, Modifiers::CTRL_SHIFT));
        assert!(!state.key_down);

        // 4. Fresh press with chord active -> triggers cleanly
        assert!(state.key_event(&mute, mute.key, true, Modifiers::CTRL_SHIFT));
    }

    #[test]
    fn hotkey_state_teardown_and_rearm() {
        let mut mute = Hotkey::default_mute();
        mute.enabled = true;
        let mut state = HotkeyState::new();

        // 1. Trigger once
        assert!(state.key_event(&mute, mute.key, true, Modifiers::CTRL_SHIFT));
        assert!(state.key_down);

        // 2. Teardown / desktop switch resets state
        state.reset();
        assert!(!state.key_down);

        // 3. Rearm where key was already physically held (detected via GetAsyncKeyState)
        state.key_down = true;
        // Repeat event during this held state does not trigger
        assert!(!state.key_event(&mute, mute.key, true, Modifiers::CTRL_SHIFT));

        // 4. Key released
        assert!(!state.key_event(&mute, mute.key, false, Modifiers::CTRL_SHIFT));
        assert!(!state.key_down);

        // 5. Next press after rearm triggers
        assert!(state.key_event(&mute, mute.key, true, Modifiers::CTRL_SHIFT));
    }
}
