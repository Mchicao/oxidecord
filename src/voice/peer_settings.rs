use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

pub const DEFAULT_PEER_VOLUME: f32 = 1.0;
pub const MIN_PEER_VOLUME: f32 = 0.0;
pub const MAX_PEER_VOLUME: f32 = 2.0;

/// Clear rule for PeerSetting deserialization:
/// - Malformed JSON (syntax errors, wrong types) is blocked and returns a deserialization error.
/// - Out-of-range finite floats (> 2.0 or < 0.0) are clamped into [MIN_PEER_VOLUME, MAX_PEER_VOLUME] (0.0..=2.0).
/// - Non-finite floats (NaN, +inf, -inf) or omitted fields fall back safely to DEFAULT_PEER_VOLUME (1.0).
/// - Atomic target_gain in PeerAudioRegistry is guaranteed finite and clamped; NaN is never stored in atomics.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(from = "StoredPeerSetting")]
pub struct PeerSetting {
    pub volume: f32,
    pub muted: bool,
}

#[derive(Deserialize)]
#[serde(default)]
struct StoredPeerSetting {
    volume: f32,
    muted: bool,
}

impl Default for StoredPeerSetting {
    fn default() -> Self {
        Self {
            volume: DEFAULT_PEER_VOLUME,
            muted: false,
        }
    }
}

impl From<StoredPeerSetting> for PeerSetting {
    fn from(stored: StoredPeerSetting) -> Self {
        let volume = if stored.volume.is_finite() {
            stored.volume.clamp(MIN_PEER_VOLUME, MAX_PEER_VOLUME)
        } else {
            DEFAULT_PEER_VOLUME
        };
        Self {
            volume,
            muted: stored.muted,
        }
    }
}

impl Default for PeerSetting {
    fn default() -> Self {
        Self {
            volume: DEFAULT_PEER_VOLUME,
            muted: false,
        }
    }
}

impl PeerSetting {
    #[cfg(test)]
    pub fn new(volume: f32, muted: bool) -> Result<Self, &'static str> {
        let value = Self { volume, muted };
        value.validate()?;
        Ok(value)
    }

    #[cfg(test)]
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.volume.is_finite() || !(MIN_PEER_VOLUME..=MAX_PEER_VOLUME).contains(&self.volume) {
            return Err("Peer volume must be a finite number between 0% and 200%.");
        }
        Ok(())
    }

    #[inline]
    #[cfg(test)]
    pub fn effective_gain(&self) -> f32 {
        if self.muted { 0.0 } else { self.volume }
    }

    pub fn volume_percent(&self) -> u32 {
        (self.volume * 100.0).round().clamp(0.0, 200.0) as u32
    }

    #[cfg(test)]
    pub fn from_percent(percent: u32, muted: bool) -> Result<Self, &'static str> {
        if percent > 200 {
            return Err("Peer volume percent must be between 0 and 200.");
        }
        Self::new(percent as f32 / 100.0, muted)
    }
}

#[derive(Debug, Clone)]
pub struct AtomicPeerControl {
    pub target_gain: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
}

