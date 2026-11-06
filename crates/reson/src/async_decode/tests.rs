use super::*;

#[derive(Clone)]
struct TestSource {
    samples: Vec<f32>,
    pos: usize,
    sample_rate: u32,
    channels: u16,
    delay: Duration,
    error: Option<String>,
    panic_at: Option<usize>,
    start_barrier: Option<Arc<std::sync::Barrier>>,
    start_barrier_waited: bool,
    dropped: Option<Arc<AtomicBool>>,
}

impl Drop for TestSource {
    fn drop(&mut self) {
        if let Some(flag) = self.dropped.as_ref() {
            flag.store(true, Ordering::Release);
        }
    }
}

impl Iterator for TestSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if !self.start_barrier_waited
            && let Some(barrier) = self.start_barrier.as_ref()
        {
            self.start_barrier_waited = true;
            barrier.wait();
        }
        if self.delay > Duration::ZERO {
            thread::sleep(self.delay);
        }
        if self.panic_at == Some(self.pos) {
            panic!("test decode panic");
        }
        if self.pos < self.samples.len() {
            let sample = self.samples[self.pos];
            self.pos += 1;
            Some(sample)
        } else {
            None
        }
    }
}

impl Source for TestSource {
    fn current_frame_len(&self) -> Option<usize> {
        Some(self.samples.len().saturating_sub(self.pos))
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }

    fn last_error(&self) -> Option<String> {
        self.error.clone()
    }
}

fn wait_for_flag(flag: &AtomicBool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if flag.load(Ordering::Acquire) {
            return true;
        }
        thread::sleep(Duration::from_millis(1));
    }
    flag.load(Ordering::Acquire)
}

fn wait_for_non_zero_sample(
    async_source: &mut AsyncSource<TestSource>,
    timeout: Duration,
) -> Option<f32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(sample) = async_source.next()
            && sample != 0.0
        {
            return Some(sample);
        }
        thread::sleep(Duration::from_millis(5));
    }
    None
}

#[test]
fn async_source_emits_samples_after_decode() {
    let source = TestSource {
        samples: vec![0.1, 0.2, 0.3],
        pos: 0,
        sample_rate: 10,
        channels: 1,
        delay: Duration::ZERO,
        error: None,
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };
    let mut async_source = AsyncSource::with_buffer_seconds(source, 1.0);
    let available =
        async_source.prefill_for_duration(Duration::from_millis(300), Duration::from_millis(100));
    assert!(
        available >= 3,
        "expected three prefetched samples, got {available}"
    );
    let mut collected = Vec::with_capacity(3);
    for _ in 0..3 {
        collected.push(async_source.next().expect("prefilled sample"));
    }
    assert_eq!(collected, vec![0.1, 0.2, 0.3]);
}

#[test]
fn async_source_returns_silence_on_underrun() {
    let source = TestSource {
        samples: vec![0.5],
        pos: 0,
        sample_rate: 10,
        channels: 1,
        delay: Duration::from_millis(30),
        error: None,
        panic_at: None,
        start_barrier: Some(Arc::new(std::sync::Barrier::new(2))),
        start_barrier_waited: false,
        dropped: None,
    };
    let start_barrier = source.start_barrier.clone().expect("barrier present");
    let mut async_source = AsyncSource::with_buffer_seconds(source, 0.1);
    let first = async_source.next().unwrap();
    assert_eq!(first, 0.0);
    start_barrier.wait();
    let second =
        wait_for_non_zero_sample(&mut async_source, Duration::from_millis(250)).unwrap_or(0.0);
    assert_eq!(second, 0.5);
}

fn controlled_stereo_ring() -> (ringbuf::HeapProd<f32>, AsyncSource<TestSource>) {
    let (producer, consumer) = HeapRb::<f32>::new(4).split();
    let source = AsyncSource {
        consumer: Some(consumer),
        sample_rate: 48_000,
        channels: 2,
        total_duration: None,
        worker: None,
        done: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
        last_error: Arc::new(Mutex::new(None)),
        output_remaining: 0,
        output_silence: false,
        _marker: PhantomData,
    };
    (producer, source)
}

