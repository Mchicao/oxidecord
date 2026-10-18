//! Picking and opening the audio devices.

use std::sync::Arc;

use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
use cpal::{DeviceId, FromSample, Sample as _, SizedSample, StreamConfig, SupportedStreamConfig};

use super::playback::{Mixer, Playback};
use super::{CHANNELS, Capture, GainRamp, Shutdown};
use crate::platform::audio::in_device_format;

/// A microphone or speaker the user can pick between, as the menu lists it.
pub struct AudioDevice {
    /// Stable across runs and reboots, so it's what a preference stores.
    pub id: String,
    pub name: String,
}

pub fn input_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let Ok(devices) = host.input_devices() else {
        return Vec::new();
    };

    devices.filter_map(audio_device).collect()
}

pub fn output_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };

    devices.filter_map(audio_device).collect()
}

fn audio_device(device: cpal::Device) -> Option<AudioDevice> {
    Some(AudioDevice {
        id: device.id().ok()?.to_string(),
        name: device.description().ok()?.name().to_string(),
    })
}

/// The chosen microphone and its supported capture configuration.
pub(super) fn chosen_input(
    host: &cpal::Host,
    id: Option<&str>,
) -> Option<(cpal::Device, SupportedStreamConfig)> {
    chosen(
        id,
        |id| host.device_by_id(id),
        || host.default_input_device(),
        input_config,
    )
}

/// The chosen speaker and its supported playback configuration.
pub(super) fn chosen_output(
    host: &cpal::Host,
    id: Option<&str>,
) -> Option<(cpal::Device, SupportedStreamConfig)> {
    chosen(
        id,
        |id| host.device_by_id(id),
        || host.default_output_device(),
        output_config,
    )
}

pub(super) fn input_config(device: cpal::Device) -> Option<(cpal::Device, SupportedStreamConfig)> {
    let supported = device.default_input_config().ok()?;
    Some((device, supported))
}

pub(super) fn output_config(device: cpal::Device) -> Option<(cpal::Device, SupportedStreamConfig)> {
    let supported = device.default_output_config().ok()?;
    Some((device, supported))
}

/// Resolves and validates the preferred device before falling back to the
/// system default. Stream creation happens later on the audio thread.
fn chosen<Device, Config>(
    id: Option<&str>,
    resolve: impl FnOnce(&DeviceId) -> Option<Device>,
    default: impl FnOnce() -> Option<Device>,
    config: impl Fn(Device) -> Option<Config>,
) -> Option<Config> {
    id.and_then(|id| id.parse::<DeviceId>().ok())
        .and_then(|id| resolve(&id))
        .and_then(&config)
        .or_else(|| default().and_then(config))
}

/// How many of a microphone's channels the capture ring carries.
///
/// The raw input the mixer is handed can only describe mono or stereo, so a
/// device offering more — a mic array, a capture interface — is read as the
/// stereo pair it starts with rather than refused outright.
pub(super) fn ring_channels(device_channels: u16) -> usize {
    usize::from(device_channels).clamp(1, CHANNELS)
}

/// Opens both streams and holds them until the call ends. Runs on its own
/// thread: cpal streams are `!Send` on some platforms, so they can't be handed
/// back to the caller.
pub(super) fn run_devices(
    input: Option<(cpal::Device, SupportedStreamConfig)>,
    output: Option<(cpal::Device, SupportedStreamConfig)>,
    capture: &Arc<Capture>,
    mixer: &Arc<Mixer>,
    shutdown: &Arc<Shutdown>,
) {
    let input = input.and_then(|(device, config)| build_input(&device, config, capture.clone()));
    let output = output.and_then(|(device, config)| build_output(&device, config, mixer.clone()));

    if input.is_none() && output.is_none() {
        return;
    }

    // Both streams stop when they're dropped, so the thread's only job from
    // here is to stay alive for as long as the call does.
    shutdown.wait();
}

