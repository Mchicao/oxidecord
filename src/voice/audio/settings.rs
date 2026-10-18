use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{Deserialize, Serialize};

use super::processing::{Options, Status};
use crate::platform::ptt::{self, Hotkey, HotkeyAction};
use crate::voice::peer_settings::{PeerAudioRegistry, PeerSetting};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u64)]
pub enum InputMode {
    #[default]
    AlwaysOn,
    VoiceActivity,
    PushToTalk,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "StoredPreferences")]
pub struct AudioPreferences {
    pub processing: crate::voice::ProcessingOptions,
    pub input_gain: f32,
    pub output_gain: f32,
    pub mode: InputMode,
    pub threshold_db: f32,
    pub ptt_key: u16,
    pub mute_hotkey: Hotkey,
    pub deafen_hotkey: Hotkey,
}

impl Default for AudioPreferences {
    fn default() -> Self {
        Self {
            processing: Options::default(),
            input_gain: 1.,
            output_gain: 1.,
            mode: InputMode::AlwaysOn,
            threshold_db: -40.,
            ptt_key: 0x77,
            mute_hotkey: Hotkey::default_mute(),
            deafen_hotkey: Hotkey::default_deafen(),
        }
    }
}

#[derive(Deserialize)]
#[serde(default)]
struct StoredPreferences {
    processing: Options,
    input_gain: f32,
    output_gain: f32,
    mode: InputMode,
    threshold_db: f32,
    ptt_key: u16,
    mute_hotkey: Hotkey,
    deafen_hotkey: Hotkey,
}

impl Default for StoredPreferences {
    fn default() -> Self {
        let value = AudioPreferences::default();
        Self {
            processing: value.processing,
            input_gain: value.input_gain,
            output_gain: value.output_gain,
            mode: value.mode,
            threshold_db: value.threshold_db,
            ptt_key: value.ptt_key,
            mute_hotkey: value.mute_hotkey,
            deafen_hotkey: value.deafen_hotkey,
        }
    }
}

impl TryFrom<StoredPreferences> for AudioPreferences {
    type Error = &'static str;

    fn try_from(value: StoredPreferences) -> Result<Self, Self::Error> {
        let value = Self {
            processing: value.processing,
            input_gain: value.input_gain,
            output_gain: value.output_gain,
            mode: value.mode,
            threshold_db: value.threshold_db,
            ptt_key: value.ptt_key,
            mute_hotkey: value.mute_hotkey,
            deafen_hotkey: value.deafen_hotkey,
        };
        value.validate()?;
        Ok(value)
    }
}

impl AudioPreferences {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.input_gain.is_finite() || !(0. ..=2.).contains(&self.input_gain) {
            return Err("Input gain must be between 0% and 200%.");
        }
        if !self.output_gain.is_finite() || !(0. ..=2.).contains(&self.output_gain) {
            return Err("Output volume must be between 0% and 200%.");
        }
        if !self.threshold_db.is_finite() || !(-80. ..=0.).contains(&self.threshold_db) {
            return Err("Voice activity threshold must be between -80 and 0 dBFS.");
        }
        if !ptt::valid_key(self.ptt_key) {
            return Err("Choose a supported push-to-talk key.");
        }
        ptt::validate_hotkeys_conflict(
            self.ptt_key,
            self.mode == InputMode::PushToTalk,
            &self.mute_hotkey,
            &self.deafen_hotkey,
        )?;
        Ok(())
    }
}

const MODE: u64 = 3;
const READY: u64 = 1 << 2;
const HELD: u64 = 1 << 3;
const DOWN: u64 = 1 << 4;
const ARMED: u64 = 1 << 5;
const MUTED: u64 = 1 << 6;
const DEAFENED: u64 = 1 << 7;
const SERVER_RESTRICTED: u64 = 1 << 8;
const KEY_SHIFT: u32 = 9;
const KEY_MASK: u64 = 0xff << KEY_SHIFT;
const GENERATION: u64 = 1 << 17;
const FLAGS: u64 = GENERATION - 1;

type HotkeyHook = Arc<dyn Fn(HotkeyAction) + Send + Sync>;

