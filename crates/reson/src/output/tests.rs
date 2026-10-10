use super::*;
use crate::Source;
use crate::output::callback::{CallbackProbe, SOURCE_CAPACITY, SourceEntry};
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

const INITIAL_COMMAND_GENERATION: u64 = 1;

fn callback_harness() -> (
    CallbackState,
    HeapProd<StreamCommand>,
    HeapCons<SourceEntry>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
    Arc<AtomicU64>,
    Arc<CallbackProbe>,
) {
    let (command_producer, command_consumer) =
        HeapRb::<StreamCommand>::new(SOURCE_CAPACITY).split();
    let (retired_producer, retired_consumer) = HeapRb::<SourceEntry>::new(SOURCE_CAPACITY).split();
    let stream_permits = Arc::new(AtomicUsize::new(0));
    let clear_pending = Arc::new(AtomicBool::new(false));
    let command_generation = Arc::new(AtomicU64::new(INITIAL_COMMAND_GENERATION));
    let probe = Arc::new(CallbackProbe::new());
    let state = CallbackState::new(
        command_consumer,
        retired_producer,
        Arc::new(AtomicU32::new(1.0_f32.to_bits())),
        Arc::new(AtomicUsize::new(0)),
        Arc::clone(&clear_pending),
        Arc::clone(&command_generation),
        Arc::new(AtomicBool::new(false)),
        1,
        Arc::clone(&probe),
    );
    (
        state,
        command_producer,
        retired_consumer,
        stream_permits,
        clear_pending,
        command_generation,
        probe,
    )
}

fn source_entry<S: Source + Send + 'static>(source: S, permits: &Arc<AtomicUsize>) -> SourceEntry {
    match SourceEntry::new(source, Arc::clone(permits)) {
        Ok(entry) => entry,
        Err(_) => panic!("source admission should have capacity"),
    }
}

fn submit<S: Source + Send + 'static>(
    producer: &mut HeapProd<StreamCommand>,
    permits: &Arc<AtomicUsize>,
    generation: u64,
    source: S,
    volume: f32,
) {
    assert!(
        producer
            .try_push(StreamCommand::Append {
                generation,
                source: source_entry(source, permits),
                volume,
            })
            .is_ok()
    );
}

struct ConstantSource {
    value: f32,
    remaining: Option<usize>,
}

impl ConstantSource {
    fn endless(value: f32) -> Self {
        Self {
            value,
            remaining: None,
        }
    }

    fn finite(value: f32, length: usize) -> Self {
        Self {
            value,
            remaining: Some(length),
        }
    }
}

impl Iterator for ConstantSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(remaining) = &mut self.remaining {
            if *remaining == 0 {
                return None;
            }
            *remaining -= 1;
        }
        Some(self.value)
    }
}

impl Source for ConstantSource {
    fn current_frame_len(&self) -> Option<usize> {
        self.remaining
    }

    fn channels(&self) -> u16 {
        1
    }

    fn sample_rate(&self) -> u32 {
        44_100
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

#[test]
fn default_config_has_no_preferences() {
    let cfg = AudioOutputConfig::default();
    assert!(cfg.host.is_none());
    assert!(cfg.device.is_none());
    assert!(cfg.sample_rate.is_none());
    assert!(cfg.buffer_size.is_none());
}

#[test]
fn sample_rate_filter_returns_common_values() {
    assert_eq!(sample_rates_in_range(40_000, 50_000), vec![44_100, 48_000]);
}

#[test]
fn callback_mix_preserves_gain_and_converts_typed_samples() {
    let (mut state, mut commands, mut retired, permits, _, _, probe) = callback_harness();
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::endless(0.5),
        0.5,
    );
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::endless(0.5),
        0.5,
    );

    let mut floats = [0.0_f32; 4];
    process_audio_callback(&mut state, &mut floats);
    assert_eq!(floats, [0.5; 4]);
    assert_eq!(probe.sample_count(0), 4);
    assert_eq!(probe.peak(0), 0.5);

