//! Bounded metadata workload evidence through the real retained source owner.
//! This does not qualify physical file operations or journal capacity admission.

use std::{fs, path::Path, time::Instant};

use serde_json::{Value, json};
use yield_library::operation_journal::{OperationId, ParticipantId};
use yield_library::sample_sources::reconciliation::RootRelativePath;
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{
        Rating, SourceCommandId, SourceCurationUpdate, SourceDatabase, SourceId, SourceRevision,
        SourceSnapshotOwner, SourceSnapshotPage, SourceWriter, SourceWriterError,
        SourceWriterRequest,
    },
};

const ROWS: usize = 4096;
const BATCH: usize = 64;
const SOURCE: &str = "source-owner-workload-v1";

fn row(index: usize) -> (String, u64, i64) {
    (
        format!("fixture-{index:05}.wav"),
        44 + index as u64 * 2,
        1000 + index as i64,
    )
}

fn hash_row(hash: &mut blake3::Hasher, path: &Path, size: u64, modified: i64) {
    hash.update(path.to_str().expect("fixture UTF-8 path").as_bytes());
    hash.update(&[0]);
    hash.update(&size.to_le_bytes());
    hash.update(&modified.to_le_bytes());
}

fn distribution(samples: &[u64]) -> Value {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    json!({"unit": "ns", "samples": samples, "count": sorted.len(),
        "p50": sorted[(sorted.len() - 1) / 2],
        "p95": sorted[(sorted.len() * 95).div_ceil(100) - 1],
        "max": sorted.last().unwrap()})
}

fn elapsed(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_nanos()).expect("bounded duration")
}

fn pages(owner: &SourceSnapshotOwner, revision: &SourceRevision) -> (String, Vec<u64>) {
    let mut cursor = None;
    let mut hash = blake3::Hasher::new();
    let mut count = 0;
    let mut timings = Vec::new();
    // The bound is explicit even if a broken cursor fails to advance.
    for _ in 0..=ROWS / BATCH {
        let start = Instant::now();
        let result = owner
            .read_browser_metadata_page(revision, cursor.as_ref(), BATCH)
            .expect("owner page");
        timings.push(elapsed(start));
        let SourceSnapshotPage::FreshAsOf {
            revision: observed,
            page,
        } = result
        else {
            panic!("unexpected stale page at current owner revision");
        };
        assert_eq!(&observed, revision);
        assert_eq!(page.revision, revision.revision());
        assert!(page.files.len() <= BATCH);
        for file in page.files {
            let (expected_path, size, modified) = row(count);
            assert_eq!(file.relative_path, Path::new(&expected_path));
            assert_eq!(file.file_size, size);
            assert_eq!(file.modified_ns, modified);
            assert!(!file.missing);
            hash_row(&mut hash, &file.relative_path, size, modified);
            count += 1;
        }
        if page.next_cursor.is_none() {
            assert_eq!(count, ROWS);
            return (hash.finalize().to_hex().to_string(), timings);
        }
        assert_ne!(cursor, page.next_cursor, "keyset cursor must advance");
        cursor = page.next_cursor;
    }
    panic!("page traversal exceeded fixed fixture bound");
}

fn storage_observation(writer: &SourceWriter, revision: &SourceRevision) -> Value {
    match writer.capture_tag_metadata_budget(revision) {
        Ok(budget) => {
            json!({"status": "measured", "main_retained_vfs_bytes": budget.resident_database_bytes(),
            "wal_retained_vfs_bytes": budget.resident_wal_bytes(), "shm_root_observed_bytes": budget.resident_shm_bytes(),
            "logical_page_count": budget.page_count(), "page_size": budget.page_size(), "capacity_claim": false})
        }
        Err(error) => {
            eprintln!("retained storage observation unavailable: {error}");
            json!({"status": "unavailable", "reason": "retained-storage-observation-unavailable", "capacity_claim": false})
        }
    }
}

