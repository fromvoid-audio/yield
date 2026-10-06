use super::*;
use std::{
    cell::RefCell,
    fs::{self, File},
};

struct Probe {
    source_lock: File,
    volume_lock: File,
    root: std::sync::Weak<BoundDatabaseRoot>,
    shared: std::sync::Weak<OwnerShared>,
    charged_remaining: usize,
    observed: bool,
}
thread_local! { static PROBE: RefCell<Option<Probe>> = const { RefCell::new(None) }; }

pub(super) fn observe_after_entries(_: &RetainedLegacyVolumes) {
    PROBE.with_borrow_mut(|probe| {
        let Some(probe) = probe else {
            return;
        };
        probe
            .volume_lock
            .try_lock()
            .expect("added native volume lock released first");
        probe.volume_lock.unlock().unwrap();
        assert!(
            probe.source_lock.try_lock().is_err(),
            "original source reservation must remain held"
        );
        assert!(
            probe.root.upgrade().is_some(),
            "original retained root must remain alive"
        );
        assert_eq!(
            probe
                .shared
                .upgrade()
                .unwrap()
                .input_bytes
                .lock()
                .unwrap()
                .remaining[0],
            probe.charged_remaining,
            "original and consumer debit must remain charged during native teardown"
        );
        probe.observed = true;
    });
}

#[test]
fn legacy_partial_native_owner_drops_before_original_source_reservation_and_charged_credit() {
    let mut f = super::tests::Fixture::new();
    let source_bytes = f.source_bytes();
    let mut retained = f.runtime.legacy_adoption.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut targets = retained.source.reserve_targets(&f.guard, deadline).unwrap();
    let target = targets.remove(0);
    drop(targets);
    let descriptor = target.root.try_clone_descriptor().unwrap();
    let registry = f.profile.path().join("native-drop-registry");
    fs::create_dir(&registry).unwrap();
    let owner = super::super::super::capacity_volume_owner::acquire_fixture_for_test(
        &descriptor,
        &registry,
        target.volume.clone(),
        f.guard.profile_root_identity(),
    )
    .unwrap();
    let lock_path = registry.join(format!("volume-{}.lock", target.volume.device));
    let marker = fs::read(&lock_path).unwrap();
    assert!(String::from_utf8_lossy(&marker).contains("state=dirty"));
    let volume_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    assert!(volume_lock.try_lock().is_err());
    let source_lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.profile.path().join("legacy/.yield-source-lease.lock"))
        .unwrap();
    assert!(source_lock.try_lock().is_err());
    let metadata = f
        .shared
        .reserve_input(InputClass::Ordinary, RETAINED_BYTES)
        .unwrap();
    PROBE.with_borrow_mut(|probe| {
        assert!(probe.is_none());
        *probe = Some(Probe {
            source_lock,
            volume_lock,
            root: Arc::downgrade(&target.root),
            shared: Arc::downgrade(&f.shared),
            charged_remaining: f.remaining(),
            observed: false,
        });
    });
    retained.volumes = Some(RetainedLegacyVolumes {
        entries: vec![LegacyVolumeEntry::Owned {
            acquisition_failure: None,
            namespace_failure: None,
            reserve: None,
            owner: Some(owner),
            root: target.root,
            volume: target.volume,
            unit: target.unit,
        }],
        original_digest: retained.digest,
        checkpoint: retained.args.clone(),
        ready: false,
        _input: metadata,
    });
    f.runtime.legacy_adoption = Some(retained);
    // Actual original worker teardown; no source lock/root/credit is manufactured by the probe.
    drop(f.runtime.legacy_adoption.take());
    PROBE.with_borrow_mut(|probe| {
        let probe = probe.take().unwrap();
        assert!(probe.observed);
        probe
            .source_lock
            .try_lock()
            .expect("original reservation closes after added backing");
        probe.source_lock.unlock().unwrap();
        assert!(probe.root.upgrade().is_none());
    });
    assert_eq!(f.remaining(), InputBudget::shared_initial().remaining[0]);
    assert_eq!(f.source_bytes(), source_bytes);
    assert_eq!(fs::read(lock_path).unwrap(), marker); // Dropping never marks an ambiguous owner clean.
}
