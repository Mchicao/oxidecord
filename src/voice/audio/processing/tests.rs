use super::super::{
    MicSource, Shutdown, activity::CaptureActivity, ring::SampleRing, settings::AudioPreferences,
};
use super::*;
use std::f64::consts::TAU;
use std::io::Read as _;

pub(super) fn voice(n: usize) -> f32 {
    let t = n as f64 / f64::from(RATE);
    let syllable = (t * 3.2).fract();
    let envelope = if syllable < 0.8 {
        (std::f64::consts::PI * syllable / 0.8).sin().powi(2)
    } else {
        0.
    };
    let phase = TAU * (125. * t + 1.5 * (TAU * 1.1 * t).sin());
    let sum: f64 = (1..=24)
        .map(|harmonic| {
            let frequency = harmonic as f64 * 125.;
            let formants = [550., 1500., 2500.]
                .into_iter()
                .map(|center| (-((frequency - center) / 220.).powi(2)).exp())
                .sum::<f64>();
            (phase * harmonic as f64).sin() * (0.15 + formants) / (harmonic as f64).sqrt()
        })
        .sum();
    (sum * envelope * 0.1) as f32
}

pub(super) fn noise(n: usize) -> f32 {
    let mut value = (n as u32).wrapping_add(0x9e37_79b9);
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    value ^= value >> 16;
    (value as f64 / f64::from(u32::MAX) * 2. - 1.) as f32
}

pub(super) fn capture(options: Options, rate: u32, channels: usize) -> Arc<Capture> {
    let control = Arc::new(AudioControl::new(&AudioPreferences {
        processing: options,
        ..AudioPreferences::default()
    }));
    let session = control.begin_session();
    control.activate_session(session);
    Arc::new(Capture {
        processing: Processing::new(control.clone(), session),
        rate,
        ring: SampleRing::new(rate, channels, 0),
        activity: CaptureActivity::default(),
        control,
        session,
    })
}

fn packet(samples: [f32; FRAME], epoch: u64, render: bool, at: Instant) -> Packet {
    Packet {
        samples,
        len: FRAME,
        rate: RATE,
        at,
        epoch,
        render,
    }
}

fn read(capture: &Arc<Capture>) -> Vec<f32> {
    let mut source = MicSource {
        capture: capture.clone(),
        shutdown: Arc::new(Shutdown::default()),
        samples: Vec::new(),
    };
    let mut bytes = vec![0u8; FRAME * capture.ring.channels * size_of::<f32>()];
    source.read_exact(&mut bytes).expect("microphone PCM");
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|sample| f32::from_le_bytes(*sample))
        .collect()
}

fn finite(samples: &[f32]) -> bool {
    samples
        .iter()
        .all(|sample| sample.is_finite() && sample.abs() <= 1.)
}

pub(super) fn power(samples: &[f32]) -> f64 {
    samples
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        / samples.len().max(1) as f64
}

fn projection(reference: &[f32], signal: &[f32]) -> (f64, f64, f64) {
    let reference_power = reference
        .iter()
        .map(|sample| f64::from(*sample).powi(2))
        .sum::<f64>()
        .max(1e-20);
    let dot = reference
        .iter()
        .zip(signal)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum::<f64>();
    let gain = dot / reference_power;
    let residual = reference
        .iter()
        .zip(signal)
        .map(|(x, y)| (f64::from(*y) - gain * f64::from(*x)).powi(2))
        .sum::<f64>()
        .max(1e-20);
    let correlation = dot
        / (reference_power
            * signal
                .iter()
                .map(|sample| f64::from(*sample).powi(2))
                .sum::<f64>()
                .max(1e-20))
        .sqrt();
    (
        gain,
        correlation,
        10. * (gain * gain * reference_power / residual)
            .max(1e-20)
            .log10(),
    )
}

