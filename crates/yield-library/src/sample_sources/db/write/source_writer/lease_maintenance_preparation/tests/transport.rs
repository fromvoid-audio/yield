//! Actual original source custody and shared inbox accounting, without native grant fixtures.
use super::*;
use crate::sample_sources::wal_transport::{
    SourceWalProducerCommand, SourceWalTransport, SourceWalTransportCorrelation,
    SourceWalTransportRejection as Rejection, SourceWalTransportStatus as Status,
};
fn correlation() -> SourceWalTransportCorrelation {
    use crate::operation_journal::{AdmissionClaimId, ParticipantId};
    SourceWalTransportCorrelation::new(
        [29; 32],
        ParticipantId::new(),
        AdmissionClaimId::new(),
        AdmissionClaimId::new(),
    )
    .unwrap()
}
fn submitted(
    writer: &SourceWriter,
    transport: &SourceWalTransport,
) -> crate::sample_sources::wal_transport::SourceWalTransportRequest {
    let deadline = Instant::now() + Duration::from_secs(2);
    transport
        .submit_lease_with_binding(
            writer
                .capture_lease_maintenance_allocation(deadline)
                .unwrap(),
            correlation(),
            deadline,
        )
        .unwrap()
}
#[test]
fn original_lease_fifo_scope_and_last_history_copy_retain_actual_context_credit() {
    let (f, writer) = fixture();
    let authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let handle = unsafe { writer.database.connection.handle() };
    let original_settings = settings(&writer);
    let before = writer.current_scan_fence().unwrap();
    let other = external(&f);
    let original_lease = lease(&other);
    let (transport, mut inbox) = SourceWalTransport::channel();
    let mut request = submitted(&writer, &transport);
    assert!(inbox.try_next().is_none());
    let command = inbox.try_next_any().unwrap();
    assert!(matches!(&command, SourceWalProducerCommand::Lease(_)));
    let mut owner = command.acquire(writer);
    assert!(owner.is_prepared());
    owner.publish_prepared(Some(&authority)).unwrap();
    owner.revalidate(Some(&authority)).unwrap();
    let holder = request.try_prepared().unwrap();
    assert_eq!(
        holder.acknowledge_settlement(),
        Err(Rejection::SettlementUnsupported)
    );
    let Status::PreparedLease(history) = holder.status() else {
        panic!("real lease owner");
    };
    assert!(matches!(
        history.original_binding().stored_fence().subject().audit(),
        crate::sample_sources::db::SourceRegistryAuditObservation::Unavailable {}
    ));
    let copy = history.clone();
    assert_eq!(
        copy.scope_descriptor_bytes().as_ptr(),
        history.scope_descriptor_bytes().as_ptr()
    );
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    drop(holder);
    let writer = owner
        .into_writer()
        .unwrap_or_else(|_| panic!("original cleanup"));
    assert_eq!(request.status(), Status::Aborted);
    assert_eq!(unsafe { writer.database.connection.handle() }, handle);
    assert_eq!(settings(&writer), original_settings);
    assert_eq!(writer.current_scan_fence().unwrap(), before);
    assert!(writer.current_revision().is_err());
    assert_eq!(lease(&other), original_lease);
    assert_eq!(transport.context_usage_for_tests().0, 0);
    let bytes = transport.context_usage_for_tests().1;
    assert!(bytes > history.scope_descriptor_bytes().len());
    transport.close();
    drop(history);
    assert_eq!(transport.context_usage_for_tests(), (0, bytes));
    drop(copy);
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
    drop(authority);
    writer.close().unwrap();
}
#[test]
fn foreign_registration_never_publishes_and_original_writer_is_restored() {
    let (f, writer) = fixture();
    let foreign = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(
            &f.media,
            &f.profile,
            Path::new("foreign"),
        )
        .unwrap(),
        SourceId::from_string("foreign"),
        1,
    )
    .unwrap();
    let foreign_authority = foreign
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let request = submitted(&writer, &transport);
    let mut owner = inbox.try_next_any().unwrap().acquire(writer);
    assert_eq!(
        owner.publish_prepared(Some(&foreign_authority)),
        Err(Rejection::Scope)
    );
    assert_eq!(request.status(), Status::Pending);
    owner.request_abort(Some(Rejection::Scope));
    let writer = owner
        .into_writer()
        .unwrap_or_else(|_| panic!("original cleanup"));
    assert_eq!(request.status(), Status::Rejected(Rejection::Scope));
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
    drop(foreign_authority);
    foreign.close().unwrap();
    writer.close().unwrap();
}
#[test]
fn genuine_audited_and_lease_pending_inputs_exhaust_one_combined_pool_and_close_drops_before_refund()
 {
    let (_f, writer) = fixture();
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
    let revision = writer.current_revision().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let mut requests = Vec::new();
    loop {
        let request = if requests.len() % 2 == 0 {
            transport.submit_lease_with_binding(input.clone(), correlation(), deadline)
        } else {
            transport.submit_with_binding(revision.clone(), correlation(), deadline)
        };
        match request {
            Ok(r) => requests.push(r),
            Err(Rejection::Full) => break,
            Err(e) => panic!("{e}"),
        }
    }
    let (slots, bytes) = transport.context_usage_for_tests();
    assert_eq!(slots, requests.len());
    assert!(slots > 2 && slots <= 64 && bytes <= 64 * 1024);
    assert!(inbox.try_next().is_none());
    let first = inbox.try_next_any().unwrap();
    assert!(matches!(&first, SourceWalProducerCommand::Lease(_)));
    first.reject(Rejection::Cancelled);
    assert!(inbox.try_next().is_some()); // Dropped pending audited command reports owner loss.
    drop(inbox);
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
    assert!(
        requests
            .iter()
            .all(|r| matches!(r.status(), Status::Rejected(_)))
    );
    writer.close().unwrap();
}
#[test]
fn cancellation_and_original_deadline_never_release_native_custody_by_themselves() {
    let (f, writer) = fixture();
    let authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let deadline = Instant::now() + Duration::from_millis(250);
    let input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let mut request = transport
        .submit_lease_with_binding(input, correlation(), deadline + Duration::from_secs(1))
        .unwrap();
    let mut owner = inbox.try_next_any().unwrap().acquire(writer);
    assert_eq!(owner.deadline(), deadline);
    owner.publish_prepared(Some(&authority)).unwrap();
    let holder = request.try_prepared().unwrap();
    let other = external(&f);
    std::thread::sleep(
        deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(2),
    );
    assert_eq!(owner.revalidate(Some(&authority)), Err(Rejection::Deadline));
    assert!(matches!(holder.status(), Status::PreparedLease(_)));
    assert_eq!(transport.context_usage_for_tests().0, 1);
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    drop(holder);
    let writer = owner
        .into_writer()
        .unwrap_or_else(|_| panic!("cleanup after original deadline"));
    assert_eq!(request.status(), Status::Aborted);
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
    drop(authority);
    writer.close().unwrap();
}
#[test]
fn preexisting_transaction_is_neither_adopted_rolled_back_nor_given_empty_close_authority() {
    let (f, writer) = fixture();
    let mut authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let request = submitted(&writer, &transport);
    writer
        .database
        .connection
        .execute_batch(
            "BEGIN IMMEDIATE; INSERT INTO metadata(key,value) VALUES ('preexisting','uncommitted')",
        )
        .unwrap();
    let owner = inbox.try_next_any().unwrap().acquire(writer);
    assert!(!owner.is_prepared());
    let owner = owner
        .into_writer()
        .err()
        .expect("preexisting transaction stays private");
    assert!(!owner.source_authority_close_ready());
    assert!(owner.close_for_retirement(&mut authority).is_err());
    assert!(matches!(
        request.status(),
        Status::Unavailable(Rejection::OwnerLost)
    ));
    let other = external(&f);
    assert_eq!(
        other
            .query_row(
                "SELECT count(*) FROM metadata WHERE key='preexisting'",
                [],
                |r| r.get::<_, u64>(0)
            )
            .unwrap(),
        0
    );
}
#[test]
fn owner_loss_preserves_unavailable_outcome_and_history_accounting_without_fake_retirement() {
    let (f, writer) = fixture();
    let authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let request = submitted(&writer, &transport);
    let mut owner = inbox.try_next_any().unwrap().acquire(writer);
    owner.publish_prepared(Some(&authority)).unwrap();
    let Status::PreparedLease(history) = request.status() else {
        panic!("original lease")
    };
    drop(owner);
    assert_eq!(request.status(), Status::Unavailable(Rejection::OwnerLost));
    let other = external(&f);
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    // The genuine Prepared authority still retains its source lease. Native connection
    // loss cannot fabricate its close/release acknowledgement.
    assert_eq!(lease(&other).3, "active");
    drop(authority);
    assert_eq!(lease(&other).3, "released");
    drop(request);
    assert_eq!(transport.context_usage_for_tests().0, 0);
    assert!(transport.context_usage_for_tests().1 > 0);
    drop(history);
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
}

