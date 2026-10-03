//! Actual retained-owner observations, isolated from process-wide optional extension taint.
use super::*;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run(case: &str) {
    let name = concat!(module_path!(), "::collection_history_selection_child")
        .strip_prefix("yield_library::")
        .unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--ignored", "--nocapture"])
        .env("YIELD_HISTORY_SELECTION_CASE", case)
        .env_remove(crate::sqlite_ext::SQLITE_EXT_ENV)
        .env_remove(crate::sqlite_ext::SQLITE_EXT_ENABLE_ENV)
        .env_remove(crate::sqlite_ext::SQLITE_EXT_UNSAFE_ENV)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            use std::io::Read;
            let mut output = String::new();
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut output)
                .unwrap();
            assert!(status.success(), "{case}: {output}");
            assert!(
                output.contains("test result: ok. 1 passed;"),
                "{case}: {output}"
            );
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{case}: child deadline");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn collection_history_selection_reads_confirmed_bits_without_writes() {
    run("read-only");
}
#[test]
fn collection_history_selection_rejects_path_batch_and_slot_bounds() {
    run("bounds");
}
#[test]
fn collection_history_selection_rejects_malformed_sqlite_membership() {
    run("malformed");
}
#[test]
fn collection_history_selection_rejects_stale_revision_and_lifecycle() {
    run("stale");
}
#[test]
fn collection_history_selection_rejects_expired_lease_without_renewing() {
    run("lease");
}
#[test]
fn collection_history_selection_expired_deadline_does_not_run_sql() {
    run("deadline");
}

#[test]
fn collection_history_selection_captures_exact_full_batch_in_sorted_order() {
    run("full-batch");
}

#[test]
fn collection_history_selection_rejects_staging_audit_and_unknown_schema() {
    run("audit-schema");
}

#[test]
fn collection_history_selection_rejects_actual_reacquired_lease_with_same_revision() {
    run("lease-handoff");
}

#[test]
fn collection_history_selection_delivery_window_expires_without_sql() {
    run("delivery-window");
}

#[test]
fn collection_history_selection_rejects_wrong_source_before_sql() {
    run("wrong-source");
}

#[test]
#[ignore = "bounded subprocess driver invoked by actual owner tests"]
fn collection_history_selection_child() {
    let Ok(case) = std::env::var("YIELD_HISTORY_SELECTION_CASE") else {
        return;
    };
    match case.as_str() {
        "group-handoff" | "group-lease" | "group-expired" | "group-cancel" | "group-progress"
        | "group-mid-lease" => group_owner(&case),
        "read-only" => read_only(),
        "bounds" => bounds(),
        "malformed" => malformed(),
        "stale" => stale(),
        "lease" => lease(),
        "deadline" => deadline(),
        "full-batch" => full_batch(),
        "audit-schema" => audit_schema(),
        "lease-handoff" => lease_handoff(),
        "delivery-window" => delivery_window(),
        "wrong-source" => wrong_source(),
        _ => panic!("unknown case"),
    }
}

fn writer(database: SourceDatabase, generation: u64) -> SourceWriter {
    SourceWriter::new(
        database,
        SourceId::from_string("collection-history-observation"),
        generation,
    )
    .unwrap()
}

fn persisted(connection: &rusqlite::Connection) -> (Vec<u8>, Vec<u8>) {
    let path = connection.path().unwrap();
    (
        std::fs::read(path).unwrap(),
        std::fs::read(format!("{path}-wal")).unwrap(),
    )
}

