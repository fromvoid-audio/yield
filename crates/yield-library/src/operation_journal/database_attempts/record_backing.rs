//! One actual cloned String witness, not aggregate record or process allocations.
thread_local! {
    pub(crate) static INITIAL_LABEL_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static HISTORICAL_LABEL_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn observe(
    original: &super::StoredOperationRecord,
    copied: &super::StoredOperationRecord,
    initial: bool,
) {
    let original = &original.intent.label;
    let copied = &copied.intent.label;
    assert_eq!(original, copied);
    assert!(!copied.is_empty());
    assert_ne!(original.as_ptr(), copied.as_ptr());
    let counter = if initial {
        &INITIAL_LABEL_CAPACITY
    } else {
        &HISTORICAL_LABEL_CAPACITY
    };
    counter.with(|n| n.set(n.get().checked_add(copied.capacity()).unwrap()));
}
