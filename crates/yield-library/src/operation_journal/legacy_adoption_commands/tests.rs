//! Actual owner fixtures; synthetic capacity values below are only attempted historical coercions.
use super::*;
use crate::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::{
        store::{JournalInsertOutcome, JournalInsertWriteStage, OperationJournalStore},
        *,
    },
    sample_sources::{DB_FILE_NAME, SourceDatabase},
};
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
use tempfile::{TempDir, tempdir};
use uuid::Uuid;
struct Fixture {
    _runtime: crate::test_runtime::TestRuntimeGuard,
    _root: AppRootGuard,
    _profile: TempDir,
    _media: TempDir,
    guard: WritableProfileGuard,
    source: crate::sample_sources::db::LegacySourceReadOwner,
    store: OperationJournalStore,
}
impl Fixture {
    fn new() -> Self {
        let runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        let profile = tempdir().unwrap();
        let media = tempdir().unwrap();
        let root = AppRootGuard::set(profile.path().to_path_buf()).unwrap();
        std::fs::create_dir(profile.path().join("legacy")).unwrap();
        rusqlite::Connection::open(profile.path().join("legacy").join(DB_FILE_NAME))
            .unwrap()
            .execute_batch(include_str!(
                "../../../tests/fixtures/profile-v11/source.sql"
            ))
            .unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let store = OperationJournalStore::open(&guard).unwrap();
        let source =
            SourceDatabase::open_legacy_read_owner(media.path(), &guard, "legacy").unwrap();
        Self {
            _runtime: runtime,
            _root: root,
            _profile: profile,
            _media: media,
            guard,
            source,
            store,
        }
    }
    fn prepare(&self) -> LegacyAdoptionIntentArgs {
        self.source
            .prepare_legacy_adoption_input(
                &self.guard,
                &self.store,
                ParticipantId::new(),
                SourceCommandId::new(),
                &AtomicBool::new(false),
            )
            .unwrap()
    }
}
#[test]
fn original_preparation_requires_exact_current_journal_schema() {
    let fixture = Fixture::new();
    let original = fixture.prepare();
    let current = u64::from(crate::operation_journal::store::JOURNAL_SCHEMA_VERSION);
    assert_eq!(original.data.journal_schema, current);
    original.validate().unwrap();
    for version in [0, current - 1, current + 1] {
        let mut foreign = original.clone();
        foreign.data.journal_schema = version;
        assert!(foreign.validate().is_err());
        let mut wire = MAGIC.to_vec();
        wire.extend_from_slice(&1u64.to_le_bytes());
        wire.extend(serde_json::to_vec(&foreign.data).unwrap());
        assert!(decode_legacy_adoption_input(&wire).is_err());
    }
}