fn alignment(reference: &[f32], output: &[f32]) -> usize {
    let score = |delay: usize| {
        let length = reference.len().min(output.len() - delay);
        let mut dot = 0.;
        let mut a = 0.;
        let mut b = 0.;
        for index in (0..length).step_by(16) {
            let x = f64::from(reference[index]);
            let y = f64::from(output[index + delay]);
            dot += x * y;
            a += x * x;
            b += y * y;
        }
        dot / (a * b).max(1e-20).sqrt()
    };
    let coarse = (0..=FRAME * 2)
        .step_by(8)
        .max_by(|a, b| score(*a).total_cmp(&score(*b)))
        .expect("alignment");
    (coarse.saturating_sub(8)..=(coarse + 8).min(FRAME * 2))
        .max_by(|a, b| score(*a).total_cmp(&score(*b)))
        .expect("fine alignment")
}

#[test]
fn all_eight_combinations_persist_and_off_has_no_worker_or_conversion() {
    for bits in 0..8 {
        let options = Options::from_bits(bits);
        let preferences = AudioPreferences {
            processing: options,
            ..AudioPreferences::default()
        };
        let restored: AudioPreferences =
            serde_json::from_str(&serde_json::to_string(&preferences).expect("serialize"))
                .expect("deserialize");
        assert_eq!(restored.processing, options);
        let capture = capture(options, RATE, 2);
        capture
            .processing
            .configure(Arc::downgrade(&capture), options, RATE, 2);
        assert_eq!(
            capture.processing.worker.lock().expect("worker").is_some(),
            bits != 0
        );
        capture
            .control
            .configure(&AudioPreferences::default(), false);
        assert!(
            capture
                .processing
                .worker
                .lock()
                .expect("worker stopped")
                .is_none()
        );
        assert!(
            capture
                .processing
                .sender
                .read()
                .expect("queue stopped")
                .is_none()
        );
    }
    let legacy: AudioPreferences = serde_json::from_str("{}").expect("legacy");
    assert_eq!(legacy.processing, Options::default());
    let capture = capture(Options::default(), 44_100, 2);
    let mut tap = Tap::new(capture.processing.clone(), 44_100, 2);
    tap.push(&[0.25f32; FRAME], Instant::now(), false);
    assert!(
        capture
            .processing
            .worker
            .lock()
            .expect("no bypass worker")
            .is_none()
    );
    capture.ring.push_gated(
        std::iter::repeat_n(0.25, FRAME * 2),
        capture.control.snapshot().epoch(),
        || true,
    );
    let pcm = read(&capture);
    assert_eq!(*pcm.last().expect("PCM"), 0.25);
}

#[test]
fn taps_average_channels_preserve_partial_frames_and_reference_actual_post_volume_pcm() {
    use super::super::playback::{Mixer, Playback};
    let options = Options {
        echo_cancellation: true,
        ..Options::default()
    };
    let capture = capture(options, RATE, 1);
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    *capture.processing.sender.write().expect("queue") = Some(sender);
    capture
        .processing
        .configured
        .store(options.bits(), Ordering::Release);
    let mut tap = Tap::new(capture.processing.clone(), RATE, 4);
    let at = Instant::now();
    tap.push(&[0.2f32, 0.4, 0.6], at, false);
    assert!(receiver.try_recv().is_err());
    tap.push(
        &[0.8f32, -0.2, -0.4, -0.6, -0.8],
        at + Duration::from_secs_f64(3. / (f64::from(RATE) * 4.)),
        false,
    );
    let first = receiver.try_recv().expect("complete native frames");
    assert_eq!(first.len, 2);
    assert!((first.samples[0] - 0.5).abs() < 1e-6);
    assert!((first.samples[1] + 0.5).abs() < 1e-6);
    let mut preferences = AudioPreferences {
        processing: options,
        output_gain: 0.5,
        ..AudioPreferences::default()
    };
    capture.control.configure(&preferences, false);
    let mixer = Arc::new(Mixer::new(capture.control.clone()));
    let ring = Arc::new(SampleRing::new(RATE, 2, 0));
    ring.push(std::iter::repeat_n(0.4, FRAME * 8));
    mixer.add(1, ring);
    let mut playback = Playback::new(mixer, RATE, 2);
    let mut rendered = [0f32; FRAME * 2];
    playback.render(&mut rendered);
    let mut reference = Tap::new(capture.processing.clone(), RATE, 2);
    reference.push(&rendered, at, true);
    let actual = receiver.try_recv().expect("actual playback reference");
    assert!(actual.render);
    assert!((actual.samples[FRAME - 1] - 0.2).abs() < 1e-6);
    preferences.processing.echo_cancellation = false;
    preferences.processing.noise_suppression = true;
    capture.control.configure(&preferences, false);
    reference.push(&rendered, at, true);
    assert!(
        receiver.try_recv().is_err(),
        "NS/AGC alone must not copy playback"
    );
}