    let mut integers = [0_i16; 2];
    process_audio_callback(&mut state, &mut integers);
    assert_eq!(integers, [16_384; 2]);
    assert_eq!(probe.sample_count(0), 6);
    assert!((probe.peak(0) - 0.5).abs() <= 1.0 / 32_768.0);
    assert!(
        commands
            .try_push(StreamCommand::Clear { generation: 2 })
            .is_ok()
    );
    process_audio_callback(&mut state, &mut floats);
    drop(state);
    while let Some(source) = retired.try_pop() {
        drop(source);
    }
}

fn async_source_from_float_wav(
    samples: &[f32],
) -> (
    Vec<f32>,
    crate::async_decode::AsyncSource<crate::decoder::SymphoniaDecoder>,
) {
    let directory = tempfile::tempdir().expect("temporary WAV directory");
    let path = directory.path().join("non-finite-float.wav");
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 44_100,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut writer = hound::WavWriter::create(&path, spec).expect("create float WAV");
    for sample in samples {
        writer
            .write_sample(*sample)
            .expect("write float WAV sample");
    }
    writer.finalize().expect("finalize float WAV");
    let bytes: Arc<[u8]> = Arc::from(
        std::fs::read(path)
            .expect("read generated float WAV")
            .into_boxed_slice(),
    );

    let mut verification_decoder = crate::decoder::SymphoniaDecoder::from_bytes(Arc::clone(&bytes))
        .expect("decode generated IEEE-float WAV");
    let decoded = (0..samples.len())
        .map(|_| verification_decoder.next().expect("decoded WAV sample"))
        .collect::<Vec<_>>();
    let decoder = crate::decoder::SymphoniaDecoder::from_bytes(bytes)
        .expect("open production decoder source");
    let mut source = crate::async_decode::AsyncSource::with_buffer_seconds(decoder, 0.01);
    let available = source.prefill_for_duration(Duration::from_millis(1), Duration::from_secs(1));
    assert_eq!(
        available,
        samples.len(),
        "production async source prefilled the WAV"
    );
    (decoded, source)
}

#[test]
fn callback_skips_non_finite_float_wav_contributions_and_preserves_healthy_mix() {
    let malformed = [
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ];
    let (decoded, source) = async_source_from_float_wav(&malformed);
    assert!(decoded[0].is_nan());
    assert_eq!(decoded[1], f32::INFINITY);
    assert_eq!(decoded[2], f32::NEG_INFINITY);
    assert!(decoded[3].is_nan());
    assert_eq!(decoded[4], f32::INFINITY);
    assert_eq!(decoded[5], f32::NEG_INFINITY);

    let (mut state, mut commands, mut retired, permits, _, _, probe) = callback_harness();
    submit(&mut commands, &permits, 1, source, 1.0);
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::endless(0.25),
        0.5,
    );

    let mut floats = [0.0_f32; 3];
    state.process_measured(&mut floats);
    assert_eq!(floats, [0.125; 3]);
    assert!(floats.iter().all(|sample| sample.is_finite()));

    let mut integers = [0_i16; 3];
    state.process_measured(&mut integers);
    assert_eq!(integers, [4_096; 3]);

    let mut finish = [0.0_f32; 1];
    state.process_measured(&mut finish);
    assert_eq!(finish, [0.125]);
    assert_eq!(probe.allocation_count(), 0);
    assert_eq!(probe.deallocation_count(), 0);
    while let Some(source) = retired.try_pop() {
        drop(source);
    }
}