pub(crate) struct AudioControl {
    events: Option<futures::channel::mpsc::UnboundedSender<crate::voice::VoiceEvent>>,
    commands: Option<futures::channel::mpsc::UnboundedSender<crate::voice::Command>>,
    transmitting: AtomicU64,
    pub(super) state: AtomicU64,
    pub(super) session: AtomicU64,
    pub(super) input_gain: AtomicU32,
    pub(super) output_gain: AtomicU32,
    threshold: AtomicU32,
    pub(super) processing_bits: AtomicU8,
    processing_status: AtomicU8,
    processing_epoch: AtomicU64,
    processing_error: Mutex<Option<&'static str>>,
    pub(super) processing_owner: AtomicU64,
    pub(super) active_processing: Mutex<Weak<super::processing::Processing>>,
    pub(crate) peer_settings: Arc<PeerAudioRegistry>,
    pub(crate) hotkey_hook: Mutex<Option<HotkeyHook>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct CaptureState {
    state: u64,
    session: u64,
}

impl CaptureState {
    pub(super) fn activity_threshold(self, manual: f64) -> f64 {
        if self.state & MODE == InputMode::VoiceActivity as u64 {
            manual
        } else {
            0.0001
        }
    }
    pub(super) fn epoch(self) -> u64 {
        self.state / GENERATION
    }

    pub(super) fn open(self, active: bool, session: u64) -> bool {
        self.session == session | 1
            && self.state & (MUTED | DEAFENED | SERVER_RESTRICTED) == 0
            && match self.state & MODE {
                0 => true,
                1 => active,
                _ => self.state & (READY | HELD) == READY | HELD,
            }
    }
}

impl AudioControl {
    pub(super) fn new(preferences: &AudioPreferences) -> Self {
        Self {
            events: None,
            commands: None,
            transmitting: AtomicU64::new(0),
            state: AtomicU64::new(
                preferences.mode as u64 | (u64::from(preferences.ptt_key) << KEY_SHIFT) | ARMED,
            ),
            session: AtomicU64::new(0),
            input_gain: AtomicU32::new(preferences.input_gain.to_bits()),
            output_gain: AtomicU32::new(preferences.output_gain.to_bits()),
            threshold: AtomicU32::new(10f32.powf(preferences.threshold_db / 10.).to_bits()),
            processing_bits: AtomicU8::new(preferences.processing.bits()),
            processing_status: AtomicU8::new(if preferences.processing.bits() == 0 {
                Status::Bypass
            } else {
                Status::Starting
            } as u8),
            processing_epoch: AtomicU64::new(u64::MAX),
            processing_error: Mutex::new(None),
            processing_owner: AtomicU64::new(0),
            active_processing: Mutex::new(Weak::new()),
            peer_settings: Arc::new(PeerAudioRegistry::new()),
            hotkey_hook: Mutex::new(None),
        }
    }

