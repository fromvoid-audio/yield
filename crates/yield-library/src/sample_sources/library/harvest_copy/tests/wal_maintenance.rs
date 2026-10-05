//! Original Source/Global/SQLite evidence; stored copy intents are not physical admission.
use super::*;
use crate::sample_sources::library::connection::LibraryDatabase;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

mod exclusions;
mod ownership;
mod replay;

#[derive(Clone, Copy)]
enum Fault {
    None,
    CheckpointDenied,
    ObservationDenied,
}

fn stop_global(f: &mut Fixture) {
    let mut global = f.global.take().unwrap();
    global.shutdown();
    drop(global);
}

fn pressure_hook(
    counter: Arc<AtomicUsize>,
    fault: Fault,
) -> Arc<dyn Fn(&Connection) + Send + Sync> {
    Arc::new(move |connection| {
        connection
            .execute_batch(
                "PRAGMA wal_autocheckpoint=0;
             CREATE TABLE copy_maintenance_payload(value BLOB);
             INSERT INTO copy_maintenance_payload VALUES(zeroblob(34603008));",
            )
            .unwrap();
        let bytes = crate::sqlite_wal::retained_wal_bytes(connection).unwrap();
        assert!(bytes >= crate::WAL_SOFT_WATERMARK_BYTES);
        assert!(bytes < crate::sqlite_wal::WAL_HARD_WATERMARK_BYTES);
        let committed = Arc::new(AtomicBool::new(false));
        if matches!(fault, Fault::ObservationDenied) {
            let committed = committed.clone();
            connection.commit_hook(Some(move || {
                committed.store(true, Ordering::SeqCst);
                false
            }));
        }
        let counter = counter.clone();
        connection.authorizer(Some(move |context: AuthContext<'_>| match context.action {
            AuthAction::Pragma {
                pragma_name: "wal_checkpoint",
                ..
            } => {
                counter.fetch_add(1, Ordering::SeqCst);
                if matches!(fault, Fault::CheckpointDenied) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }
            AuthAction::Pragma {
                pragma_name: "journal_mode",
                ..
            } if committed.load(Ordering::SeqCst) => Authorization::Deny,
            _ => Authorization::Allow,
        }));
    })
}

fn start_pressure(f: &mut Fixture, counter: Arc<AtomicUsize>, fault: Fault) {
    stop_global(f);
    let global = GlobalLibraryWriter::start_with_database_hook_for_test(
        f.guard(),
        pressure_hook(counter, fault),
        None,
    )
    .unwrap();
    wait_global(&global);
    f.global = Some(global);
}

#[test]
fn harvest_copy_wal_actual_worker_marks_new_applied_and_retained_dispositions() {
    for disposition in [
        HarvestCopyDisposition::Applied,
        HarvestCopyDisposition::GraphChanged,
        HarvestCopyDisposition::IdentityConflict,
    ] {
        let mut f = Fixture::new();
        if disposition == HarvestCopyDisposition::IdentityConflict {
            f.db().execute("INSERT INTO harvest_files(source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at) VALUES('destination','new.wav',10,1,?1,'done',1)", ["b".repeat(64)]).unwrap();
        }
        let counter = Arc::new(AtomicUsize::new(0));
        start_pressure(&mut f, counter.clone(), Fault::None);
        let request = f.request();
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "snapshot stays maintenance-free"
        );
        if disposition == HarvestCopyDisposition::GraphChanged {
            f.db().execute("INSERT INTO harvest_files(source_id,relative_path,harvest_state,discovered_at) VALUES('origin','one.wav','done',1)", []).unwrap();
        }
        let result = f.apply(request.clone()).unwrap();
        assert!(matches!(result, HarvestCopyResult::Committed(_)));
        assert_eq!(result.receipt().disposition(), disposition);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        let replay = f.apply(request).unwrap();
        assert!(matches!(replay, HarvestCopyResult::Replayed(_)));
        assert_eq!(replay.receipt().sequence(), result.receipt().sequence());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(f.count("harvest_copy_receipts"), 1);
    }
}

#[test]
fn harvest_copy_wal_actual_worker_preserves_commit_with_reader_and_maintenance_faults() {
    for fault in [
        Fault::None,
        Fault::CheckpointDenied,
        Fault::ObservationDenied,
    ] {
        let mut f = Fixture::new();
        let counter = Arc::new(AtomicUsize::new(0));
        start_pressure(&mut f, counter.clone(), fault);
        let request = f.request();
        let reader = f.db();
        reader.execute_batch("BEGIN").unwrap();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM harvest_derivations", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let result = f.apply(request.clone()).unwrap();
        assert!(matches!(result, HarvestCopyResult::Committed(_)));
        assert_eq!(
            result.receipt().disposition(),
            HarvestCopyDisposition::Applied
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            if matches!(fault, Fault::ObservationDenied) {
                0
            } else {
                1
            }
        );
        assert_eq!(f.count("harvest_derivations"), 1);
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM harvest_derivations", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        reader.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM harvest_derivations", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(matches!(
            f.apply(request).unwrap(),
            HarvestCopyResult::Replayed(_)
        ));
        drop(reader);
    }
}
