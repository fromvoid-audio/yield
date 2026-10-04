//! Normal-build genuine source+journal/profile observation, never an adoption/capacity grant.
#![cfg(all(
    not(feature = "test-support"),
    any(target_os = "linux", target_os = "macos")
))]
use std::path::PathBuf;
use tempfile::{TempDir, tempdir};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::store::{
        JOURNAL_DATABASE_FILE_NAME, JOURNAL_DATABASE_ROOT_NAME, OperationJournalStore,
    },
    sample_sources::{DB_FILE_NAME, SourceDatabase, db::LegacySourceReadOwner},
};
struct Fixture {
    _root: AppRootGuard,
    profile: TempDir,
    _media: TempDir,
    guard: WritableProfileGuard,
    source: LegacySourceReadOwner,
    journal: OperationJournalStore,
}
impl Fixture {
    fn new() -> Self {
        let profile = tempdir().unwrap();
        let media = tempdir().unwrap();
        let root = AppRootGuard::set(profile.path().to_path_buf()).unwrap();
        std::fs::create_dir(profile.path().join("legacy")).unwrap();
        rusqlite::Connection::open(profile.path().join("legacy").join(DB_FILE_NAME))
            .unwrap()
            .execute_batch(include_str!("fixtures/profile-v11/source.sql"))
            .unwrap();
        let guard = WritableProfileGuard::acquire_current().unwrap();
        let journal = OperationJournalStore::open(&guard).unwrap();
        let source =
            SourceDatabase::open_legacy_read_owner(media.path(), &guard, "legacy").unwrap();
        Self {
            _root: root,
            profile,
            _media: media,
            guard,
            source,
            journal,
        }
    }
    fn source_path(&self) -> PathBuf {
        self.profile.path().join("legacy").join(DB_FILE_NAME)
    }
    fn journal_path(&self) -> PathBuf {
        self.profile
            .path()
            .join(JOURNAL_DATABASE_ROOT_NAME)
            .join(JOURNAL_DATABASE_FILE_NAME)
    }
}