#[test]
fn supported_wire_semantic_corruptions_refuse_even_with_canonical_field_order() {
    let f = Fixture::new();
    let original = f.prepare();
    for mutation in 0..19 {
        let mut bad = original.clone();
        let d = &mut bad.data;
        match mutation {
            0 => d.participant = ParticipantId::from_uuid(Uuid::nil()),
            1 => d.command = SourceCommandId::from_uuid(Uuid::nil()),
            2 => d.profile_owner.clear(),
            3 => d.source_policy.selected_max_pages += 1,
            4 => d.source_policy.existing_max_pages = u64::MAX,
            5 => d.journal_policy.page_count = u64::MAX,
            6 => d.objects[1].identity = d.objects[0].identity.clone(),
            7 => d.objects[1].observed = 1,
            8 => d.objects[4].observed = 1,
            9 => d.objects[0].unit = 3,
            10 => d.objects[7].conditional_logical_bytes = 0,
            11 => d.objects[2].device = d.objects[1].device.wrapping_add(1),
            12 => d.objects[3].unit = d.objects[1].unit * 2,
            13 => d.objects[0].observed = 17 * 1024 * 1024,
            14 => d.metadata_rows = 129,
            15 => d.target_version = 14,
            16 => d.sqlite_version += 1,
            17 => d.journal_schema = 11,
            _ => d.source_policy.existing_max_pages = u64::MAX,
        }
        // A larger EXISTING cap is allowed when the selected cap is still the fixed ceiling:
        // alter both caps here so the selected cap would raise/contradict the original proposal.
        if mutation == 4 {
            d.source_policy.selected_max_pages = u64::MAX;
        }
        assert!(bad.validate().is_err(), "mutation {mutation}");
        let mut b = MAGIC.to_vec();
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend(serde_json::to_vec(&bad.data).unwrap());
        assert!(
            decode_legacy_adoption_input(&b).is_err(),
            "canonical mutation {mutation}"
        );
    }
    let mut bad = original.clone();
    bad.data.objects[0].identity = "x".repeat(1025);
    assert!(bad.canonical_bytes().is_err());
    // Canonical ordering is independent of JSON shape/semantic validation.
    let b = original.canonical_bytes().unwrap();
    let mut spaced = b[..16].to_vec();
    spaced.extend_from_slice(b" ");
    spaced.extend_from_slice(&b[16..]);
    assert!(decode_legacy_adoption_input(&spaced).is_err());
}
fn passive_capacity() -> FullCapacityPlan {
    FullCapacityPlan::new(vec![
        FullCapacityAllocation::new(
            crate::durable_capacity::VolumeIdentity { device: 1 },
            4096,
            FullCapacityAllocationClass::JournalRecord,
            4096,
            4096,
            crate::durable_capacity::PROTECTED_FREE_SPACE_FLOOR,
        )
        .unwrap(),
    ])
    .unwrap()
}
#[test]
fn actual_store_codec_and_owner_refuse_family_before_rows_or_operation_id() {
    let f = Fixture::new();
    let request = f.prepare();
    let args = UniversalCommandArgs::SourceLegacyAdoption(request.clone());
    let p = request.participant_observation().unwrap();
    let plan = derive_work_plan(&args, &p).unwrap();
    let operation = OperationId::new();
    let capacity = passive_capacity();
    let claims = AdmissionClaimInventory::new(vec![
        AdmissionClaim::new(
            AdmissionClaimId::from_uuid(Uuid::new_v4()).unwrap(),
            capacity.allocations()[0].volume().clone(),
            FullCapacityAllocationClass::JournalRecord,
            4096,
        )
        .unwrap(),
    ])
    .unwrap();
    assert_eq!(FullProgressIntentObservation::new(operation, args.clone(), p.clone(), plan,
        capacity.clone(), claims).unwrap_err(), full_progress_record::FullProgressIntentObservationError::LegacyAdoptionAdmissionUnavailable);
    let record = codec::StoredOperationRecord::new_v16_command(
        operation,
        OperationIntent {
            actor: OperationActor::User,
            kind: OperationKind::SourceLegacyAdoption,
            label: "legacy".into(),
        },
        UniversalIntent::new(
            None,
            UniversalCommandKind::SourceLegacyAdoption,
            "legacy",
            p.clone(),
        )
        .unwrap(),
        args.clone(),
        capacity,
        1,
    );
    assert!(codec::encode(&record).is_err());
    for schema in [1, 3, 6, 16, 17] {
        for marker in [
            serde_json::json!({"intent":{"kind":"SourceLegacyAdoption"}}),
            serde_json::json!({"universal_intent":{"command_kind":"SourceLegacyAdoption"}}),
            serde_json::json!({"universal_command_args":{"SourceLegacyAdoption":"ignored"}}),
            serde_json::json!({"universal_intent":{"participants":{"descriptors":[{"kind":"LegacySource"}]}}}),
        ] {
            let mut marker = marker;
            marker["schema_version"] = schema.into();
            assert!(matches!(
                codec::decode(&serde_json::to_vec(&marker).unwrap()),
                Err(codec::DecodeError::Malformed { .. })
            ));
        }
    }
    let future = br#"{"schema_version":999,"intent":{"kind":"SourceLegacyAdoption"}}"#;
    assert!(matches!(
        codec::decode(future),
        Err(codec::DecodeError::UnsupportedVersion { version: 999 })
    ));
    let before = std::fs::read(
        f._profile
            .path()
            .join(store::JOURNAL_DATABASE_ROOT_NAME)
            .join(store::JOURNAL_DATABASE_FILE_NAME),
    )
    .unwrap();
    let outcome = f.store.insert_intent_with_outcome(&record);
    assert!(
        matches!(outcome, JournalInsertOutcome::Failed(f) if f.stage() == JournalInsertWriteStage::NoMutationAttempted)
    );
    assert!(
        f.store
            .scan_coordinator_recovery_page(None)
            .unwrap()
            .entries()
            .is_empty()
    );
    assert_eq!(
        std::fs::read(
            f._profile
                .path()
                .join(store::JOURNAL_DATABASE_ROOT_NAME)
                .join(store::JOURNAL_DATABASE_FILE_NAME)
        )
        .unwrap(),
        before
    );
    let mut owner = OperationJournalOwner::start_without_capacity_for_test(f.store).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while owner.status() == OperationJournalOwnerStatus::Initializing {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(owner.status(), OperationJournalOwnerStatus::Available);
    let client = owner.client();
    let error = client
        .admit_full(args, p)
        .unwrap()
        .recv_timeout(Duration::from_secs(2))
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("original-owner capacity and durable admission bridge")
    );
    assert!(
        client
            .scan_coordinator_recovery_page(None)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap()
            .entries()
            .is_empty()
    );
    assert!(owner.shutdown());
    let reopened = OperationJournalStore::open(&f.guard).unwrap();
    assert!(
        reopened
            .scan_coordinator_recovery_page(None)
            .unwrap()
            .entries()
            .is_empty()
    );
    assert_eq!(
        std::fs::read(
            f._profile
                .path()
                .join(store::JOURNAL_DATABASE_ROOT_NAME)
                .join(store::JOURNAL_DATABASE_FILE_NAME)
        )
        .unwrap(),
        before
    );
}

mod initial;