    fn change(&self, update: impl Fn(u64) -> u64) {
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                let flags = update(state & FLAGS) & FLAGS;
                (flags != state & FLAGS)
                    .then_some((state & !FLAGS).wrapping_add(GENERATION) | flags)
            });
        let current = self.snapshot();
        if !current.open(true, current.session & !1) {
            self.transmission(false, current.session & !1);
        }
    }

    pub(super) fn transmission(&self, open: bool, session: u64) {
        let current = self.snapshot();
        if current.session != session | 1 {
            return;
        }
        let value = (current.epoch() << 1) | u64::from(open && current.open(true, session));
        if self.transmitting.load(Ordering::Acquire) != value
            && self.transmitting.swap(value, Ordering::AcqRel) != value
            && let Some(commands) = &self.commands
        {
            let _ = commands.unbounded_send(crate::voice::Command::Gate);
        }
    }

    pub(super) fn snapshot(&self) -> CaptureState {
        CaptureState {
            state: self.state.load(Ordering::Acquire),
            session: self.session.load(Ordering::Acquire),
        }
    }

    pub(super) fn threshold(&self) -> f64 {
        f64::from(f32::from_bits(self.threshold.load(Ordering::Relaxed)))
    }

    pub(super) fn processing_ready(&self, epoch: u64) -> bool {
        self.processing_bits.load(Ordering::Acquire) == 0
            || (self.processing_status.load(Ordering::Acquire) == Status::Active as u8
                && self.processing_epoch.load(Ordering::Acquire) == epoch)
    }

    pub(super) fn processing_state(
        &self,
        status: Status,
        error: Option<&'static str>,
        session: u64,
        epoch: u64,
        owner: u64,
    ) {
        let current = self.snapshot();
        if self.processing_owner.load(Ordering::Acquire) != owner
            || current.session & !1 != session
            || current.epoch() != epoch
            || self.processing_bits.load(Ordering::Acquire) == 0
        {
            return;
        }
        if status == Status::Active {
            self.processing_epoch.store(epoch, Ordering::Release);
        }
        if self.processing_status.swap(status as u8, Ordering::AcqRel) != status as u8 {
            if let Ok(mut current) = self.processing_error.lock() {
                *current = error;
            }
            if status != Status::Active {
                self.transmission(false, session);
            }
            self.notify();
        }
    }

    pub(in crate::voice) fn invalidate_capture(&self) {
        self.state.fetch_add(GENERATION, Ordering::AcqRel);
        self.processing_epoch.store(u64::MAX, Ordering::Release);
        self.transmission(false, self.session.load(Ordering::Acquire) & !1);
    }

    pub(crate) fn ptt_ready(&self, down: bool) {
        self.change(|state| {
            (state & !(HELD | DOWN | ARMED)) | READY | if down { DOWN } else { ARMED }
        });
        self.notify();
    }

    pub(crate) fn ptt_failed(&self) {
        self.change(|state| state & !(READY | HELD | ARMED));
        self.notify();
    }

    fn notify(&self) {
        if let Some(events) = &self.events {
            let _ = events.unbounded_send(crate::voice::VoiceEvent::AudioSettingsChanged);
        }
    }

    pub(crate) fn trigger_hotkey(&self, action: HotkeyAction) {
        let hook = self.hotkey_hook.lock().ok().and_then(|guard| guard.clone());
        if let Some(hook) = hook {
            hook(action);
            return;
        }
        self.notify();
    }

    pub fn set_hotkey_hook(&self, hook: impl Fn(HotkeyAction) + Send + Sync + 'static) {
        if let Ok(mut guard) = self.hotkey_hook.lock() {
            *guard = Some(Arc::new(hook));
        }
    }

    pub(crate) fn key_event(&self, key: u16, down: bool) {
        self.change(|state| {
            if (state & KEY_MASK) >> KEY_SHIFT != u64::from(key) {
                return state;
            }
            if !down {
                (state & !(HELD | DOWN)) | ARMED
            } else if state & DOWN != 0 {
                state
            } else {
                state
                    | DOWN
                    | if state & (READY | ARMED) == READY | ARMED {
                        HELD
                    } else {
                        0
                    }
            }
        });
    }

    pub(crate) fn release_ptt(&self) {
        self.change(|state| (state & !(HELD | ARMED)) | if state & DOWN == 0 { ARMED } else { 0 });
    }

    #[cfg(test)]
    pub(crate) fn set_muted(&self, value: bool) {
        self.set_flag(MUTED, value);
    }

    #[cfg(test)]
    pub(crate) fn set_deafened(&self, value: bool) {
        self.set_flag(DEAFENED, value);
    }

    pub(crate) fn set_server_restricted(&self, value: bool) {
        self.set_flag(SERVER_RESTRICTED, value);
    }

    pub(crate) fn set_voice_flags(&self, muted: bool, deafened: bool, restricted: bool) {
        self.change(|state| {
            (state & !(MUTED | DEAFENED | SERVER_RESTRICTED))
                | if muted { MUTED } else { 0 }
                | if deafened { DEAFENED } else { 0 }
                | if restricted { SERVER_RESTRICTED } else { 0 }
        });
    }

    fn set_flag(&self, flag: u64, value: bool) {
        self.change(|state| if value { state | flag } else { state & !flag });
    }

    pub(super) fn configure(&self, preferences: &AudioPreferences, rebind: bool) {
        self.invalidate_capture();
        let processing_changed = self
            .processing_bits
            .swap(preferences.processing.bits(), Ordering::AcqRel)
            != preferences.processing.bits();
        if processing_changed {
            self.processing_epoch.store(u64::MAX, Ordering::Release);
            self.processing_status.store(
                if preferences.processing.bits() == 0 {
                    Status::Bypass
                } else {
                    Status::Starting
                } as u8,
                Ordering::Release,
            );
            if let Ok(mut error) = self.processing_error.lock() {
                *error = None;
            }
            if preferences.processing.bits() == 0 {
                let active = self
                    .active_processing
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .upgrade();
                if let Some(active) = active {
                    active.stop();
                }
            }
        }
        if rebind {
            self.ptt_failed();
        }
        self.input_gain
            .store(preferences.input_gain.to_bits(), Ordering::Relaxed);
        self.output_gain
            .store(preferences.output_gain.to_bits(), Ordering::Relaxed);
        self.threshold.store(
            10f32.powf(preferences.threshold_db / 10.).to_bits(),
            Ordering::Relaxed,
        );
        self.change(|state| {
            (state & !(MODE | KEY_MASK))
                | preferences.mode as u64
                | (u64::from(preferences.ptt_key) << KEY_SHIFT)
        });
        self.state.fetch_add(GENERATION, Ordering::AcqRel);
        if preferences.input_gain == 0. {
            self.transmission(false, self.session.load(Ordering::Acquire) & !1);
        }
        if processing_changed && let Some(commands) = &self.commands {
            let _ = commands.unbounded_send(crate::voice::Command::Processing);
        }
    }

    pub(crate) fn begin_session(&self) -> u64 {
        self.transmission(false, self.session.load(Ordering::Acquire) & !1);
        self.release_ptt();
        self.state.fetch_add(GENERATION, Ordering::AcqRel);
        let previous = self
            .session
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some((current & !1).wrapping_add(2))
            })
            .unwrap_or_else(|current| current);
        (previous & !1).wrapping_add(2)
    }

    pub(crate) fn activate_session(&self, session: u64) {
        let _ = self.session.compare_exchange(
            session,
            session | 1,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    pub(crate) fn end_session(&self, session: u64) {
        if self
            .session
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current & !1 == session).then_some((current & !1).wrapping_add(2))
            })
            .is_ok()
        {
            self.release_ptt();
        }
    }
}