// Whole owner-call boundaries, not independently attributed SQL or sync durations.
fn curation_receipts(
    mut writer: SourceWriter,
    media: &Path,
    profile: &WritableProfileGuard,
) -> Value {
    let initial = writer.current_revision().expect("curation owner fence");
    let updates = (0..BATCH)
        .map(|index| SourceCurationUpdate::SetRating {
            path: RootRelativePath::try_from_path(row(index).0.into()).unwrap(),
            rating: Rating::KEEP_3,
        })
        .collect::<Vec<_>>();
    let request = SourceWriterRequest::new(
        OperationId::from_uuid(uuid::Uuid::from_u128(100)),
        ParticipantId::from_uuid(uuid::Uuid::from_u128(101)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(102)),
        initial.clone(),
        updates,
    )
    .expect("bounded production curation request");
    let started = Instant::now();
    let receipt = writer
        .commit_curation(request.clone())
        .expect("actual persisted receipt");
    let commit_ns = elapsed(started);
    let committed = writer.current_revision().unwrap();
    assert_eq!(committed.revision(), initial.revision() + 1);
    assert_eq!(receipt.source_revision(), &committed);
    assert_eq!(receipt.curation().files().len(), BATCH);
    for (index, file) in receipt.curation().files().iter().enumerate() {
        assert_eq!(file.relative_path, Path::new(&row(index).0));
        assert_eq!(file.rating, Rating::KEEP_3);
    }
    let started = Instant::now();
    assert_eq!(writer.commit_curation(request.clone()).unwrap(), receipt);
    let replay_ns = elapsed(started);
    assert_eq!(writer.current_revision().unwrap(), committed);
    let conflicting = SourceWriterRequest::new(
        request.identity().operation_id(),
        request.identity().participant_id(),
        request.identity().command_id(),
        initial.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: RootRelativePath::try_from_path(row(0).0.into()).unwrap(),
            locked: true,
        }],
    )
    .unwrap();
    let started = Instant::now();
    assert!(matches!(
        writer.commit_curation(conflicting),
        Err(SourceWriterError::ConflictingCommand { .. })
    ));
    let conflict_ns = elapsed(started);
    let stale = SourceWriterRequest::new(
        OperationId::from_uuid(uuid::Uuid::from_u128(103)),
        ParticipantId::from_uuid(uuid::Uuid::from_u128(104)),
        SourceCommandId::from_uuid(uuid::Uuid::from_u128(105)),
        initial.clone(),
        vec![SourceCurationUpdate::SetLocked {
            path: RootRelativePath::try_from_path(row(0).0.into()).unwrap(),
            locked: true,
        }],
    )
    .unwrap();
    let started = Instant::now();
    assert!(matches!(
        writer.commit_curation(stale),
        Err(SourceWriterError::StaleRevision { .. })
    ));
    let stale_ns = elapsed(started);
    assert_eq!(writer.current_revision().unwrap(), committed);
    let storage_after_commit = storage_observation(&writer, &committed);
    let started = Instant::now();
    writer.close().expect("curation owner closes");
    let close_ns = elapsed(started);
    let started = Instant::now();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        media,
        profile,
        Path::new("source-workload"),
    )
    .unwrap();
    let mut writer = SourceWriter::new(database, SourceId::from_string(SOURCE), 3).unwrap();
    let current = writer.current_revision().unwrap();
    let reopen_ns = elapsed(started);
    assert_eq!(current.database_identity(), committed.database_identity());
    assert_eq!(current.revision(), committed.revision());
    assert_ne!(current.lease_epoch(), committed.lease_epoch());
    let started = Instant::now();
    assert_eq!(
        writer
            .read_receipt_observation(request.identity())
            .unwrap()
            .unwrap(),
        receipt
    );
    let historical_observation_ns = elapsed(started);
    let started = Instant::now();
    assert!(matches!(
        writer.commit_curation(request),
        Err(SourceWriterError::SupersededLifecycle { .. })
    ));
    let superseded_rejection_ns = elapsed(started);
    assert_eq!(writer.current_revision().unwrap(), current);
    let owner = SourceSnapshotOwner::start(writer).unwrap();
    let started = Instant::now();
    let page = owner
        .read_browser_metadata_page(&current, None, BATCH)
        .unwrap();
    let first_selection_ns = elapsed(started);
    let SourceSnapshotPage::FreshAsOf { revision, page } = page else {
        panic!("current bounded selection");
    };
    assert_eq!(revision, current);
    assert_eq!(page.files.len(), BATCH);
    for (index, file) in page.files.iter().enumerate() {
        assert_eq!(file.relative_path, Path::new(&row(index).0));
        assert_eq!(file.rating, Rating::KEEP_3);
        assert!(
            !file.locked,
            "rejected requests cannot leak metadata effects"
        );
    }
    let started = Instant::now();
    let next = owner
        .read_browser_metadata_page(&current, page.next_cursor.as_ref(), 1)
        .unwrap();
    let persisted_selection_ns = first_selection_ns.checked_add(elapsed(started)).unwrap();
    let SourceSnapshotPage::FreshAsOf {
        revision,
        page: next,
    } = next
    else {
        panic!("current continuation");
    };
    assert_eq!(revision, current);
    assert_eq!(next.files.len(), 1);
    assert_eq!(next.files[0].relative_path, Path::new(&row(BATCH).0));
    assert_eq!(next.files[0].rating, Rating::NEUTRAL);
    assert!(!next.files[0].locked);
    owner.begin_retirement().join().unwrap().close().unwrap();
    json!({"selected_rows": BATCH, "expected_revision": initial.revision(),
        "committed_revision": committed.revision(), "reopened_revision": current.revision(),
        "exact_replay": true, "conflict_rejected": true, "stale_rejected": true,
        "historical_receipt_equal": true, "superseded_request_rejected": true,
        "selected_metadata_persisted": true, "first_unselected_unchanged": true, "storage_after_commit": storage_after_commit,
        "timings": {"commit_ns": commit_ns, "exact_replay_ns": replay_ns,
            "conflict_rejection_ns": conflict_ns, "stale_rejection_ns": stale_ns,
            "close_ns": close_ns, "reopen_ns": reopen_ns,
            "historical_observation_ns": historical_observation_ns,
            "superseded_rejection_ns": superseded_rejection_ns,
            "persisted_selection_ns": persisted_selection_ns}})
}