fn read_only() {
    let fixture = CurationFixture::new_profile_bound();
    fixture
        .commit(&[
            SourceCurationUpdate::AddCollection {
                path: CurationFixture::path("one.wav"),
                collection: SampleCollection::new(2).unwrap(),
            },
            SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: true,
            },
        ])
        .unwrap();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    let connection = &writer.database_for_tests().connection;
    let before = persisted(connection);
    let changes = connection.total_changes();
    connection.pragma_update(None, "query_only", true).unwrap();
    connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Read { .. }
        | AuthAction::Select
        | AuthAction::Transaction { .. }
        | AuthAction::Function { .. }
        | AuthAction::Recursive => Authorization::Allow,
        AuthAction::Pragma {
            pragma_name,
            pragma_value,
        } if (pragma_value.is_none() && pragma_name != "wal_checkpoint")
            || [
                "table_info",
                "table_xinfo",
                "index_list",
                "index_info",
                "index_xinfo",
                "foreign_key_list",
            ]
            .contains(&pragma_name) =>
        {
            Authorization::Allow
        }
        _ => Authorization::Deny,
    }));
    assert!(
        connection
            .execute("UPDATE metadata SET value=value", [])
            .is_err()
    );
    assert!(
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE)")
            .is_err()
    );
    assert!(
        connection
            .execute_batch("CREATE TABLE history_bypass(id)")
            .is_err()
    );
    let paths = [
        CurationFixture::path("two.wav"),
        CurationFixture::path("one.wav"),
    ];
    let selected = writer
        .capture_collection_history_selection(
            &expected,
            SampleCollection::new(2).unwrap(),
            true,
            &paths,
        )
        .unwrap();
    assert_eq!(selected.revision(), &expected);
    assert_eq!(selected.directory_generation(), 1);
    assert_eq!(selected.collection(), SampleCollection::new(2).unwrap());
    assert_eq!(selected.changes().len(), 1);
    assert_eq!(selected.changes()[0].path().as_path(), Path::new("two.wav"));
    assert!(!selected.changes()[0].before());
    assert!(selected.changes()[0].after());
    let no_op = writer
        .capture_collection_history_selection(
            &expected,
            SampleCollection::new(2).unwrap(),
            true,
            &[CurationFixture::path("one.wav")],
        )
        .unwrap();
    assert!(no_op.changes().is_empty());
    let removed = writer
        .capture_collection_history_selection(
            &expected,
            SampleCollection::new(2).unwrap(),
            false,
            &paths,
        )
        .unwrap();
    assert_eq!(removed.changes().len(), 1);
    assert!(removed.changes()[0].before());
    assert!(!removed.changes()[0].after());
    assert_eq!(connection.total_changes(), changes);
    assert_eq!(
        persisted(connection),
        before,
        "DB/WAL and lease state remain byte-identical"
    );
}

