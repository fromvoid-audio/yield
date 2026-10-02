//! Real temporary source SQLite exercises existing production progress callbacks.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn progress_large_source_counts_targets_without_labeling_row_visits_as_files_or_jobs() {
    let (_root, mut connection) = open_fixture();
    let mut targets = Vec::new();
    for i in 0..7232 {
        for stage in [
            ReadinessStage::IndexedIdentity,
            ReadinessStage::AnalysisFeatures,
            ReadinessStage::EmbeddingAspects,
        ] {
            targets.push(file_target(&format!("file-{i}"), stage, 1));
        }
    }
    targets.push(ReadinessTarget::source(
        SOURCE_ID,
        ReadinessStage::SimilarityLayout,
        "layout-v1",
        1,
        "membership-v1",
    ));
    connection.execute_batch("BEGIN").unwrap();
    sync_manifest(&connection, &targets);
    connection.execute_batch("COMMIT").unwrap();
    replace_readiness_targets(
        &mut connection,
        SOURCE_ID,
        1,
        1,
        SourceAvailability::Active,
        &targets,
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut next = 0;
    let mut inspecting = 0;
    let snapshot = ReadinessStore::new(&mut connection)
        .reconcile_with_cancel_and_progress(SOURCE_ID, 2, &cancel, &mut |event| match event {
            ReadinessProgress::Inspecting => inspecting += 1,
            ReadinessProgress::ComparingTargets { completed, total } => {
                assert_eq!(total, 21697);
                assert_eq!(completed, next);
                next += 1;
            }
            _ => panic!("comparison must not claim queued jobs"),
        })
        .unwrap();
    assert!(inspecting > 0);
    assert_eq!(snapshot.entries.len(), 21697);
    assert_eq!(next, 21698); // zero plus each actually classified target
    let deficits = &snapshot.deficits[..snapshot.deficits.len().min(64)];
    let mut next = 0;
    let changed = ReadinessStore::new(&mut connection)
        .persist_deficits_with_cancel_and_progress(deficits, 3, &cancel, &mut |event| {
            let ReadinessProgress::QueueingTargets { completed, total } = event else {
                panic!("queue phase")
            };
            assert_eq!(total, deficits.len());
            assert_eq!(completed, next);
            next += 1;
        })
        .unwrap();
    assert_eq!(next, deficits.len() + 1);
    // Checked targets can already have work. They are never a newly queued job count.
    let mut final_count = 0;
    assert_eq!(
        ReadinessStore::new(&mut connection)
            .persist_deficits_with_cancel_and_progress(deficits, 4, &cancel, &mut |event| {
                if let ReadinessProgress::QueueingTargets { completed, .. } = event {
                    final_count = completed;
                }
            })
            .unwrap(),
        0
    );
    assert_eq!(final_count, deficits.len());
    assert!(changed <= final_count);
}

