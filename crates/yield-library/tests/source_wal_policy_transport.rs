//! Normal-build real source reservations. No capacity or journal-registration fixtures.
#![cfg(all(target_os = "linux", not(feature = "test-support")))]
use rusqlite::{Connection, ErrorCode};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::{
        DB_FILE_NAME, SourceDatabase, SourceId, SourceRegistryRegistrationPhase, SourceWriter,
    },
};
fn writer(media: &Path, profile: &WritableProfileGuard, id: &str) -> SourceWriter {
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(media, profile, Path::new(id)).unwrap(),
        SourceId::from_string(id),
        1,
    )
    .unwrap();
    writer
        .catalog_database()
        .upsert_file(Path::new("one.wav"), 1, 1)
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
fn busy(other: &Connection) {
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
}
fn row(other: &Connection) -> (String, i64, i64, String) {
    other
        .query_row(
            "SELECT owner_identity,lease_epoch,expires_at,state FROM source_lease",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}
fn transport_correlation()
-> yield_library::sample_sources::wal_transport::SourceWalTransportCorrelation {
    use yield_library::operation_journal::{AdmissionClaimId, ParticipantId};
    yield_library::sample_sources::wal_transport::SourceWalTransportCorrelation::new(
        [37; 32],
        ParticipantId::new(),
        AdmissionClaimId::new(),
        AdmissionClaimId::new(),
    )
    .unwrap()
}

#[test]
fn normal_build_original_tag_and_layout_policies_cross_the_source_inbox() {
    use yield_library::sample_sources::{
        SourceTagAssignmentMutation as Mutation,
        wal_transport::{
            SourceWalPolicyFamily as Family, SourceWalTransport,
            SourceWalTransportRejection as Rejection, SourceWalTransportStatus as Status,
        },
    };
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    for (family, id) in [
        (Family::Tag, "policy-tag"),
        (Family::Layout, "policy-layout"),
    ] {
        let original = writer(&media, &profile, id);
        let authority = original
            .catalog_database()
            .prepare_source_authority()
            .unwrap();
        let revision = original.current_revision().unwrap();
        let other = Connection::open(profile.profile_root().join(id).join(DB_FILE_NAME)).unwrap();
        other.busy_timeout(Duration::ZERO).unwrap();
        // Fixture seed only; this creates no production admission/settlement authority.
        other.execute("INSERT INTO source_tags(normalized_text, display_label) VALUES ('old tag','Old Tag')", []).unwrap();
        other.execute("INSERT INTO wav_file_tags(path,tag_id) SELECT 'one.wav',id FROM source_tags WHERE normalized_text='old tag'", []).unwrap();
        let before = row(&other);
        let (transport, mut inbox) = SourceWalTransport::channel();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut request = match family {
            Family::Tag => {
                let observation = original
                    .capture_tag_admission_observation(&revision, "old tag", Mutation::Delete)
                    .unwrap();
                assert_eq!(observation.selection().paths().len(), 1);
                original
                    .validate_tag_admission_observation(&observation)
                    .unwrap();
                transport
                    .submit_tag_with_binding(observation, transport_correlation(), deadline)
                    .unwrap()
            }
            Family::Layout => transport
                .submit_layout_with_binding(
                    original.capture_layout_metadata_budget(&revision).unwrap(),
                    transport_correlation(),
                    deadline,
                )
                .unwrap(),
        };
        let mut owner = inbox.try_next().unwrap().acquire(original);
        assert!(owner.is_prepared());
        busy(&other);
        owner.publish_prepared(Some(&authority)).unwrap();
        owner.revalidate(Some(&authority)).unwrap();
        let holder = request.try_prepared().unwrap();
        let Status::Prepared(observation) = holder.status() else {
            panic!("original policy entered")
        };
        assert_eq!(observation.policy_family(), Some(family));
        assert_eq!(
            observation
                .original_binding()
                .unwrap()
                .subject()
                .registration()
                .unwrap()
                .phase(),
            SourceRegistryRegistrationPhase::Prepared
        );
        assert_eq!(
            holder.acknowledge_settlement(),
            Err(Rejection::SettlementUnsupported)
        );
        drop(holder);
        assert!(owner.is_cancelled());
        let returned = owner
            .into_writer()
            .unwrap_or_else(|_| panic!("actual original cleanup"));
        assert_eq!(request.status(), Status::Aborted);
        assert_eq!(returned.current_revision().unwrap(), revision);
        assert_eq!(row(&other), before);
        other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(observation.policy_family(), Some(family));
        drop(authority);
        returned.close().unwrap();
    }
}
