use super::super::playback::tests::{allocations, allocations_here, live_bytes, peak_bytes};
use super::tests::{capture, noise, voice};
use super::*;

#[cfg(windows)]
fn thread_cpu_ns() -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
        .ok()?;
    }
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some((ticks(kernel) + ticks(user)) * 100)
}

#[cfg(not(windows))]
fn thread_cpu_ns() -> Option<u64> {
    None
}

fn impulse_peak_delay(options: Options) -> Option<usize> {
    if options.bits() == 0 {
        return Some(0);
    }
    let mut engine = Engine::new(options);
    let mut output = [0.; FRAME];
    for _ in 0..30 {
        if options.echo_cancellation {
            engine.render(&[0.; FRAME]).expect("silent test reference");
        }
        engine
            .capture(&[0.; FRAME], &mut output, 0)
            .expect("warmup");
    }
    let mut peak: (f32, usize) = (0., 0);
    for block in 0..20 {
        let mut input = [0.; FRAME];
        if block == 0 {
            input[0] = 0.5;
        }
        if options.echo_cancellation {
            engine.render(&[0.; FRAME]).expect("silent test reference");
        }
        engine
            .capture(&input, &mut output, 0)
            .expect("impulse response");
        for (index, sample) in output.iter().enumerate() {
            if sample.abs() > peak.0 {
                peak = (sample.abs(), block * FRAME + index);
            }
        }
    }
    (peak.0 > 1e-8).then_some(peak.1)
}

