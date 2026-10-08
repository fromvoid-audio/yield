//! One original admitted preserving transaction; passive receipts never grant mutation.
use super::super::schema;
use super::*;
use crate::operation_journal::legacy_adoption_commands::receipt::{
    ADOPTION_METADATA_KEY, AdoptionReceiptEnvelope, MAX_ADOPTION_RECEIPT_BYTES,
};
use crate::operation_journal::{OperationId, owner::OriginalLegacyMigrationAdmission};

// The caller retains the original input loan after these bounded allocations.
pub(crate) struct VerifiedLegacyMigration {
    pub(crate) operation: OperationId,
    pub(super) encoded: Vec<u8>,
    pub(super) expected: schema::SchemaImage,
    pub(super) rows_digest: [u8; 32],
}
pub(crate) enum LegacyMigrationSettlement {
    NotStarted,
    RollbackConfirmed,
    Unknown {
        candidate: Option<VerifiedLegacyMigration>,
        restored: bool,
    },
    Committed(VerifiedLegacyMigration),
}
impl LegacySourceReadOwner {
    pub(super) fn validate_migration_bindings(&self, profile: &WritableProfileGuard) -> Result<()> {
        if profile.owner_identity() != self.profile.owner_identity() {
            return Err(LegacyObservationError("foreign migration profile"));
        }
        self.profile.validate_current().map_err(refused)?;
        self.media_root
            .validate_profile_guard(profile)
            .map_err(refused)?;
        self.reservation
            .validate_before_open(profile, &self.binding)
            .map_err(refused)?;
        self.binding.validate(&self.connection).map_err(refused)
    }
    pub(crate) fn migrate_original_admitted(
        &self,
        admission: &OriginalLegacyMigrationAdmission<'_>,
    ) -> LegacyMigrationSettlement {
        use LegacyMigrationSettlement as S;
        let (profile, request, operation, deadline) = admission.migration_inputs();
        let prepared = (|| -> Result<_> {
            admission.validate_original_connection(&self.connection)?;
            let before_schema = image::capture(&self.connection)?;
            let before_rows = rows::capture(&self.connection, deadline)?;
            let original = schema::bounded_schema_image(&self.connection).map_err(refused)?;
            let expected = schema::expected_legacy_postimage(&original).map_err(refused)?;
            Ok((before_schema, before_rows, expected))
        })();
        let Ok((before_schema, before_rows, expected)) = prepared else {
            return S::NotStarted;
        };
        let Ok(scope) = self.connection.enter_original_legacy_mutation(admission) else {
            return S::NotStarted;
        };
        // Explicit transaction: no rusqlite Transaction Drop can replay rollback after uncertainty.
        if self.connection.execute_batch("BEGIN EXCLUSIVE").is_err() {
            return if self.connection.is_autocommit() && scope.finish().is_ok() {
                S::NotStarted
            } else {
                S::Unknown {
                    candidate: None,
                    restored: false,
                }
            };
        }
        let progress = Progress::install(&self.connection, deadline);
        let body = apply_atomic_migration_body(
            &self.connection,
            request,
            operation,
            before_schema,
            &before_rows,
            &expected,
            deadline,
            || self.validate_migration_bindings(profile),
        );
        // A deadline interruption must not prevent the one original rollback attempt.
        drop(progress);
        let encoded = match body {
            Ok(encoded) => encoded,
            Err(_) => {
                let rolled_back = self.connection.execute_batch("ROLLBACK").is_ok()
                    && self.connection.is_autocommit();
                let preserved = rolled_back
                    && (|| -> Result<bool> {
                        self.validate_migration_bindings(profile)?;
                        Ok(
                            image::capture_inside_migration(&self.connection)? == before_schema
                                && rows::capture(&self.connection, deadline)? == before_rows,
                        )
                    })()
                    .unwrap_or(false);
                return if scope.finish().is_ok() && preserved {
                    S::RollbackConfirmed
                } else {
                    S::Unknown {
                        candidate: None,
                        restored: false,
                    }
                };
            }
        };
        // Retain the exact pre-COMMIT candidate under the original input loan. It is data,
        // not a grant; only read-only committed readback may later confirm an unknown result.
        let candidate = VerifiedLegacyMigration {
            operation,
            encoded,
            expected,
            rows_digest: before_rows.digest,
        };
        if self.connection.execute_batch("COMMIT").is_err() {
            let restored = scope.finish().is_ok();
            return S::Unknown {
                candidate: Some(candidate),
                restored,
            };
        }
        #[cfg(all(test, target_os = "macos"))]
        if admission.lose_readback_once() {
            // Fault after the real original COMMIT. Keep its candidate and restored-policy
            // fact; recovery still must inspect the actual committed receipt and every claim.
            let restored = scope.finish().is_ok();
            return S::Unknown {
                candidate: Some(candidate),
                restored,
            };
        }
        let verified = self
            .verify_committed_migration(
                profile,
                request,
                candidate.operation,
                &candidate.encoded,
                &candidate.expected,
                &candidate.expected,
                candidate.rows_digest,
                deadline,
            )
            .is_ok();
        let restored = scope.finish().is_ok();
        if !restored || !verified {
            return S::Unknown {
                candidate: Some(candidate),
                restored,
            };
        }
        S::Committed(candidate)
    }

