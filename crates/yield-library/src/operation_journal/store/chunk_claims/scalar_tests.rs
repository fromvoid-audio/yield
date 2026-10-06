use super::*;

struct Fixture {
    connection: Connection,
    _directory: tempfile::TempDir,
    chunk: ChunkId,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let connection = Connection::open(directory.path().join("original.sqlite3")).unwrap();
        // Isolated row-decoder fixture; cross-table admission audit is not bypassed in production.
        connection.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
        connection.execute_batch(CREATE_TABLE).unwrap();
        let chunk = ChunkId::from_uuid(Uuid::from_u128(1)).unwrap();
        connection.execute(
            "INSERT INTO operation_journal_chunk_claim VALUES(?1,?2,?3,?4,'segment',0,4096,'7','DestinationStaging',4096,4096,268435456,'Claimed',NULL,NULL)",
            params![chunk.to_string(), Uuid::from_u128(2).to_string(), Uuid::from_u128(3).to_string(), Uuid::from_u128(4).to_string()],
        ).unwrap();
        Self {
            connection,
            _directory: directory,
            chunk,
        }
    }

    fn text(&self, field: &str, value: &str) {
        self.connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        self.connection
            .execute(&format!("UPDATE {TABLE_NAME} SET {field}=?1"), [value])
            .unwrap();
    }

    fn read(&self) -> Result<ChunkClaimRecord, ChunkClaimReadError> {
        read(&self.connection, self.chunk).map(|record| record.unwrap())
    }
}

#[test]
fn chunk_scalar_projection_preserves_original_states_optional_uuid_forms_and_pages() {
    let fixture = Fixture::new();
    let initial = fixture.read().unwrap();
    let page = read_page(&fixture.connection, None, 1).unwrap();
    assert_eq!(page.records(), &[initial.clone()]);
    assert!(page.next_cursor().is_none());
    for state in ["Claimed", "Issued", "Released"] {
        fixture.text("state", state);
        fixture
            .connection
            .execute(
                "UPDATE operation_journal_chunk_claim SET issue_id=NULL, release_id=NULL",
                [],
            )
            .unwrap();
        if state != "Claimed" {
            // Preserve the original parser's accepted noncanonical optional UUID forms.
            fixture.text("issue_id", &Uuid::from_u128(5).simple().to_string());
        }
        if state == "Released" {
            fixture.text("release_id", &Uuid::from_u128(6).urn().to_string());
        }
        let decoded = fixture.read().unwrap();
        let mut statement = fixture.connection.prepare("SELECT chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset, logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit, protected_free_bytes, state, issue_id, release_id FROM operation_journal_chunk_claim").unwrap();
        let mut rows = statement.query([]).unwrap();
        assert_eq!(
            decoded,
            original_scalar_probe::original_decode_row(rows.next().unwrap().unwrap()).unwrap()
        );
        assert_eq!(decoded.chunk_id(), initial.chunk_id());
        assert_eq!(decoded.output_key(), "segment");
        assert_eq!(
            read_page(&fixture.connection, None, 1).unwrap().records(),
            &[decoded]
        );
    }
    // Original Released semantics do not require an issue identity.
    fixture
        .connection
        .execute("UPDATE operation_journal_chunk_claim SET issue_id=NULL", [])
        .unwrap();
    assert!(fixture.read().is_ok());
}

#[test]
fn chunk_scalar_projection_enforces_original_output_byte_bounds_before_copy() {
    let fixture = Fixture::new();
    fixture.text("output_key", &"é".repeat(512));
    assert_eq!(
        fixture.read().unwrap().output_key().len(),
        MAX_OUTPUT_KEY_BYTES
    );
    for value in [String::new(), "é".repeat(513), "x".repeat(1 << 20)] {
        fixture.text("output_key", &value);
        assert!(
            matches!(fixture.read(), Err(ChunkClaimReadError::Malformed(reason)) if reason == "output key bounds")
        );
    }
}