/// Fills the capture ring with what the device hears, as `f32` at the device's
/// own rate; the mixer is told that rate and does the resampling.
fn build_input(
    device: &cpal::Device,
    config: SupportedStreamConfig,
    capture: Arc<Capture>,
) -> Option<cpal::Stream> {
    let device_channels = usize::from(config.channels());
    let stream = in_device_format!(
        config.sample_format(),
        input_stream,
        device,
        config.into(),
        device_channels,
        capture,
    )?;
    stream.play().ok()?;
    Some(stream)
}

fn input_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    device_channels: usize,
    capture: Arc<Capture>,
) -> Option<cpal::Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let mut gain = GainRamp::new(f32::from_bits(
        capture
            .control
            .input_gain
            .load(std::sync::atomic::Ordering::Relaxed),
    ));
    let step = 1. / (config.sample_rate.max(1) as f32 * capture.ring.channels as f32 * 0.005);
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &_| push_capture(data, device_channels, &capture, &mut gain, step),
            // Usually the device going away mid-call, which is worth knowing
            // about when a call has gone one-way.
            |err| eprintln!("microphone stream error: {err}"),
            None,
        )
        .ok()
}

/// Copies one callback's worth of microphone into the ring, keeping only the
/// channels [`ring_channels`] promised the mixer.
fn push_capture<T>(
    data: &[T],
    device_channels: usize,
    capture: &Capture,
    gain: &mut GainRamp,
    step: f32,
) where
    T: Copy,
    f32: FromSample<T>,
{
    if device_channels == 0 {
        return;
    }
    let state = capture.control.snapshot();
    let target = f32::from_bits(
        capture
            .control
            .input_gain
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let power = std::cell::Cell::new(0.);
    let samples = std::cell::Cell::new(0);
    let mut measure = |&sample| {
        let sample = gain.apply(f32::from_sample(sample), target, step);
        power.set(power.get() + f64::from(sample).powi(2));
        samples.set(samples.get() + 1);
        sample
    };
    let finish = || {
        capture.activity.observe(
            power.get(),
            samples.get(),
            std::time::Instant::now(),
            state.activity_threshold(capture.control.threshold()),
            state.epoch(),
        );
        let open = state == capture.control.snapshot() && capture.open(state);
        capture.control.transmission(open, capture.session);
        open
    };
    let keep = ring_channels(device_channels.try_into().unwrap_or(u16::MAX));
    if device_channels <= keep {
        capture
            .ring
            .push_gated(data.iter().map(&mut measure), state.epoch(), finish);
    } else {
        capture.ring.push_gated(
            data.chunks_exact(device_channels)
                .flat_map(|frame| &frame[..keep])
                .map(&mut measure),
            state.epoch(),
            finish,
        );
    }
}

fn build_output(
    device: &cpal::Device,
    config: SupportedStreamConfig,
    mixer: Arc<Mixer>,
) -> Option<cpal::Stream> {
    let stream = in_device_format!(
        config.sample_format(),
        output_stream,
        device,
        config.into(),
        mixer,
    )?;
    stream.play().ok()?;
    Some(stream)
}

fn output_stream<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mixer: Arc<Mixer>,
) -> Option<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let mut render = Playback::new(mixer, config.sample_rate, usize::from(config.channels));

    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &_| render.render(data),
            |err| eprintln!("speaker stream error: {err}"),
            None,
        )
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_id(name: &str) -> String {
        DeviceId::new(cpal::default_host().id(), name).to_string()
    }

    #[test]
    fn valid_non_default_choice_is_kept_without_resolving_default() {
        let id = stored_id("preferred");
        assert_eq!(
            chosen(
                Some(&id),
                |id| (id.id() == "preferred").then_some("headphones"),
                || panic!("a valid preferred device must not query the default"),
                Some,
            ),
            Some("headphones"),
        );
    }

    #[test]
    fn missing_invalid_and_unknown_choices_use_the_default() {
        let unknown = stored_id("unavailable");
        assert!(unknown.parse::<DeviceId>().is_ok());
        for id in [None, Some("garbage"), Some(unknown.as_str())] {
            assert_eq!(
                chosen(id, |_| None, || Some("speakers"), Some),
                Some("speakers"),
            );
        }
    }

    #[test]
    fn preferred_device_without_output_configuration_uses_default() {
        let id = stored_id("capture-only");
        assert_eq!(
            chosen(
                Some(&id),
                |_| Some("microphone"),
                || Some("speakers"),
                |device| (device == "speakers").then_some("configured output"),
            ),
            Some("configured output"),
        );
    }

    #[test]
    fn unavailable_or_unconfigurable_default_leaves_the_direction_empty() {
        let id = stored_id("unavailable");
        assert_eq!(chosen(Some(&id), |_| None::<u8>, || None, Some), None);
        assert_eq!(
            chosen(None, |_| None::<u8>, || Some(1), |_| None::<u8>),
            None
        );
    }
}

