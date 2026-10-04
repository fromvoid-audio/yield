//! SQL storage shapes only: these zero blobs are not authenticated subjects or receipts.
#![cfg(all(test, target_os = "linux"))]
use super::*;
use crate::{
    app_dirs::{AppRootGuard, WritableProfileGuard},
    lifecycle_allocation::workloads::isolated,
    sqlite_budget::{BudgetFailure, LogicalBudget},
};
use rusqlite::{ErrorCode, params};
fn evidence(
    case: &str,
    database: &LibraryDatabase,
    allocation: &LifecycleDatabaseAllocation,
    report: crate::sqlite_budget::BudgetReport,
) {
    let actual = database
        .connection
        .retained_wal_objects(
            database.preparation_root().unwrap(),
            std::path::Path::new(super::super::super::super::LIBRARY_DB_FILE_NAME),
            deadline(),
        )
        .unwrap();
    assert_eq!(actual.main.identity, allocation.main().identity());
    assert_eq!(actual.wal.identity, allocation.wal().identity());
    crate::lifecycle_allocation::workloads::evidence(
        case,
        &database.connection,
        allocation,
        report,
        actual.shm,
    );
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(2)
}
fn fixture(page_size: u64, full: bool, direction: &str, test: impl FnOnce(&LibraryDatabase)) {
    let temp = tempfile::tempdir().unwrap();
    let _root = AppRootGuard::set(temp.path().to_path_buf()).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let database = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
    let c = &database.connection;
    // Fixture construction only, BEFORE capture/preparation/restriction. No measured effect
    // checkpoints, vacuums, retries, or kills a reader to make its operation fit.
    c.execute_batch(&format!("PRAGMA journal_mode=DELETE; PRAGMA page_size={page_size}; VACUUM; PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; PRAGMA mmap_size=0")).unwrap();
    assert_eq!(
        c.pragma_query_value(None, "page_size", |r| r.get::<_, u64>(0))
            .unwrap(),
        page_size
    );
    if full {
        c.execute_batch("BEGIN; WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<8191) INSERT INTO source_lifecycle_references SELECT printf('%036d',x),printf('%0256d',x),zeroblob(16384),CASE WHEN x<=4095 THEN 'Enrolled' ELSE 'Retired' END,printf('%036d',x),printf('%0256d',x),zeroblob(32) FROM n;
        WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<8191) INSERT INTO source_lifecycle_reference_receipts(operation_id,work_key,payload_identity,owner_identity,registration_id,before_identity,after_identity,disposition,receipt) SELECT printf('%036d',x),printf('%0256d',x),zeroblob(32),printf('%036d',x),printf('%036d',x),zeroblob(32),zeroblob(32),'ExpectedRelationChanged',zeroblob(16384) FROM n; COMMIT;").unwrap();
    } else if matches!(direction, "Draining" | "Retired") {
        c.execute_batch("INSERT INTO source_lifecycle_references VALUES(printf('%036d',1),printf('%0256d',1),zeroblob(16384),'Enrolled',printf('%036d',1),printf('%0256d',1),zeroblob(32))").unwrap();
    }
    c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    test(&database);
}
fn effect(database: &LibraryDatabase, direction: &str) -> usize {
    let c = &database.connection;
    let operation = format!("{:036}", 999999);
    let work = "w".repeat(256);
    let changed=match direction {
        "Enrolled" => c.execute("INSERT INTO source_lifecycle_references VALUES(?1,?2,zeroblob(16384),'Enrolled',?1,?3,zeroblob(32))", params![operation, "s".repeat(256), work]).unwrap(),
        "Draining" | "Retired" => c.execute("UPDATE source_lifecycle_references SET state=?1,last_operation_id=?2,last_work_key=?3 WHERE registration_id=printf('%036d',1) AND state='Enrolled' AND row_identity=zeroblob(32)", params![direction,operation,work]).unwrap(),
        "ExpectedRelationChanged" => c.execute("UPDATE source_lifecycle_references SET state='Retired' WHERE registration_id=printf('%036d',1) AND row_identity=x'01'", []).unwrap(),
        _ => unreachable!(),
    };
    c.execute("INSERT INTO source_lifecycle_reference_receipts(operation_id,work_key,payload_identity,owner_identity,registration_id,before_identity,after_identity,disposition,receipt) VALUES(?1,?2,zeroblob(32),?1,?1,zeroblob(32),zeroblob(32),?3,zeroblob(16384))", params![operation,work,direction]).unwrap();
    changed
}
#[test]
fn lifecycle_vfs_workloads_global_matrix() {
    let Some(case) = std::env::var("YIELD_LIFECYCLE_VFS_WORKLOAD").ok() else {
        for page in [512, 4096, 65536] {
            for full in [false, true] {
                for direction in ["Enrolled", "Draining", "Retired", "ExpectedRelationChanged"] {
                    isolated(
                        &format!("{}::lifecycle_vfs_workloads_global_matrix", module_path!()),
                        &format!("{page}:{full}:{direction}"),
                    );
                }
            }
        }
        return;
    };
    let fields: Vec<_> = case.split(':').collect();
    let page: u64 = fields[0].parse().unwrap();
    let full: bool = fields[1].parse().unwrap();
    let direction = fields[2];
    fixture(page, full, direction, |database| {
        let c = &database.connection;
        // Include a real lower original pager cap, sufficient for one bounded SQL shape.
        let count: u64 = c
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        c.pragma_update(None, "max_page_count", count + 1024 * 1024 / page)
            .unwrap();
        let before = Settings::read(c).unwrap();
        let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
        let allocation = policy.allocation().clone();
        assert_eq!(allocation.selected_maximum_pages(), before.max_pages);
        policy.install().unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        policy.validate_reserved(deadline()).unwrap();
        let attempt = c
            .restrict(allocation.logical_restriction().unwrap())
            .unwrap();
        assert_eq!(
            effect(database, direction),
            usize::from(direction != "ExpectedRelationChanged")
        );
        policy.validate_settings().unwrap();
        c.execute_batch("COMMIT").unwrap();
        let expected_references = if full {
            8191 + u64::from(direction == "Enrolled")
        } else {
            u64::from(direction != "ExpectedRelationChanged")
        };
        let counts = super::super::counts(database).unwrap();
        assert_eq!(counts.0, expected_references);
        assert_eq!(counts.1, if full { 8192 } else { 1 });
        assert_eq!(
            counts.2,
            if full {
                4095 + u64::from(direction == "Enrolled") - u64::from(direction == "Retired")
            } else {
                u64::from(matches!(direction, "Enrolled" | "Draining"))
            }
        );
        let preserved: u64=c.query_row("SELECT count(*) FROM source_lifecycle_reference_receipts WHERE operation_id<>printf('%036d',999999) AND receipt=zeroblob(16384) AND length(CAST(work_key AS BLOB))=256 AND disposition='ExpectedRelationChanged'",[],|r|r.get(0)).unwrap();
        assert_eq!(preserved, if full { 8191 } else { 0 });
        assert_eq!(attempt.report().failure, None);
        assert!(attempt.report().charged[1] > 0);
        evidence(&case, database, &allocation, attempt.report());
        policy.finish().unwrap();
        assert_eq!(Settings::read(c).unwrap(), before);
        let frozen = attempt.report();
        drop(attempt);
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(
            c.restrict(allocation.logical_restriction().unwrap())
                .is_err()
        );
        c.execute_batch("ROLLBACK").unwrap();
        assert_eq!(frozen.failure, None); // No reset, refund, second effect or alleged release.
    });
}
#[test]
fn lifecycle_vfs_workloads_insufficient_wal_preserves_receipts_and_original_rows() {
    if std::env::var("YIELD_LIFECYCLE_VFS_WORKLOAD").is_err() {
        isolated(
            &format!(
                "{}::lifecycle_vfs_workloads_insufficient_wal_preserves_receipts_and_original_rows",
                module_path!()
            ),
            "wal-full",
        );
        return;
    }
    fixture(4096, true, "Retired", |database| {
        let c = &database.connection;
        let before = super::super::counts(database).unwrap();
        let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
        let allocation = policy.allocation().clone();
        policy.install().unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        policy.validate_reserved(deadline()).unwrap();
        let unit = allocation.main().allocation_unit();
        let spec = LogicalBudget::new(
            [
                allocation.main().write_ceiling(),
                0,
                allocation.shm().write_ceiling(),
            ],
            unit,
            unit,
            unit,
        )
        .unwrap();
        let attempt = c.restrict(spec).unwrap();
        let wal_before = crate::sqlite_wal::retained_wal_bytes(c).unwrap();
        effect(database, "Retired");
        assert_eq!(
            c.execute_batch("COMMIT").unwrap_err().sqlite_error_code(),
            Some(ErrorCode::DiskFull)
        );
        if !c.is_autocommit() {
            c.execute_batch("ROLLBACK").unwrap();
        }
        assert!(c.is_autocommit());
        assert_eq!(super::super::counts(database).unwrap(), before);
        assert_eq!(c.query_row("SELECT count(*) FROM source_lifecycle_reference_receipts WHERE receipt=zeroblob(16384) AND length(CAST(work_key AS BLOB))=256 AND disposition='ExpectedRelationChanged'",[],|r|r.get::<_,u64>(0)).unwrap(),8191);
        assert_eq!(c.query_row("SELECT state FROM source_lifecycle_references WHERE registration_id=printf('%036d',1)",[],|r|r.get::<_,String>(0)).unwrap(),"Enrolled");
        assert_eq!(
            crate::sqlite_wal::retained_wal_bytes(c).unwrap(),
            wal_before
        );
        assert_eq!(attempt.report().failure, Some(BudgetFailure::Exhausted));
        assert_eq!(attempt.report().charged[1], 0);
        evidence("wal-full", database, &allocation, attempt.report());
        policy.finish().unwrap();
    });
}
#[test]
fn lifecycle_vfs_workloads_pager_full_rolls_back_without_receipt_or_relation_eviction() {
    if std::env::var("YIELD_LIFECYCLE_VFS_WORKLOAD").is_err() {
        isolated(
            &format!(
                "{}::lifecycle_vfs_workloads_pager_full_rolls_back_without_receipt_or_relation_eviction",
                module_path!()
            ),
            "pager-full",
        );
        return;
    }
    fixture(512, false, "Enrolled", |database| {
        let c = &database.connection;
        let pages: u64 = c
            .pragma_query_value(None, "page_count", |r| r.get(0))
            .unwrap();
        c.pragma_update(None, "max_page_count", pages).unwrap();
        let before = super::super::counts(database).unwrap();
        let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
        let allocation = policy.allocation().clone();
        policy.install().unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        policy.validate_reserved(deadline()).unwrap();
        let attempt = c
            .restrict(allocation.logical_restriction().unwrap())
            .unwrap();
        assert_eq!(c.execute_batch("INSERT INTO source_lifecycle_references VALUES(printf('%036d',999999),'pager-full',zeroblob(16384),'Enrolled',printf('%036d',999999),'work',zeroblob(32))").unwrap_err().sqlite_error_code(),Some(ErrorCode::DiskFull));
        if !c.is_autocommit() {
            c.execute_batch("ROLLBACK").unwrap();
        }
        assert!(c.is_autocommit());
        assert_eq!(super::super::counts(database).unwrap(), before);
        // SQLite's own pager refuses before any VFS ceiling is exhausted.
        assert_eq!(attempt.report().failure, None);
        assert_eq!(attempt.report().charged[1], 0);
        evidence("pager-full", database, &allocation, attempt.report());
        policy.finish().unwrap();
    });
}