#[test]
fn conversion_preserves_rational_rates_across_slices_and_flushes_history() {
    for rate in [
        8_000, 16_000, 22_050, 44_100, 44_117, RATE, 96_000, 192_000, MAX_RATE,
    ] {
        let mut resampler = Resampler::new(rate, RATE).expect("rate");
        let first = Instant::now();
        let mut total = 0usize;
        let mut emitted = 0usize;
        let mut output = [0.; FRAME];
        for chunk in (0..rate as usize / 2).step_by(137) {
            let count = (rate as usize / 2 - chunk).min(137);
            let source = vec![0.25; count];
            resampler
                .push(
                    &source,
                    first + Duration::from_secs_f64(total as f64 / f64::from(rate)),
                )
                .expect("bounded input");
            total += count;
            while resampler.pull(&mut output).expect("convert").is_some() {
                emitted += FRAME;
                assert!(finite(&output));
            }
        }
        let expected = total as u64 * u64::from(RATE) / u64::from(rate);
        let slack = FRAME as u64 + (128 * u64::from(RATE)).div_ceil(u64::from(rate));
        assert!(
            (emitted as u64) <= expected && expected - emitted as u64 <= slack,
            "rate {rate}: {emitted}/{expected}"
        );
        resampler.reset();
        resampler.push(&vec![0.; 8192], first).expect("fresh input");
        assert!(
            resampler
                .pull(&mut output)
                .expect("fresh conversion")
                .is_some()
        );
        assert!(
            output.iter().all(|sample| *sample == 0.),
            "old signal survived converter reset"
        );
    }
    assert!(Resampler::new(0, RATE).is_err());
    assert!(Resampler::new(384_001, RATE).is_err());
}

#[test]
fn all_enabled_combinations_process_finite_audio_with_only_requested_reference() {
    for bits in 0..8 {
        let options = Options::from_bits(bits);
        let mut engine = (bits != 0).then(|| Engine::new(options));
        for block in 0..16 {
            let reference: [f32; FRAME] = std::array::from_fn(|i| noise(block * FRAME + i) * 0.03);
            let input: [f32; FRAME] =
                std::array::from_fn(|i| voice((block + 20) * FRAME + i) + reference[i] * 0.4);
            let mut output = input;
            if let Some(engine) = &mut engine {
                if options.echo_cancellation {
                    engine.render(&reference).expect("requested reference only");
                }
                engine
                    .capture(&input, &mut output, 0)
                    .expect("independent processing combination");
            } else {
                assert_eq!(output, input, "all-off is exact bypass");
            }
            assert!(finite(&output), "non-finite combination {bits}");
        }
    }
}

#[test]
fn backpressure_and_missing_reference_fail_closed_without_silent_bypass() {
    let options = Options {
        echo_cancellation: true,
        ..Options::default()
    };
    let capture = capture(options, RATE, 1);
    let state = capture.control.snapshot();
    let at = Instant::now();
    let (missing_reference, receiver) = mpsc::sync_channel(1);
    missing_reference
        .try_send(packet([0.1; FRAME], state.epoch(), false, at))
        .expect("capture without playback");
    drop(missing_reference);
    run(
        &capture.processing,
        &Arc::downgrade(&capture),
        &receiver,
        &AtomicBool::new(false),
        (options, RATE, 1),
        |at| at,
    )
    .expect("AEC waits for real reference");
    assert!(!capture.open(state));
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    *capture.processing.sender.write().expect("queue") = Some(sender);
    capture
        .processing
        .configured
        .store(options.bits(), Ordering::Release);
    let mut tap = Tap::new(capture.processing.clone(), RATE, 1);
    for _ in 0..=QUEUE {
        tap.push(&[0.1f32; FRAME], Instant::now(), false);
    }
    assert!(capture.processing.has_fault());
    assert_eq!(receiver.try_iter().count(), QUEUE);
    assert!(!capture.open(capture.control.snapshot()));
    capture
        .control
        .configure(&AudioPreferences::default(), false);
    assert!(
        capture.open(capture.control.snapshot()),
        "only explicit processing-off selects bypass"
    );
    capture.processing.stop();
}

