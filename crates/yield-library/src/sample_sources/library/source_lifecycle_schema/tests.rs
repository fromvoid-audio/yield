use super::*;
use rusqlite::{
    hooks::{AuthAction, AuthContext, Authorization},
    params,
};

fn database() -> crate::sqlite_budget::OwnedConnection {
    let mut connection = Connection::open_in_memory().unwrap().into();
    install(&mut connection).unwrap();
    connection
}

#[test]
fn exact_image_reopen_is_read_only_and_preserves_unrelated_rows() {
    let mut connection = database();
    assert_eq!(image(&connection).unwrap().len(), MAX_OBJECTS);
    connection
        .execute_batch("CREATE TABLE unrelated(value BLOB); INSERT INTO unrelated VALUES (x'ff');")
        .unwrap();
    let version: i64 = connection
        .query_row("PRAGMA schema_version", [], |r| r.get(0))
        .unwrap();
    let changes = connection.total_changes();
    connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::CreateTable { .. }
        | AuthAction::CreateIndex { .. }
        | AuthAction::Insert { .. }
        | AuthAction::Update { .. }
        | AuthAction::Delete { .. }
        | AuthAction::Transaction { .. } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    install(&mut connection).unwrap();
    assert_eq!(
        version,
        connection
            .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
            .unwrap()
    );
    assert_eq!(changes, connection.total_changes());
    assert_eq!(
        connection
            .query_row("SELECT value FROM unrelated", [], |r| r
                .get::<_, Vec<u8>>(0))
            .unwrap(),
        vec![255]
    );
}

#[test]
fn partial_shadow_index_trigger_altered_and_temp_images_preserve_evidence() {
    for mutation in [
        "DROP TABLE source_lifecycle_reference_receipts;",
        "DROP INDEX source_lifecycle_references_active_source;",
        "CREATE INDEX extra_lifecycle_index ON source_lifecycle_references(state);",
        "CREATE TRIGGER extra_lifecycle_trigger AFTER INSERT ON source_lifecycle_references BEGIN SELECT 1; END;",
        "ALTER TABLE source_lifecycle_references ADD COLUMN unreviewed TEXT;",
        "CREATE TEMP TABLE source_lifecycle_references(unreviewed TEXT);",
        "DROP TABLE source_lifecycle_references; CREATE VIEW source_lifecycle_references AS SELECT 1;",
    ] {
        let mut connection = database();
        connection.execute_batch(mutation).unwrap();
        let before: i64 = connection
            .query_row("PRAGMA schema_version", [], |r| r.get(0))
            .unwrap();
        assert!(
            matches!(
                install(&mut connection),
                Err(LibraryError::SourceLifecycleSchemaIncompatible)
            ),
            "{mutation}"
        );
        assert_eq!(
            before,
            connection
                .query_row("PRAGMA schema_version", [], |r| r.get::<_, i64>(0))
                .unwrap()
        );
    }
    let mut shadow: crate::sqlite_budget::OwnedConnection =
        Connection::open_in_memory().unwrap().into();
    shadow.execute_batch("CREATE TABLE SOURCE_LIFECYCLE_REFERENCES(unreviewed TEXT); INSERT INTO SOURCE_LIFECYCLE_REFERENCES VALUES ('evidence');").unwrap();
    assert!(matches!(
        install(&mut shadow),
        Err(LibraryError::SourceLifecycleSchemaIncompatible)
    ));
    assert_eq!(
        shadow
            .query_row(
                "SELECT unreviewed FROM SOURCE_LIFECYCLE_REFERENCES",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "evidence"
    );
}

#[test]
fn actual_second_table_fault_rolls_back_first_table_and_index() {
    let mut connection: crate::sqlite_budget::OwnedConnection =
        Connection::open_in_memory().unwrap().into();
    connection
        .execute_batch("CREATE TABLE unrelated(value); INSERT INTO unrelated VALUES (2143)")
        .unwrap();
    connection.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::CreateTable {
            table_name: "source_lifecycle_reference_receipts",
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }));
    assert!(matches!(
        install(&mut connection),
        Err(LibraryError::Sql(_))
    ));
    assert!(connection.is_autocommit());
    assert!(image(&connection).unwrap().is_empty());
    assert_eq!(
        connection
            .query_row("SELECT value FROM unrelated", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2143
    );
    connection.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
    install(&mut connection).unwrap();
    assert_eq!(image(&connection).unwrap(), expected_image().unwrap());
}

fn reference(
    connection: &Connection,
    registration: &str,
    source: &str,
    state: &str,
) -> rusqlite::Result<usize> {
    connection.execute("INSERT INTO source_lifecycle_references VALUES (?1, ?2, x'01', ?3, ?4, 'full-work-key', zeroblob(32))",
        params![registration, source, state, "o".repeat(36)])
}

