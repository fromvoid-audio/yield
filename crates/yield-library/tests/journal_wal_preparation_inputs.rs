//! Public factory paths on genuine SQLite workers with explicit memory-only journal startup.
//! No fixture supplies physical capacity or effect qualification.
#![cfg(all(target_os = "linux", feature = "test-support"))]
use std::{
    fs,
    time::{Duration, Instant},
};
use yield_library::durable_capacity::PROTECTED_FREE_SPACE_FLOOR;
use yield_library::operation_journal::FullCapacityAllocationClass as Class;
use yield_library::operation_journal::{
    owner::{
        SourceWalPreparationInput, WalPreparationError, WalPreparationInput, WalPreparationSource,
    },
    *,
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{
        SourceCommandId, SourceDatabase, SourceId, SourceRevision, SourceTagAssignmentMutation,
        SourceWriter, wal_transport::SourceWalTransport,
    },
};
struct Fixture {
    journal: owner::OperationJournalOwner,
    owner: WritableProfileGuard,
    _root: AppRootGuard,
    _base: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let base = tempfile::tempdir().unwrap();
        let root = AppRootGuard::set(base.path().join("profile")).unwrap();
        let owner = WritableProfileGuard::acquire_current().unwrap();
        let store = OperationJournalStore::open(&owner).unwrap();
        let journal = owner::OperationJournalOwner::start_without_capacity_for_test(store).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while journal.status() != owner::OperationJournalOwnerStatus::Available {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }
        Self {
            journal,
            owner,
            _root: root,
            _base: base,
        }
    }
}
fn source(fixture: &Fixture, name: &str) -> SourceWriter {
    let media = fixture._base.path().join(name);
    fs::create_dir(&media).unwrap();
    fs::write(media.join("one.wav"), b"original fixture bytes").unwrap();
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(&media, &fixture.owner, name).unwrap(),
        SourceId::from_string(name),
        1,
    )
    .unwrap();
    writer
        .catalog_database()
        .upsert_file(std::path::Path::new("one.wav"), 1, 1)
        .unwrap();
    let generation = writer.begin_scan_directory_truth_generation(0).unwrap();
    let fence = writer.current_scan_fence().unwrap();
    let batch = writer.catalog_database().write_batch().unwrap();
    writer
        .commit_scan_audit_completion(
            &fence,
            batch,
            generation,
            1,
            fence.root_identity().clone(),
            None,
        )
        .unwrap();
    writer
}
fn fence(revision: &SourceRevision) -> StoredSourceFence {
    StoredSourceFence::new(
        revision.source_id().clone(),
        std::str::from_utf8(revision.root_identity().as_bytes()).unwrap(),
        revision.database_identity(),
        revision.lifecycle_generation(),
        revision.revision(),
        revision.lease_epoch(),
    )
    .unwrap()
}
fn command(
    fixture: &Fixture,
    revisions: &[SourceRevision],
) -> (
    UniversalCommandArgs,
    ParticipantSet,
    [yield_library::operation_journal::ParticipantId; 2],
) {
    let ids = [
        yield_library::operation_journal::ParticipantId::new(),
        yield_library::operation_journal::ParticipantId::new(),
    ];
    let mut descriptors = revisions
        .iter()
        .zip(ids)
        .map(|(revision, id)| {
            ParticipantDescriptor::new(
                id,
                ParticipantKind::SourceDatabase,
                ParticipantRequirement::Required,
                Some(fence(revision)),
                None,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    for kind in [
        ParticipantKind::Filesystem,
        ParticipantKind::GlobalLibrary,
        ParticipantKind::Harvest,
    ] {
        descriptors.push(
            ParticipantDescriptor::new(
                yield_library::operation_journal::ParticipantId::new(),
                kind,
                ParticipantRequirement::Required,
                None,
                None,
            )
            .unwrap(),
        );
    }
    for kind in [ParticipantKind::Projection, ParticipantKind::Readiness] {
        descriptors.push(
            ParticipantDescriptor::new(
                yield_library::operation_journal::ParticipantId::new(),
                kind,
                ParticipantRequirement::Required,
                Some(fence(&revisions[1])),
                None,
            )
            .unwrap(),
        );
    }
    let checkpoints = descriptors
        .iter()
        .map(|d| {
            ParticipantCheckpoint::new(d.id(), ParticipantCheckpointState::Pending, None, None)
                .unwrap()
        })
        .collect();
    let participants = ParticipantSet::new(descriptors, checkpoints).unwrap();
    let file = fs::File::open(
        fixture
            ._base
            .path()
            .join(revisions[0].source_id().as_str())
            .join("one.wav"),
    )
    .unwrap();
    let metadata = file.metadata().unwrap();
    let identity =
        yield_library::filesystem_identity::stable_filesystem_identity_from_open_file(&file)
            .unwrap();
    let modified_ns = i64::try_from(
        metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap();
    let args = UniversalCommandArgs::Copy(
        CopyIntentArgs::new(
            CopyEndpoint::new(
                ids[0],
                revisions[0].source_id().clone(),
                StoredRelativePath::new("one.wav").unwrap(),
            )
            .unwrap(),
            CopyEndpoint::new(
                ids[1],
                revisions[1].source_id().clone(),
                StoredRelativePath::new("copied.wav").unwrap(),
            )
            .unwrap(),
            CopySourceExpectation::new(
                revisions[0].revision(),
                identity,
                metadata.len(),
                modified_ns,
            )
            .unwrap(),
            CopyCollisionPolicy::NoReplace,
            None,
        )
        .unwrap(),
    );
    yield_library::operation_journal::derive_work_plan(&args, &participants).unwrap();
    (args, participants, ids)
}
fn plan(
    fixture: &Fixture,
) -> (
    FullCapacityPlan,
    AdmissionClaimInventory,
    AdmissionClaimId,
    AdmissionClaimId,
) {
    // Logical observed claim correlations only. No capacity provider or native grant is installed.
    let facts = yield_library::durable_capacity::descriptor_capacity_facts(
        &fs::File::open(fixture.owner.profile_root()).unwrap(),
    )
    .unwrap();
    let volume = facts.identity;
    let unit = facts.allocation_unit;
    let classes = [
        Class::SourceDatabaseCommit,
        Class::SourceWalShm,
        Class::GlobalDatabaseCommit,
        Class::GlobalWalShm,
    ];
    let allocations = classes
        .iter()
        .map(|class| {
            FullCapacityAllocation::new(
                volume.clone(),
                unit,
                *class,
                8192,
                8192,
                PROTECTED_FREE_SPACE_FLOOR,
            )
            .unwrap()
        })
        .collect();
    let claims = classes
        .iter()
        .map(|class| {
            AdmissionClaim::new(AdmissionClaimId::new(), volume.clone(), *class, 8192).unwrap()
        })
        .collect::<Vec<_>>();
    let main = claims[0].claim_id();
    let side = claims[1].claim_id();
    (
        FullCapacityPlan::new(allocations).unwrap(),
        AdmissionClaimInventory::new(claims).unwrap(),
        main,
        side,
    )
}

#[test]
fn normal_public_two_source_factory_requires_actual_complete_coverage_without_preparing() {
    use yield_library::sample_sources::library::{GlobalLibraryWriter, GlobalLibraryWriterStatus};
    let fixture = Fixture::new();
    let a = source(&fixture, "normal-a");
    let b = source(&fixture, "normal-b");
    let revisions = vec![a.current_revision().unwrap(), b.current_revision().unwrap()];
    let (args, participants, ids) = command(&fixture, &revisions);
    let (plan, claims, main, side) = plan(&fixture);
    let mut global = GlobalLibraryWriter::start(&fixture.owner).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while global.status() != GlobalLibraryWriterStatus::Available {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let (requester, mut inbox) = SourceWalTransport::channel();
    let sources = (0..2)
        .rev()
        .map(|i| {
            WalPreparationSource::new(
                requester.clone(),
                ids[i],
                main,
                side,
                SourceWalPreparationInput::Audited(revisions[i].clone()),
            )
        })
        .collect();
    let input = WalPreparationInput::from_universal(
        fixture.journal.client().reserve_wal_preparation().unwrap(),
        &args,
        &participants,
        &plan,
        &claims,
        sources,
        Some(global.wal_preparation_client()),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    assert!(inbox.try_next_any().is_none());
    for child in ["normal-a", "normal-b"] {
        rusqlite::Connection::open(
            fixture
                .owner
                .profile_root()
                .join(child)
                .join(yield_library::sample_sources::DB_FILE_NAME),
        )
        .unwrap()
        .execute_batch("BEGIN IMMEDIATE; ROLLBACK")
        .unwrap();
    }
    drop(input);
    let sources = vec![WalPreparationSource::new(
        requester,
        ids[0],
        main,
        side,
        SourceWalPreparationInput::Audited(revisions[0].clone()),
    )];
    assert!(matches!(
        WalPreparationInput::from_universal(
            fixture.journal.client().reserve_wal_preparation().unwrap(),
            &args,
            &participants,
            &plan,
            &claims,
            sources,
            Some(global.wal_preparation_client()),
            Instant::now() + Duration::from_secs(2)
        ),
        Err(WalPreparationError::InvalidInput)
    ));
    drop(a);
    drop(b);
    assert!(global.shutdown());
}
#[test]
fn normal_public_tag_factory_borrows_exact_original_selection_and_queues_no_effect() {
    let fixture = Fixture::new();
    let writer = source(&fixture, "normal-tag");
    let other = rusqlite::Connection::open(
        fixture
            .owner
            .profile_root()
            .join("normal-tag")
            .join(yield_library::sample_sources::DB_FILE_NAME),
    )
    .unwrap();
    other.execute_batch("INSERT INTO source_tags(normalized_text,display_label) VALUES('old tag','Old Tag'); INSERT INTO wav_file_tags(path,tag_id) SELECT 'one.wav',id FROM source_tags WHERE normalized_text='old tag';").unwrap();
    let revision = writer.current_revision().unwrap();
    let observation = writer
        .capture_tag_admission_observation(
            &revision,
            "old tag",
            SourceTagAssignmentMutation::Delete,
        )
        .unwrap();
    let participant = ParticipantId::new();
    let args = MetadataTagSourceIntentArgs::from_observation(
        &writer,
        &observation,
        participant,
        SourceCommandId::new(),
    )
    .unwrap();
    let (plan, claims, main, side) = plan(&fixture);
    let (requester, mut inbox) = SourceWalTransport::channel();
    let input = WalPreparationInput::from_tag(
        fixture.journal.client().reserve_wal_preparation().unwrap(),
        &writer,
        &args,
        &plan,
        &claims,
        WalPreparationSource::new(
            requester,
            participant,
            main,
            side,
            SourceWalPreparationInput::Tag(observation),
        ),
        Instant::now() + Duration::from_secs(2),
    )
    .unwrap();
    assert!(inbox.try_next_any().is_none());
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(
        other
            .query_row("SELECT count(*) FROM wav_file_tags", [], |r| r
                .get::<_, u64>(0))
            .unwrap(),
        1
    );
    drop(input);
    drop(writer);
}
#[test]
fn normal_public_pre_audit_factory_compares_owner_scalars_without_registered_grant() {
    let fixture = Fixture::new();
    let media = fixture._base.path().join("normal-pre-audit");
    fs::create_dir(&media).unwrap();
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(
            &media,
            &fixture.owner,
            "normal-pre-audit",
        )
        .unwrap(),
        SourceId::from_string("normal-pre-audit"),
        1,
    )
    .unwrap();
    let authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let allocation = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let prepared = writer
        .prepare_lease_maintenance(allocation.clone(), deadline)
        .unwrap();
    let binding = prepared.capture_lease_binding(&authority).unwrap();
    let fence = binding.stored_fence().clone();
    let writer = prepared.abort().unwrap();
    let participant = ParticipantId::new();
    let participants = ParticipantSet::new(
        vec![ParticipantDescriptor::source_lease_maintenance(participant, fence).unwrap()],
        vec![
            ParticipantCheckpoint::new(
                participant,
                ParticipantCheckpointState::Pending,
                None,
                None,
            )
            .unwrap(),
        ],
    )
    .unwrap();
    let (plan, claims, main, side) = plan(&fixture);
    let (requester, mut inbox) = SourceWalTransport::channel();
    let input = WalPreparationInput::from_lease_scope(
        fixture.journal.client().reserve_wal_preparation().unwrap(),
        &participants,
        &plan,
        &claims,
        WalPreparationSource::new(
            requester,
            participant,
            main,
            side,
            SourceWalPreparationInput::Lease(allocation),
        ),
        deadline,
    )
    .unwrap();
    assert!(writer.current_revision().is_err());
    assert!(inbox.try_next_any().is_none());
    assert_eq!(
        binding
            .stored_fence()
            .subject()
            .registration()
            .unwrap()
            .phase(),
        yield_library::sample_sources::SourceRegistryRegistrationPhase::Prepared
    );
    drop(input);
    drop(binding);
    drop(writer);
    drop(authority);
}