#[test]
fn capture_staging_full_returns_the_untouched_original_owner_without_new_budget() {
    let (_f, writer) = fixture();
    let handle = unsafe { writer.database.connection.handle() };
    let original_settings = settings(&writer);
    let deadline = Instant::now() + Duration::from_secs(2);
    let input = writer
        .capture_lease_maintenance_allocation(deadline)
        .unwrap();
    let (transport, mut inbox) = SourceWalTransport::channel();
    let mut requests = Vec::new();
    loop {
        match transport.submit_lease_with_binding(input.clone(), correlation(), deadline) {
            Ok(request) => requests.push(request),
            Err(Rejection::Full) => break,
            Err(e) => panic!("{e}"),
        }
    }
    let owner = inbox.try_next_any().unwrap().acquire(writer);
    assert!(!owner.is_prepared());
    assert_eq!(owner.rejection(), Some(Rejection::Full));
    let writer = owner
        .into_writer()
        .unwrap_or_else(|_| panic!("unchanged original owner"));
    assert_eq!(unsafe { writer.database.connection.handle() }, handle);
    assert_eq!(settings(&writer), original_settings);
    assert!(writer.current_revision().is_err());
    drop(inbox);
    assert_eq!(transport.context_usage_for_tests(), (0, 0));
    writer.close().unwrap();
}

