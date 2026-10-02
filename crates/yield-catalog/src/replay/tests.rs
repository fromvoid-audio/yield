//! Real worker/owned SQLite regressions; injected producer evidence is not native qualification.
use super::*;
use yield_library::sample_sources::reconciliation::WatcherGeneration;
use yield_source_watcher::replay::{ReplayEventCondition, ReplayIngress, ReplayPathKind};
mod failures;
mod fixture;
mod namespace;
mod recovery;
mod subtree;
use fixture::Fixture;

#[test]
fn ordered_audio_index_commit_and_duplicate_are_atomic() {
    let f = Fixture::new(true);
    let service = f.start();
    f.put("one.wav", b"changed audio");
    f.put("notes.txt", b"changed notes");
    let batch = f.batch(10, &[(Some(14), "one.wav"), (Some(19), "notes.txt")]);
    let before = f.read().get_revision().unwrap();
    let request = service.submit_replay(batch.clone(), 7, before).unwrap();
    let CatalogReplayOutcome::Committed { receipt, delta } = Fixture::receive(&request) else {
        panic!("expected atomic replay commit");
    };
    assert_eq!(delta.revision, before + 1);
    assert!(delta.authoritative_snapshot.is_none());
    assert_eq!(delta.touched_path_changes.len(), 1);
    assert_eq!(
        delta
            .source_index_commit
            .as_ref()
            .unwrap()
            .upserted_entries
            .len(),
        1
    );
    assert_eq!(receipt.prior().acknowledged_sequence(), 19);
    assert!(receipt.matches_interval(11, 19));
    assert!(!receipt.matches_interval(12, 19));
    assert_eq!(receipt.source_revision(), delta.revision);
    assert_eq!(f.read().get_revision().unwrap(), delta.revision);
    let entry = f
        .read()
        .entry_for_path(Path::new("one.wav"))
        .unwrap()
        .unwrap();
    assert!(entry.looped && entry.locked && entry.tag_named);
    assert_eq!(entry.user_tag.as_deref(), Some("retained curation"));
    let duplicate = service.submit_replay(batch, 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&duplicate),
        CatalogReplayOutcome::PreviouslyCommitted { .. }
    ));
    assert_eq!(f.read().get_revision().unwrap(), delta.revision);
    drop(request);
    drop(duplicate);
    f.put("one.wav", b"second change");
    let next = service
        .submit_replay(f.batch(19, &[(Some(25), "one.wav")]), 7, delta.revision)
        .unwrap();
    assert!(matches!(
        Fixture::receive(&next),
        CatalogReplayOutcome::Committed { .. }
    ));
    service.shutdown();
}

#[test]
fn restart_and_lost_receiver_use_persisted_receipt_without_new_delta() {
    let f = Fixture::new(true);
    let service = f.start();
    f.put("one.wav", b"lost response");
    let batch = f.batch(10, &[(Some(17), "one.wav")]);
    let before = f.read().get_revision().unwrap();
    let lost = service.submit_replay(batch.clone(), 7, before).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.read().get_revision().unwrap() == before {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(2));
    }
    drop(lost); // Discard the public response after actual SQL commit, before receiving it.
    let retry = service.submit_replay(batch.clone(), 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&retry),
        CatalogReplayOutcome::PreviouslyCommitted { .. }
    ));
    assert_eq!(f.read().get_revision().unwrap(), before + 1);
    drop(retry);
    service.shutdown();
    let restarted = f.start();
    let retry = restarted.submit_replay(batch, 7, before).unwrap();
    assert!(matches!(
        Fixture::receive(&retry),
        CatalogReplayOutcome::PreviouslyCommitted { .. }
    ));
    assert_eq!(f.read().get_revision().unwrap(), before + 1);
    restarted.shutdown();
}

#[test]
fn queued_cancellation_and_stale_lifecycle_do_not_mutate() {
    let f = Fixture::new(true);
    let (service, gate) = f.start_with_gate();
    let pause = gate.pause_next();
    let before = f.read().get_revision().unwrap();
    f.put("one.wav", b"must not commit");
    let batch = f.batch(10, &[(Some(11), "one.wav")]);
    let canceled = service.submit_replay(batch.clone(), 7, before).unwrap();
    canceled.cancel();
    let stale = service.submit_replay(batch, 6, before).unwrap();
    drop(pause);
    assert!(matches!(
        Fixture::receive(&canceled),
        CatalogReplayOutcome::Retained {
            reason: CatalogReplayError::Canceled,
            ..
        }
    ));
    assert!(matches!(
        Fixture::receive(&stale),
        CatalogReplayOutcome::Retained {
            reason: CatalogReplayError::StaleLifecycle,
            ..
        }
    ));
    assert_eq!(f.read().get_revision().unwrap(), before);
    service.shutdown();
}

#[test]
fn missing_cursor_and_stale_revision_retain_durable_uncertainty() {
    for missing_cursor in [true, false] {
        let f = Fixture::new(true);
        let (service, gate) = f.start_with_gate();
        let before = f.read().get_revision().unwrap();
        f.put("one.wav", b"last good must remain");
        let batch = f.batch(
            10,
            &[(if missing_cursor { None } else { Some(11) }, "one.wav")],
        );
        let request = service
            .submit_replay(
                batch.clone(),
                7,
                if missing_cursor { before } else { before + 9 },
            )
            .unwrap();
        assert!(matches!(
            Fixture::receive(&request),
            CatalogReplayOutcome::AuditRequired { .. }
        ));
        let pause = gate.pause_next();
        assert_eq!(f.read().get_revision().unwrap(), before);
        assert!(
            f.read()
                .read_durable_replay_receipt(
                    &f.source_id(),
                    &f.identity(),
                    WatcherGeneration::new(7),
                    WatcherGeneration::new(7)
                )
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            f.read().read_durable_replay_uncertainty().unwrap(),
            yield_library::sample_sources::db::DurableReplayUncertaintyState::Retained(_)
        ));
        drop(pause);
        let retry = service.submit_replay(batch, 7, before).unwrap();
        assert!(matches!(
            Fixture::receive(&retry),
            CatalogReplayOutcome::AuditRequired { .. }
        ));
        service.shutdown();
    }
}
