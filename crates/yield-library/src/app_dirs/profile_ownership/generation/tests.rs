use super::*;

#[test]
fn profile_generation_refuses_zero_exhaustion_and_overflow_without_reset() {
    for invalid in [0, i64::MAX as u64 + 1, u64::MAX] {
        let counter = AtomicU64::new(invalid);
        for _ in 0..2 {
            assert!(matches!(
                issue_from(&counter),
                Err(ProfileOwnershipError::OwnershipGenerationExhausted)
            ));
            assert_eq!(counter.load(Ordering::Relaxed), invalid);
        }
    }
    let counter = AtomicU64::new(i64::MAX as u64);
    assert_eq!(issue_from(&counter).unwrap(), i64::MAX as u64);
    assert!(issue_from(&counter).is_err());
    assert_eq!(counter.load(Ordering::Relaxed), i64::MAX as u64 + 1);
}

#[test]
fn concurrent_profile_generation_issuance_never_reuses_a_serial() {
    let counter = AtomicU64::new(1);
    let mut generations = std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    (0..128)
                        .map(|_| issue_from(&counter).unwrap())
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        threads
            .into_iter()
            .flat_map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    generations.sort_unstable();
    assert_eq!(generations, (1..=1024).collect::<Vec<_>>());
    assert_eq!(counter.load(Ordering::Relaxed), 1025);
}