#[test]
fn progress_empty_manifest_counts_source_target_and_zero_queue_without_a_recount() {
    let (_root, mut connection) = open_fixture();
    replace_readiness_targets(
        &mut connection,
        SOURCE_ID,
        1,
        1,
        SourceAvailability::Active,
        &complete_targets(1, &[]),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut events = Vec::new();
    let snapshot = ReadinessStore::new(&mut connection)
        .reconcile_with_cancel_and_progress(SOURCE_ID, 2, &cancel, &mut |p| events.push(p))
        .unwrap();
    assert_eq!(snapshot.entries.len(), 1); // the source-level similarity target is not a file
    assert_eq!(
        events.last(),
        Some(&ReadinessProgress::ComparingTargets {
            completed: 1,
            total: 1
        })
    );
    events.clear();
    assert_eq!(
        ReadinessStore::new(&mut connection)
            .persist_deficits_with_cancel_and_progress(&[], 3, &cancel, &mut |p| events.push(p))
            .unwrap(),
        0
    );
    assert_eq!(
        events,
        vec![ReadinessProgress::QueueingTargets {
            completed: 0,
            total: 0
        }]
    );
}

#[test]
fn progress_duplicate_deficits_count_one_distinct_checked_target() {
    let (_root, mut connection) = open_fixture();
    let target = file_target("one", ReadinessStage::AnalysisFeatures, 1);
    sync_manifest(
        &connection,
        &complete_targets(1, std::slice::from_ref(&target)),
    );
    replace_readiness_targets(
        &mut connection,
        SOURCE_ID,
        1,
        1,
        SourceAvailability::Active,
        &complete_targets(1, &[target]),
        1,
    )
    .unwrap();
    let snapshot = reconcile_readiness(&connection, SOURCE_ID, 2).unwrap();
    let deficits = vec![snapshot.deficits[0].clone(); 3];
    let mut events = Vec::new();
    let changed = ReadinessStore::new(&mut connection)
        .persist_deficits_with_cancel_and_progress(
            &deficits,
            3,
            &AtomicBool::new(false),
            &mut |p| events.push(p),
        )
        .unwrap();
    assert_eq!(changed, 1);
    assert_eq!(
        events,
        vec![
            ReadinessProgress::QueueingTargets {
                completed: 0,
                total: 1
            },
            ReadinessProgress::QueueingTargets {
                completed: 1,
                total: 1
            }
        ]
    );
}

#[test]
fn progress_comparison_cancels_at_the_next_existing_target_checkpoint() {
    let (_root, mut connection) = open_fixture();
    let targets: Vec<_> = (0..128)
        .map(|i| file_target(&format!("cancel-{i}"), ReadinessStage::AnalysisFeatures, 1))
        .collect();
    sync_manifest(&connection, &complete_targets(1, &targets));
    replace_readiness_targets(
        &mut connection,
        SOURCE_ID,
        1,
        1,
        SourceAvailability::Active,
        &complete_targets(1, &targets),
        1,
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let mut last = 0;
    assert!(
        ReadinessStore::new(&mut connection)
            .reconcile_with_cancel_and_progress(SOURCE_ID, 2, &cancel, &mut |p| {
                if let ReadinessProgress::ComparingTargets { completed, .. } = p {
                    last = completed;
                    if completed == 32 {
                        cancel.store(true, Ordering::Release);
                    }
                }
            })
            .is_err()
    );
    assert_eq!(last, 32);
}

#[test]
fn progress_cancelled_queue_reports_no_checked_targets_and_rolls_back() {
    let (_root, mut connection) = open_fixture();
    let target = file_target("cancel-queue", ReadinessStage::AnalysisFeatures, 1);
    sync_manifest(
        &connection,
        &complete_targets(1, std::slice::from_ref(&target)),
    );
    replace_readiness_targets(
        &mut connection,
        SOURCE_ID,
        1,
        1,
        SourceAvailability::Active,
        &complete_targets(1, &[target]),
        1,
    )
    .unwrap();
    let snapshot = reconcile_readiness(&connection, SOURCE_ID, 2).unwrap();
    let cancel = AtomicBool::new(false);
    let mut events = Vec::new();
    assert!(
        ReadinessStore::new(&mut connection)
            .persist_deficits_with_cancel_and_progress(&snapshot.deficits, 3, &cancel, &mut |p| {
                events.push(p);
                cancel.store(true, Ordering::Release);
            })
            .is_err()
    );
    assert_eq!(
        events,
        vec![ReadinessProgress::QueueingTargets {
            completed: 0,
            total: 1
        }]
    );
    assert!(connection.is_autocommit());
    assert_eq!(
        persist_readiness_deficits(&mut connection, &snapshot.deficits, 4).unwrap(),
        1
    );
}

#[test]
fn progress_failed_target_persistence_does_not_report_it_checked() {
    let (_root, mut connection) = open_fixture();
    let target = file_target("queue-fault", ReadinessStage::AnalysisFeatures, 1);
    replace(&mut connection, 1, &[target]);
    let snapshot = reconcile_readiness(&connection, SOURCE_ID, 2).unwrap();
    connection.execute_batch("CREATE TRIGGER progress_abort BEFORE INSERT ON analysis_jobs BEGIN SELECT RAISE(ABORT,'forced queue failure'); END;").unwrap();
    let mut events = Vec::new();
    assert!(
        ReadinessStore::new(&mut connection)
            .persist_deficits_with_cancel_and_progress(
                &snapshot.deficits,
                3,
                &AtomicBool::new(false),
                &mut |event| events.push(event)
            )
            .is_err()
    );
    assert_eq!(
        events,
        vec![ReadinessProgress::QueueingTargets {
            completed: 0,
            total: 1
        }]
    );
    assert!(connection.is_autocommit());
    connection
        .execute_batch("DROP TRIGGER progress_abort")
        .unwrap();
    assert_eq!(
        persist_readiness_deficits(&mut connection, &snapshot.deficits, 4).unwrap(),
        1
    );
}