#[test]
fn callback_preserves_saturation_when_finite_contributions_overflow() {
    for (sample, expected) in [(f32::MAX, 1.0_f32), (-f32::MAX, -1.0_f32)] {
        let (mut state, mut commands, mut retired, permits, _, _, _) = callback_harness();
        submit(
            &mut commands,
            &permits,
            1,
            ConstantSource::endless(sample),
            1.0,
        );
        submit(
            &mut commands,
            &permits,
            1,
            ConstantSource::endless(sample),
            1.0,
        );

        let mut data = [0.0_f32; 1];
        process_audio_callback(&mut state, &mut data);
        assert_eq!(data, [expected]);
        assert!(data[0].is_finite());
        while let Some(source) = retired.try_pop() {
            drop(source);
        }
    }
}

#[test]
fn callback_muted_non_finite_float_wav_cannot_poison_healthy_source() {
    let malformed = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];
    let (decoded, source) = async_source_from_float_wav(&malformed);
    assert!(decoded[0].is_nan());
    assert_eq!(decoded[1], f32::INFINITY);
    assert_eq!(decoded[2], f32::NEG_INFINITY);

    let (mut state, mut commands, mut retired, permits, _, _, probe) = callback_harness();
    submit(&mut commands, &permits, 1, source, 0.0);
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::endless(0.25),
        0.5,
    );

    let mut data = [0.0_f32; 4];
    state.process_measured(&mut data);
    assert_eq!(data, [0.125; 4]);
    assert!(data.iter().all(|sample| sample.is_finite()));
    assert_eq!(probe.allocation_count(), 0);
    assert_eq!(probe.deallocation_count(), 0);
    while let Some(source) = retired.try_pop() {
        drop(source);
    }
}

#[test]
fn callback_pending_clear_retires_old_generation_and_accepts_new_generation() {
    let (mut state, mut commands, mut retired, permits, clear_pending, generation, _) =
        callback_harness();
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::endless(0.75),
        1.0,
    );
    let mut data = [0.0_f32; 8];
    process_audio_callback(&mut state, &mut data);
    assert_eq!(data, [0.75; 8]);

    assert!(
        commands
            .try_push(StreamCommand::Clear { generation: 2 })
            .is_ok()
    );
    generation.store(2, Ordering::Release);
    clear_pending.store(true, Ordering::Release);
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::endless(0.25),
        1.0,
    );
    process_audio_callback(&mut state, &mut data);
    assert_eq!(data, [0.0; 8]);
    assert_eq!(permits.load(Ordering::Acquire), 2);

    submit(
        &mut commands,
        &permits,
        2,
        ConstantSource::endless(0.5),
        1.0,
    );
    process_audio_callback(&mut state, &mut data);
    assert_eq!(data, [0.5; 8]);
    assert_eq!(permits.load(Ordering::Acquire), 3);

    while let Some(source) = retired.try_pop() {
        drop(source);
    }
    assert_eq!(permits.load(Ordering::Acquire), 1);
    assert!(
        commands
            .try_push(StreamCommand::Clear { generation: 3 })
            .is_ok()
    );
    process_audio_callback(&mut state, &mut data);
    drop(state);
    while let Some(source) = retired.try_pop() {
        drop(source);
    }
    assert_eq!(permits.load(Ordering::Acquire), 0);
}