#[cfg(test)]
mod local_speaking_tests {
    use super::*;
    use crate::voice::audio::AudioIo;
    use crate::voice::audio::{
        activity::CaptureActivity,
        ring::SampleRing,
        settings::{AudioControl, AudioPreferences},
    };
    use crate::voice::{Speakers, TickHandler, VoiceEvent};
    use std::sync::Mutex;
    use twilight_model::id::Id;

    fn test_audio() -> Arc<AudioIo> {
        let control = Arc::new(AudioControl::new(&AudioPreferences::default()));
        let session = control.begin_session();
        control.activate_session(session);
        Arc::new(AudioIo {
            capture: Arc::new(Capture {
                ring: SampleRing::new(48_000, 1, 30),
                activity: CaptureActivity::default(),
                control: control.clone(),
                session,
            }),
            mixer: Arc::new(Mixer::new(control)),
            incoming: Mutex::default(),
            shutdown: Arc::default(),
            deafened: Arc::default(),
            capture_format: (48_000, 1),
        })
    }

    fn push(data: &[f32], audio: &AudioIo) {
        push_capture(data, 1, &audio.capture, &mut GainRamp::new(1.), 1.);
    }

    #[test]
    fn local_microphone_lights_shared_participant_state_without_remote_packets() {
        let local = Id::new(42);
        let audio = test_audio();
        let (events, mut received) = futures::channel::mpsc::unbounded();
        let handler = TickHandler {
            audio: Arc::new(Mutex::new(audio.clone())),
            local_user: local,
            speakers: Arc::new(Mutex::new(Speakers::default())),
            events,
        };
        push(&[0.1f32; 960], &audio);
        handler.update_speaking(std::iter::empty());
        assert!(
            matches!(received.try_recv(), Ok(VoiceEvent::Speaking(users)) if users.contains(&local)),
            "microphone activity must reach the participant state used by both voice views"
        );
        handler.update_speaking(std::iter::empty());
        assert!(
            received.try_recv().is_err(),
            "unchanged activity must not repaint every tick"
        );

        let remote = Id::new(43);
        if let Ok(mut speakers) = handler.speakers.lock() {
            speakers.users.insert(7, remote);
        }
        handler.update_speaking([7].into_iter());
        assert!(
            matches!(received.try_recv(), Ok(VoiceEvent::Speaking(users)) if users == [local, remote].into())
        );
        audio.set_muted(true);
        push(&[0.1f32; 960], &audio);
        handler.update_speaking([7].into_iter());
        assert!(
            matches!(received.try_recv(), Ok(VoiceEvent::Speaking(users)) if users == [remote].into())
        );
        audio.set_muted(false);
        handler.update_speaking([7].into_iter());
        assert!(
            received.try_recv().is_err(),
            "unmuting must not reuse old microphone activity"
        );

        push(&[0.1f32; 960], &audio);
        handler.update_speaking(std::iter::empty());
        assert!(
            matches!(received.try_recv(), Ok(VoiceEvent::Speaking(users)) if users == [local].into())
        );
        audio.set_deafened(true);
        handler.update_speaking(std::iter::empty());
        assert!(matches!(received.try_recv(), Ok(VoiceEvent::Speaking(users)) if users.is_empty()));
        push(&[0.1f32; 960], &audio);
        audio.set_deafened(false);
        handler.update_speaking(std::iter::empty());
        assert!(received.try_recv().is_err());

        let replacement = test_audio();
        if let Ok(mut current) = handler.audio.lock() {
            *current = replacement.clone();
        }
        push(&[0.1f32; 960], &audio);
        handler.update_speaking(std::iter::empty());
        assert!(
            received.try_recv().is_err(),
            "the old microphone must no longer drive the indicator"
        );
        push(&[0.1f32; 960], &replacement);
        handler.update_speaking(std::iter::empty());
        assert!(
            matches!(received.try_recv(), Ok(VoiceEvent::Speaking(users)) if users == [local].into())
        );
        replacement.stop();
        handler.update_speaking(std::iter::empty());
        assert!(matches!(received.try_recv(), Ok(VoiceEvent::Speaking(users)) if users.is_empty()));
    }