#[test]
fn chunk_scalar_projection_large_unknown_diagnostics_are_bounded() {
    for field in ["allocation_class", "state"] {
        let fixture = Fixture::new();
        fixture.text(field, &"é".repeat(1 << 19));
        let mut statement = fixture.connection.prepare("SELECT chunk_id, operation_id, claim_id, participant_id, output_key, logical_offset, logical_length, volume_device, allocation_class, allocated_bytes, allocation_unit, protected_free_bytes, state, issue_id, release_id FROM operation_journal_chunk_claim").unwrap();
        let mut rows = statement.query([]).unwrap();
        let Err(ChunkClaimReadError::Malformed(original_reason)) =
            original_scalar_probe::original_decode_row(rows.next().unwrap().unwrap())
        else {
            panic!("original unknown scalar also refuses")
        };
        assert!(
            original_reason.len() >= 1 << 20,
            "before-reader must expose its unbounded projection"
        );
        let Err(ChunkClaimReadError::Malformed(reason)) = fixture.read() else {
            panic!("unknown closed scalar must refuse")
        };
        assert!(
            reason.len() <= 256,
            "diagnostic copied original large {field}: {}",
            reason.len()
        );
    }
}

#[test]
fn chunk_scalar_projection_keeps_all_original_semantic_refusals() {
    for (field, value) in [
        ("chunk_id", "not-a-uuid"),
        ("operation_id", "00000000-0000-0000-0000-000000000000"),
        ("claim_id", "00000000-0000-0000-0000-000000000000"),
        ("participant_id", "not-a-uuid"),
        ("volume_device", "18446744073709551616"),
        ("allocation_class", "ExistingDestination"),
        ("allocation_class", "ExistingRecoveryPayload"),
        ("state", "unknown"),
        ("issue_id", "not-a-uuid"),
        ("release_id", "not-a-uuid"),
    ] {
        let fixture = Fixture::new();
        fixture.text(field, value);
        // The original chunk identity column may now be corrupt; use the actual paged decoder.
        assert!(
            read_page(&fixture.connection, None, 1).is_err(),
            "accepted corrupt {field}"
        );
    }
    for (field, value) in [
        ("logical_offset", -1),
        ("logical_length", 0),
        ("allocated_bytes", 0),
        ("allocation_unit", -1),
        ("protected_free_bytes", 0),
    ] {
        let fixture = Fixture::new();
        fixture
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        fixture
            .connection
            .execute(&format!("UPDATE {TABLE_NAME} SET {field}=?1"), [value])
            .unwrap();
        assert!(fixture.read().is_err(), "accepted corrupt {field}");
    }
    for (state, issue, release) in [
        ("Claimed", Some(5), None),
        ("Claimed", None, Some(6)),
        ("Issued", None, None),
        ("Issued", Some(5), Some(6)),
        ("Released", Some(5), None),
    ] {
        let fixture = Fixture::new();
        fixture
            .connection
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        fixture
            .connection
            .execute(
                "UPDATE operation_journal_chunk_claim SET state=?1,issue_id=?2,release_id=?3",
                params![
                    state,
                    issue.map(|n| Uuid::from_u128(n).to_string()),
                    release.map(|n| Uuid::from_u128(n).to_string())
                ],
            )
            .unwrap();
        assert!(fixture.read().is_err(), "accepted wrong {state} identities");
    }
}

#[test]
fn chunk_scalar_projection_matches_original_sqlite_type_and_utf8_errors() {
    let fixture = Fixture::new();
    for expression in ["42", "NULL", "X'80'", "CAST(X'80' AS TEXT)"] {
        let mut statement = fixture
            .connection
            .prepare(&format!("SELECT {expression} AS output_key"))
            .unwrap();
        let mut rows = statement.query([]).unwrap();
        let row = rows.next().unwrap().unwrap();
        let original = row.get::<_, String>(0).unwrap_err();
        let Err(ChunkClaimReadError::Sql(projected)) = borrowed_text(row, 0) else {
            panic!("typed SQL error required")
        };
        assert_eq!(format!("{original:?}"), format!("{projected:?}"));
        assert_eq!(original.to_string(), projected.to_string());
    }
    let mut statement = fixture
        .connection
        .prepare("SELECT NULL AS issue_id")
        .unwrap();
    let mut rows = statement.query([]).unwrap();
    let row = rows.next().unwrap().unwrap();
    assert_eq!(
        borrowed_optional_text(row, 0).unwrap(),
        row.get::<_, Option<String>>(0).unwrap().as_deref()
    );
}
