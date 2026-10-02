use super::*;
use rusqlite::{params, types::Value};

fn audit_pair(db: &SourceDatabase, root: Value, revision: Value) {
    db.connection
        .execute(
            "DELETE FROM metadata WHERE key IN (?1, ?2)",
            params![
                META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
                META_LAST_AUTHORITATIVE_AUDIT_REVISION
            ],
        )
        .unwrap();
    for (key, value) in [
        (META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY, root),
        (META_LAST_AUTHORITATIVE_AUDIT_REVISION, revision),
    ] {
        db.connection
            .execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .unwrap();
    }
}

#[test]
fn bounded_metadata_audit_preserves_missing_and_accepts_maximum_identity_and_revision() {
    let root = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_source_write(root.path()).unwrap();
    assert!(db.last_authoritative_audit_metadata().unwrap().is_none());
    let identity = vec![255u8; 4096];
    let mut encoded = serde_json::to_string(&identity).unwrap();
    encoded.push_str(&" ".repeat(24576 - encoded.len()));
    assert_eq!(encoded.len(), 24576);
    audit_pair(&db, Value::Text(encoded), Value::Text(u64::MAX.to_string()));
    let audit = db.last_authoritative_audit_metadata().unwrap().unwrap();
    assert_eq!(audit.root_identity.as_bytes(), identity);
    assert_eq!(audit.source_revision, u64::MAX);
}

#[test]
fn bounded_metadata_audit_rejects_oversized_malformed_and_nontext_values_without_changes() {
    let root = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_source_write(root.path()).unwrap();
    let cases = [
        (
            Value::Text(format!("[1]{}", " ".repeat(24574))),
            Value::Text("1".into()),
        ),
        (
            Value::Text(serde_json::to_string(&vec![0u8; 4097]).unwrap()),
            Value::Text("1".into()),
        ),
        (Value::Text("[]".into()), Value::Text("1".into())),
        (Value::Text("invalid".into()), Value::Text("1".into())),
        (Value::Blob(b"[1]".to_vec()), Value::Text("1".into())),
        (Value::Text("[1]".into()), Value::Blob(b"1".to_vec())),
        (Value::Text("[1]".into()), Value::Text("0".repeat(21))),
        (
            Value::Text("[1]".into()),
            Value::Text("18446744073709551616".into()),
        ),
        (Value::Text("[1]".into()), Value::Text("no".into())),
    ];
    for (identity, revision) in cases {
        audit_pair(&db, identity, revision);
        let before = db.connection.total_changes();
        assert!(db.last_authoritative_audit_metadata().is_err());
        assert_eq!(db.connection.total_changes(), before);
    }
}

#[test]
fn bounded_metadata_audit_rejects_partial_null_and_duplicate_legacy_pairs() {
    let root = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_source_write(root.path()).unwrap();
    // Simulate a legacy/corrupt schema without the current primary key/NOT NULL contract.
    db.connection
        .execute_batch("DROP TABLE metadata; CREATE TABLE metadata(key TEXT, value TEXT)")
        .unwrap();
    for missing in [
        META_LAST_AUTHORITATIVE_AUDIT_REVISION,
        META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
    ] {
        audit_pair(&db, Value::Text("[1]".into()), Value::Text("1".into()));
        db.connection
            .execute("DELETE FROM metadata WHERE key=?1", [missing])
            .unwrap();
        assert!(db.last_authoritative_audit_metadata().is_err());
    }
    for (identity, revision) in [
        (Value::Null, Value::Text("1".into())),
        (Value::Text("[1]".into()), Value::Null),
    ] {
        audit_pair(&db, identity, revision);
        assert!(db.last_authoritative_audit_metadata().is_err());
    }
    for duplicate in [
        META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
        META_LAST_AUTHORITATIVE_AUDIT_REVISION,
    ] {
        audit_pair(&db, Value::Text("[1]".into()), Value::Text("1".into()));
        db.connection
            .execute(
                "INSERT INTO metadata SELECT key,value FROM metadata WHERE key=?1",
                [duplicate],
            )
            .unwrap();
        assert!(db.last_authoritative_audit_metadata().is_err());
    }
}

#[test]
fn bounded_metadata_numeric_revisions_accept_missing_and_max_and_reject_corruption() {
    let root = tempfile::tempdir().unwrap();
    let db = SourceDatabase::open_for_source_write(root.path()).unwrap();
    db.connection
        .execute_batch("DROP TABLE metadata; CREATE TABLE metadata(key TEXT, value TEXT)")
        .unwrap();
    assert_eq!(db.get_revision().unwrap(), 0);
    assert_eq!(db.get_wav_paths_revision().unwrap(), 0);
    for key in ["revision", META_WAV_PATHS_REVISION] {
        for value in [
            Value::Text(u64::MAX.to_string()),
            Value::Text("0".repeat(21)),
            Value::Text("".into()),
            Value::Text("-1".into()),
            Value::Text("18446744073709551616".into()),
            Value::Blob(b"1".to_vec()),
            Value::Null,
        ] {
            db.connection
                .execute("DELETE FROM metadata WHERE key=?1", [key])
                .unwrap();
            db.connection
                .execute(
                    "INSERT INTO metadata(key,value) VALUES (?1,?2)",
                    params![key, value],
                )
                .unwrap();
            let result = if key == "revision" {
                db.get_revision()
            } else {
                db.get_wav_paths_revision()
            };
            if key == "revision" {
                let before = db.connection.total_changes();
                let batch = db.write_batch().unwrap();
                let batch_result = batch.get_revision();
                if value == Value::Text(u64::MAX.to_string()) {
                    assert_eq!(batch_result.unwrap(), u64::MAX);
                } else {
                    assert!(batch_result.is_err());
                }
                drop(batch);
                assert_eq!(db.connection.total_changes(), before);
            }
            if value == Value::Text(u64::MAX.to_string()) {
                assert_eq!(result.unwrap(), u64::MAX);
            } else {
                assert!(result.is_err());
            }
        }
        db.connection
            .execute("DELETE FROM metadata WHERE key=?1", [key])
            .unwrap();
        for _ in 0..2 {
            db.connection
                .execute("INSERT INTO metadata(key,value) VALUES (?1,'1')", [key])
                .unwrap();
        }
        assert!(
            (if key == "revision" {
                db.get_revision()
            } else {
                db.get_wav_paths_revision()
            })
            .is_err()
        );
    }
}
