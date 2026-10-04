//! Normal-build real source reservations. No capacity or journal-registration fixtures.
#![cfg(all(target_os = "linux", not(feature = "test-support")))]
use rusqlite::{Connection, ErrorCode, OpenFlags};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    sample_sources::db::write::{SOURCE_WAL_BINDING_MAX_BYTES, SourceWalPreparationError},
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
#[test]
fn two_real_audited_sources_issue_distinct_original_bindings_without_lease_mutation() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let a = writer(&media, &profile, "binding-a");
    let b = writer(&media, &profile, "binding-b");
    let ar = a.catalog_database().prepare_source_authority().unwrap();
    let br = b.catalog_database().prepare_source_authority().unwrap();
    let rev_a = a.current_revision().unwrap();
    let rev_b = b.current_revision().unwrap();
    let ca = Connection::open_with_flags(
        profile.profile_root().join("binding-a").join(DB_FILE_NAME),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    let cb = Connection::open_with_flags(
        profile.profile_root().join("binding-b").join(DB_FILE_NAME),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    ca.busy_timeout(Duration::ZERO).unwrap();
    cb.busy_timeout(Duration::ZERO).unwrap();
    let before = (row(&ca), row(&cb));
    let deadline = Instant::now() + Duration::from_secs(2);
    let a = a.prepare_wal_reservation(&rev_a, deadline).unwrap();
    let b = b.prepare_wal_reservation(&rev_b, deadline).unwrap();
    let ba = a.capture_wal_binding(&ar).unwrap();
    let bb = b.capture_wal_binding(&br).unwrap();
    assert_ne!(ba.main().identity(), bb.main().identity());
    assert_ne!(ba.wal().identity(), bb.wal().identity());
    assert_eq!(
        ba.subject().profile_acquisition(),
        bb.subject().profile_acquisition()
    );
    assert_eq!(
        ba.subject().registration().unwrap().phase(),
        SourceRegistryRegistrationPhase::Prepared
    );
    assert!(ba.descriptor_bytes().len() <= SOURCE_WAL_BINDING_MAX_BYTES);
    assert!(bb.descriptor_bytes().len() <= SOURCE_WAL_BINDING_MAX_BYTES);
    assert!(matches!(
        a.validate_wal_binding(&bb, &ar),
        Err(SourceWalPreparationError::BindingUnavailable)
    ));
    assert!(matches!(
        a.capture_wal_binding(&br),
        Err(SourceWalPreparationError::BindingUnavailable)
    ));
    a.validate_wal_binding(&ba, &ar).unwrap();
    b.validate_wal_binding(&bb, &br).unwrap();
    busy(&ca);
    busy(&cb);
    assert_eq!((row(&ca), row(&cb)), before);
    let a = a.abort().unwrap();
    let b = b.abort().unwrap();
    assert_eq!(a.current_revision().unwrap(), rev_a);
    assert_eq!(b.current_revision().unwrap(), rev_b);
    ca.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    cb.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    drop(ar);
    drop(br);
    a.close().unwrap();
    b.close().unwrap();
}

#[test]
fn actual_media_root_replacement_invalidates_binding_until_original_namespace_returns() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let owner = writer(&media, &profile, "binding-root");
    let registration = owner.catalog_database().prepare_source_authority().unwrap();
    let revision = owner.current_revision().unwrap();
    let other = Connection::open_with_flags(
        profile
            .profile_root()
            .join("binding-root")
            .join(DB_FILE_NAME),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )
    .unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let preparation = owner
        .prepare_wal_reservation(&revision, Instant::now() + Duration::from_secs(2))
        .unwrap();
    let binding = preparation.capture_wal_binding(&registration).unwrap();
    let saved = temp.path().join("original-media");
    fs::rename(&media, &saved).unwrap();
    fs::create_dir(&media).unwrap();
    assert!(matches!(
        preparation.validate_wal_binding(&binding, &registration),
        Err(SourceWalPreparationError::Owner(_))
    ));
    busy(&other);
    fs::remove_dir(&media).unwrap();
    fs::rename(saved, &media).unwrap();
    preparation
        .validate_wal_binding(&binding, &registration)
        .unwrap();
    let owner = preparation.abort().unwrap();
    drop(registration);
    owner.close().unwrap();
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
fn library_transport_requires_original_native_registration_and_retains_historical_binding() {
    use yield_library::sample_sources::wal_transport::{
        SourceWalTransport, SourceWalTransportRejection as Rejection,
        SourceWalTransportStatus as Status,
    };
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let a = writer(&media, &profile, "transport-a");
    let b = writer(&media, &profile, "transport-b");
    let authority = a.catalog_database().prepare_source_authority().unwrap();
    let wrong_authority = b.catalog_database().prepare_source_authority().unwrap();
    let revision = a.current_revision().unwrap();
    let other = Connection::open(
        profile
            .profile_root()
            .join("transport-a")
            .join(DB_FILE_NAME),
    )
    .unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    let before = row(&other);
    let (transport, mut inbox) = SourceWalTransport::channel();
    let mut request = transport
        .submit_with_binding(
            revision.clone(),
            transport_correlation(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    let mut producer = inbox.try_next().unwrap().acquire(a);
    busy(&other);
    assert_eq!(producer.publish_prepared(None), Err(Rejection::Scope));
    assert_eq!(
        producer.publish_prepared(Some(&wrong_authority)),
        Err(Rejection::Scope)
    );
    assert_eq!(request.status(), Status::Pending);
    producer.publish_prepared(Some(&authority)).unwrap();
    let holder = request.try_prepared().unwrap();
    let Status::Prepared(observation) = holder.status() else {
        panic!("genuine native preparation")
    };
    let copy = observation.clone();
    assert_eq!(observation.revision(), &revision);
    assert_eq!(
        observation.scope_descriptor_bytes(),
        Some(observation.original_binding().unwrap().descriptor_bytes())
    );
    assert_eq!(
        producer.publish_prepared(Some(&authority)),
        Err(Rejection::InvalidContext)
    );
    assert_eq!(
        holder.acknowledge_settlement(),
        Err(Rejection::SettlementUnsupported)
    );
    drop(holder);
    assert!(producer.is_cancelled());
    busy(&other);
    producer.request_abort(None);
    let returned = producer
        .into_writer()
        .unwrap_or_else(|_| panic!("actual rollback"));
    assert_eq!(request.status(), Status::Aborted);
    assert_eq!(row(&other), before);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(copy, observation);
    assert!(copy.original_binding().is_some());
    transport.close();
    assert!(copy.scope_descriptor_bytes().is_some());
    drop(returned);
}

#[test]
fn library_transport_command_and_native_owner_loss_never_claim_reuse() {
    use yield_library::sample_sources::wal_transport::{
        SourceWalTransport, SourceWalTransportRejection as Rejection,
        SourceWalTransportStatus as Status,
    };
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let a = writer(&media, &profile, "transport-loss");
    let revision = a.current_revision().unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let request = transport
        .submit(
            revision.clone(),
            transport_correlation(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    drop(inbox.try_next().unwrap());
    assert_eq!(request.status(), Status::Rejected(Rejection::OwnerLost));
    let request = transport
        .submit(
            revision,
            transport_correlation(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    let mut producer = inbox.try_next().unwrap().acquire(a);
    producer.publish_prepared(None).unwrap();
    let witness = producer.outcome_observation();
    drop(producer);
    assert_eq!(witness.status(), Status::Unavailable(Rejection::OwnerLost));
    assert_eq!(request.status(), Status::Unavailable(Rejection::OwnerLost));
    let queued = transport
        .submit(
            writer(&media, &profile, "transport-queued")
                .current_revision()
                .unwrap(),
            transport_correlation(),
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
    drop(inbox);
    assert_eq!(queued.status(), Status::Rejected(Rejection::Closed));
}
