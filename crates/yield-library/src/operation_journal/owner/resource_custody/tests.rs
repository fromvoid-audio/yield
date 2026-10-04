use super::*;
#[test]
fn resource_custody_unique_bounded_reads_and_effect_upgrade_never_zero_the_count() {
    let admission = ResourceAdmission::new();
    assert!(admission.activate(|| true));
    let mut uses: Vec<_> = (0..MAX_USES)
        .map(|_| admission.begin_read(|| true).unwrap())
        .collect();
    assert!(matches!(admission.begin_read(|| true), Err(UseError::Full)));
    let generation = uses[0].generation;
    uses[0].enter_effect(|| true).unwrap();
    assert_eq!(admission.lock().count, MAX_USES);
    assert!(matches!(
        uses[0].enter_effect(|| true),
        Err(UseError::Unknown)
    ));
    assert!(admission.lock().unknown);
    assert_eq!(uses[0].generation, generation);
    drop(uses);
    assert!(
        !admission.close_and_wait(),
        "quiescence does not settle an entered use"
    );
}
#[test]
fn resource_custody_original_generation_overflow_and_poison_stay_closed() {
    for poison in [false, true] {
        let admission = ResourceAdmission::new();
        assert!(admission.activate(|| true));
        if poison {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = admission.state.lock().unwrap();
                panic!("actual resource mutex poison");
            }));
        } else {
            admission.lock().next = u64::MAX;
        }
        assert!(matches!(
            admission.begin_read(|| true),
            Err(UseError::Unknown)
        ));
        assert!(!admission.close_and_wait());
        assert!(!admission.activate(|| true));
    }
}
#[test]
fn resource_custody_read_refusal_and_closed_upgrade_preserve_unique_release() {
    let admission = ResourceAdmission::new();
    assert!(matches!(
        admission.begin_read(|| true),
        Err(UseError::Closed)
    ));
    assert!(admission.activate(|| true));
    assert!(matches!(
        admission.begin_read(|| false),
        Err(UseError::Closed)
    ));
    assert_eq!(admission.lock().count, 0);
    let mut read = admission.begin_read(|| true).unwrap();
    admission.close();
    assert!(matches!(read.enter_effect(|| true), Err(UseError::Closed)));
    assert_eq!(admission.lock().count, 1);
    drop(read);
    assert!(admission.close_and_wait());
}
#[test]
fn resource_custody_known_startup_close_does_not_become_unknown_or_reactivate() {
    let admission = ResourceAdmission::new();
    admission.close();
    assert!(!admission.activate(|| panic!("closed original admission cannot publish Available")));
    assert!(admission.close_and_wait());
}
#[test]
fn resource_custody_corrupt_count_refuses_without_issuing_an_original_use() {
    let admission = ResourceAdmission::new();
    assert!(admission.activate(|| true));
    admission.lock().count = usize::MAX;
    assert!(matches!(
        admission.begin_read(|| true),
        Err(UseError::Unknown)
    ));
    assert!(admission.lock().uses.iter().all(Option::is_none));
    assert!(admission.lock().unknown);
    // Test-only corrupt scalar input had no actual resource borrower. Restore only its count
    // so the fixture can finish; unknown remains sticky and cannot grant clean handoff.
    admission.lock().count = 0;
    assert!(!admission.close_and_wait());
}