    pub(super) fn verify_committed_migration(
        &self,
        profile: &WritableProfileGuard,
        request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        operation: OperationId,
        encoded: &[u8],
        expected: &schema::SchemaImage,
        actual_expected: &schema::SchemaImage,
        rows_digest: [u8; 32],
        deadline: Instant,
    ) -> Result<()> {
        if !self.connection.is_autocommit() {
            return Err(LegacyObservationError("migration commit is not quiescent"));
        }
        self.verify_migration_image(
            profile,
            request,
            operation,
            encoded,
            expected,
            actual_expected,
            rows_digest,
            deadline,
        )
    }
    pub(super) fn verify_migration_image(
        &self,
        profile: &WritableProfileGuard,
        request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        operation: OperationId,
        encoded: &[u8],
        expected: &schema::SchemaImage,
        actual_expected: &schema::SchemaImage,
        rows_digest: [u8; 32],
        deadline: Instant,
    ) -> Result<()> {
        self.validate_migration_bindings(profile)?;
        absent_sidecars(self.binding.root())?;
        let version: i64 = self
            .connection
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .map_err(refused)?;
        if version != schema::SOURCE_DB_SCHEMA_VERSION {
            return Err(LegacyObservationError(
                "migration commit is not quiescent/current",
            ));
        }
        schema::validate_bounded_schema(&self.connection, actual_expected).map_err(refused)?;
        let mut statement = self.connection.prepare(
            "SELECT CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB)) BETWEEN 1 AND 65536 THEN value END FROM metadata WHERE key=?1"
        ).map_err(refused)?;
        statement
            .query_row([ADOPTION_METADATA_KEY], |row| {
                let rusqlite::types::ValueRef::Text(bytes) = row.get_ref(0)? else {
                    return Err(rusqlite::Error::InvalidQuery);
                };
                if bytes.len() > MAX_ADOPTION_RECEIPT_BYTES || bytes != encoded {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                let receipt = AdoptionReceiptEnvelope::decode(bytes)
                    .map_err(|_| rusqlite::Error::InvalidQuery)?;
                if !receipt.matches_original(request, operation)
                    || !receipt
                        .matches_postimage(schema::legacy_schema_digest(expected), rows_digest)
                {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                Ok(())
            })
            .map_err(refused)?;
        drop(statement);
        if rows::capture_preserved(&self.connection, deadline)?.digest != rows_digest {
            return Err(LegacyObservationError("committed migration rows changed"));
        }
        self.validate_migration_bindings(profile)?;
        if Instant::now() >= deadline {
            return Err(LegacyObservationError(
                "migration readback deadline elapsed",
            ));
        }
        Ok(())
    }
    pub(super) fn validate_original_post_migration_policy(
        &self,
        request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
    ) -> Result<()> {
        let (page_size, original_max, _, sector) = request.original_migration_policy();
        let mode: String = self
            .connection
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .map_err(refused)?;
        let page: u64 = self
            .connection
            .pragma_query_value(None, "page_size", |r| r.get(0))
            .map_err(refused)?;
        let max: u64 = self
            .connection
            .pragma_query_value(None, "max_page_count", |r| r.get(0))
            .map_err(refused)?;
        if !self.connection.validate_original_legacy_read_fence()
            || !mode.eq_ignore_ascii_case("delete")
            || page != page_size
            || max != original_max
            || self.connection.known_delete_sector_size() != Some(sector)
        {
            return Err(LegacyObservationError(
                "original committed source policy changed",
            ));
        }
        Ok(())
    }
    pub(crate) fn revalidate_committed_migration(
        &self,
        profile: &WritableProfileGuard,
        request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
        committed: &VerifiedLegacyMigration,
        deadline: Instant,
    ) -> Result<()> {
        self.validate_original_post_migration_policy(request)?;
        self.verify_committed_migration(
            profile,
            request,
            committed.operation,
            &committed.encoded,
            &committed.expected,
            &committed.expected,
            committed.rows_digest,
            deadline,
        )
    }
}

// The request is canonical base64 and the fixed receipt has only ASCII keys/scalars.
// SQLite TEXT in a UTF-16 source consumes two encoded bytes per ASCII character.
fn verify_receipt_storage_bound(connection: &rusqlite::Connection, encoded: &[u8]) -> Result<()> {
    let encoding: String = connection
        .pragma_query_value(None, "encoding", |r| r.get(0))
        .map_err(refused)?;
    let multiplier = if encoding.eq_ignore_ascii_case("UTF-8") {
        1
    } else if encoding.eq_ignore_ascii_case("UTF-16le") || encoding.eq_ignore_ascii_case("UTF-16be")
    {
        2
    } else {
        return Err(LegacyObservationError("unknown migration text encoding"));
    };
    if encoded.is_empty()
        || !encoded.is_ascii()
        || encoded
            .len()
            .checked_mul(multiplier)
            .is_none_or(|n| n > MAX_ADOPTION_RECEIPT_BYTES)
    {
        return Err(LegacyObservationError(
            "migration receipt encoded storage bound",
        ));
    }
    Ok(())
}

// Private SQL/data body. Its sole production caller holds the original mutation scope.
// Direct fixture calls test SQLite atomicity/data only and manufacture no owner capability.
fn apply_atomic_migration_body(
    connection: &rusqlite::Connection,
    request: &crate::operation_journal::legacy_adoption_commands::LegacyAdoptionIntentArgs,
    operation: OperationId,
    before_schema: [u8; 32],
    before_rows: &rows::Rows,
    expected: &schema::SchemaImage,
    deadline: Instant,
    validate_bindings: impl Fn() -> Result<()>,
) -> Result<Vec<u8>> {
    let synchronous: u64 = connection
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .map_err(refused)?;
    let fullfsync: u64 = connection
        .pragma_query_value(None, "fullfsync", |r| r.get(0))
        .map_err(refused)?;
    if synchronous != 3 || (cfg!(target_os = "macos") && fullfsync != 1) {
        return Err(LegacyObservationError(
            "migration durability policy changed",
        ));
    }
    validate_bindings()?;
    if image::capture_inside_migration(connection)? != before_schema
        || rows::capture(connection, deadline)? != *before_rows
    {
        return Err(LegacyObservationError("original migration image changed"));
    }
    schema::apply_preserving_legacy_schema(connection).map_err(refused)?;
    schema::validate_bounded_schema(connection, expected).map_err(refused)?;
    if rows::capture(connection, deadline)? != *before_rows {
        return Err(LegacyObservationError("migration changed original rows"));
    }
    let receipt = AdoptionReceiptEnvelope::capture(
        request,
        operation,
        before_schema,
        schema::legacy_schema_digest(expected),
        before_rows.digest,
        synchronous,
        fullfsync,
    )
    .map_err(refused)?;
    let encoded = receipt.encode().map_err(refused)?;
    verify_receipt_storage_bound(connection, &encoded)?;
    let text = std::str::from_utf8(&encoded).map_err(refused)?;
    if connection
        .execute(
            "INSERT INTO metadata(key,value) VALUES (?1,?2)",
            rusqlite::params![ADOPTION_METADATA_KEY, text],
        )
        .map_err(refused)?
        != 1
    {
        return Err(LegacyObservationError(
            "migration receipt insertion changed",
        ));
    }
    connection
        .pragma_update(None, "user_version", schema::SOURCE_DB_SCHEMA_VERSION)
        .map_err(refused)?;
    if rows::capture_preserved(connection, deadline)? != *before_rows {
        return Err(LegacyObservationError(
            "migration receipt changed preserved rows",
        ));
    }
    validate_bindings()?;
    if Instant::now() >= deadline {
        return Err(LegacyObservationError("migration deadline elapsed"));
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_dirs::AppRootGuard;
    use crate::operation_journal::{
        ParticipantId, legacy_adoption_commands::LegacyAdoptionIntentArgs,
        store::OperationJournalStore,
    };
    use crate::sample_sources::db::SourceCommandId;
    use std::sync::atomic::AtomicBool;

    // Actual original readonly request capture; the second SQLite file below is only a data
    // transaction fixture. It has no Source custody, physical grant or Native qualification.
    fn with_data_fixture(test: impl FnOnce(&rusqlite::Connection, &LegacyAdoptionIntentArgs)) {
        with_data_fixture_sql(
            include_str!("../../../../tests/fixtures/profile-v11/source.sql"),
            test,
        )
    }
    fn with_data_fixture_sql(
        sql: &str,
        test: impl FnOnce(&rusqlite::Connection, &LegacyAdoptionIntentArgs),
    ) {
        let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        let profile_root = tempfile::tempdir().unwrap();
        let media = tempfile::tempdir().unwrap();
        let _root = AppRootGuard::set(profile_root.path().to_path_buf()).unwrap();
        std::fs::create_dir(profile_root.path().join("legacy")).unwrap();
        rusqlite::Connection::open(
            profile_root
                .path()
                .join("legacy")
                .join(super::super::super::DB_FILE_NAME),
        )
        .unwrap()
        .execute_batch(sql)
        .unwrap();
        let profile = WritableProfileGuard::acquire_current().unwrap();
        let journal = OperationJournalStore::open(&profile).unwrap();
        let owner =
            SourceDatabase::open_legacy_read_owner(media.path(), &profile, "legacy").unwrap();
        let args = owner
            .prepare_legacy_adoption_input(
                &profile,
                &journal,
                ParticipantId::new(),
                SourceCommandId::new(),
                &AtomicBool::new(false),
            )
            .unwrap();
        let data = tempfile::tempdir().unwrap();
        let connection = rusqlite::Connection::open(data.path().join("data.sqlite")).unwrap();
        connection.execute_batch(sql).unwrap();
        connection.execute_batch("PRAGMA max_page_count=16384; PRAGMA synchronous=EXTRA; PRAGMA fullfsync=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF; PRAGMA query_only=ON").unwrap();
        test(&connection, &args);
    }
    #[test]
    fn legacy_migration_current_allowlist_requires_exact_compiled_shape_and_canonical_schema_receipt()
     {
        let sql = include_str!("../../../../tests/fixtures/profile-v11/source.sql");
        for sql in [
            sql.to_owned(),
            sql.replace("CREATE TABLE", "create\n table")
                .replace("PRIMARY KEY", "primary\n key"),
        ] {
            with_data_fixture_sql(&sql, |connection, args| {
                let deadline = Instant::now() + Duration::from_secs(2);
                let before_schema = image::capture(connection).unwrap();
                let before_rows = rows::capture(connection, deadline).unwrap();
                let original = schema::bounded_schema_image(connection).unwrap();
                let expected = schema::expected_legacy_postimage(&original).unwrap();
                connection
                    .execute_batch("PRAGMA query_only=OFF; BEGIN EXCLUSIVE")
                    .unwrap();
                let encoded = apply_atomic_migration_body(
                    connection,
                    args,
                    OperationId::new(),
                    before_schema,
                    &before_rows,
                    &expected,
                    deadline,
                    || Ok(()),
                )
                .unwrap();
                connection.execute_batch("COMMIT").unwrap();
                // Data-only compiled table fixture; its presence is not a current SourceLease.
                connection
                    .execute_batch(super::super::super::source_lease::LEASE_TABLE_SQL)
                    .unwrap();
                schema::require_known_metadata_schema(connection).unwrap();
                // The immutable receipt describes migration, not every future legitimate row edit.
                connection.execute_batch("INSERT INTO metadata VALUES ('later.edit', 'allowed'); UPDATE wav_files SET modified_ns=modified_ns+1").unwrap();
                schema::require_known_metadata_schema(connection).unwrap();
                connection
                    .execute_batch(
                        "CREATE TRIGGER unexpected AFTER UPDATE ON metadata BEGIN SELECT 1; END",
                    )
                    .unwrap();
                assert!(schema::require_known_metadata_schema(connection).is_err());
                connection.execute_batch("DROP TRIGGER unexpected").unwrap();
                schema::require_known_metadata_schema(connection).unwrap();
                connection
                    .execute("DELETE FROM metadata WHERE key=?1", [ADOPTION_METADATA_KEY])
                    .unwrap();
                assert!(schema::require_known_metadata_schema(connection).is_err());
                connection
                    .execute(
                        "INSERT INTO metadata VALUES (?1,?2)",
                        rusqlite::params![ADOPTION_METADATA_KEY, "{}"],
                    )
                    .unwrap();
                assert!(schema::require_known_metadata_schema(connection).is_err());
                connection
                    .execute(
                        "UPDATE metadata SET value=?2 WHERE key=?1",
                        rusqlite::params![
                            ADOPTION_METADATA_KEY,
                            std::str::from_utf8(&encoded).unwrap()
                        ],
                    )
                    .unwrap();
                schema::require_known_metadata_schema(connection).unwrap();
                // Same known token grammar, different raw DDL: the original receipt must reject it.
                connection.execute_batch("PRAGMA writable_schema=ON; UPDATE sqlite_master SET sql=sql||' ' WHERE name='metadata'; PRAGMA writable_schema=OFF").unwrap();
                assert!(schema::require_known_metadata_schema(connection).is_err());
            });
        }
    }

    #[test]
    fn legacy_migration_receipt_storage_bound_accounts_for_original_utf16_without_changing_encoding()
     {
        for encoding in ["UTF-8", "UTF-16le", "UTF-16be"] {
            let connection = rusqlite::Connection::open_in_memory().unwrap();
            connection
                .pragma_update(None, "encoding", encoding)
                .unwrap();
            connection
                .execute_batch("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
            let limit = MAX_ADOPTION_RECEIPT_BYTES / if encoding == "UTF-8" { 1 } else { 2 };
            assert!(verify_receipt_storage_bound(&connection, &vec![b'x'; limit]).is_ok());
            assert!(verify_receipt_storage_bound(&connection, &vec![b'x'; limit + 1]).is_err());
            assert!(verify_receipt_storage_bound(&connection, b"non-ascii:\xc3\xa9").is_err());
            let text = "x".repeat(limit);
            connection
                .execute(
                    "INSERT INTO metadata VALUES ('source.legacy_adoption.v1',?1)",
                    [&text],
                )
                .unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT length(CAST(value AS BLOB)) FROM metadata",
                        [],
                        |r| r.get::<_, usize>(0)
                    )
                    .unwrap(),
                MAX_ADOPTION_RECEIPT_BYTES
            );
            assert_eq!(
                connection
                    .pragma_query_value(None, "encoding", |r| r.get::<_, String>(0))
                    .unwrap(),
                encoding
            );
        }
    }
    #[test]
    fn legacy_migration_sql_data_commits_exact_schema_receipt_version_and_preserved_rows_together()
    {
        with_data_fixture(|connection, args| {
            let deadline = Instant::now() + Duration::from_secs(2);
            let before_schema = image::capture(connection).unwrap();
            let before_rows = rows::capture(connection, deadline).unwrap();
            let original = schema::bounded_schema_image(connection).unwrap();
            let expected = schema::expected_legacy_postimage(&original).unwrap();
            let operation = OperationId::new();
            connection
                .execute_batch("PRAGMA query_only=OFF; BEGIN EXCLUSIVE")
                .unwrap();
            let encoded = apply_atomic_migration_body(
                connection,
                args,
                operation,
                before_schema,
                &before_rows,
                &expected,
                deadline,
                || Ok(()),
            )
            .unwrap();
            connection
                .execute_batch("COMMIT; PRAGMA query_only=ON")
                .unwrap();
            assert_eq!(
                connection
                    .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                    .unwrap(),
                13
            );
            schema::validate_bounded_schema(connection, &expected).unwrap();
            assert_eq!(
                rows::capture_preserved(connection, deadline).unwrap(),
                before_rows
            );
            let text: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key=?1",
                    [ADOPTION_METADATA_KEY],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(text.as_bytes(), encoded);
            let receipt = AdoptionReceiptEnvelope::decode(text.as_bytes()).unwrap();
            assert!(receipt.matches_original(args, operation));
            assert!(
                receipt
                    .matches_postimage(schema::legacy_schema_digest(&expected), before_rows.digest)
            );
            assert!(!receipt.matches_original(args, OperationId::new()));
            // No lease row/relation, audit, SourceRevision or physical receipt is invented.
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM sqlite_schema WHERE name='source_lease'",
                        [],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                0
            );
            connection
                .execute_batch("PRAGMA query_only=OFF; BEGIN EXCLUSIVE")
                .unwrap();
            assert!(
                apply_atomic_migration_body(
                    connection,
                    args,
                    operation,
                    before_schema,
                    &before_rows,
                    &expected,
                    deadline,
                    || Ok(())
                )
                .is_err()
            );
            connection
                .execute_batch("ROLLBACK; PRAGMA query_only=ON")
                .unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM metadata WHERE key=?1",
                        [ADOPTION_METADATA_KEY],
                        |r| r.get::<_, i64>(0)
                    )
                    .unwrap(),
                1
            );
            // Exclude exactly the reserved key, never arbitrary prefixes.
            connection.execute_batch("PRAGMA query_only=OFF; INSERT INTO metadata VALUES ('source.legacy_adoption.v1.extra', 'retained'); PRAGMA query_only=ON").unwrap();
            assert_ne!(
                rows::capture_preserved(connection, deadline)
                    .unwrap()
                    .digest,
                before_rows.digest
            );
        });
    }
    #[test]
    fn legacy_migration_sql_data_failed_final_binding_check_rolls_back_receipt_ddl_and_version() {
        with_data_fixture(|connection, args| {
            let deadline = Instant::now() + Duration::from_secs(2);
            let before_schema = image::capture(connection).unwrap();
            let before_rows = rows::capture(connection, deadline).unwrap();
            let original = schema::bounded_schema_image(connection).unwrap();
            let expected = schema::expected_legacy_postimage(&original).unwrap();
            let calls = std::cell::Cell::new(0);
            connection
                .execute_batch("PRAGMA query_only=OFF; BEGIN EXCLUSIVE")
                .unwrap();
            assert!(
                apply_atomic_migration_body(
                    connection,
                    args,
                    OperationId::new(),
                    before_schema,
                    &before_rows,
                    &expected,
                    deadline,
                    || {
                        calls.set(calls.get() + 1);
                        if calls.get() == 2 {
                            Err(LegacyObservationError("fixture final binding rejection"))
                        } else {
                            Ok(())
                        }
                    }
                )
                .is_err()
            );
            assert_eq!(calls.get(), 2);
            // The SQL body reached receipt and version stamping, all within this same transaction.
            assert_eq!(
                connection
                    .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                    .unwrap(),
                13
            );
            connection
                .execute_batch("ROLLBACK; PRAGMA query_only=ON")
                .unwrap();
            assert_eq!(image::capture(connection).unwrap(), before_schema);
            assert_eq!(schema::bounded_schema_image(connection).unwrap(), original);
            assert_eq!(rows::capture(connection, deadline).unwrap(), before_rows);
        });
    }
}