#[test]
#[ignore = "Offline benchmark: run exactly this test, --ignored --nocapture --test-threads=1; reports measurements, not real-call claims"]
fn benchmark_sonora_offline() {
    let selected = std::env::var("SONORA_BENCH_BITS")
        .ok()
        .map(|value| value.parse::<u8>().expect("SONORA_BENCH_BITS is 0..7"));
    if let Some(bits) = selected {
        assert!(bits < 8);
    }
    let rate = std::env::var("SONORA_BENCH_RATE")
        .ok()
        .map(|value| value.parse::<u32>().expect("integer sample rate"))
        .unwrap_or(RATE);
    assert!((8_000..=MAX_RATE).contains(&rate));
    let blocks = std::env::var("SONORA_BENCH_BLOCKS")
        .ok()
        .map(|value| value.parse::<u64>().expect("integer frame count"))
        .unwrap_or(1000);
    assert!((100..=10_000).contains(&blocks));
    let fixture: Arc<[f32]> = (0..RATE as usize)
        .map(|n| voice(n) + noise(n) * 0.005)
        .collect::<Vec<_>>()
        .into();
    for bits in 0..8 {
        if selected.is_some_and(|selected| selected != bits) {
            continue;
        }
        let options = Options::from_bits(bits);
        let capture = capture(options, rate, 1);
        let epoch = capture.control.snapshot().epoch();
        let start = Instant::now();
        let count_before = allocations();
        let bytes_before = live_bytes();
        let heap_high_water_before = peak_bytes();
        let wall = Instant::now();
        let (cpu_ns, worker_allocations, sampled_peak_live, resident_after_init) = if bits == 0 {
            let cpu = thread_cpu_ns();
            let local = allocations_here();
            let mut gain = GainRamp::new(1.);
            for block in 0..blocks {
                let first = block * u64::from(rate) / 100;
                let end = (block + 1) * u64::from(rate) / 100;
                for offset in (first..end).step_by(FRAME) {
                    let len = (end - offset).min(FRAME as u64) as usize;
                    let mut data = [0.; FRAME];
                    for (index, slot) in data[..len].iter_mut().enumerate() {
                        *slot = fixture[((offset + index as u64) * u64::from(RATE)
                            / u64::from(rate)) as usize
                            % fixture.len()];
                    }
                    super::super::devices::push_capture(
                        std::hint::black_box(&data[..len]),
                        1,
                        &capture,
                        &mut gain,
                        1. / (rate as f32 * 0.005),
                    );
                }
            }
            (
                cpu.zip(thread_cpu_ns())
                    .map(|(before, after)| after.saturating_sub(before)),
                allocations_here() - local,
                live_bytes(),
                live_bytes(),
            )
        } else {
            let (sender, receiver) = mpsc::sync_channel(QUEUE);
            let worker_capture = capture.clone();
            let worker = std::thread::spawn(move || {
                let cpu = thread_cpu_ns();
                let local = allocations_here();
                let peak = std::cell::Cell::new(live_bytes());
                let initialized = std::cell::Cell::new(0u64);
                let result = run(
                    &worker_capture.processing,
                    &Arc::downgrade(&worker_capture),
                    &receiver,
                    &AtomicBool::new(false),
                    (options, rate, 1),
                    |at| {
                        let bytes = live_bytes();
                        peak.set(peak.get().max(bytes));
                        if initialized.get() == 0 && worker_capture.control.processing_ready(epoch)
                        {
                            initialized.set(bytes);
                        }
                        at
                    },
                );
                result.expect("offline worker pipeline");
                (
                    cpu.zip(thread_cpu_ns())
                        .map(|(before, after)| after.saturating_sub(before)),
                    allocations_here() - local,
                    peak.get(),
                    initialized.get(),
                )
            });
            for block in 0..blocks {
                if options.echo_cancellation {
                    sender
                        .send(Packet {
                            samples: std::array::from_fn(|index| {
                                fixture[(block as usize * FRAME + index) % fixture.len()]
                            }),
                            len: FRAME,
                            rate: RATE,
                            at: start + Duration::from_millis(block * 10),
                            epoch,
                            render: true,
                        })
                        .expect("bounded render queue");
                }
                let first = block * u64::from(rate) / 100;
                let end = (block + 1) * u64::from(rate) / 100;
                for offset in (first..end).step_by(FRAME) {
                    let len = (end - offset).min(FRAME as u64) as usize;
                    let mut data = [0.; FRAME];
                    for (index, slot) in data[..len].iter_mut().enumerate() {
                        *slot = fixture[((offset + index as u64) * u64::from(RATE)
                            / u64::from(rate)) as usize
                            % fixture.len()];
                    }
                    sender
                        .send(Packet {
                            samples: data,
                            len,
                            rate,
                            at: start + Duration::from_secs_f64(offset as f64 / f64::from(rate)),
                            epoch,
                            render: false,
                        })
                        .expect("bounded capture queue");
                }
            }
            drop(sender);
            worker.join().expect("worker shutdown")
        };
        let elapsed = wall.elapsed();
        let allocation_events = allocations() - count_before;
        let bytes_after = live_bytes();
        let high_water_after = peak_bytes();
        let impulse = impulse_peak_delay(options);
        eprintln!(
            "{}",
            serde_json::json!({
                "benchmark":"sonora_offline", "bits":bits, "noise_suppression":options.noise_suppression,
                "echo_cancellation":options.echo_cancellation, "automatic_gain_control":options.automatic_gain_control,
                "capture_rate":rate, "render_rate":RATE, "capture_channels":1,
                "audio_ms":blocks * 10, "wall_ms":elapsed.as_secs_f64() * 1000., "processing_thread_cpu_ns":cpu_ns,
                "cpu_fraction_one_core":cpu_ns.map(|ns| ns as f64 / (blocks as f64 * 10_000_000.)),
                "global_allocation_events_including_init":allocation_events,
                "processing_thread_allocation_events_including_init":worker_allocations,
                "global_heap_live_before":bytes_before, "global_heap_live_after_worker_shutdown":bytes_after,
                "sampled_global_heap_live_peak_during_worker":sampled_peak_live,
                "global_heap_live_when_pipeline_first_ready":resident_after_init,
                "global_heap_high_water_since_process_start_before":heap_high_water_before,
                "global_heap_high_water_since_process_start_after":high_water_after,
                "queue_capacity_packets":if bits == 0 { 0 } else { QUEUE },
                "queue_payload_capacity_bytes":if bits == 0 { 0 } else { QUEUE * size_of::<Packet>() },
            "core_impulse_peak_response_delay_samples_48k":impulse,
                "processing_frame_ms":if bits == 0 { 0 } else { 10 },
                "sinc_group_delay_ms":if bits != 0 && rate != RATE { 16_000. / f64::from(rate) + 16_000. / f64::from(RATE) } else { 0. },
                "capture_ring_prime_ms":super::super::CAPTURE_PRIME_MS,
                "deadline_policy_ms":MAX_AGE.as_millis(),
                "scope":"Offline virtual packet timestamps, real DSP/converters/bounded worker queue. OFF measures existing capture bypass on the calling thread. ON counts CPU and TLS allocations on the worker itself. Includes initialization; excludes CPAL tap/conversion-to-f32, OS device latency, actual callback scheduling, network, codec and listener audio. Heap counters are NOT RSS/PrivateBytes; high-water marks are cumulative. Impulse peak is an observation, not an end-to-end latency bound. Run --exact --ignored --nocapture --test-threads=1; use SONORA_BENCH_BITS for a fresh-process per-feature high-water mark."
            })
        );
    }
}
