use super::*;
#[test]
fn native_reader_original_counted_origin_and_single_sample_refuse_peer_and_replay() {
    let original = ResourceAdmission::new();
    let peer = ResourceAdmission::new();
    assert!(original.activate(|| true));
    assert!(peer.activate(|| true));
    let mut resource = original.begin_read(|| true).unwrap();
    assert!(matches!(
        resource.begin_observation(&peer),
        Err(UseError::Unknown)
    ));
    let generation = resource.begin_observation(&original).unwrap();
    assert_eq!(generation, resource.generation());
    assert!(matches!(
        resource.begin_observation(&original),
        Err(UseError::Closed)
    ));
    original.close();
    assert!(matches!(
        resource.verify_read_for(&original),
        Err(UseError::Closed)
    ));
    drop(resource);
    assert!(original.close_and_wait());
    assert!(peer.close_and_wait());
}
#[test]
fn native_reader_types_share_only_readonly_observation_and_counter_state() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<NativeReadCapsule>();
    send_sync::<NativeReadFacts>();
}

#[test]
fn native_reader_retained_fact_headers_stay_bounded_after_read_use_release() {
    let budget = FactBudget::default();
    for _ in 0..MAX_FACTS {
        assert!(budget.reserve());
    }
    assert!(!budget.reserve());
    assert!(budget.release());
    assert!(budget.reserve());
    for _ in 0..MAX_FACTS {
        assert!(budget.release());
    }
    assert!(!budget.release());
    assert!(budget.reserve());
    assert!(budget.release());
}

#[test]
fn native_reader_fact_header_overflow_and_corrupt_count_refuse_without_wrap() {
    let budget = FactBudget(AtomicUsize::new(usize::MAX));
    assert!(!budget.reserve());
    assert!(!budget.release());
    assert_eq!(budget.0.load(Ordering::Acquire), usize::MAX);
}