#[test]
fn callback_retires_completed_source_without_reading_error_on_callback() {
    use std::sync::Mutex;
    use std::thread::{self, ThreadId};

    struct ErrorSource {
        callback_thread: Arc<Mutex<Option<ThreadId>>>,
        error_thread: Arc<Mutex<Option<ThreadId>>>,
        drop_thread: Arc<Mutex<Option<ThreadId>>>,
    }

    impl Iterator for ErrorSource {
        type Item = f32;
        fn next(&mut self) -> Option<Self::Item> {
            *self.callback_thread.lock().unwrap() = Some(thread::current().id());
            None
        }
    }

    impl Source for ErrorSource {
        fn current_frame_len(&self) -> Option<usize> {
            Some(0)
        }
        fn channels(&self) -> u16 {
            1
        }
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
        fn last_error(&self) -> Option<String> {
            *self.error_thread.lock().unwrap() = Some(thread::current().id());
            Some("decode failed".into())
        }
    }

    impl Drop for ErrorSource {
        fn drop(&mut self) {
            *self.drop_thread.lock().unwrap() = Some(thread::current().id());
        }
    }

    let callback_thread = Arc::new(Mutex::new(None));
    let error_thread = Arc::new(Mutex::new(None));
    let drop_thread = Arc::new(Mutex::new(None));
    let (mut state, mut commands, mut retired, permits, _, _, _) = callback_harness();
    let source = crate::loop_diagnostic::LoopDiagnostic::new(
        ErrorSource {
            callback_thread: Arc::clone(&callback_thread),
            error_thread: Arc::clone(&error_thread),
            drop_thread: Arc::clone(&drop_thread),
        },
        1,
    );
    let source = crate::FadeOutOnRequest::new(source, crate::FadeOutHandle::new());
    let source = crate::player::EditFadeSource::new_looped(
        source,
        crate::player::EditFadeHandle::new(),
        0.0,
        1,
        0,
    );
    submit(&mut commands, &permits, 1, source, 1.0);
    let callback_thread_id = std::thread::current().id();
    let mut data = [0.0_f32; 1];
    process_audio_callback(&mut state, &mut data);
    assert!(callback_thread.lock().unwrap().is_some());
    assert!(error_thread.lock().unwrap().is_none());
    assert!(drop_thread.lock().unwrap().is_none());

    let retired = retired.try_pop().expect("completed source is retired");
    std::thread::spawn(move || {
        assert_eq!(retired.last_error().as_deref(), Some("decode failed"));
        drop(retired);
    })
    .join()
    .unwrap();
    let callback_id = callback_thread.lock().unwrap().clone().unwrap();
    let error_id = error_thread.lock().unwrap().clone().unwrap();
    let drop_id = drop_thread.lock().unwrap().clone().unwrap();
    assert_eq!(callback_id, callback_thread_id);
    assert_ne!(error_id, callback_id);
    assert_eq!(drop_id, error_id);
}

