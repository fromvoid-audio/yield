//! Incremental schema migrations for older source database files.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use super::super::SourceDbError;
use super::super::util::{
    map_sql_error, normalize_source_index_path, source_index_plain_path_needs_rekey,
};

mod analysis_jobs;
mod aspect_descriptors;
mod columns;
mod invalid_paths;
mod tag_catalog;

use self::analysis_jobs::{
    ensure_analysis_jobs_optional_columns, ensure_source_readiness_schema,
    retire_legacy_analysis_runtime_state,
};
use self::aspect_descriptors::ensure_aspect_descriptor_tables;
use self::columns::{
    ensure_feature_metric_columns, ensure_file_ops_journal_optional_columns,
    ensure_pending_rename_optional_columns, ensure_samples_optional_columns,
    ensure_wav_files_optional_columns,
};
pub(super) use self::invalid_paths::remove_invalid_relative_paths;
use self::tag_catalog::{backfill_tag_catalog, ensure_tag_catalog_schema};

/// Apply additive column migrations needed by older source databases.
pub(super) fn apply_optional_migrations(connection: &Connection) -> Result<(), SourceDbError> {
    apply_structural_migrations(connection)?;
    retire_legacy_analysis_runtime_state(connection)?;
    migrate_canonical_file_identities(connection)?;
    backfill_tag_catalog(connection)?;
    Ok(())
}

/// Apply low-cost additive repairs that must not be skipped for current stamps.
pub(super) fn apply_current_stamp_repairs(connection: &Connection) -> Result<(), SourceDbError> {
    apply_structural_migrations(connection)
}

fn apply_structural_migrations(connection: &Connection) -> Result<(), SourceDbError> {
    ensure_wav_files_optional_columns(connection)?;
    ensure_source_index_schema(connection)?;
    apply_structural_after_wav_and_index(connection)
}

// Known-11 adoption has only metadata/wav before base DDL. The new source-index
// table has no historic paths to rekey; keep its DDL in the caller's one transaction.
pub(super) fn apply_legacy_structural_inside_transaction(
    connection: &Connection,
) -> Result<(), SourceDbError> {
    if connection.is_autocommit() {
        return Err(SourceDbError::MaintenanceRequired);
    }
    ensure_wav_files_optional_columns(connection)?;
    connection
        .execute_batch(SOURCE_INDEX_SCHEMA_SQL)
        .map_err(map_sql_error)?;
    apply_structural_after_wav_and_index(connection)
}

fn apply_structural_after_wav_and_index(connection: &Connection) -> Result<(), SourceDbError> {
    ensure_pending_rename_optional_columns(connection)?;
    ensure_file_ops_journal_optional_columns(connection)?;
    ensure_analysis_jobs_optional_columns(connection)?;
    ensure_source_readiness_schema(connection)?;
    connection
        .execute_batch(crate::sample_sources::readiness::FEATURE_ARTIFACT_SCHEMA)
        .map_err(map_sql_error)?;
    ensure_samples_optional_columns(connection)?;
    ensure_feature_metric_columns(connection, "features")?;
    ensure_feature_metric_columns(connection, "analysis_cache_features")?;
    ensure_tag_catalog_schema(connection)?;
    ensure_collection_membership_schema(connection)?;
    ensure_pending_rename_destination_schema(connection)?;
    ensure_aspect_descriptor_tables(connection)?;
    ensure_directory_truth_schema(connection)?;
    ensure_source_participant_curation_receipts_schema(connection)?;
    Ok(())
}