#[test]
fn async_source_recovers_a_partial_stereo_frame_in_channel_order() {
    let (mut producer, mut source) = controlled_stereo_ring();
    let allocation_counts = crate::test_support::count_thread_allocations_and_deallocations(|| {
        producer.try_push(0.25).unwrap();
        assert_eq!(source.next(), Some(0.0));
        producer.try_push(-0.25).unwrap();
        assert_eq!(source.next(), Some(0.0));
        assert_eq!(source.next(), Some(0.25));
        assert_eq!(source.next(), Some(-0.25));
    });
    assert_eq!(allocation_counts, (0, 0));
}

#[test]
fn async_source_finishes_silence_then_drains_final_frames_and_partial_tail() {
    let (mut producer, mut source) = controlled_stereo_ring();
    assert_eq!(source.next(), Some(0.0));
    for sample in [0.25, -0.25, 0.5] {
        producer.try_push(sample).unwrap();
    }
    source.done.store(true, Ordering::Release);
    assert_eq!(source.current_frame_len(), Some(4));
    assert_eq!(source.next(), Some(0.0));
    assert_eq!(source.current_frame_len(), Some(3));
    assert_eq!(source.next(), Some(0.25));
    assert_eq!(source.next(), Some(-0.25));
    assert_eq!(source.next(), Some(0.5));
    assert_eq!(source.current_frame_len(), Some(0));
    assert_eq!(source.next(), None);
}

#[test]
fn async_source_tiny_buffer_still_prefills_one_stereo_frame() {
    let source = TestSource {
        samples: vec![0.25, -0.25],
        pos: 0,
        sample_rate: 1,
        channels: 2,
        delay: Duration::ZERO,
        error: None,
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };
    let mut source = AsyncSource::with_buffer_seconds(source, 0.01);
    assert_eq!(
        source.prefill_for_duration(Duration::from_secs(1), Duration::from_millis(100)),
        2
    );
    assert_eq!(source.next(), Some(0.25));
    assert_eq!(source.next(), Some(-0.25));
}

#[test]
fn async_source_prefill_waits_for_samples() {
    let source = TestSource {
        samples: vec![0.4],
        pos: 0,
        sample_rate: 10,
        channels: 1,
        delay: Duration::from_millis(5),
        error: None,
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };
    let mut async_source = AsyncSource::with_buffer_seconds(source, 0.1);
    let available =
        async_source.prefill_for_duration(Duration::from_millis(1), Duration::from_millis(100));
    assert!(available >= 1);
    assert_eq!(async_source.next(), Some(0.4));
}

#[test]
fn async_source_buffer_seconds_caps_prefill_window() {
    let source = TestSource {
        samples: (0..100).map(|sample| sample as f32).collect(),
        pos: 0,
        sample_rate: 1_000,
        channels: 1,
        delay: Duration::ZERO,
        error: None,
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };
    let mut async_source = AsyncSource::with_buffer_seconds(source, 0.01);
    let available =
        async_source.prefill_for_duration(Duration::from_millis(100), Duration::from_millis(50));
    assert!(
        available <= 10,
        "tiny stream policy should not prefill beyond its ring buffer, got {available}"
    );
}

#[test]
fn async_source_waits_for_consumer_when_buffer_full() {
    let source = TestSource {
        samples: vec![0.1, 0.2],
        pos: 0,
        sample_rate: 1,
        channels: 1,
        delay: Duration::ZERO,
        error: None,
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };
    let mut async_source = AsyncSource::with_buffer_seconds(source, 0.1);
    let first =
        wait_for_non_zero_sample(&mut async_source, Duration::from_millis(250)).unwrap_or(0.0);
    assert_eq!(first, 0.1);
    let second = wait_for_non_zero_sample(&mut async_source, Duration::from_millis(250));
    assert_eq!(second, Some(0.2));
}