    #[test]
    fn only_transmitted_microphone_channels_contribute_to_activity() {
        let control = Arc::new(AudioControl::new(&AudioPreferences::default()));
        let session = control.begin_session();
        control.activate_session(session);
        let capture = Capture {
            ring: SampleRing::new(44_100, 2, 0),
            activity: CaptureActivity::default(),
            control,
            session,
        };
        let epoch = capture.control.snapshot().epoch();
        let mut gain = GainRamp::new(1.);
        push_capture(&[0i16, 0, i16::MAX, i16::MIN], 4, &capture, &mut gain, 1.);
        assert!(!capture.activity.active(std::time::Instant::now(), epoch));
        push_capture(&[0.001f32; 960], 2, &capture, &mut gain, 1.);
        push_capture(&[f32::NAN, f32::INFINITY], 2, &capture, &mut gain, 1.);
        assert!(!capture.activity.active(std::time::Instant::now(), epoch));
        push_capture(&[i16::MAX, i16::MIN, 0, 0], 4, &capture, &mut gain, 1.);
        assert!(capture.activity.active(std::time::Instant::now(), epoch));
        let mut output = [0.; 8];
        capture.ring.fill_gated(&mut output, epoch, true);
        assert!(output.iter().all(|value| value.is_finite()));
    }

