//! Actual original SQLite owners through the sealed source inbox, never admission grants.
use super::tag_wal_preparation::{
    assert_busy, authorizer, control_changes, db, external, request, settings, writer,
};
use super::*;
use crate::operation_journal::{AdmissionClaimId, ParticipantId};
use crate::sample_sources::wal_transport::{
    SourceWalPolicyFamily as Family, SourceWalTransport, SourceWalTransportCorrelation,
    SourceWalTransportRejection as Reject, SourceWalTransportStatus as Status,
};
use rusqlite::ffi;
use std::{
    ffi::{CStr, c_char, c_void},
    time::Duration,
};
fn correlation() -> SourceWalTransportCorrelation {
    SourceWalTransportCorrelation::new(
        [49; 32],
        ParticipantId::new(),
        AdmissionClaimId::new(),
        AdmissionClaimId::new(),
    )
    .unwrap()
}
macro_rules! case {
    ($name:ident, $case:literal) => {
        #[test]
        fn $name() {
            tag_metadata_budget::run_case($case);
        }
    };
}
case!(
    source_policy_transport_parks_actual_families_without_ordinary_sql,
    "policy-transport-success"
);
case!(
    source_policy_transport_rejects_stale_full_facts_before_publication,
    "policy-transport-stale"
);
case!(
    source_policy_transport_keeps_original_deadline,
    "policy-transport-deadline"
);
case!(
    source_policy_transport_retains_failed_rollback_and_settings,
    "policy-transport-rollback"
);
case!(
    source_policy_transport_never_returns_preexisting_transaction,
    "policy-transport-active"
);
case!(
    source_policy_transport_accounts_and_releases_pending_policy_payloads,
    "policy-transport-credits"
);

case!(
    source_policy_transport_rejects_real_complete_scope_larger_than_input_budget,
    "policy-transport-bounds"
);

case!(
    source_policy_transport_sticky_restoration_requires_genuine_retirement,
    "policy-transport-restore"
);

case!(
    source_policy_transport_rejects_wrong_original_owner,
    "policy-transport-origin"
);
case!(
    source_policy_transport_busy_attempt_restores_exact_original_owner,
    "policy-transport-busy"
);
case!(
    source_policy_transport_owner_loss_never_claims_reuse,
    "policy-transport-loss"
);