#[test]
fn muted_ptt_suppressed_and_old_epoch_processing_cannot_reach_current_packets() {
    use super::super::microphone::microphone;
    use crate::voice::InputMode;
    let options = Options {
        automatic_gain_control: true,
        ..Options::default()
    };
    let capture = capture(options, RATE, 1);
    let mut preferences = AudioPreferences {
        processing: options,
        ..AudioPreferences::default()
    };
    let mut input = microphone(
        MicSource {
            capture: capture.clone(),
            shutdown: Arc::default(),
            samples: Vec::new(),
        },
        RATE,
        1,
    );
    for restriction in 0..5 {
        let before = capture.control.snapshot();
        capture
            .processing
            .state(Status::Active, None, before.epoch());
        capture
            .ring
            .push_gated(std::iter::repeat_n(0.9, FRAME * 4), before.epoch(), || true);
        match restriction {
            0 => preferences.input_gain = 0.,
            1 => capture.control.set_muted(true),
            2 => capture.control.set_deafened(true),
            3 => capture.control.set_server_restricted(true),
            _ => preferences.mode = InputMode::PushToTalk,
        }
        capture.control.configure(&preferences, false);
        let current = capture.control.snapshot();
        capture
            .processing
            .state(Status::Active, None, before.epoch());
        assert!(!capture.open(current));
        let parsed = input.parsed_mut().expect("parsed mic");
        assert!(
            parsed
                .format
                .next_packet()
                .expect("closed gate packet")
                .data
                .iter()
                .all(|byte| *byte == 0)
        );
        capture.control.set_muted(false);
        capture.control.set_deafened(false);
        capture.control.set_server_restricted(false);
        preferences.input_gain = 1.;
        preferences.mode = InputMode::AlwaysOn;
        capture.control.configure(&preferences, false);
        let current = capture.control.snapshot();
        capture
            .processing
            .state(Status::Active, None, current.epoch());
        assert!(read(&capture).iter().all(|sample| *sample == 0.));
    }
    let old = capture.control.snapshot();
    let (sender, receiver) = mpsc::sync_channel(2);
    sender
        .try_send(packet([0.9; FRAME], old.epoch(), false, Instant::now()))
        .expect("queued old packet");
    drop(sender);
    capture.control.invalidate_capture();
    run(
        &capture.processing,
        &Arc::downgrade(&capture),
        &receiver,
        &AtomicBool::new(false),
        (options, RATE, 1),
        |_| Instant::now(),
    )
    .expect("old queue discarded");
    assert!(read(&capture).iter().all(|sample| *sample == 0.));
}

#[test]
fn offline_worker_processes_real_sonora_frames_and_stale_deadlines_stay_closed() {
    let options = Options {
        noise_suppression: true,
        ..Options::default()
    };
    let capture = capture(options, RATE, 1);
    let state = capture.control.snapshot();
    let at = Instant::now();
    let (sender, receiver) = mpsc::sync_channel(3);
    for block in 0..2 {
        sender
            .try_send(packet(
                std::array::from_fn(|i| voice((block + 20) * FRAME + i)),
                state.epoch(),
                false,
                at + Duration::from_millis(block as u64 * 10),
            ))
            .expect("offline frame");
    }
    drop(sender);
    run(
        &capture.processing,
        &Arc::downgrade(&capture),
        &receiver,
        &AtomicBool::new(false),
        (options, RATE, 1),
        |_| at + Duration::from_millis(10),
    )
    .expect("offline Sonora worker");
    assert!(capture.control.processing_ready(state.epoch()));
    let pcm = read(&capture);
    assert!(finite(&pcm) && pcm.iter().any(|sample| sample.abs() > 1e-7));
    capture.control.invalidate_capture();
    let state = capture.control.snapshot();
    let (sender, receiver) = mpsc::sync_channel(1);
    sender
        .try_send(packet([0.9; FRAME], state.epoch(), false, at))
        .expect("late frame");
    drop(sender);
    assert!(
        run(
            &capture.processing,
            &Arc::downgrade(&capture),
            &receiver,
            &AtomicBool::new(false),
            (options, RATE, 1),
            |_| at + MAX_AGE + Duration::from_millis(1)
        )
        .is_err()
    );
    assert!(read(&capture).iter().all(|sample| *sample == 0.));
}