fn bounds() {
    let fixture = CurationFixture::new_profile_bound();
    fixture
        .database
        .connection
        .execute(
            "INSERT INTO wav_files(path,file_size,modified_ns) VALUES('unsupported.txt',10,1)",
            [],
        )
        .unwrap();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    let slot = SampleCollection::new(0).unwrap();
    assert!(
        writer
            .capture_collection_history_selection(&expected, slot, true, &[])
            .is_err()
    );
    let path = CurationFixture::path("one.wav");
    assert!(
        writer
            .capture_collection_history_selection(
                &expected,
                slot,
                true,
                &[path.clone(), path.clone()]
            )
            .is_err()
    );
    assert!(
        writer
            .capture_collection_history_selection(&expected, slot, true, &vec![path.clone(); 129])
            .is_err()
    );
    let huge = CurationFixture::path(&format!("{}.wav", "a".repeat(4097)));
    assert!(matches!(
        writer.capture_collection_history_selection(&expected, slot, true, &[huge]),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
    let invalid: SampleCollection = serde_json::from_str("6").unwrap();
    assert!(
        writer
            .capture_collection_history_selection(&expected, invalid, true, &[path.clone()])
            .is_err()
    );
    for missing in ["absent.wav", "unsupported.txt"] {
        assert!(
            writer
                .capture_collection_history_selection(
                    &expected,
                    slot,
                    true,
                    &[CurationFixture::path(missing)]
                )
                .is_err()
        );
    }
}

fn malformed() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    let connection = &writer.database_for_tests().connection;
    for value in ["-1", "6", "zeroblob(1048576)"] {
        connection.execute_batch(&format!("DELETE FROM wav_file_collections; INSERT INTO wav_file_collections(path,collection) VALUES('one.wav',{value});")).unwrap();
        let changes = connection.total_changes();
        assert!(
            writer
                .capture_collection_history_selection(
                    &expected,
                    SampleCollection::new(0).unwrap(),
                    true,
                    &[CurationFixture::path("one.wav")]
                )
                .is_err()
        );
        assert_eq!(connection.total_changes(), changes);
    }
    connection
        .execute_batch("DELETE FROM wav_file_collections")
        .unwrap();
    connection
        .execute(
            "UPDATE metadata SET value=printf('%1000000s','x') WHERE key=?1",
            [crate::sample_sources::db::META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY],
        )
        .unwrap();
    assert!(
        writer
            .capture_collection_history_selection(
                &expected,
                SampleCollection::new(0).unwrap(),
                true,
                &[CurationFixture::path("one.wav")]
            )
            .is_err()
    );
}

fn stale() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    writer
        .commit_catalog_curation_command(
            &expected,
            &fixture.profile_guard,
            &[SourceCurationUpdate::SetLocked {
                path: CurationFixture::path("one.wav"),
                locked: true,
            }],
        )
        .unwrap();
    assert!(matches!(
        writer.capture_collection_history_selection(
            &expected,
            SampleCollection::new(0).unwrap(),
            true,
            &[CurationFixture::path("one.wav")]
        ),
        Err(SourceWriterError::StaleFence)
    ));
    let fresh = writer.current_revision().unwrap();
    let writer = SourceWriter::new(
        writer.into_catalog_database(),
        SourceId::from_string("collection-history-observation"),
        8,
    )
    .unwrap();
    assert!(matches!(
        writer.capture_collection_history_selection(
            &fresh,
            SampleCollection::new(0).unwrap(),
            true,
            &[CurationFixture::path("one.wav")]
        ),
        Err(SourceWriterError::StaleFence)
    ));
}

fn lease() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    let connection = &writer.database_for_tests().connection;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    connection
        .execute("UPDATE source_lease SET expires_at=?1", [now + 10])
        .unwrap();
    let near_expiry = persisted(connection);
    writer
        .capture_collection_history_selection(
            &expected,
            SampleCollection::new(0).unwrap(),
            true,
            &[CurationFixture::path("one.wav")],
        )
        .unwrap();
    assert_eq!(
        persisted(connection),
        near_expiry,
        "near-expiry read must not renew"
    );
    connection
        .execute("UPDATE source_lease SET acquired_at=0, expires_at=0", [])
        .unwrap();
    let before = persisted(connection);
    assert!(
        writer
            .capture_collection_history_selection(
                &expected,
                SampleCollection::new(0).unwrap(),
                true,
                &[CurationFixture::path("one.wav")]
            )
            .is_err()
    );
    assert_eq!(
        persisted(connection),
        before,
        "expired lease must not renew"
    );
}