fn submit(
    t: &SourceWalTransport,
    w: &SourceWriter,
    family: Family,
) -> crate::sample_sources::wal_transport::SourceWalTransportRequest {
    match family {
        Family::Tag => t
            .submit_tag_with_binding(
                w.capture_tag_admission_observation(
                    &w.current_revision().unwrap(),
                    "old tag",
                    crate::sample_sources::SourceTagAssignmentMutation::Delete,
                )
                .unwrap(),
                correlation(),
                request(),
            )
            .unwrap(),
        Family::Layout => t
            .submit_layout_with_binding(
                w.capture_layout_metadata_budget(&w.current_revision().unwrap())
                    .unwrap(),
                correlation(),
                request(),
            )
            .unwrap(),
    }
}
// Deny all ordinary row reads, schema/page queries, writes and nested transaction entry.
// The original separately retained CONTROL connection is unaffected by this authorizer.
unsafe extern "C" fn parked(
    _: *mut c_void,
    action: i32,
    a: *const c_char,
    _: *const c_char,
    _: *const c_char,
    _: *const c_char,
) -> i32 {
    let name = if a.is_null() {
        b"".as_slice()
    } else {
        unsafe { CStr::from_ptr(a) }.to_bytes()
    };
    if matches!(
        action,
        ffi::SQLITE_READ | ffi::SQLITE_INSERT | ffi::SQLITE_UPDATE | ffi::SQLITE_DELETE
    ) || (action == ffi::SQLITE_PRAGMA
        && matches!(name, b"page_count" | b"page_size" | b"schema_version"))
        || (action == ffi::SQLITE_TRANSACTION && name == b"BEGIN")
    {
        ffi::SQLITE_DENY
    } else {
        ffi::SQLITE_OK
    }
}
pub(super) fn dispatch(case: &str) {
    for family in [Family::Tag, Family::Layout] {
        let (mut w, _fixture, old) = writer();
        let original = settings(&w);
        let changes = db(&w).total_changes();
        let control = control_changes(&w);
        let handle = unsafe { db(&w).handle() };
        let other = external(&w);
        let mut authority = w.catalog_database().prepare_source_authority().unwrap();
        let (transport, mut inbox) = SourceWalTransport::channel();
        if case == "policy-transport-bounds" {
            if family == Family::Layout {
                continue;
            }
            db(&w)
                .execute("DELETE FROM wav_file_tags WHERE tag_id=?1", [old])
                .unwrap();
            for i in 0..128 {
                let path = format!("{i:03}/{}/file.wav", "x".repeat(700));
                db(&w).execute("INSERT INTO wav_files(path,file_size,modified_ns,extension) VALUES (?1,1,1,'wav')", [&path]).unwrap();
                db(&w)
                    .execute(
                        "INSERT INTO wav_file_tags(path,tag_id) VALUES (?1,?2)",
                        rusqlite::params![path, old],
                    )
                    .unwrap();
            }
            let observation = w
                .capture_tag_admission_observation(
                    &w.current_revision().unwrap(),
                    "old tag",
                    crate::sample_sources::SourceTagAssignmentMutation::Delete,
                )
                .unwrap();
            assert_eq!(observation.selection().paths().len(), 128);
            assert!(observation.retained_input_bytes().unwrap() > 65536);
            assert!(matches!(
                transport.submit_tag_with_binding(observation, correlation(), request()),
                Err(Reject::InvalidContext)
            ));
            assert_eq!(transport.context_usage_for_tests(), (0, 0));
            assert!(inbox.try_next().is_none());
            assert!(db(&w).is_autocommit());
            assert_eq!(settings(&w), original);
            continue;
        }
        let mut r = submit(&transport, &w, family);
        match case {
            "policy-transport-credits" => {
                let (slots, bytes) = transport.context_usage_for_tests();
                assert_eq!(slots, 1);
                assert!(bytes > 0 && bytes <= 65536);
                drop(inbox.try_next().unwrap());
                assert_eq!(r.status(), Status::Rejected(Reject::OwnerLost));
                assert_eq!(transport.context_usage_for_tests(), (0, 0));
                let _r = submit(&transport, &w, family);
                transport.close();
                assert_eq!(transport.context_usage_for_tests(), (0, 0));
                continue;
            }
            "policy-transport-stale" => {
                db(&w)
                    .execute_batch(
                        "PRAGMA ignore_check_constraints=ON; UPDATE source_directory_generations SET complete=0 WHERE status='active'; PRAGMA ignore_check_constraints=OFF",
                    )
                    .unwrap();
            }
            "policy-transport-deadline" => {
                std::thread::sleep(Duration::from_millis(2050));
            }
            "policy-transport-origin" => {
                w = SourceWriter::new(
                    w.into_catalog_database(),
                    SourceId::from_string("tag-owned"),
                    7,
                )
                .unwrap();
            }
            "policy-transport-busy" => {
                other.execute_batch("BEGIN IMMEDIATE").unwrap();
            }
            "policy-transport-active" => {
                db(&w).execute_batch("BEGIN IMMEDIATE").unwrap();
            }
            _ => {}
        }
        let mut owner = inbox.try_next().unwrap().acquire(w);
        if matches!(
            case,
            "policy-transport-stale"
                | "policy-transport-deadline"
                | "policy-transport-origin"
                | "policy-transport-busy"
        ) {
            assert!(!owner.is_prepared());
            assert!(owner.publish_prepared(Some(&authority)).is_err());
            let returned = owner
                .into_writer()
                .unwrap_or_else(|_| panic!("rejected original owner reusable"));
            assert_eq!(settings(&returned), original);
            assert_eq!(unsafe { db(&returned).handle() }, handle);
            if case == "policy-transport-busy" {
                other.execute_batch("ROLLBACK").unwrap();
            }
            continue;
        }
        if case == "policy-transport-active" {
            assert!(!owner.is_prepared());
            let owner = owner
                .into_writer()
                .err()
                .expect("preexisting transaction retained");
            assert_busy(&other);
            assert!(matches!(r.status(), Status::Unavailable(_)));
            drop(owner);
            other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
            continue;
        }
        assert!(owner.is_prepared());
        assert_busy(&other);
        assert_eq!(owner.publish_prepared(None), Err(Reject::Scope));
        owner.publish_prepared(Some(&authority)).unwrap();
        // Initial full capture precedes parking; subsequent current validation is SQL-free.
        authorizer(handle, Some(parked));
        for _ in 0..3 {
            owner.revalidate(Some(&authority)).unwrap();
        }
        authorizer(handle, None);
        if case == "policy-transport-loss" {
            let witness = owner.outcome_observation();
            drop(owner);
            assert_eq!(witness.status(), Status::Unavailable(Reject::OwnerLost));
            assert_eq!(r.status(), Status::Unavailable(Reject::OwnerLost));
            other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
            continue;
        }
        let holder = r.try_prepared().unwrap();
        let Status::Prepared(observation) = holder.status() else {
            panic!("actual preparation")
        };
        assert_eq!(observation.policy_family(), Some(family));
        assert!(observation.original_binding().is_some());
        assert_eq!(
            holder.acknowledge_settlement(),
            Err(Reject::SettlementUnsupported)
        );
        drop(holder);
        assert!(owner.is_cancelled());
        if case == "policy-transport-restore" {
            authorizer(handle, Some(tag_wal_preparation::deny_restore));
            owner = owner
                .into_writer()
                .err()
                .expect("unconfirmed settings retain actual owner");
            assert!(matches!(r.status(), Status::Unavailable(_)));
            authorizer(handle, None);
            owner = owner
                .into_writer()
                .err()
                .expect("restoration failure remains sticky");
            let witness = owner.outcome_observation();
            assert!(owner.source_authority_close_ready());
            // Prepared is not a journal drain. The real authority must reject, without an ack.
            assert!(owner.close_for_retirement(&mut authority).is_err());
            assert!(matches!(
                witness.status(),
                Status::Unavailable(Reject::OwnerLost)
            ));
            assert!(!matches!(r.status(), Status::Retired));
            continue;
        }
        if case == "policy-transport-rollback" {
            authorizer(handle, Some(tag_wal_preparation::deny));
            owner = owner.into_writer().err().expect("failed rollback retained");
            assert!(matches!(r.status(), Status::Unavailable(_)));
            assert_busy(&other);
            authorizer(handle, None);
        }
        let returned = owner
            .into_writer()
            .unwrap_or_else(|_| panic!("original cleanup"));
        assert_eq!(unsafe { db(&returned).handle() }, handle);
        assert_eq!(settings(&returned), original);
        assert_eq!(db(&returned).total_changes(), changes);
        assert_eq!(control_changes(&returned), control);
        assert_eq!(r.status(), Status::Aborted);
        assert_eq!(observation.policy_family(), Some(family));
        assert!(
            transport.context_usage_for_tests().1 > 0,
            "historical copies retain their own credits"
        );
        drop(observation);
        assert_eq!(transport.context_usage_for_tests(), (0, 0));
        other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    }
}