struct Configuration {
    enabled: bool,
    preferences: AudioPreferences,
    listener: Option<ptt::Listener>,
    error: Option<String>,
}

#[derive(Clone)]
pub struct AudioSettings {
    pub(crate) control: Arc<AudioControl>,
    configuration: Arc<Mutex<Configuration>>,
}

impl AudioSettings {
    pub fn offline(preferences: AudioPreferences) -> Self {
        Self::with_events(preferences, None, None)
    }

    pub(in crate::voice) fn with_events(
        preferences: AudioPreferences,
        events: Option<futures::channel::mpsc::UnboundedSender<crate::voice::VoiceEvent>>,
        commands: Option<futures::channel::mpsc::UnboundedSender<crate::voice::Command>>,
    ) -> Self {
        let preferences = if preferences.validate().is_ok() {
            preferences
        } else {
            AudioPreferences::default()
        };
        let mut control = AudioControl::new(&preferences);
        let enabled = events.is_some();
        control.events = events;
        control.commands = commands;
        let this = Self {
            control: Arc::new(control),
            configuration: Arc::new(Mutex::new(Configuration {
                enabled,
                preferences,
                listener: None,
                error: None,
            })),
        };
        this.configure_ptt();
        this
    }

    pub fn preferences(&self) -> AudioPreferences {
        self.configuration
            .lock()
            .map(|value| value.preferences.clone())
            .unwrap_or_default()
    }