#[test]
fn failed_real_empty_rollback_cannot_retire_under_local_prepared_authority() {
    let (f, writer) = fixture();
    let mut authority = writer
        .catalog_database()
        .prepare_source_authority()
        .unwrap();
    let deny = Arc::new(AtomicBool::new(false));
    let active = deny.clone();
    writer
        .database
        .connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if active.load(Ordering::Acquire)
                && matches!(
                    context.action,
                    AuthAction::Transaction {
                        operation: TransactionOperation::Rollback
                    }
                )
            {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
    let (transport, mut inbox) = SourceWalTransport::channel();
    let request = submitted(&writer, &transport);
    let mut owner = inbox.try_next_any().unwrap().acquire(writer);
    owner.publish_prepared(Some(&authority)).unwrap();
    deny.store(true, Ordering::Release);
    let owner = owner
        .into_writer()
        .err()
        .expect("actual denied rollback retains custody");
    assert!(owner.source_authority_close_ready());
    let other = external(&f);
    assert_eq!(
        other
            .execute_batch("BEGIN IMMEDIATE")
            .unwrap_err()
            .sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy)
    );
    assert!(owner.close_for_retirement(&mut authority).is_err());
    assert_eq!(request.status(), Status::Unavailable(Rejection::OwnerLost));
    other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(lease(&other).3, "active");
    drop(authority);
    assert_eq!(lease(&other).3, "released");
}