#[test]
fn echo_delay_uses_playback_and_capture_times_and_rejects_desynchronization() {
    let now = Instant::now();
    assert_eq!(
        stream_delay_ms(
            now + Duration::from_millis(20),
            now,
            now - Duration::from_millis(10),
            now
        )
        .expect("hardware delay"),
        30
    );
    assert!(stream_delay_ms(now + Duration::from_secs(1), now, now, now).is_err());
    assert!(stream_delay_ms(now - Duration::from_millis(100), now, now, now).is_err());
    let options = Options {
        echo_cancellation: true,
        ..Options::default()
    };
    let mut old = Pipeline::new(options, 44_100, 1, 1.).expect("old device");
    old.reference = Some((now, now));
    old.output.push(&[0.9; FRAME], now).expect("queued output");
    let new = Pipeline::new(options, 96_000, 2, 1.).expect("new device");
    assert!(new.reference.is_none());
    assert_eq!(new.epoch, 2);
    assert!(new.render_rate.is_none());
}

#[test]
fn replaced_devices_cannot_publish_readiness_or_old_pcm_into_a_new_capture() {
    let options = Options {
        echo_cancellation: true,
        ..Options::default()
    };
    let old = capture(options, RATE, 1);
    let epoch = old.control.snapshot().epoch();
    old.processing.state(Status::Active, None, epoch);
    old.ring
        .push_gated(std::iter::repeat_n(0.9, FRAME * 2), epoch, || true);
    let replacement = Processing::new(old.control.clone(), old.session);
    replacement.state(Status::WaitingReference, None, epoch);
    old.processing.state(Status::Active, None, epoch);
    assert!(
        !old.control.processing_ready(epoch),
        "old device's worker cannot announce the new device ready"
    );
    replacement.state(Status::Active, None, epoch);
    assert!(
        !old.open(old.control.snapshot()),
        "even if the new worker is ready, the old capture is retired"
    );
    assert!(read(&old).iter().all(|sample| *sample == 0.));
}

#[test]
fn an_obsolete_callback_cannot_fault_or_close_a_reconfigured_queue() {
    let options = Options {
        noise_suppression: true,
        ..Options::default()
    };
    let capture = capture(options, RATE, 1);
    let old_epoch = capture.control.snapshot().epoch();
    let new_options = Options {
        automatic_gain_control: true,
        ..Options::default()
    };
    capture.control.configure(
        &AudioPreferences {
            processing: new_options,
            ..AudioPreferences::default()
        },
        false,
    );
    let (sender, receiver) = mpsc::sync_channel(2);
    *capture.processing.sender.write().expect("new queue") = Some(sender);
    capture
        .processing
        .configured
        .store(new_options.bits(), Ordering::Release);
    capture
        .processing
        .fail_at("obsolete callback failed", old_epoch);
    assert!(!capture.processing.has_fault());
    let mut tap = Tap::new(capture.processing.clone(), RATE, 1);
    tap.push(&[0.2f32; FRAME], Instant::now(), false);
    assert_eq!(
        receiver
            .try_recv()
            .expect("new queue still receives current audio")
            .epoch,
        capture.control.snapshot().epoch()
    );
}

