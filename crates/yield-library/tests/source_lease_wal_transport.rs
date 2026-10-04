//! Normal-build original pre-audit owner and typed shared source inbox.
#![cfg(all(target_os = "linux", not(feature = "test-support")))]
use rusqlite::{Connection, ErrorCode};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};
use yield_library::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    operation_journal::{AdmissionClaimId, ParticipantId},
    sample_sources::{
        DB_FILE_NAME, SourceDatabase, SourceId, SourceRegistryAuditObservation, SourceWriter,
        wal_transport::{
            SourceWalProducerCommand, SourceWalTransport, SourceWalTransportCorrelation,
            SourceWalTransportRejection as Rejection, SourceWalTransportStatus as Status,
        },
    },
};
#[test]
fn normal_build_original_never_audited_lease_owner_crosses_shared_fifo_and_preserves_custody() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(&media, &profile, Path::new("lease"))
            .unwrap(),
        SourceId::from_string("lease"),
        1,
    )
    .unwrap();
    assert!(writer.current_revision().is_err());
    let fence = writer.current_scan_fence().unwrap();
    let authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    assert_eq!(
        input.subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    let correlation = SourceWalTransportCorrelation::new(
        [226; 32],
        ParticipantId::new(),
        AdmissionClaimId::new(),
        AdmissionClaimId::new(),
    )
    .unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let mut request = transport
        .submit_lease_with_binding(input, correlation, deadline)
        .unwrap();
    assert!(inbox.try_next().is_none());
    let command = inbox.try_next_any().unwrap();
    assert!(matches!(&command, SourceWalProducerCommand::Lease(_)));
    let mut owner = command.acquire(writer);
    assert!(owner.is_prepared());
    owner.publish_prepared(Some(&authority)).unwrap();
    owner.revalidate(Some(&authority)).unwrap();
    let other = Connection::open(profile.profile_root().join("lease").join(DB_FILE_NAME)).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    let holder = request.try_prepared().unwrap();
    assert_eq!(
        holder.acknowledge_settlement(),
        Err(Rejection::SettlementUnsupported)
    );
    let Status::PreparedLease(history) = holder.status() else {
        panic!("original lease")
    };
    assert_eq!(
        history.original_binding().stored_fence().subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    drop(holder);
    // Original cleanup remains funded after admission closes during shutdown.
    inbox.close();
    let writer = owner
        .into_writer()
        .unwrap_or_else(|_| panic!("original cleanup"));
    assert_eq!(request.status(), Status::Aborted);
    assert_eq!(writer.current_scan_fence().unwrap(), fence);
    assert!(writer.current_revision().is_err());
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert!(
        history.scope_descriptor_bytes().len()
            <= yield_library::operation_journal::source_lease_binding::MAX_SOURCE_LEASE_SCOPE_BYTES
    );
    drop(authority);
    writer.close().unwrap();
}

#[test]
fn normal_build_exhausted_lease_cleanup_closes_without_reuse_or_retirement() {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let media = temp.path().join("media");
    fs::create_dir(&media).unwrap();
    let writer = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(&media, &profile, Path::new("lease"))
            .unwrap(),
        SourceId::from_string("lease"),
        1,
    )
    .unwrap();
    assert!(writer.current_revision().is_err());
    let authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    assert_eq!(
        input.subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    let correlation = SourceWalTransportCorrelation::new(
        [226; 32],
        ParticipantId::new(),
        AdmissionClaimId::new(),
        AdmissionClaimId::new(),
    )
    .unwrap();
    let pending_input = input.clone();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let mut request = transport
        .submit_lease_with_binding(input, correlation.clone(), deadline)
        .unwrap();
    assert!(inbox.try_next().is_none());
    let command = inbox.try_next_any().unwrap();
    assert!(matches!(&command, SourceWalProducerCommand::Lease(_)));
    let mut owner = command.acquire(writer);
    assert!(owner.is_prepared());
    owner.publish_prepared(Some(&authority)).unwrap();
    owner.revalidate(Some(&authority)).unwrap();
    let other = Connection::open(profile.profile_root().join("lease").join(DB_FILE_NAME)).unwrap();
    other.busy_timeout(Duration::ZERO).unwrap();
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    let holder = request.try_prepared().unwrap();
    assert_eq!(
        holder.acknowledge_settlement(),
        Err(Rejection::SettlementUnsupported)
    );
    let Status::PreparedLease(history) = holder.status() else {
        panic!("original lease")
    };
    assert_eq!(
        history.original_binding().stored_fence().subject().audit(),
        &SourceRegistryAuditObservation::Unavailable {}
    );
    let mut pending = Vec::new();
    loop {
        match transport.submit_lease_with_binding(
            pending_input.clone(),
            correlation.clone(),
            deadline,
        ) {
            Ok(request) => pending.push(request),
            Err(Rejection::Full) => break,
            Err(other) => panic!("unexpected input rejection: {other:?}"),
        }
    }
    assert!(!pending.is_empty());
    drop(holder);
    let owner = match owner.into_writer() {
        Err(owner) => owner,
        Ok(_) => panic!("unfunded cleanup must not return a reusable writer"),
    };
    assert_eq!(request.status(), Status::Unavailable(Rejection::Full));
    // The unfunded destructor closes the actual native connection, without a
    // full allocating validation capture or a fabricated cleanup acknowledgement.
    drop(owner);
    assert_eq!(request.status(), Status::Unavailable(Rejection::OwnerLost));
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert!(!history.scope_descriptor_bytes().is_empty());
    inbox.close();
    assert!(
        pending
            .iter()
            .all(|r| matches!(r.status(), Status::Rejected(_)))
    );
    drop(pending);
    drop(history);
    drop(authority);
}