#[test]
fn lifecycle_vfs_workloads_source_global_same_volume_have_distinct_original_objects() {
    if std::env::var("YIELD_LIFECYCLE_VFS_WORKLOAD").is_err() {
        isolated(
            &format!(
                "{}::lifecycle_vfs_workloads_source_global_same_volume_have_distinct_original_objects",
                module_path!()
            ),
            "same-volume",
        );
        return;
    }
    use crate::sample_sources::{SourceDatabase, SourceId, SourceWriter};
    let temp = tempfile::tempdir().unwrap();
    let media = temp.path().join("media");
    std::fs::create_dir(&media).unwrap();
    let _root = AppRootGuard::set(temp.path().join("profile")).unwrap();
    let profile = WritableProfileGuard::acquire_current().unwrap();
    let global = LibraryDatabase::open_for_profile_guard(&profile, None).unwrap();
    let source = SourceWriter::new_for_scan(
        SourceDatabase::open_for_catalog_with_profile_guard(
            &media,
            &profile,
            std::path::Path::new("source"),
        )
        .unwrap(),
        SourceId::from_string("source"),
        1,
    )
    .unwrap();
    let policy = EnteredLifecyclePolicy::new(&global, deadline()).unwrap();
    let source = source
        .capture_lease_maintenance_allocation(deadline())
        .unwrap();
    let global = policy.allocation();
    let source = source.allocation();
    let objects = [
        global.main(),
        global.wal(),
        global.shm(),
        source.main(),
        source.wal(),
        source.shm(),
    ];
    for (i, a) in objects.iter().enumerate() {
        for b in &objects[i + 1..] {
            assert_eq!(a.device(), b.device());
            assert_ne!(a.identity(), b.identity());
        }
    }
    println!(
        "logical-workload=same-volume distinct_objects=6 same_device=true physical_qualification=false"
    );
}