#[test]
fn async_source_propagates_errors() {
    let source = TestSource {
        samples: vec![0.7],
        pos: 0,
        sample_rate: 10,
        channels: 1,
        delay: Duration::ZERO,
        error: Some("decode failed".to_string()),
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };
    let mut async_source = AsyncSource::with_buffer_seconds(source, 1.0);
    thread::sleep(Duration::from_millis(20));
    while async_source.next().is_some() {}
    assert_eq!(async_source.last_error(), Some("decode failed".to_string()));
}

#[test]
fn async_source_drop_requests_worker_stop_and_releases_source() {
    let dropped = Arc::new(AtomicBool::new(false));
    let source = TestSource {
        samples: vec![0.1, 0.2, 0.3, 0.4],
        pos: 0,
        sample_rate: 1,
        channels: 1,
        delay: Duration::ZERO,
        error: None,
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: Some(Arc::clone(&dropped)),
    };

    let async_source = AsyncSource::with_buffer_seconds(source, 0.1);
    thread::sleep(Duration::from_millis(10));
    drop(async_source);

    assert!(
        wait_for_flag(&dropped, Duration::from_millis(100)),
        "decoder worker should observe stop and drop the source promptly"
    );
}

#[test]
fn async_source_reports_worker_panics_and_ends_stream() {
    let source = TestSource {
        samples: vec![0.9],
        pos: 0,
        sample_rate: 10,
        channels: 1,
        delay: Duration::ZERO,
        error: None,
        panic_at: Some(1),
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };

    let mut async_source = AsyncSource::with_buffer_seconds(source, 1.0);
    assert_eq!(
        async_source.prefill_for_duration(Duration::from_millis(5), Duration::from_millis(50)),
        1
    );
    assert_eq!(async_source.next(), Some(0.9));
    for _ in 0..50 {
        if async_source.next().is_none() {
            break;
        }
        thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(async_source.next(), None);
    let error = async_source
        .last_error()
        .expect("worker panic should be reported");
    assert!(error.starts_with("Async decode thread panicked:"));
}

#[test]
fn decoder_credit_remains_held_until_detached_worker_really_exits() {
    static TEST_POOL: DecoderWorkerPool = DecoderWorkerPool::new(1);

    struct BlockedSource {
        entered: Arc<std::sync::Barrier>,
        release: Arc<std::sync::Barrier>,
        dropped: Arc<AtomicBool>,
    }

    impl Iterator for BlockedSource {
        type Item = f32;

        fn next(&mut self) -> Option<Self::Item> {
            self.entered.wait();
            self.release.wait();
            None
        }
    }

    impl Source for BlockedSource {
        fn current_frame_len(&self) -> Option<usize> {
            None
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

    impl Drop for BlockedSource {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    let entered = Arc::new(std::sync::Barrier::new(2));
    let release = Arc::new(std::sync::Barrier::new(2));
    let dropped = Arc::new(AtomicBool::new(false));
    let source = BlockedSource {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        dropped: Arc::clone(&dropped),
    };
    let async_source = AsyncSource::with_buffer_seconds_from_pool(source, 0.1, &TEST_POOL);
    entered.wait();
    drop(async_source);

    assert_eq!(TEST_POOL.active.load(Ordering::Acquire), 1);
    let rejected_source = TestSource {
        samples: vec![0.1],
        pos: 0,
        sample_rate: 44_100,
        channels: 1,
        delay: Duration::ZERO,
        error: None,
        panic_at: None,
        start_barrier: None,
        start_barrier_waited: false,
        dropped: None,
    };
    let rejected = AsyncSource::with_buffer_seconds_from_pool(rejected_source, 0.1, &TEST_POOL);
    assert_eq!(
        rejected.last_error().as_deref(),
        Some("Async decode worker capacity reached")
    );
    assert_eq!(TEST_POOL.active.load(Ordering::Acquire), 1);

    release.wait();
    assert!(wait_for_flag(&dropped, Duration::from_secs(1)));
    let deadline = Instant::now() + Duration::from_secs(1);
    while TEST_POOL.active.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(TEST_POOL.active.load(Ordering::Acquire), 0);
    drop(rejected);
}