impl Default for AtomicPeerControl {
    fn default() -> Self {
        Self {
            target_gain: Arc::new(AtomicU32::new(DEFAULT_PEER_VOLUME.to_bits())),
            muted: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[derive(Default, Clone)]
pub struct PeerAudioRegistry {
    peers: Arc<Mutex<HashMap<u64, AtomicPeerControl>>>,
}

impl PeerAudioRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_create(&self, user_id: u64) -> AtomicPeerControl {
        let mut map = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.entry(user_id).or_default().clone()
    }

    pub fn set_volume(&self, user_id: u64, volume: f32) -> Result<(), &'static str> {
        if !volume.is_finite() || !(MIN_PEER_VOLUME..=MAX_PEER_VOLUME).contains(&volume) {
            return Err("Peer volume must be a finite number between 0% and 200%.");
        }
        let control = self.get_or_create(user_id);
        control
            .target_gain
            .store(volume.to_bits(), Ordering::Release);
        Ok(())
    }

    pub fn set_muted(&self, user_id: u64, muted: bool) {
        let control = self.get_or_create(user_id);
        control.muted.store(muted, Ordering::Release);
    }

    pub fn get_setting(&self, user_id: u64) -> PeerSetting {
        let map = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(control) = map.get(&user_id) {
            PeerSetting {
                volume: f32::from_bits(control.target_gain.load(Ordering::Acquire)),
                muted: control.muted.load(Ordering::Acquire),
            }
        } else {
            PeerSetting::default()
        }
    }

    #[cfg(test)]
    pub fn dump_settings(&self) -> HashMap<u64, PeerSetting> {
        let map = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.iter()
            .map(|(&user_id, control)| {
                (
                    user_id,
                    PeerSetting {
                        volume: f32::from_bits(control.target_gain.load(Ordering::Acquire)),
                        muted: control.muted.load(Ordering::Acquire),
                    },
                )
            })
            .collect()
    }

    pub fn load_settings(&self, settings: &HashMap<u64, PeerSetting>) {
        let mut map = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (&user_id, setting) in settings {
            let gain = if setting.volume.is_finite() {
                setting.volume.clamp(MIN_PEER_VOLUME, MAX_PEER_VOLUME)
            } else {
                DEFAULT_PEER_VOLUME
            };
            let control = map.entry(user_id).or_default();
            control.target_gain.store(gain.to_bits(), Ordering::Release);
            control.muted.store(setting.muted, Ordering::Release);
        }
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ScopedPeerAudioPrefs {
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub accounts: HashMap<u64, HashMap<u64, PeerSetting>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_setting_validation_and_clamping() {
        assert!(PeerSetting::new(1.0, false).is_ok());
        assert!(PeerSetting::new(0.0, true).is_ok());
        assert!(PeerSetting::new(2.0, false).is_ok());

        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.01, 2.01] {
            assert!(PeerSetting::new(invalid, false).is_err());
        }

        assert_eq!(PeerSetting::from_percent(100, false).unwrap().volume, 1.0);
        assert_eq!(PeerSetting::from_percent(200, false).unwrap().volume, 2.0);
        assert_eq!(PeerSetting::from_percent(0, false).unwrap().volume, 0.0);
        assert!(PeerSetting::from_percent(201, false).is_err());
    }

    #[test]
    fn peer_setting_json_deserialization_clamps_range_and_rejects_malformed() {
        // Valid round-trip
        let valid: PeerSetting = serde_json::from_str(r#"{"volume":1.5,"muted":false}"#).unwrap();
        assert_eq!(valid.volume, 1.5);
        assert!(!valid.muted);

        // Clamping rule: > 2.0 clamped to 2.0
        let over: PeerSetting = serde_json::from_str(r#"{"volume":2.1,"muted":false}"#).unwrap();
        assert_eq!(over.volume, 2.0);

        let way_over: PeerSetting =
            serde_json::from_str(r#"{"volume":1e20,"muted":false}"#).unwrap();
        assert_eq!(way_over.volume, 2.0);

        // Clamping rule: < 0.0 clamped to 0.0
        let under: PeerSetting = serde_json::from_str(r#"{"volume":-0.5,"muted":true}"#).unwrap();
        assert_eq!(under.volume, 0.0);
        assert!(under.muted);

        // Omitted fields use defaults
        let empty: PeerSetting = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(empty.volume, DEFAULT_PEER_VOLUME);
        assert!(!empty.muted);

        // Malformed JSON (syntax error, wrong type) is blocked
        assert!(serde_json::from_str::<PeerSetting>(r#"{"volume":"not_a_number"}"#).is_err());
        assert!(serde_json::from_str::<PeerSetting>(r#"{"volume":[1.0]}"#).is_err());
        assert!(serde_json::from_str::<PeerSetting>(r#"{"volume": 1.0"#).is_err());
        assert!(serde_json::from_str::<PeerSetting>(r#"123"#).is_err());
    }

    #[test]
    fn registry_load_settings_clamps_and_prevents_nan_in_atomics() {
        let registry = PeerAudioRegistry::new();
        let user = 42u64;

        let mut map = HashMap::new();
        map.insert(
            user,
            PeerSetting {
                volume: 5.0,
                muted: false,
            },
        );
        registry.load_settings(&map);
        assert_eq!(registry.get_setting(user).volume, 2.0);

        map.insert(
            user,
            PeerSetting {
                volume: -1.0,
                muted: true,
            },
        );
        registry.load_settings(&map);
        assert_eq!(registry.get_setting(user).volume, 0.0);
        assert!(registry.get_setting(user).muted);

        map.insert(
            user,
            PeerSetting {
                volume: f32::NAN,
                muted: false,
            },
        );
        registry.load_settings(&map);
        assert_eq!(registry.get_setting(user).volume, 1.0);
        let control = registry.get_or_create(user);
        assert!(!f32::from_bits(control.target_gain.load(Ordering::Relaxed)).is_nan());
    }

    #[test]
    fn default_peer_setting_is_unity_and_unmuted() {
        let default = PeerSetting::default();
        assert_eq!(default.volume, 1.0);
        assert!(!default.muted);
        assert_eq!(default.effective_gain(), 1.0);
    }

    #[test]
    fn atomic_registry_thread_safe_updates_and_isolation() {
        let registry = PeerAudioRegistry::new();
        let user_a = 123456789012345678u64;
        let user_b = 987654321098765432u64;

        assert_eq!(registry.get_setting(user_a), PeerSetting::default());
        assert_eq!(registry.get_setting(user_b), PeerSetting::default());

        registry.set_volume(user_a, 1.5).unwrap();
        registry.set_muted(user_a, false);

        registry.set_volume(user_b, 0.5).unwrap();
        registry.set_muted(user_b, true);

        let snap_a = registry.get_setting(user_a);
        assert_eq!(snap_a.volume, 1.5);
        assert!(!snap_a.muted);
        assert_eq!(snap_a.effective_gain(), 1.5);

        let snap_b = registry.get_setting(user_b);
        assert_eq!(snap_b.volume, 0.5);
        assert!(snap_b.muted);
        assert_eq!(snap_b.effective_gain(), 0.0);

        registry.set_volume(user_a, 0.8).unwrap();
        assert_eq!(registry.get_setting(user_a).volume, 0.8);
        assert_eq!(registry.get_setting(user_b).volume, 0.5);

        let dumped = registry.dump_settings();
        let new_registry = PeerAudioRegistry::new();
        new_registry.load_settings(&dumped);
        assert_eq!(
            new_registry.get_setting(user_a),
            registry.get_setting(user_a)
        );
        assert_eq!(
            new_registry.get_setting(user_b),
            registry.get_setting(user_b)
        );
    }
}