fn deadline() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    writer.database_for_tests().connection.authorizer(Some(
        |_: AuthContext<'_>| -> Authorization { panic!("expired request must not execute SQL") },
    ));
    assert!(matches!(
        writer.capture_collection_history_selection_with_deadline(
            &expected,
            SampleCollection::new(0).unwrap(),
            true,
            &[CurationFixture::path("one.wav")],
            Instant::now() - Duration::from_millis(1)
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
}

fn full_batch() {
    let fixture = CurationFixture::new_profile_bound();
    let mut paths = vec![
        CurationFixture::path("one.wav"),
        CurationFixture::path("two.wav"),
    ];
    for index in 0..126 {
        let path = if index == 0 {
            "sub/one.wav".to_owned()
        } else {
            format!("entry-{index:03}.wav")
        };
        fixture
            .database
            .upsert_file(Path::new(&path), 10, 1)
            .unwrap();
        paths.push(CurationFixture::path(&path));
    }
    fixture
        .database
        .begin_source_directory_truth_generation(2, 0)
        .unwrap();
    let revision = fixture.database.get_revision().unwrap();
    fixture
        .database
        .write_batch()
        .unwrap()
        .commit_manifest_audit_with_staged_directory_truth(
            2,
            revision,
            20,
            fixture.root_identity.clone(),
            None,
        )
        .unwrap();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    #[cfg(unix)]
    {
        let changes = writer.database_for_tests().connection.total_changes();
        assert!(
            matches!(
                writer.capture_collection_history_selection(
                    &expected,
                    SampleCollection::new(5).unwrap(),
                    true,
                    &[CurationFixture::path(r"sub\one.wav")],
                ),
                Err(SourceWriterError::InvalidRequest(_))
            ),
            "a literal backslash must not select the slash-separated database row"
        );
        assert_eq!(
            writer.database_for_tests().connection.total_changes(),
            changes
        );
    }
    paths.reverse();
    let selection = writer
        .capture_collection_history_selection(
            &expected,
            SampleCollection::new(5).unwrap(),
            true,
            &paths,
        )
        .unwrap();
    assert_eq!(selection.changes().len(), 128);
    assert_eq!(selection.directory_generation(), 2);
    assert!(
        selection
            .changes()
            .windows(2)
            .all(|pair| pair[0].path().as_path() < pair[1].path().as_path())
    );
    assert!(
        selection
            .changes()
            .iter()
            .all(|change| !change.before() && change.after())
    );
    let oversized: Vec<_> = (0..128)
        .map(|index| CurationFixture::path(&format!("{}{:04}.wav", "x".repeat(4088), index)))
        .collect();
    assert!(matches!(
        writer.capture_collection_history_selection(
            &expected,
            SampleCollection::new(5).unwrap(),
            true,
            &oversized
        ),
        Err(SourceWriterError::SnapshotBudgetExceeded)
    ));
}

fn audit_schema() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    let database = writer.database_for_tests();
    database
        .begin_source_directory_truth_generation(2, 0)
        .unwrap();
    assert!(
        writer
            .capture_collection_history_selection(
                &expected,
                SampleCollection::new(0).unwrap(),
                true,
                &[CurationFixture::path("one.wav")]
            )
            .is_err()
    );
    database
        .connection
        .execute(
            "DELETE FROM source_directory_generations WHERE status='staging'",
            [],
        )
        .unwrap();
    database
        .connection
        .execute_batch(
            "CREATE TRIGGER unknown_history_trigger AFTER UPDATE ON wav_files BEGIN SELECT 1; END;",
        )
        .unwrap();
    let changes = database.connection.total_changes();
    assert!(
        writer
            .capture_collection_history_selection(
                &expected,
                SampleCollection::new(0).unwrap(),
                true,
                &[CurationFixture::path("one.wav")]
            )
            .is_err()
    );
    assert_eq!(database.connection.total_changes(), changes);
}

fn lease_handoff() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let old = writer.current_revision().unwrap();
    writer.close().unwrap();
    let database = SourceDatabase::open_for_catalog_with_profile_guard(
        fixture._source.path(),
        &fixture.profile_guard,
        fixture.profile_child.as_ref().unwrap(),
    )
    .unwrap();
    let writer = self::writer(database, 7);
    let fresh = writer.current_revision().unwrap();
    assert_eq!(old.revision(), fresh.revision());
    assert_eq!(old.database_identity(), fresh.database_identity());
    assert_eq!(old.lifecycle_generation(), fresh.lifecycle_generation());
    assert!(fresh.lease_epoch() > old.lease_epoch());
    let before = persisted(&writer.database_for_tests().connection);
    assert!(matches!(
        writer.capture_collection_history_selection(
            &old,
            SampleCollection::new(0).unwrap(),
            true,
            &[CurationFixture::path("one.wav")]
        ),
        Err(SourceWriterError::StaleFence)
    ));
    assert_eq!(persisted(&writer.database_for_tests().connection), before);
    assert_eq!(
        writer
            .capture_collection_history_selection(
                &fresh,
                SampleCollection::new(0).unwrap(),
                true,
                &[CurationFixture::path("one.wav")]
            )
            .unwrap()
            .changes()
            .len(),
        1
    );
}