/// Ensure the durable source-writer receipt relation used by the bounded source command path.
///
/// This relation is deliberately source-local. It is not an operation-journal table: the
/// source database owns the curation transaction and retains the exact source-participant
/// request/receipt bytes needed for replay after a lost acknowledgement.
fn ensure_source_participant_curation_receipts_schema(
    connection: &Connection,
) -> Result<(), SourceDbError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS source_participant_curation_receipts (
                operation_id TEXT NOT NULL,
                participant_id TEXT NOT NULL,
                command_id TEXT NOT NULL,
                source_id TEXT NOT NULL CHECK(length(trim(source_id)) > 0),
                root_identity BLOB NOT NULL,
                root_binding_identity TEXT NOT NULL CHECK(length(trim(root_binding_identity)) > 0),
                database_identity TEXT NOT NULL CHECK(length(trim(database_identity)) > 0),
                lifecycle_generation TEXT NOT NULL CHECK(length(trim(lifecycle_generation)) > 0),
                lease_epoch INTEGER NOT NULL CHECK(lease_epoch > 0),
                expected_revision INTEGER NOT NULL CHECK(expected_revision >= 0),
                committed_revision INTEGER NOT NULL CHECK(committed_revision > 0),
                request_bytes BLOB NOT NULL CHECK(length(request_bytes) > 0),
                receipt_bytes BLOB NOT NULL CHECK(length(receipt_bytes) > 0),
                request_len INTEGER NOT NULL CHECK(request_len = length(request_bytes)),
                receipt_len INTEGER NOT NULL CHECK(receipt_len = length(receipt_bytes)),
                PRIMARY KEY(operation_id, participant_id, command_id)
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_source_participant_curation_receipts_source
                ON source_participant_curation_receipts(source_id, committed_revision);
            CREATE INDEX IF NOT EXISTS idx_source_participant_curation_receipts_revision
                ON source_participant_curation_receipts(committed_revision);",
        )
        .map_err(map_sql_error)?;

    let columns = table_columns(connection, "source_participant_curation_receipts")?;
    let required = [
        "operation_id",
        "participant_id",
        "command_id",
        "source_id",
        "root_identity",
        "root_binding_identity",
        "database_identity",
        "lifecycle_generation",
        "lease_epoch",
        "expected_revision",
        "committed_revision",
        "request_bytes",
        "receipt_bytes",
        "request_len",
        "receipt_len",
    ];
    if required.iter().any(|column| !columns.contains(*column)) {
        return Err(SourceDbError::Unexpected);
    }
    Ok(())
}

fn ensure_directory_truth_schema(connection: &Connection) -> Result<(), SourceDbError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS source_directory_generations (
                generation INTEGER PRIMARY KEY CHECK(generation > 0),
                status TEXT NOT NULL CHECK(status IN ('staging', 'active', 'inactive')),
                expected_entry_count INTEGER NOT NULL CHECK(expected_entry_count >= 0),
                staged_entry_count INTEGER NOT NULL DEFAULT 0
                    CHECK(staged_entry_count >= 0 AND staged_entry_count <= expected_entry_count),
                complete INTEGER NOT NULL DEFAULT 0 CHECK(complete IN (0, 1)),
                published_source_revision INTEGER
                    CHECK(published_source_revision IS NULL OR published_source_revision > 0),
                created_at INTEGER NOT NULL DEFAULT 0,
                CHECK (
                    (status = 'staging' AND published_source_revision IS NULL)
                    OR (status IN ('active', 'inactive') AND published_source_revision IS NOT NULL)
                ),
                CHECK(
                    status = 'staging'
                    OR (complete = 1 AND staged_entry_count = expected_entry_count)
                )
            );
            CREATE TABLE IF NOT EXISTS source_directory_entries (
                generation INTEGER NOT NULL,
                path TEXT NOT NULL CHECK(length(path) > 0),
                path_encoding INTEGER NOT NULL DEFAULT 0 CHECK(path_encoding IN (0, 1)),
                directory_identity TEXT NOT NULL CHECK(length(trim(directory_identity)) > 0),
                PRIMARY KEY (generation, path),
                UNIQUE (generation, directory_identity),
                FOREIGN KEY(generation) REFERENCES source_directory_generations(generation)
                    ON DELETE CASCADE
            ) WITHOUT ROWID;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_source_directory_generations_active
                ON source_directory_generations(status)
                WHERE status = 'active';
            CREATE INDEX IF NOT EXISTS idx_source_directory_generations_status_generation
                ON source_directory_generations(status, generation);",
        )
        .map_err(map_sql_error)?;

    let generation_columns = table_columns(connection, "source_directory_generations")?;
    let entry_columns = table_columns(connection, "source_directory_entries")?;
    let generation_required = [
        "generation",
        "status",
        "expected_entry_count",
        "staged_entry_count",
        "complete",
        "published_source_revision",
        "created_at",
    ];
    let entry_required = ["generation", "path", "path_encoding", "directory_identity"];
    if generation_required
        .iter()
        .any(|column| !generation_columns.contains(*column))
        || entry_required
            .iter()
            .any(|column| !entry_columns.contains(*column))
    {
        return Err(SourceDbError::Unexpected);
    }
    Ok(())
}