#[test]
fn real_unique_partial_index_allows_retired_history_and_separates_sources() {
    let connection = database();
    reference(&connection, &"a".repeat(36), "source", "Enrolled").unwrap();
    assert!(reference(&connection, &"b".repeat(36), "source", "Draining").is_err());
    reference(&connection, &"b".repeat(36), "source", "Retired").unwrap();
    reference(&connection, &"c".repeat(36), "other-source", "Draining").unwrap();
    connection
        .execute(
            "UPDATE source_lifecycle_references SET state='Retired' WHERE registration_id=?1",
            [&"a".repeat(36)],
        )
        .unwrap();
    reference(&connection, &"d".repeat(36), "source", "Enrolled").unwrap();
    assert!(reference(&connection, &"d".repeat(36), "new-source", "Retired").is_err());
}

#[test]
fn actual_sql_constraints_reject_unbounded_and_wrong_typed_relation_fields() {
    for (field, value) in [
        ("registration_id", "NULL"),
        ("registration_id", "'short'"),
        ("registration_id", "zeroblob(36)"),
        ("registration_id", "CAST(zeroblob(36) AS TEXT)"),
        ("source_id", "''"),
        ("source_id", "CAST(zeroblob(257) AS TEXT)"),
        ("source_id", "zeroblob(2)"),
        ("subject", "'text'"),
        ("subject", "zeroblob(0)"),
        ("subject", "zeroblob(16385)"),
        ("state", "'Unsupported'"),
        ("last_operation_id", "'short'"),
        ("last_work_key", "''"),
        ("last_work_key", "zeroblob(256)"),
        ("row_identity", "zeroblob(31)"),
        ("row_identity", "'text'"),
    ] {
        let connection = database();
        reference(&connection, &"a".repeat(36), "source", "Enrolled").unwrap();
        assert!(
            connection
                .execute(
                    &format!("UPDATE source_lifecycle_references SET {field}={value}"),
                    []
                )
                .is_err(),
            "{field}={value}"
        );
        assert_eq!(
            connection
                .query_row("SELECT state FROM source_lifecycle_references", [], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
            "Enrolled"
        );
    }
}

fn receipt(connection: &Connection, work: &str) -> rusqlite::Result<usize> {
    connection.execute("INSERT INTO source_lifecycle_reference_receipts(operation_id,work_key,payload_identity,owner_identity,registration_id,before_identity,after_identity,disposition,receipt) VALUES (?1,?2,zeroblob(32),?1,?1,NULL,zeroblob(32),'Enrolled',x'01')", params!["a".repeat(36),work])
}

#[test]
fn receipt_sequence_full_work_uniqueness_and_field_bounds_are_actual_constraints() {
    let connection = database();
    receipt(&connection, "enroll-work").unwrap();
    assert!(receipt(&connection, "enroll-work").is_err());
    receipt(&connection, "drain-work").unwrap();
    assert_eq!(connection.query_row("SELECT sequence FROM source_lifecycle_reference_receipts WHERE work_key='drain-work'", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    for (field, value) in [
        ("sequence", "0"),
        ("sequence", "-1"),
        ("sequence", "1.5"),
        ("operation_id", "'short'"),
        ("operation_id", "zeroblob(36)"),
        ("work_key", "''"),
        ("work_key", "CAST(zeroblob(257) AS TEXT)"),
        ("payload_identity", "zeroblob(31)"),
        ("owner_identity", "'short'"),
        ("registration_id", "NULL"),
        ("before_identity", "'text'"),
        ("after_identity", "zeroblob(33)"),
        ("disposition", "'Unknown'"),
        ("receipt", "NULL"),
        ("receipt", "'text'"),
        ("receipt", "zeroblob(0)"),
        ("receipt", "zeroblob(16385)"),
    ] {
        assert!(connection.execute(&format!("UPDATE source_lifecycle_reference_receipts SET {field}={value} WHERE sequence=1"), []).is_err(), "{field}={value}");
    }
}

#[test]
fn overlong_sql_and_schema_walk_refuse_with_finite_retained_output() {
    let mut connection: crate::sqlite_budget::OwnedConnection =
        Connection::open_in_memory().unwrap().into();
    let sql = format!(
        "CREATE TABLE source_lifecycle_references(value TEXT /*{}*/)",
        "x".repeat(MAX_SQL_BYTES)
    );
    connection.execute_batch(&sql).unwrap();
    assert!(matches!(
        install(&mut connection),
        Err(LibraryError::SourceLifecycleSchemaIncompatible)
    ));
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("PRAGMA writable_schema=ON;")
        .unwrap();
    // An actual corrupted/overfull sqlite_schema fixture, never accepted production input.
    connection.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO sqlite_schema(type,name,tbl_name,rootpage,sql) SELECT 'index','unrelated'||x,'unrelated',0,NULL FROM n;").unwrap();
    assert!(matches!(
        image(&connection),
        Err(LibraryError::SourceLifecycleSchemaBudget)
    ));
    // The interruption hook is cleared; it must not poison later close/rollback work.
    assert_eq!(
        connection
            .query_row("SELECT 1", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