fn delivery_window() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    let connection = &writer.database_for_tests().connection;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    connection
        .execute("UPDATE source_lease SET expires_at=?1", [now + 2])
        .unwrap();
    connection.pragma_update(None, "query_only", true).unwrap();
    let before = persisted(connection);
    let changes = connection.total_changes();
    // A longer private SQL test deadline isolates the real lease bound. The public
    // worker API still always installs its original two-second deadline.
    let selection = writer
        .capture_collection_history_selection_with_deadline(
            &expected,
            SampleCollection::new(0).unwrap(),
            true,
            &[CurationFixture::path("one.wav")],
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
    assert!(selection.within_observation_window());
    assert!(selection.observation_deadline() <= Instant::now() + Duration::from_secs(2));
    assert_eq!(persisted(connection), before);
    assert_eq!(connection.total_changes(), changes);
    connection.authorizer(Some(|_: AuthContext<'_>| -> Authorization {
        panic!("delivery timing check must never run SQL");
    }));
    let deadline = Instant::now() + Duration::from_secs(3);
    while selection.within_observation_window() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!selection.within_observation_window());
    assert_eq!(persisted(connection), before);
    assert_eq!(connection.total_changes(), changes);
}

fn wrong_source() {
    let fixture = CurationFixture::new_profile_bound();
    let writer = writer(fixture.database, 7);
    let expected = writer.current_revision().unwrap();
    let other = CurationFixture::new_profile_bound();
    let writer = SourceWriter::new(
        other.database,
        SourceId::from_string("another-history-source"),
        7,
    )
    .unwrap();
    writer.database_for_tests().connection.authorizer(Some(
        |_: AuthContext<'_>| -> Authorization {
            panic!("wrong source must reject before SQL or owner revision materialization");
        },
    ));
    assert!(matches!(
        writer.capture_collection_history_selection(
            &expected,
            SampleCollection::new(0).unwrap(),
            true,
            &[CurationFixture::path("one.wav")],
        ),
        Err(SourceWriterError::StaleFence)
    ));
}

#[test]
fn collection_history_group_rejects_identical_numeric_owner_handoff_before_sql() {
    run("group-handoff");
}
#[test]
fn collection_history_group_rejects_actual_reacquired_owner_lease_before_sql() {
    run("group-lease");
}
#[test]
fn collection_history_group_original_deadline_rejects_before_sql() {
    run("group-expired");
}
#[test]
fn collection_history_group_cancellation_rejects_before_sql() {
    run("group-cancel");
}
#[test]
fn collection_history_group_progress_retains_issuer_across_real_receipts() {
    run("group-progress");
}