const SOURCE_INDEX_SCHEMA_SQL: &str = "CREATE TABLE IF NOT EXISTS source_index_entries (
                path TEXT PRIMARY KEY,
                path_encoding INTEGER NOT NULL DEFAULT 0,
                classification TEXT NOT NULL CHECK(classification IN (
                    'unsupported_audio',
                    'unsupported_non_audio',
                    'inaccessible',
                    'practically_unsupported_audio'
                )),
                file_size INTEGER,
                modified_ns INTEGER,
                file_identity TEXT,
                diagnostic TEXT,
                format_policy_version INTEGER NOT NULL
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_source_index_entries_classification_path
                ON source_index_entries(classification, path);";

fn ensure_source_index_schema(connection: &Connection) -> Result<(), SourceDbError> {
    let tx = connection.unchecked_transaction().map_err(map_sql_error)?;
    tx.execute_batch(SOURCE_INDEX_SCHEMA_SQL)
        .map_err(map_sql_error)?;
    if !table_columns(&tx, "source_index_entries")?.contains("path_encoding") {
        tx.execute(
            "ALTER TABLE source_index_entries
                 ADD COLUMN path_encoding INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(map_sql_error)?;
    }
    migrate_legacy_source_index_paths(&tx)?;
    tx.commit().map_err(map_sql_error)?;
    Ok(())
}

fn migrate_legacy_source_index_paths(connection: &Connection) -> Result<(), SourceDbError> {
    let legacy_paths = {
        let mut statement = connection
            .prepare(
                "SELECT path
                 FROM source_index_entries
                 WHERE path_encoding = 0
                 ORDER BY path",
            )
            .map_err(map_sql_error)?;
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(map_sql_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(map_sql_error)?
    };

    for legacy_path in legacy_paths {
        if !source_index_plain_path_needs_rekey(&legacy_path) {
            continue;
        }
        let (canonical_path, path_encoding) =
            match normalize_source_index_path(Path::new(&legacy_path)) {
                Ok(normalized) => normalized,
                Err(error) => {
                    tracing::warn!(
                        path = legacy_path,
                        %error,
                        "Skipping invalid legacy source index path during reserved-prefix migration"
                    );
                    continue;
                }
            };
        if path_encoding == 0 || canonical_path == legacy_path {
            continue;
        }
        let canonical_exists = connection
            .query_row(
                "SELECT 1 FROM source_index_entries WHERE path = ?1",
                [&canonical_path],
                |_| Ok(()),
            )
            .optional()
            .map_err(map_sql_error)?
            .is_some();
        if canonical_exists {
            connection
                .execute(
                    "DELETE FROM source_index_entries WHERE path = ?1",
                    [&legacy_path],
                )
                .map_err(map_sql_error)?;
        } else {
            connection
                .execute(
                    "UPDATE source_index_entries
                     SET path = ?1, path_encoding = ?2
                     WHERE path = ?3",
                    params![canonical_path, path_encoding, legacy_path],
                )
                .map_err(map_sql_error)?;
        }
    }
    Ok(())
}

fn ensure_pending_rename_destination_schema(connection: &Connection) -> Result<(), SourceDbError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS pending_wav_rename_destinations (
                path TEXT PRIMARY KEY,
                scan_generation INTEGER NOT NULL,
                retained_hash TEXT
            );",
        )
        .map_err(map_sql_error)?;
    let columns = table_columns(connection, "pending_wav_rename_destinations")?;
    if !columns.contains("retained_hash") {
        connection
            .execute(
                "ALTER TABLE pending_wav_rename_destinations
                 ADD COLUMN retained_hash TEXT",
                [],
            )
            .map_err(map_sql_error)?;
    }
    Ok(())
}