    pub fn set_peer_volume(&self, user_id: u64, volume: f32) -> Result<(), &'static str> {
        self.control.peer_settings.set_volume(user_id, volume)
    }

    pub fn set_peer_muted(&self, user_id: u64, muted: bool) {
        self.control.peer_settings.set_muted(user_id, muted);
    }

    pub fn peer_setting(&self, user_id: u64) -> PeerSetting {
        self.control.peer_settings.get_setting(user_id)
    }

    pub fn load_peer_settings(&self, settings: &HashMap<u64, PeerSetting>) {
        self.control.peer_settings.load_settings(settings);
    }

    pub fn set_hotkey_hook(&self, hook: impl Fn(HotkeyAction) + Send + Sync + 'static) {
        self.control.set_hotkey_hook(hook);
    }

    pub fn set(&self, preferences: AudioPreferences) -> Result<(), &'static str> {
        preferences.validate()?;
        let mut configuration = self
            .configuration
            .lock()
            .map_err(|_| "Audio settings are unavailable.")?;
        let rebind = configuration.preferences.mode != preferences.mode
            || configuration.preferences.ptt_key != preferences.ptt_key
            || configuration.preferences.mute_hotkey != preferences.mute_hotkey
            || configuration.preferences.deafen_hotkey != preferences.deafen_hotkey
            || (preferences.mode == InputMode::PushToTalk
                && self.control.state.load(Ordering::Acquire) & READY == 0);
        if rebind {
            self.control.ptt_failed();
            configuration.listener = None;
        }
        self.control.configure(&preferences, rebind);
        configuration.preferences = preferences;
        drop(configuration);
        if rebind {
            self.configure_ptt();
        }
        Ok(())
    }

    fn configure_ptt(&self) {
        let Ok(mut configuration) = self.configuration.lock() else {
            return;
        };
        configuration.error = None;
        let ptt_needed = configuration.preferences.mode == InputMode::PushToTalk;
        let hotkeys_needed = configuration.preferences.mute_hotkey.enabled
            || configuration.preferences.deafen_hotkey.enabled;

        if ptt_needed || hotkeys_needed {
            if !configuration.enabled {
                if ptt_needed {
                    configuration.error = Some("The voice engine is not running. Reconnect your account to enable background push to talk.".into());
                }
                return;
            }
            match ptt::Listener::start_with_hotkeys(
                self.control.clone(),
                configuration.preferences.ptt_key,
                ptt_needed,
                configuration.preferences.mute_hotkey,
                configuration.preferences.deafen_hotkey,
            ) {
                Ok(listener) => configuration.listener = Some(listener),
                Err(error) => configuration.error = Some(error),
            }
        }
    }

    pub fn ptt_status(&self) -> String {
        let Ok(configuration) = self.configuration.lock() else {
            return "Microphone blocked: audio settings are unavailable.".into();
        };
        if let Some(error) = &configuration.error {
            return format!(
                "Microphone blocked: {error} Switch modes or reselect Push to talk to retry."
            );
        }
        if configuration.preferences.mode != InputMode::PushToTalk {
            return "Push to talk uses a background Windows key; other apps still receive it."
                .into();
        }
        if self.control.state.load(Ordering::Acquire) & READY == 0 {
            return "Microphone blocked: background keyboard input stopped. Switch modes and reselect Push to talk to retry.".into();
        }
        "Background push to talk ready. Hold the chosen key to transmit; release it to stop.".into()
    }

    pub fn processing_status(&self) -> String {
        let bits = self.control.processing_bits.load(Ordering::Acquire);
        if bits == 0 {
            return "Sonora bypassed: all processing is off; no DSP worker or rate conversion."
                .into();
        }
        match self.control.processing_status.load(Ordering::Acquire) {
            value if value == Status::Active as u8 && self.control.processing_ready(self.control.snapshot().epoch()) => "Sonora active. Echo reference covers this call's playback only, not other apps or system audio.".into(),
            value if value == Status::WaitingReference as u8 => "Microphone blocked: echo cancellation is waiting for a fresh, synchronized call playback reference. Disable Echo cancellation to use NS/AGC without it.".into(),
            value if value == Status::Failed as u8 => format!("Microphone blocked: {} Turn processing off for explicit bypass, or change processing toggles to retry.", self.control.processing_error.lock().ok().and_then(|error| *error).unwrap_or("Sonora processing failed.")),
            _ => "Sonora requested; microphone blocked until the call's processing pipeline is ready.".into(),
        }
    }

    pub(crate) fn begin_session(&self) -> u64 {
        let session = self.control.begin_session();
        if let Ok(configuration) = self.configuration.lock()
            && let Some(listener) = &configuration.listener
        {
            listener.rearm();
        }
        session
    }

    pub(crate) fn stop(&self) {
        self.control.begin_session();
        self.control.ptt_failed();
        if let Ok(mut configuration) = self.configuration.lock() {
            configuration.enabled = false;
            configuration.listener = None;
        }
    }
}