use std::sync::atomic::AtomicBool;
use uuid::Uuid;
use yield_library::operation_journal::{
    OperationActor, OperationId, OperationKind, ParticipantDescriptor, ParticipantId,
    ParticipantKind, ParticipantRequirement, StoredSourceFence, UniversalCommandArgs,
    UniversalCommandKind, UniversalIntent, WorkProgress, WorkUnitRole, derive_work_plan,
    derive_work_plan_v1, legacy_adoption_commands::*, work_plan::WORK_PLAN_VERSION_LEGACY_ADOPTION,
};
use yield_library::sample_sources::{
    SourceDbError, SourceId, SourceLeaseError, db::SourceCommandId,
};
impl Fixture {
    fn prepare(&self, cancel: bool) -> Result<LegacyAdoptionIntentArgs, LegacyAdoptionInputError> {
        self.source.prepare_legacy_adoption_input(
            &self.guard,
            &self.journal,
            ParticipantId::from_uuid(Uuid::from_u128(11)),
            SourceCommandId::from_uuid(Uuid::from_u128(12)),
            &AtomicBool::new(cancel),
        )
    }
}
#[test]
fn original_owner_preparation_is_deterministic_bounded_and_never_adopts() {
    let f = Fixture::new();
    let original = std::fs::read(f.source_path()).unwrap();
    let journal = std::fs::read(f.journal_path()).unwrap();
    let request = f.prepare(false).unwrap();
    assert_eq!(request, f.prepare(false).unwrap());
    assert_eq!(request.original_revision(), 7);
    assert_eq!(request.profile_owner_identity(), f.guard.owner_identity());
    assert_eq!(
        request.original_image_digest(),
        f.source.capture(&f.guard).unwrap().digest()
    );
    let original_observation = f
        .source
        .capture_common_allocation_observation(&f.guard, &f.journal)
        .unwrap();
    assert_eq!(
        request.proposed_volume_totals().unwrap(),
        original_observation
            .volumes()
            .iter()
            .map(|v| (
                v.volume().device,
                v.allocation_unit(),
                v.proposed_logical_bytes()
            ))
            .collect::<Vec<_>>()
    );
    let b = request.canonical_bytes().unwrap();
    assert!(b.len() < MAX_LEGACY_ADOPTION_INPUT_BYTES);
    assert_eq!(
        decode_legacy_adoption_input(&b).unwrap(),
        StoredLegacyAdoptionInput::Supported(request.clone())
    );
    let transport = serde_json::to_string(&request).unwrap();
    assert!(transport.len() <= 32 * 1024);
    assert!(4 * MAX_LEGACY_ADOPTION_INPUT_BYTES.div_ceil(3) + 2 <= 32 * 1024);
    assert_eq!(
        serde_json::from_str::<LegacyAdoptionIntentArgs>(&transport).unwrap(),
        request
    );
    let args = UniversalCommandArgs::SourceLegacyAdoption(request.clone());
    assert_eq!(
        args.command_kind(),
        UniversalCommandKind::SourceLegacyAdoption
    );
    assert_eq!(
        args.command_kind().stable_id(),
        "family.source_legacy_adoption"
    );
    assert_eq!(
        serde_json::from_slice::<UniversalCommandArgs>(&serde_json::to_vec(&args).unwrap())
            .unwrap(),
        args
    );
    let participants = request.participant_observation().unwrap();
    let d = &participants.descriptors()[0];
    assert_eq!(d.kind(), ParticipantKind::LegacySource);
    assert!(d.source_fence().is_none());
    let plan = derive_work_plan(&args, &participants).unwrap();
    assert_eq!(plan.plan_version(), WORK_PLAN_VERSION_LEGACY_ADOPTION);
    assert_eq!(plan.units().len(), 1);
    assert_eq!(
        plan.units()[0].key().role(),
        WorkUnitRole::SourceLegacyAdoption
    );
    assert!(plan.edges().is_empty());
    assert!(derive_work_plan_v1(&args, &participants).is_err());
    assert!(WorkProgress::from_plan(OperationId::new(), &plan, &args, &participants).is_err());
    let intent =
        UniversalIntent::new(None, args.command_kind(), "legacy adoption", participants).unwrap();
    assert!(
        intent
            .validate_for_new_intent(
                OperationId::new(),
                OperationActor::User,
                &OperationKind::SourceLegacyAdoption
            )
            .is_ok()
    );
    for kind in [
        OperationKind::FileMutation,
        OperationKind::FileHistory,
        OperationKind::ExternalReconciliation,
        OperationKind::SourceDerivedLayout,
    ] {
        assert!(
            intent
                .validate_for_new_intent(OperationId::new(), OperationActor::User, &kind)
                .is_err()
        );
    }
    assert!(
        intent
            .validate_for_new_intent(
                OperationId::new(),
                OperationActor::ExternalFs,
                &OperationKind::SourceLegacyAdoption
            )
            .is_err()
    );
    assert_eq!(std::fs::read(f.source_path()).unwrap(), original);
    assert_eq!(std::fs::read(f.journal_path()).unwrap(), journal);
    assert_eq!(
        f.journal.recovery_readiness().unwrap(),
        yield_library::operation_journal::OperationJournalRecoveryReadiness::Clear
    );
    let path = f.source_path();
    let media = f._media.path().to_path_buf();
    drop(f.source);
    assert!(matches!(
        SourceDatabase::open_for_catalog_with_profile_guard(media, &f.guard, "legacy"),
        Err(SourceDbError::SourceLease(
            SourceLeaseError::MigrationRequired {
                user_version: 11,
                ..
            }
        ))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let c = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM metadata WHERE key='source.legacy_adoption.v1'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='source_lease'",
            [],
            |r| r.get::<_, u64>(0)
        )
        .unwrap(),
        0
    );
}
#[test]
fn wire_rejects_noncanonical_malformed_and_unbounded_data_retaining_future_version() {
    let f = Fixture::new();
    let request = f.prepare(false).unwrap();
    let b = request.canonical_bytes().unwrap();
    for len in [0, 7, 15, 16, b.len() - 1] {
        assert!(decode_legacy_adoption_input(&b[..len]).is_err());
    }
    let mut trailing = b.clone();
    trailing.push(b' ');
    assert!(decode_legacy_adoption_input(&trailing).is_err());
    let mut duplicate = b[..16].to_vec();
    duplicate.extend_from_slice(b"{\"legacy_version\":11,");
    duplicate.extend_from_slice(&b[17..]);
    assert!(decode_legacy_adoption_input(&duplicate).is_err());
    let mut unknown = b.clone();
    unknown[8..16].copy_from_slice(&999u64.to_le_bytes());
    assert_eq!(
        decode_legacy_adoption_input(&unknown).unwrap(),
        StoredLegacyAdoptionInput::Unsupported {
            version: 999,
            bytes: unknown.clone()
        }
    );
    use base64::Engine;
    assert!(
        serde_json::from_value::<LegacyAdoptionIntentArgs>(serde_json::json!(
            base64::engine::general_purpose::STANDARD.encode(unknown)
        ))
        .is_err()
    );
    let mut too_large = b[..16].to_vec();
    too_large.resize(MAX_LEGACY_ADOPTION_INPUT_BYTES + 1, 0);
    assert!(decode_legacy_adoption_input(&too_large).is_err());
    assert!(
        serde_json::from_value::<LegacyAdoptionIntentArgs>(serde_json::json!(
            "A".repeat(4 * MAX_LEGACY_ADOPTION_INPUT_BYTES.div_ceil(3) + 1)
        ))
        .is_err()
    );
    assert!(serde_json::from_str::<LegacyAdoptionIntentArgs>("[1,2,3]").is_err());
}
#[test]
fn prelease_topology_refuses_current_fence_extra_changed_and_wrong_work_family() {
    let f = Fixture::new();
    let request = f.prepare(false).unwrap();
    let p = request.participant_observation().unwrap();
    let args = UniversalCommandArgs::SourceLegacyAdoption(request.clone());
    for mutation in 0..4 {
        let mut v = serde_json::to_value(&p).unwrap();
        match mutation {
            0 => {
                v["descriptors"][0]["id"] = Uuid::new_v4().to_string().into();
                v["checkpoints"][0]["participant_id"] = v["descriptors"][0]["id"].clone();
            }
            1 => {
                v["descriptors"][0]["kind"] = "GlobalLibrary".into();
            }
            2 => {
                v["checkpoints"][0]["state"] = "Completed".into();
            }
            _ => {
                let extra = v["descriptors"][0].clone();
                v["descriptors"].as_array_mut().unwrap().push(extra);
            }
        }
        if let Ok(bad) = serde_json::from_value(v) {
            assert!(derive_work_plan(&args, &bad).is_err());
        }
    }
    let fence =
        StoredSourceFence::new(SourceId::from_string("legacy"), "root", "db", 1, 7, 1).unwrap();
    assert!(
        ParticipantDescriptor::new(
            request.participant_id(),
            ParticipantKind::LegacySource,
            ParticipantRequirement::Required,
            Some(fence),
            None
        )
        .is_err()
    );
    let plan = derive_work_plan(&args, &p).unwrap();
    for version in [1, 2, 999] {
        let mut v = serde_json::to_value(&plan).unwrap();
        v["plan_version"] = version.into();
        let bad: yield_library::operation_journal::WorkPlan = serde_json::from_value(v).unwrap();
        assert!(bad.validate().is_err());
    }
}
#[test]
fn cancellation_foreign_originals_changes_and_replacements_refuse_without_writes() {
    for which in [
        "cancel",
        "source-rows",
        "source-control",
        "journal",
        "media-root",
        "source-directory",
        "foreign",
    ] {
        let f = Fixture::new();
        let original = std::fs::read(f.source_path()).unwrap();
        if which == "cancel" {
            assert_eq!(
                f.prepare(true).unwrap_err(),
                LegacyAdoptionInputError::Cancelled
            );
        } else if which == "foreign" {
            let other = tempdir().unwrap();
            let _root = AppRootGuard::set(other.path().to_path_buf()).unwrap();
            let other_guard = WritableProfileGuard::acquire_current().unwrap();
            assert!(
                f.source
                    .prepare_legacy_adoption_input(
                        &other_guard,
                        &f.journal,
                        ParticipantId::new(),
                        SourceCommandId::new(),
                        &AtomicBool::new(false)
                    )
                    .is_err()
            );
        } else {
            if which == "source-rows" {
                rusqlite::Connection::open(f.source_path())
                    .unwrap()
                    .execute("UPDATE metadata SET value='8' WHERE key='revision'", [])
                    .unwrap();
            } else if which == "source-control" {
                std::fs::write(
                    f.profile.path().join("legacy/.yield-source-lease.lock"),
                    b"changed",
                )
                .unwrap();
            } else {
                let path = match which {
                    "journal" => f.journal_path(),
                    "media-root" => f._media.path().to_path_buf(),
                    _ => f.profile.path().join("legacy"),
                };
                std::fs::rename(&path, path.with_extension("retained-original")).unwrap();
                if which == "journal" {
                    std::fs::write(path, b"replaced").unwrap();
                } else {
                    std::fs::create_dir(path).unwrap();
                }
            }
            let before = if f.source_path().exists() {
                Some(std::fs::read(f.source_path()).unwrap())
            } else {
                None
            };
            assert!(f.prepare(false).is_err(), "{which}");
            if let Some(before) = before {
                assert_eq!(std::fs::read(f.source_path()).unwrap(), before);
            }
            if which == "source-directory" {
                assert_eq!(
                    std::fs::read(
                        f.profile
                            .path()
                            .join("legacy.retained-original")
                            .join(DB_FILE_NAME)
                    )
                    .unwrap(),
                    original
                );
            }
            continue;
        }
        assert_eq!(std::fs::read(f.source_path()).unwrap(), original);
        assert_eq!(
            f.journal.recovery_readiness().unwrap(),
            yield_library::operation_journal::OperationJournalRecoveryReadiness::Clear
        );
    }
}