#[test]
fn ns_reduces_noise_without_passing_only_a_volume_test() {
    fn reference(samples: &[f32]) -> Vec<f32> {
        use sonora::high_pass_filter::HighPassFilter;
        use sonora::three_band_filter_bank::ThreeBandFilterBank;

        let mut analysis = ThreeBandFilterBank::new();
        let mut synthesis = ThreeBandFilterBank::new();
        let mut high_pass = HighPassFilter::new(16_000, 1);
        let mut low_band = [vec![0.; 160]];
        let mut result = Vec::with_capacity(samples.len());
        for frame in samples.as_chunks::<FRAME>().0 {
            let mut bands = [[0.; 160]; 3];
            let mut output = [0.; FRAME];
            analysis.analysis(frame, &mut bands);
            low_band[0].copy_from_slice(&bands[0]);
            high_pass.process_channels(&mut low_band);
            bands[0].copy_from_slice(&low_band[0]);
            synthesis.synthesis(&bands, &mut output);
            result.extend(output);
        }
        result
    }

    let mut engine = Engine::new(Options {
        noise_suppression: true,
        ..Options::default()
    });
    let mut output = [0.; FRAME];
    let mut before_noise = Vec::new();
    let mut after_noise = Vec::new();
    for block in 0..80 {
        let frame = std::array::from_fn(|i| noise(block * FRAME + i) * 0.025);
        engine.capture(&frame, &mut output, 0).expect("noise frame");
        if block >= 40 {
            before_noise.extend(frame);
            after_noise.extend(output);
        }
    }
    let length = RATE as usize * 3;
    let clean: Vec<_> = (0..length).map(voice).collect();
    let noisy: Vec<_> = clean
        .iter()
        .enumerate()
        .map(|(i, sample)| sample + noise(i + 80 * FRAME) * 0.025)
        .collect();
    let mut result = Vec::with_capacity(length);
    for frame in noisy.as_chunks::<FRAME>().0 {
        engine
            .capture(frame, &mut output, 0)
            .expect("speech-like frame");
        result.extend(output);
    }
    let delay = alignment(&clean, &result);
    let length = clean.len() - delay;
    let (_, correlation, raw_processed_sdr) =
        projection(&clean[..length], &result[delay..delay + length]);
    let (_, _, raw_input_sdr) = projection(&clean[..length], &noisy[..length]);
    // NS implicitly includes a high-pass stage; isolate suppression from that phase response.
    let filtered_clean = reference(&clean);
    let filtered_noisy = reference(&noisy);
    let filtered_delay = alignment(&filtered_clean, &result);
    let length = filtered_clean.len() - filtered_delay;
    let (_, filtered_correlation, processed_sdr) = projection(
        &filtered_clean[..length],
        &result[filtered_delay..filtered_delay + length],
    );
    let (_, _, input_sdr) = projection(&filtered_clean, &filtered_noisy);
    let attenuation_db = 10. * (power(&before_noise) / power(&after_noise).max(1e-20)).log10();
    eprintln!(
        "{}",
        serde_json::json!({"test":"synthetic_ns", "noise_attenuation_db":attenuation_db, "raw_speech_correlation":correlation, "raw_input_si_sdr_db":raw_input_sdr, "raw_processed_si_sdr_db":raw_processed_sdr, "highpass_reference_correlation":filtered_correlation, "highpass_reference_input_si_sdr_db":input_sdr, "highpass_reference_processed_si_sdr_db":processed_sdr, "alignment_samples":delay, "highpass_reference_alignment_samples":filtered_delay})
    );
    assert!(
        attenuation_db > 3.,
        "NS did not attenuate stationary noise: {attenuation_db} dB"
    );
    assert!(
        correlation > 0.65,
        "NS damaged the synthetic voiced waveform: correlation={correlation}"
    );
    assert!(
        filtered_correlation > 0.95,
        "NS damaged the high-pass speech reference: correlation={filtered_correlation}"
    );
    assert!(
        processed_sdr >= input_sdr + 1.,
        "NS did not improve scale-invariant signal quality: {processed_sdr} < {input_sdr} + 1 dB"
    );
    assert!(finite(&result));
}