pub(super) struct GainRamp {
    current: f32,
}

impl GainRamp {
    pub(super) fn new(gain: f32) -> Self {
        Self { current: gain }
    }

    pub(super) fn apply(&mut self, sample: f32, target: f32, step: f32) -> f32 {
        if self.current != target {
            self.current += (target - self.current).clamp(-step, step);
        }
        crate::platform::audio::sanitize(if self.current == 1. {
            sample
        } else {
            sample * self.current
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundaries_reject_non_finite_out_of_range_and_invalid_keys() {
        let settings = AudioSettings::offline(AudioPreferences::default());
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.01, 2.01] {
            for input in [true, false] {
                let mut value = settings.preferences();
                if input {
                    value.input_gain = invalid;
                } else {
                    value.output_gain = invalid;
                }
                assert!(settings.set(value).is_err());
                assert_eq!(settings.preferences(), AudioPreferences::default());
            }
        }
        for threshold in [f32::NAN, f32::INFINITY, -80.01, 0.01] {
            assert!(
                settings
                    .set(AudioPreferences {
                        threshold_db: threshold,
                        ..AudioPreferences::default()
                    })
                    .is_err()
            );
        }
        assert!(
            settings
                .set(AudioPreferences {
                    ptt_key: 0,
                    ..AudioPreferences::default()
                })
                .is_err()
        );
    }

    #[test]
    fn hotkeys_round_trip_and_conflict_validation() {
        let settings = AudioSettings::offline(AudioPreferences::default());
        let mut prefs = settings.preferences();

        // Enable default mute and deafen
        prefs.mute_hotkey.enabled = true;
        prefs.deafen_hotkey.enabled = true;
        assert!(settings.set(prefs.clone()).is_ok());

        // Make mute and deafen conflict
        prefs.deafen_hotkey.key = prefs.mute_hotkey.key;
        prefs.deafen_hotkey.modifiers = prefs.mute_hotkey.modifiers;
        assert!(settings.set(prefs.clone()).is_err());
    }

    #[test]
    fn peer_settings_audio_control_and_settings_api() {
        let settings = AudioSettings::offline(AudioPreferences::default());
        let user_id = 9988776655u64;

        assert_eq!(settings.peer_setting(user_id), PeerSetting::default());

        assert!(settings.set_peer_volume(user_id, 1.8).is_ok());
        settings.set_peer_muted(user_id, true);

        let snap = settings.peer_setting(user_id);
        assert_eq!(snap.volume, 1.8);
        assert!(snap.muted);

        settings.set_peer_muted(user_id, false);
        assert!(!settings.peer_setting(user_id).muted);
    }

    #[test]
    fn ptt_is_fail_closed_repeat_safe_and_released_by_reconfiguration_and_sessions() {
        let mut preferences = AudioPreferences {
            mode: InputMode::PushToTalk,
            ..AudioPreferences::default()
        };
        let control = AudioControl::new(&preferences);
        let session = control.begin_session();
        control.activate_session(session);
        let open = |session| control.snapshot().open(true, session);
        control.key_event(preferences.ptt_key, true);
        assert!(
            !open(session),
            "registration failure cannot open the microphone"
        );
        control.ptt_ready(true);
        control.key_event(preferences.ptt_key, true);
        assert!(!open(session), "an already-held key requires release");
        control.key_event(preferences.ptt_key, false);
        control.key_event(preferences.ptt_key + 1, true);
        assert!(!open(session));
        control.key_event(preferences.ptt_key, true);
        assert!(open(session));
        let pressed = control.snapshot();
        control.key_event(preferences.ptt_key, true);
        assert!(
            control.snapshot() == pressed,
            "repeat must not reset activity or queue epochs"
        );
        for flag in [MUTED, DEAFENED, SERVER_RESTRICTED] {
            control.set_flag(flag, true);
            control.key_event(preferences.ptt_key, false);
            control.key_event(preferences.ptt_key, true);
            assert!(!open(session));
            control.set_flag(flag, false);
        }
        control.release_ptt();
        control.key_event(preferences.ptt_key, true);
        assert!(
            !open(session),
            "desktop/session reset cannot be undone by repeat"
        );
        control.key_event(preferences.ptt_key, false);
        control.key_event(preferences.ptt_key, true);
        assert!(open(session));
        let next = control.begin_session();
        control.activate_session(next);
        control.key_event(preferences.ptt_key, true);
        assert!(!open(session));
        assert!(!open(next));
        control.end_session(session);
        control.key_event(preferences.ptt_key, false);
        control.key_event(preferences.ptt_key, true);
        assert!(open(next), "an old disconnect must not close a newer call");
        preferences.ptt_key += 1;
        control.configure(&preferences, true);
        assert!(!open(next));
        control.ptt_ready(false);
        control.key_event(preferences.ptt_key - 1, false);
        assert!(!open(next));
        control.key_event(preferences.ptt_key, true);
        assert!(open(next));
        control.ptt_failed();
        control.key_event(preferences.ptt_key, false);
        control.key_event(preferences.ptt_key, true);
        assert!(!open(next));
        preferences.mode = InputMode::AlwaysOn;
        control.set_muted(true);
        control.configure(&preferences, true);
        assert!(!open(next), "mode changes cannot override mute");
        control.set_muted(false);
        assert!(open(next));
        control.end_session(next);
        assert!(!open(next));
        control.activate_session(next);
        assert!(
            !open(next),
            "a late handshake cannot resurrect a disconnected session"
        );
    }

    #[test]
    fn gain_ramps_are_bounded_and_sanitize_samples() {
        let mut gain = GainRamp::new(1.);
        assert_eq!(gain.apply(0.25, 1., 0.1), 0.25);
        let mut previous = 0.25;
        for _ in 0..10 {
            let sample = gain.apply(0.25, 0., 0.1);
            assert!(sample <= previous && previous - sample <= 0.026);
            previous = sample;
        }
        assert_eq!(gain.apply(1., 0., 1.), 0.);
        assert_eq!(gain.apply(0.75, 2., 2.), 1.);
        assert_eq!(gain.apply(-0.75, 2., 2.), -1.);
        for invalid in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(gain.apply(invalid, 2., 1.), 0.);
        }
    }

    #[test]
    fn transport_notifications_follow_gate_transitions_without_callback_spam() {
        let (commands, mut received) = futures::channel::mpsc::unbounded();
        let mut control = AudioControl::new(&AudioPreferences::default());
        control.commands = Some(commands);
        let session = control.begin_session();
        control.activate_session(session);
        control.transmission(true, session);
        assert!(matches!(
            received.try_recv(),
            Ok(crate::voice::Command::Gate)
        ));
        for _ in 0..100 {
            control.transmission(true, session);
        }
        assert!(received.try_recv().is_err());
        control.set_muted(true);
        assert!(matches!(
            received.try_recv(),
            Ok(crate::voice::Command::Gate)
        ));
        control.transmission(true, session);
        assert!(
            received.try_recv().is_err(),
            "a stale open callback cannot reopen a muted gate"
        );
        control.set_muted(false);
        control.transmission(true, session);
        assert!(matches!(
            received.try_recv(),
            Ok(crate::voice::Command::Gate)
        ));
        control.begin_session();
        while received.try_recv().is_ok() {}
        control.transmission(true, session);
        assert!(
            received.try_recv().is_err(),
            "old sessions cannot command a newer transport"
        );
    }
}