fn group_owner(case: &str) {
    use std::sync::{Arc, atomic::AtomicBool};
    let fixture = CurationFixture::new_profile_bound();
    let mut paths = vec![
        CurationFixture::path("one.wav"),
        CurationFixture::path("two.wav"),
    ];
    if case == "group-progress" || case == "group-mid-lease" {
        for i in 0..127 {
            let path = format!("group-{i:03}.wav");
            fixture
                .database
                .upsert_file(Path::new(&path), 10, 1)
                .unwrap();
            paths.push(CurationFixture::path(&path));
        }
        fixture
            .database
            .begin_source_directory_truth_generation(2, 0)
            .unwrap();
        let revision = fixture.database.get_revision().unwrap();
        fixture
            .database
            .write_batch()
            .unwrap()
            .commit_manifest_audit_with_staged_directory_truth(
                2,
                revision,
                20,
                fixture.root_identity.clone(),
                None,
            )
            .unwrap();
    }
    let mut owner = writer(fixture.database, 7);
    let original = owner.current_revision().unwrap();
    let cancel = AtomicBool::new(case == "group-cancel");
    let deadline = if case == "group-expired" {
        Instant::now() - Duration::from_millis(1)
    } else {
        Instant::now() + Duration::from_secs(2)
    };
    if case == "group-expired" || case == "group-cancel" {
        owner.database_for_tests().connection.authorizer(Some(
            |_: AuthContext<'_>| -> Authorization {
                panic!("expired/cancelled capture must not run SQL")
            },
        ));
        assert!(
            owner
                .capture_collection_history_group(
                    &original,
                    SampleCollection::new(0).unwrap(),
                    true,
                    &[CurationFixture::path("one.wav")],
                    &cancel,
                    deadline
                )
                .is_err()
        );
        return;
    }
    let group = Arc::new(
        owner
            .capture_collection_history_group(
                &original,
                SampleCollection::new(0).unwrap(),
                true,
                &paths,
                &cancel,
                deadline,
            )
            .unwrap(),
    );
    if case == "group-progress" || case == "group-mid-lease" {
        let mut execution = owner
            .begin_collection_history_execution(Arc::clone(&group))
            .unwrap();
        let receipt = owner
            .commit_collection_history_next(&mut execution, &fixture.profile_guard)
            .unwrap();
        assert_eq!(receipt.source_revision(), original.revision() + 1);
        assert_eq!(execution.revision(), receipt.source_revision());
        assert_eq!(execution.next_batch(), 1);
        assert_eq!(receipt.files().len(), 128);
        if case == "group-mid-lease" {
            owner
                .database_for_tests()
                .connection
                .execute("UPDATE source_lease SET lease_epoch=lease_epoch+1", [])
                .unwrap();
            assert!(
                owner
                    .commit_collection_history_next(&mut execution, &fixture.profile_guard)
                    .is_err()
            );
            assert_eq!(execution.next_batch(), 1);
            assert_eq!(execution.revision(), receipt.source_revision());
            return;
        }
        let second = owner
            .commit_collection_history_next(&mut execution, &fixture.profile_guard)
            .unwrap();
        assert_eq!(second.files().len(), 1);
        assert_eq!(second.source_revision(), original.revision() + 2);
        assert_eq!(execution.next_batch(), 2);

        assert!(
            owner
                .commit_collection_history_next(&mut execution, &fixture.profile_guard)
                .is_err()
        );
        assert!(owner.begin_collection_history_execution(group).is_err());
        return;
    }
    owner = if case == "group-handoff" {
        writer(owner.into_catalog_database(), 7)
    } else {
        owner.close().unwrap();
        writer(
            SourceDatabase::open_for_catalog_with_profile_guard(
                fixture._source.path(),
                &fixture.profile_guard,
                fixture.profile_child.as_ref().unwrap(),
            )
            .unwrap(),
            7,
        )
    };
    let fresh = owner.current_revision().unwrap();
    assert_eq!(original.revision(), fresh.revision());
    assert_eq!(original.database_identity(), fresh.database_identity());
    if case == "group-lease" {
        assert!(fresh.lease_epoch() > original.lease_epoch());
    } else {
        assert_eq!(original.lease_epoch(), fresh.lease_epoch());
    }
    owner
        .database_for_tests()
        .connection
        .authorizer(Some(|_: AuthContext<'_>| -> Authorization {
            panic!("foreign issuer must reject before SQL")
        }));
    assert!(owner.begin_collection_history_execution(group).is_err());
}

#[test]
fn collection_history_group_checks_actual_lease_before_every_batch() {
    run("group-mid-lease");
}