#[test]
#[ignore = "explicit bounded measurement: run through run_source_owner_workload.py"]
fn retained_source_owner_workload() {
    let temp_root = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary root");
    let temp = tempfile::tempdir_in(temp_root).expect("isolated workload");
    let media = temp.path().join("media");
    fs::create_dir(&media).expect("fixture media root");
    let profile_path = temp.path().join("profile");
    let _app_root = AppRootGuard::set(profile_path.clone());
    let profile = WritableProfileGuard::acquire_current().expect("actual profile ownership");
    let open_start = Instant::now();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        Path::new("source-workload"),
    )
    .expect("actual profile-bound database");
    let writer = SourceWriter::new_for_scan(database, SourceId::from_string(SOURCE), 1)
        .expect("retained writer");
    let open_ns = elapsed(open_start);
    let initial_revision = writer.current_scan_fence().unwrap().revision();
    let mut mutations = Vec::new();
    let mut fixture_hash = blake3::Hasher::new();
    for start_row in (0..ROWS).step_by(BATCH) {
        let fence = writer.current_scan_fence().expect("scan fence");
        let start = Instant::now();
        let mut batch = writer
            .catalog_database()
            .write_batch()
            .expect("owned batch");
        for index in start_row..start_row + BATCH {
            let (path, size, modified) = row(index);
            batch
                .upsert_file_with_hash(
                    Path::new(&path),
                    size,
                    modified,
                    &format!("fixture-content-{index:05}"),
                )
                .expect("fixture manifest row");
            hash_row(&mut fixture_hash, Path::new(&path), size, modified);
        }
        let receipt = writer
            .commit_scan_manifest_batch(&fence, batch)
            .expect("fenced owner commit");
        mutations.push(elapsed(start));
        assert_eq!(receipt.revision, fence.revision() + 1);
        assert_eq!(receipt.touched_path_changes.len(), BATCH);
        assert!(
            receipt.authoritative_snapshot.is_none(),
            "no extra full-table measurement pass"
        );
    }
    let audit_start = Instant::now();
    let generation = writer
        .begin_scan_directory_truth_generation(0)
        .expect("empty-directory fixture staging");
    let fence = writer.current_scan_fence().expect("audit fence");
    let batch = writer
        .catalog_database()
        .write_batch()
        .expect("audit batch");
    let (_, manifest) = writer
        .commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            2000,
            fence.root_identity().clone(),
            None,
        )
        .expect("actual owner audit completion");
    let audit_ns = elapsed(audit_start);
    assert_eq!(manifest.len(), ROWS);
    drop(manifest);
    let revision = writer.current_revision().expect("audited revision");
    let storage = storage_observation(&writer, &revision);
    let owner = SourceSnapshotOwner::start(writer).expect("existing bounded worker transport");
    let (first_hash, first_pages) = pages(&owner, &revision);
    let page_queue = owner.queue_metrics();
    assert_eq!(page_queue.accepted, first_pages.len() as u64);
    assert_eq!(page_queue.dequeued, first_pages.len() as u64);
    assert_eq!(page_queue.pending_dispatch, 0);
    let close_start = Instant::now();
    owner
        .begin_retirement()
        .join()
        .expect("worker joins")
        .close()
        .expect("clean owner close");
    let close_ns = elapsed(close_start);
    let reopen_start = Instant::now();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        &media,
        &profile,
        Path::new("source-workload"),
    )
    .expect("reopen same profile database");
    let writer =
        SourceWriter::new(database, SourceId::from_string(SOURCE), 2).expect("new lifecycle owner");
    let reopened_revision = writer.current_revision().expect("persisted audit");
    assert_eq!(
        revision.database_identity(),
        reopened_revision.database_identity()
    );
    assert_eq!(revision.revision(), reopened_revision.revision());
    let owner = SourceSnapshotOwner::start(writer).expect("reopened worker");
    let reopen_ns = elapsed(reopen_start);
    assert!(
        matches!(owner.read_browser_metadata_page(&revision, None, BATCH).expect("old observation"),
        SourceSnapshotPage::Stale { current } if current == reopened_revision)
    );
    let (second_hash, second_pages) = pages(&owner, &reopened_revision);
    let reopened_page_queue = owner.queue_metrics();
    assert_eq!(reopened_page_queue.accepted, second_pages.len() as u64 + 1);
    assert_eq!(reopened_page_queue.dequeued, second_pages.len() as u64 + 1);
    assert_eq!(reopened_page_queue.pending_dispatch, 0);
    let writer = owner
        .begin_retirement()
        .join()
        .expect("reopened worker joins");
    let curation = curation_receipts(writer, &media, &profile);
    let expected_hash = fixture_hash.finalize().to_hex().to_string();
    assert_eq!(first_hash, expected_hash);
    assert_eq!(second_hash, expected_hash);
    assert!(
        !media.join(".yield.db").exists(),
        "profile-owned fixture cannot write the media root"
    );
    assert!(!temp.path().join("library.db").exists());
    let evidence = json!({"schema_version": 2, "fixture": "source-owner-metadata-v1", "rows": ROWS,
        "batch_size": BATCH, "page_size": BATCH, "fixture_blake3": expected_hash,
        "initial_revision": initial_revision, "final_revision": revision.revision(),
        "reopened_revision": reopened_revision.revision(), "restart_rows_match": true, "old_lifecycle_stale": true,
        "curation": curation, "storage": storage, "page_queue": page_queue, "reopened_page_queue": reopened_page_queue,
        "timings": {"open_ns": open_ns, "manifest_batches": distribution(&mutations),
            "audit_ns": audit_ns, "pages": distribution(&first_pages), "close_ns": close_ns,
            "reopen_ns": reopen_ns, "reopened_pages": distribution(&second_pages)},
        "limits": ["synthetic metadata only; no physical media traversal or hashing", "no journal/capacity claim or crash qualification",
            "page dispatch wait measured separately; SQL/checkpoint time unmeasured", "audit uses existing full-manifest acceptance API",
            "debug/test build; no SLO threshold or native GPUI measurement"]});
    println!("SOURCE_OWNER_WORKLOAD_JSON={evidence}");
}