fn ensure_collection_membership_schema(connection: &Connection) -> Result<(), SourceDbError> {
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS wav_file_collections (
                path TEXT NOT NULL,
                collection INTEGER NOT NULL,
                PRIMARY KEY (path, collection),
                FOREIGN KEY(path) REFERENCES wav_files(path) ON DELETE CASCADE
            ) WITHOUT ROWID;
            CREATE INDEX IF NOT EXISTS idx_wav_file_collections_collection
                ON wav_file_collections(collection);
            INSERT OR IGNORE INTO wav_file_collections (path, collection)
            SELECT path, collection
            FROM wav_files
            WHERE collection IS NOT NULL;",
        )
        .map_err(map_sql_error)?;
    Ok(())
}

fn migrate_canonical_file_identities(connection: &Connection) -> Result<(), SourceDbError> {
    let mut migrated_identities = 0usize;
    for table in ["wav_files", "pending_wav_renames"] {
        if !table_columns(connection, table)?.contains("file_identity") {
            continue;
        }
        let sql = format!(
            "UPDATE {table}
             SET file_identity = CASE
                 WHEN file_identity LIKE 'unix-v2:%'
                     THEN 'unix:' || substr(file_identity, length('unix-v2:') + 1)
                 WHEN file_identity LIKE 'windows-v2:%'
                     THEN 'windows:' || substr(file_identity, length('windows-v2:') + 1)
                 WHEN (
                     file_identity LIKE 'unix:%'
                     OR file_identity LIKE 'windows:%'
                 ) AND (
                     length(file_identity) - length(replace(file_identity, ':', ''))
                 ) = 3
                     THEN file_identity
                 ELSE NULL
             END
             WHERE file_identity IS NOT NULL
               AND (
                   file_identity LIKE 'unix-v2:%'
                   OR file_identity LIKE 'windows-v2:%'
                   OR NOT (
                       (file_identity LIKE 'unix:%' OR file_identity LIKE 'windows:%')
                       AND (
                           length(file_identity) - length(replace(file_identity, ':', ''))
                       ) = 3
                   )
               )"
        );
        migrated_identities = migrated_identities
            .saturating_add(connection.execute(&sql, []).map_err(map_sql_error)?);
    }
    if migrated_identities == 0 {
        return Ok(());
    }
    // Readiness rows are derived from the manifest. Rebuilding them is both
    // simpler and safer than rewriting identity-bearing keys in place: mixed
    // experimental/canonical databases may already contain both spellings,
    // making an UPDATE collide with their uniqueness constraints. Existing
    // durable analysis artifacts are deliberately preserved and will be
    // rediscovered under the canonical identities.
    connection
        .execute_batch(
            "DELETE FROM analysis_jobs WHERE readiness_managed = 1;
             DELETE FROM source_readiness_targets;
             DELETE FROM source_readiness_artifacts;
             DELETE FROM source_readiness_sources;
             DELETE FROM metadata
             WHERE key = 'readiness_target_fingerprint_v1';",
        )
        .map_err(map_sql_error)?;
    Ok(())
}

pub(crate) fn table_columns(
    connection: &Connection,
    table_name: &str,
) -> Result<HashSet<String>, SourceDbError> {
    let pragma = format!("PRAGMA table_info({table_name})");
    let mut stmt = connection.prepare(&pragma).map_err(map_sql_error)?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(map_sql_error)?
        .collect::<Result<HashSet<_>, _>>()
        .map_err(map_sql_error)?;
    Ok(columns)
}

#[cfg(test)]
mod tests;