#[test]
fn lifecycle_vfs_workloads_irreversible_budget_denies_positive_mmap_restoration_and_reuse() {
    if std::env::var("YIELD_LIFECYCLE_VFS_WORKLOAD").is_err() {
        isolated(
            &format!(
                "{}::lifecycle_vfs_workloads_irreversible_budget_denies_positive_mmap_restoration_and_reuse",
                module_path!()
            ),
            "restore-mmap-refused",
        );
        return;
    }
    fixture(4096, false, "Enrolled", |database| {
        let c = &database.connection;
        c.pragma_update(None, "mmap_size", 1048576).unwrap();
        let mut policy = EnteredLifecyclePolicy::new(database, deadline()).unwrap();
        assert!(policy.original.mmap_size > 0);
        let allocation = policy.allocation().clone();
        policy.install().unwrap();
        c.execute_batch("BEGIN IMMEDIATE").unwrap();
        policy.validate_reserved(deadline()).unwrap();
        let attempt = c
            .restrict(allocation.logical_restriction().unwrap())
            .unwrap();
        effect(database, "Enrolled");
        c.execute_batch("COMMIT").unwrap();
        assert!(policy.finish().is_err());
        assert!(
            c.pragma_query_value(None, "query_only", |r| r.get::<_, bool>(0))
                .unwrap()
        );
        assert_eq!(
            attempt.report().failure,
            Some(BudgetFailure::UnknownCoverage)
        );
        assert!(EnteredLifecyclePolicy::new(database, deadline()).is_err());
        evidence(
            "restore-mmap-refused",
            database,
            &allocation,
            attempt.report(),
        );
    });
}