    fn read_mic(audio: &AudioIo) -> Vec<f32> {
        use std::io::Read as _;
        let mut source = super::super::MicSource {
            capture: audio.capture.clone(),
            shutdown: audio.shutdown.clone(),
            samples: Vec::new(),
        };
        let mut bytes = [0u8; 512 * size_of::<f32>()];
        assert_eq!(source.read(&mut bytes).expect("read PCM"), bytes.len());
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|sample| f32::from_le_bytes(*sample))
            .collect()
    }

    #[test]
    fn input_gain_changes_transmitted_pcm_with_zero_unity_clipping_and_invalid_samples() {
        for (gain, input, expected) in [
            (0., 0.25, 0.),
            (1., 0.25, 0.25),
            (2., 0.25, 0.5),
            (2., 0.75, 1.),
            (2., -0.75, -1.),
            (1., f32::NAN, 0.),
            (1., f32::INFINITY, 0.),
        ] {
            let audio = test_audio();
            audio.capture.control.configure(
                &AudioPreferences {
                    input_gain: gain,
                    ..AudioPreferences::default()
                },
                false,
            );
            push(&[input; 4096], &audio);
            let pcm = read_mic(&audio);
            assert!(
                pcm.iter()
                    .all(|sample| sample.is_finite() && sample.abs() <= 1.)
            );
            assert!(
                (pcm[511] - expected).abs() < 0.00001,
                "gain {gain}, input {input}: got {}",
                pcm[511]
            );
            if gain == 0. {
                assert!(!audio.is_speaking());
            }
        }
    }

    #[test]
    fn closing_reopening_gate_never_replays_queued_or_closed_capture() {
        use crate::voice::InputMode;
        let audio = test_audio();
        let control = &audio.capture.control;
        let mut preferences = AudioPreferences {
            mode: InputMode::PushToTalk,
            ..AudioPreferences::default()
        };
        control.configure(&preferences, true);
        control.ptt_ready(false);
        control.key_event(preferences.ptt_key, true);
        push(&[0.7; 4096], &audio);
        control.key_event(preferences.ptt_key, false);
        assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
        assert!(!audio.is_speaking());
        push(&[0.8; 4096], &audio);
        control.key_event(preferences.ptt_key, true);
        assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
        push(&[0.2; 4096], &audio);
        assert!((read_mic(&audio)[511] - 0.2).abs() < 0.00001);
        push(&[0.9; 4096], &audio);
        control.key_event(preferences.ptt_key, false);
        control.key_event(preferences.ptt_key, true);
        assert!(
            read_mic(&audio).iter().all(|sample| *sample == 0.),
            "even a close/reopen between callbacks must discard old PCM"
        );
        for restriction in 0..3 {
            push(&[0.9; 4096], &audio);
            match restriction {
                0 => control.set_muted(true),
                1 => control.set_deafened(true),
                _ => control.set_server_restricted(true),
            }
            push(&[0.8; 4096], &audio);
            assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
            assert!(!audio.is_speaking());
            match restriction {
                0 => control.set_muted(false),
                1 => control.set_deafened(false),
                _ => control.set_server_restricted(false),
            }
            assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
            push(&[0.1; 4096], &audio);
            assert!((read_mic(&audio)[511] - 0.1).abs() < 0.00001);
        }
        push(&[0.9; 4096], &audio);
        preferences.mode = InputMode::AlwaysOn;
        control.configure(&preferences, true);
        assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
        push(&[0.2; 4096], &audio);
        assert!((read_mic(&audio)[511] - 0.2).abs() < 0.00001);
        control.end_session(audio.capture.session);
        push(&[0.9; 4096], &audio);
        assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
    }

    #[test]
    fn voice_activity_gates_real_pcm_and_speaking_at_manual_threshold() {
        use crate::voice::InputMode;
        let audio = test_audio();
        let mut preferences = AudioPreferences {
            mode: InputMode::VoiceActivity,
            threshold_db: -20.,
            ..AudioPreferences::default()
        };
        audio.capture.control.configure(&preferences, true);
        push(&[0.02; 4096], &audio);
        assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
        assert!(!audio.is_speaking());
        preferences.threshold_db = -40.;
        audio.capture.control.configure(&preferences, false);
        push(&[0.02; 4096], &audio);
        assert!(audio.is_speaking());
        assert!((read_mic(&audio)[511] - 0.02).abs() < 0.00001);
        preferences.threshold_db = -20.;
        audio.capture.control.configure(&preferences, false);
        assert!(
            !audio.is_speaking(),
            "raising the threshold must invalidate the old hold"
        );
        push(&[0.02; 4096], &audio);
        assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
    }

    #[test]
    fn songbird_packets_do_not_prefetch_capture_across_a_gate_change() {
        use crate::voice::InputMode;
        let audio = test_audio();
        let control = &audio.capture.control;
        let preferences = AudioPreferences {
            mode: InputMode::PushToTalk,
            ..AudioPreferences::default()
        };
        control.configure(&preferences, true);
        control.ptt_ready(false);
        control.key_event(preferences.ptt_key, true);
        let mut input = audio.mic_input();
        let parsed = input
            .parsed_mut()
            .expect("microphone is already parsed without read-ahead");
        push(&[0.7; 8192], &audio);
        let first = parsed.format.next_packet().expect("PCM packet");
        assert_eq!(
            parsed
                .decoder
                .decode(&first)
                .expect("decode f32 PCM")
                .frames(),
            960
        );
        assert!(
            (f32::from_le_bytes(
                first.data[first.data.len() - 4..]
                    .try_into()
                    .expect("sample")
            ) - 0.7)
                .abs()
                < 0.00001
        );
        control.key_event(preferences.ptt_key, false);
        for _ in 0..3 {
            let packet = parsed.format.next_packet().expect("silence packet");
            assert!(packet.data.iter().all(|byte| *byte == 0));
            assert_eq!(
                parsed
                    .decoder
                    .decode(&packet)
                    .expect("decode silence")
                    .frames(),
                960
            );
        }
        push(&[0.8; 8192], &audio);
        control.key_event(preferences.ptt_key, true);
        assert!(
            parsed
                .format
                .next_packet()
                .expect("no stale capture")
                .data
                .iter()
                .all(|byte| *byte == 0)
        );
        push(&[0.2; 8192], &audio);
        let packet = parsed.format.next_packet().expect("fresh capture");
        assert!(
            (f32::from_le_bytes(
                packet.data[packet.data.len() - 4..]
                    .try_into()
                    .expect("sample")
            ) - 0.2)
                .abs()
                < 0.00001
        );
        audio.stop();
        assert!(parsed.format.next_packet().is_err());
    }

    #[test]
    fn replacement_capture_keeps_controls_but_not_device_audio() {
        let old = test_audio();
        old.capture.control.configure(
            &AudioPreferences {
                input_gain: 0.5,
                output_gain: 0.25,
                ..AudioPreferences::default()
            },
            false,
        );
        push(&[0.8; 4096], &old);
        let capture = Arc::new(Capture {
            ring: SampleRing::new(48_000, 1, 30),
            activity: CaptureActivity::default(),
            control: old.capture.control.clone(),
            session: old.capture.session,
        });
        old.stop();
        let mut source = super::super::MicSource {
            capture: capture.clone(),
            shutdown: Arc::default(),
            samples: Vec::new(),
        };
        let mut bytes = [1u8; 512 * 4];
        use std::io::Read as _;
        source.read_exact(&mut bytes).expect("PCM");
        assert!(bytes.iter().all(|byte| *byte == 0));
        push_capture(&[0.2f32; 4096], 1, &capture, &mut GainRamp::new(0.5), 1.);
        source.read_exact(&mut bytes).expect("PCM");
        assert!(
            (f32::from_le_bytes(bytes[2044..2048].try_into().expect("sample")) - 0.1).abs()
                < 0.00001
        );
        assert_eq!(
            f32::from_bits(
                capture
                    .control
                    .output_gain
                    .load(std::sync::atomic::Ordering::Relaxed)
            ),
            0.25
        );
    }

    #[test]
    fn server_suppression_keeps_pcm_speaking_and_songbird_transport_closed() {
        use crate::discord::VoiceUserState;
        use crate::voice::{Call, InputMode};
        let _runtime = crate::platform::runtime::handle().enter();
        let audio = test_audio();
        let control = &audio.capture.control;
        let mut preferences = AudioPreferences {
            mode: InputMode::PushToTalk,
            ..AudioPreferences::default()
        };
        control.configure(&preferences, true);
        control.ptt_ready(false);
        control.key_event(preferences.ptt_key, true);
        let mut state = VoiceUserState {
            user_id: Id::new(42),
            guild_id: Some(Id::new(1)),
            channel_id: Some(Id::new(2)),
            session_id: String::new(),
            self_mute: false,
            self_deaf: false,
            mute: false,
            deaf: false,
            suppress: true,
            self_stream: false,
            name: None,
            avatar_url: None,
        };
        control.set_server_restricted(state.server_restricted());
        let mut driver = songbird::driver::Driver::new(songbird::Config::default());
        let mic = driver.play_input(audio.mic_input());
        let mut call = Call {
            driver,
            audio: Arc::new(Mutex::new(audio.clone())),
            mic,
        };
        for mode in [
            InputMode::PushToTalk,
            InputMode::AlwaysOn,
            InputMode::VoiceActivity,
        ] {
            preferences.mode = mode;
            preferences.input_gain = 2.;
            control.configure(&preferences, false);
            control.key_event(preferences.ptt_key, false);
            control.key_event(preferences.ptt_key, true);
            push(&[0.4; 4096], &audio);
            call.update_transmission();
            assert!(call.driver.is_mute());
            assert!(!audio.is_speaking());
            assert!(read_mic(&audio).iter().all(|sample| *sample == 0.));
            assert!(
                !state.mute && !state.deaf,
                "Stage suppression is not a mute badge"
            );
        }
        state.suppress = false;
        control.set_server_restricted(state.server_restricted());
        push(&[0.1; 4096], &audio);
        call.update_transmission();
        assert!(!call.driver.is_mute());
        assert!((read_mic(&audio)[511] - 0.2).abs() < 0.00001);
    }
}