#[test]
fn source_credit_is_retained_until_blocked_source_drop_finishes() {
    use crate::output::callback::SourcePermitPool;
    use std::sync::Barrier;
    use std::thread;

    static TEST_POOL: SourcePermitPool = SourcePermitPool::new(1);

    struct BlockedDrop {
        entered: Arc<Barrier>,
        release: Arc<Barrier>,
    }

    impl Iterator for BlockedDrop {
        type Item = f32;
        fn next(&mut self) -> Option<Self::Item> {
            None
        }
    }

    impl Source for BlockedDrop {
        fn current_frame_len(&self) -> Option<usize> {
            Some(0)
        }
        fn channels(&self) -> u16 {
            1
        }
        fn sample_rate(&self) -> u32 {
            44_100
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    impl Drop for BlockedDrop {
        fn drop(&mut self) {
            self.entered.wait();
            self.release.wait();
        }
    }

    let permits = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let entry = SourceEntry::new_with_pool(
        BlockedDrop {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        },
        Arc::clone(&permits),
        &TEST_POOL,
    )
    .unwrap_or_else(|_| panic!("initial test source should receive the only credit"));
    assert_eq!(permits.load(Ordering::Acquire), 1);

    let dropper = thread::spawn(move || drop(entry));
    entered.wait();
    assert_eq!(permits.load(Ordering::Acquire), 1);
    assert!(
        SourceEntry::new_with_pool(
            ConstantSource::endless(0.2),
            Arc::clone(&permits),
            &TEST_POOL,
        )
        .is_err()
    );
    assert_eq!(TEST_POOL.active_count(), 1);
    release.wait();
    dropper.join().unwrap();
    assert_eq!(permits.load(Ordering::Acquire), 0);
    let recovered = SourceEntry::new_with_pool(
        ConstantSource::endless(0.2),
        Arc::clone(&permits),
        &TEST_POOL,
    );
    assert!(
        recovered.is_ok(),
        "credit returns after Source::drop finishes"
    );
    let recovered = match recovered {
        Ok(entry) => entry,
        Err(_) => panic!("source credit should have been returned"),
    };
    drop(recovered);
}

#[test]
fn callback_has_no_allocation_or_deallocation_across_append_clear_and_completion() {
    let (mut state, mut commands, mut retired, permits, _, _, _) = callback_harness();
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::endless(0.25),
        1.0,
    );
    submit(
        &mut commands,
        &permits,
        1,
        ConstantSource::finite(0.5, 1),
        1.0,
    );
    assert!(
        commands
            .try_push(StreamCommand::Clear { generation: 2 })
            .is_ok()
    );
    submit(
        &mut commands,
        &permits,
        2,
        ConstantSource::endless(0.75),
        1.0,
    );
    submit(
        &mut commands,
        &permits,
        2,
        ConstantSource::finite(0.25, 1),
        1.0,
    );
    let mut short = [0.0_f32; 8];
    let mut long = [0.0_f32; 256];
    let mut varying = [0.0_f32; 32];
    let (allocations, deallocations) =
        crate::test_support::count_thread_allocations_and_deallocations(|| {
            process_audio_callback(&mut state, &mut short);
            process_audio_callback(&mut state, &mut long);
            process_audio_callback(&mut state, &mut varying);
            process_audio_callback(&mut state, &mut short);
            process_audio_callback(&mut state, &mut varying);
            process_audio_callback(&mut state, &mut long);
        });
    assert_eq!(allocations, 0);
    assert_eq!(deallocations, 0);
    assert!(
        commands
            .try_push(StreamCommand::Clear { generation: 3 })
            .is_ok()
    );
    process_audio_callback(&mut state, &mut varying);
    drop(state);
    while let Some(source) = retired.try_pop() {
        drop(source);
    }
}

#[test]
fn callback_stays_non_blocking_while_control_producer_holds_its_lock() {
    use std::sync::{Mutex, mpsc};
    use std::thread;

    let (state, mut commands, mut retired, permits, _, _, _) = callback_harness();
    let ui_lock = Arc::new(Mutex::new(()));
    let guard = ui_lock.lock().unwrap();
    let sender_lock = Arc::clone(&ui_lock);
    let sender = thread::spawn(move || {
        let _held = sender_lock.lock().unwrap();
        for _ in 0..128 {
            assert!(
                commands
                    .try_push(StreamCommand::Append {
                        generation: 1,
                        source: source_entry(ConstantSource::endless(0.25), &permits),
                        volume: 1.0,
                    })
                    .is_ok()
            );
        }
    });

    let (completed_sender, completed_receiver) = mpsc::channel();
    let callback = thread::spawn(move || {
        let mut state = state;
        let mut data = [0.0_f32; 64];
        for _ in 0..128 {
            process_audio_callback(&mut state, &mut data);
        }
        completed_sender.send(()).unwrap();
    });
    // The callback must finish while the producer is unable to acquire its
    // unrelated control-side lock. A timeout only bounds a broken test.
    completed_receiver
        .recv_timeout(std::time::Duration::from_millis(200))
        .expect("audio callback waited on the control-side lock");
    drop(guard);
    sender.join().unwrap();
    callback.join().unwrap();
    while let Some(source) = retired.try_pop() {
        drop(source);
    }
}

#[test]
fn resolved_output_uses_fallback_stream_config() {
    let fallback_config = cpal::StreamConfig {
        channels: 1,
        sample_rate: 48_000,
        buffer_size: cpal::BufferSize::Fixed(512),
    };
    let resolved = resolved_output_from_stream_config(
        "fallback_host".to_string(),
        "fallback_device".to_string(),
        &fallback_config,
        true,
    );
    assert_eq!(resolved.sample_rate, 48_000);
    assert_eq!(resolved.channel_count, 1);
    assert_eq!(resolved.buffer_size_frames, Some(512));
    assert!(resolved.used_fallback);
}