#[test]
fn aec_reduces_delayed_echo_and_preserves_synthetic_double_talk() {
    let mut engine = Engine::new(Options {
        echo_cancellation: true,
        ..Options::default()
    });
    let length = FRAME * 800;
    let far: Vec<_> = (0..length)
        .map(|n| noise(n) * 0.12 + voice(n) * 0.5)
        .collect();
    let mut output = [0.; FRAME];
    let mut echo_before = Vec::new();
    let mut echo_after = Vec::new();
    let mut near = Vec::new();
    let mut near_after = Vec::new();
    for block in 0..800 {
        let start = block * FRAME;
        let reference: &[f32; FRAME] = far[start..start + FRAME]
            .try_into()
            .expect("reference frame");
        let local: [f32; FRAME] = std::array::from_fn(|i| {
            if block >= 600 {
                voice(start + i + 7919)
            } else {
                0.
            }
        });
        let captured = std::array::from_fn(|i| {
            let n = start + i;
            local[i]
                + n.checked_sub(1440).map_or(0., |n| far[n] * 0.55)
                + n.checked_sub(2400).map_or(0., |n| far[n] * 0.15)
        });
        engine.render(reference).expect("real far-end signal");
        engine
            .capture(&captured, &mut output, 0)
            .expect("echo capture");
        if (400..600).contains(&block) {
            echo_before.extend(captured);
            echo_after.extend(output);
        }
        if block >= 650 {
            near.extend(local);
            near_after.extend(output);
        }
        assert!(finite(&output));
    }
    let erle = 10. * (power(&echo_before) / power(&echo_after).max(1e-20)).log10();
    let delay = alignment(&near, &near_after);
    let (gain, correlation, _) = projection(&near[..near.len() - delay], &near_after[delay..]);
    eprintln!(
        "{}",
        serde_json::json!({"test":"synthetic_aec", "tail_erle_db":erle, "double_talk_correlation":correlation, "near_end_gain":gain, "alignment_samples":delay})
    );
    assert!(
        erle > 8.,
        "AEC failed to cancel the delayed reference: {erle} dB"
    );
    assert!(
        correlation > 0.5 && (0.2..=2.5).contains(&gain),
        "AEC damaged double talk: correlation={correlation}, gain={gain}"
    );
}

#[test]
fn agc_handles_level_changes_and_manual_gain_remains_last_and_finite() {
    let mut engine = Engine::new(Options {
        automatic_gain_control: true,
        ..Options::default()
    });
    let mut output = [0.; FRAME];
    let mut low_in = Vec::new();
    let mut low_out = Vec::new();
    let mut high_in = Vec::new();
    let mut high_out = Vec::new();
    for block in 0..800 {
        let scale = if block < 400 { 0.15 } else { 2.5 };
        let frame = std::array::from_fn(|i| sanitize(voice(block * FRAME + i) * scale));
        engine.capture(&frame, &mut output, 0).expect("AGC frame");
        assert!(finite(&output));
        if (300..400).contains(&block) {
            low_in.extend(frame);
            low_out.extend(output);
        }
        if block >= 700 {
            high_in.extend(frame);
            high_out.extend(output);
        }
    }
    let low_gain = (power(&low_out) / power(&low_in).max(1e-20)).sqrt();
    let high_gain = (power(&high_out) / power(&high_in).max(1e-20)).sqrt();
    eprintln!(
        "{}",
        serde_json::json!({"test":"synthetic_agc", "quiet_gain":low_gain, "loud_gain":high_gain})
    );
    assert!(low_gain.is_finite() && high_gain.is_finite() && low_gain <= 32.);
    assert!(
        low_gain >= high_gain * 0.9,
        "AGC amplified louder speech more: {low_gain}/{high_gain}"
    );
    let mut zero = GainRamp::new(0.);
    assert!(
        output
            .iter()
            .all(|sample| zero.apply(*sample, 0., 1.) == 0.)
    );
    let mut half = GainRamp::new(0.5);
    for sample in output {
        assert!((half.apply(sample, 0.5, 1.) - sample * 0.5).abs() < 1e-6);
    }
    let mut invalid = [0.; FRAME];
    invalid[0] = f32::NAN;
    invalid[1] = f32::INFINITY;
    for sample in &mut invalid {
        *sample = sanitize(*sample);
    }
    engine
        .capture(&invalid, &mut output, 0)
        .expect("sanitized invalid input");
    assert!(finite(&output));
}
