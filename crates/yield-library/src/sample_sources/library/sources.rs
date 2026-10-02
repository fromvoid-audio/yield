use std::path::{Path, PathBuf};
use std::time::Instant;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use super::connection::LibraryDatabase;
use super::error::map_sql_error;
use super::owner::SourceRegistrySnapshot;
use super::registry_limits::{
    RegistryBoundError, SOURCE_REGISTRY_MAX_BYTES, SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
    SOURCE_REGISTRY_MAX_DESCRIPTORS, SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES, bounded_json,
    bounded_json_at_limit, validate_sources, validate_sources_encoded,
};
use super::telemetry::record_library_db_event;
use super::{
    KNOWN_SOURCES_KEY, LibraryError, LibraryState, UI_PREFERENCES_KEY, UI_PREFERENCES_MAX_BYTES,
};
use crate::sample_sources::normalize_path;
use crate::sample_sources::{
    SampleSource, SourceId, SourceMetadataStorage, SourceRole, default_primary_import_folder,
};

impl LibraryDatabase {
    pub(super) fn load_state(&self) -> Result<LibraryState, LibraryError> {
        let started_at = Instant::now();
        let sources = self.load_sources()?;
        let state = LibraryState { sources };
        record_library_db_event("library.load_state", started_at, Ok(()));
        Ok(state)
    }

    pub(super) fn replace_state(&mut self, state: &LibraryState) -> Result<(), LibraryError> {
        self.replace_source_registry_with_event(
            &SourceRegistrySnapshot::from_sources(state.sources.clone()),
            "library.replace_state",
        )
    }

    pub(super) fn load_source_registry(&self) -> Result<SourceRegistrySnapshot, LibraryError> {
        Ok(SourceRegistrySnapshot::from_sources(self.load_sources()?))
    }

    pub(super) fn load_ui_preferences_payload(&self) -> Result<Option<String>, LibraryError> {
        let started_at = Instant::now();
        let result = load_bounded_metadata(
            &self.connection,
            UI_PREFERENCES_KEY,
            UI_PREFERENCES_MAX_BYTES,
        );
        record_library_db_event(
            "library.load_ui_preferences_payload",
            started_at,
            result.as_ref().map(|_| ()),
        );
        result
    }

    pub(super) fn replace_ui_preferences_payload(
        &mut self,
        payload: &str,
    ) -> Result<(), LibraryError> {
        if payload.len() > UI_PREFERENCES_MAX_BYTES {
            return Err(LibraryError::MetadataTooLarge {
                key: UI_PREFERENCES_KEY,
                limit: UI_PREFERENCES_MAX_BYTES,
                actual: payload.len(),
            });
        }
        let started_at = Instant::now();
        let tx = super::ordinary_retry::begin(&self.connection)?;
        // Observe the retained WAL while this owner holds the SQLite writer reservation.
        crate::sqlite_wal::enforce_wal_admission(&tx)?;
        Self::set_metadata_in_tx(&tx, UI_PREFERENCES_KEY, payload)?;
        tx.commit().map_err(map_sql_error)?;
        record_library_db_event("library.replace_ui_preferences_payload", started_at, Ok(()));
        Ok(())
    }

    pub(super) fn replace_source_registry(
        &mut self,
        snapshot: &SourceRegistrySnapshot,
    ) -> Result<(), LibraryError> {
        self.replace_source_registry_with_event(snapshot, "library.replace_source_registry")
    }

    fn replace_source_registry_with_event(
        &mut self,
        snapshot: &SourceRegistrySnapshot,
        event: &'static str,
    ) -> Result<(), LibraryError> {
        validate_sources_encoded(snapshot.as_slice()).map_err(map_registry_bound_error)?;
        let started_at = Instant::now();
        let tx = super::ordinary_retry::begin(&self.connection)?;
        // Observe the retained WAL while this owner holds the SQLite writer reservation.
        crate::sqlite_wal::enforce_wal_admission(&tx)?;
        Self::replace_source_registry_in_tx(&tx, snapshot)?;
        tx.commit().map_err(map_sql_error)?;
        record_library_db_event(event, started_at, Ok(()));
        Ok(())
    }

    pub(super) fn replace_source_registry_in_tx(
        tx: &Transaction<'_>,
        snapshot: &SourceRegistrySnapshot,
    ) -> Result<(), LibraryError> {
        let mut mappings = Self::load_known_sources_from(tx)?;
        Self::replace_sources(tx, snapshot.as_slice())?;
        Self::remember_known_sources_in_tx(tx, &mut mappings, snapshot.as_slice())
    }

    pub(super) fn load_sources(&self) -> Result<Vec<SampleSource>, LibraryError> {
        let started_at = Instant::now();
        let result = load_sources_from_connection(&self.connection);
        record_library_db_event(
            "library.load_sources",
            started_at,
            result.as_ref().map(|_| ()),
        );
        result
    }

    pub(super) fn lookup_known_source_id(
        &self,
        root: &Path,
    ) -> Result<Option<SourceId>, LibraryError> {
        let started_at = Instant::now();
        let normalized = normalize_path(root);
        let needle = normalized.to_string_lossy().to_string();
        let mappings = self.load_known_sources()?;
        let result = mappings
            .into_iter()
            .find(|entry| entry.root == needle)
            .map(|entry| SourceId::from_string(entry.source_id));
        record_library_db_event("library.lookup_known_source_id", started_at, Ok(()));
        Ok(result)
    }

    pub(super) fn lookup_retained_source(
        &self,
        root: &Path,
    ) -> Result<Option<SampleSource>, LibraryError> {
        let started_at = Instant::now();
        let normalized = normalize_path(root);
        let needle = normalized.to_string_lossy().to_string();
        let result = self
            .load_known_sources()?
            .into_iter()
            .find(|entry| entry.root == needle)
            .map(KnownSourceMapping::try_into_sample_source)
            .transpose()?;
        record_library_db_event("library.lookup_retained_source", started_at, Ok(()));
        Ok(result)
    }

    pub(super) fn load_retained_sources(&self) -> Result<Vec<SampleSource>, LibraryError> {
        self.load_known_sources()?
            .into_iter()
            .map(KnownSourceMapping::try_into_sample_source)
            .collect()
    }

    fn replace_sources(tx: &Transaction<'_>, sources: &[SampleSource]) -> Result<(), LibraryError> {
        tx.execute("DELETE FROM sources", [])
            .map_err(map_sql_error)?;
        if sources.is_empty() {
            return Ok(());
        }

        let mut stmt = tx
            .prepare(
                "INSERT INTO sources (
                    id, root, sort_order, role, metadata_storage, primary_import_folder
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .map_err(map_sql_error)?;
        for (idx, source) in sources.iter().enumerate() {
            let source = normalized_source(source.clone());
            stmt.execute(params![
                source.id.as_str(),
                source.root.to_string_lossy(),
                idx as i64,
                source.role.as_str(),
                source.metadata_storage.as_str(),
                source.primary_import_folder.to_string_lossy(),
            ])
            .map_err(map_sql_error)?;
        }
        Ok(())
    }

    fn remember_known_sources_in_tx(
        tx: &Transaction<'_>,
        mappings: &mut Vec<KnownSourceMapping>,
        sources: &[SampleSource],
    ) -> Result<(), LibraryError> {
        for source in sources {
            upsert_known_source_mapping(mappings, source);
        }
        mappings.sort_by(|a, b| a.root.cmp(&b.root));
        validate_known_source_mappings(mappings)?;
        let payload = bounded_json(mappings).map_err(map_registry_bound_error)?;
        let payload =
            String::from_utf8(payload).map_err(|_| LibraryError::SourceRegistryMalformed {
                field: "known_sources_v1",
            })?;
        Self::set_metadata_in_tx(tx, KNOWN_SOURCES_KEY, &payload)?;
        Ok(())
    }

    fn load_known_sources(&self) -> Result<Vec<KnownSourceMapping>, LibraryError> {
        let started_at = Instant::now();
        let result = Self::load_known_sources_from(&self.connection);
        record_library_db_event(
            "library.load_known_sources",
            started_at,
            result.as_ref().map(|_| ()),
        );
        result
    }

    fn load_known_sources_from(conn: &Connection) -> Result<Vec<KnownSourceMapping>, LibraryError> {
        let Some(value) =
            load_bounded_metadata(conn, KNOWN_SOURCES_KEY, SOURCE_REGISTRY_MAX_BYTES)?
        else {
            return Ok(Vec::new());
        };
        let mappings =
            serde_json::from_str::<Vec<KnownSourceMapping>>(&value).map_err(|source| {
                LibraryError::MetadataJson {
                    key: KNOWN_SOURCES_KEY,
                    source,
                }
            })?;
        validate_known_source_mappings(&mappings)?;
        Ok(mappings)
    }

    fn set_metadata_in_tx(
        tx: &Transaction<'_>,
        key: &str,
        value: &str,
    ) -> Result<(), LibraryError> {
        tx.execute(
            "INSERT INTO metadata (key, value)
             VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )
        .map_err(map_sql_error)?;
        Ok(())
    }
}

pub(super) fn load_sources_from_connection(
    connection: &Connection,
) -> Result<Vec<SampleSource>, LibraryError> {
    let mut statement = connection
        .prepare(
            "SELECT
                length(CAST(id AS BLOB)),
                CASE WHEN length(CAST(id AS BLOB)) <= ?1 THEN CAST(id AS BLOB) END,
                length(CAST(root AS BLOB)),
                CASE WHEN length(CAST(root AS BLOB)) <= ?2 THEN CAST(root AS BLOB) END,
                length(CAST(role AS BLOB)),
                CASE WHEN length(CAST(role AS BLOB)) <= ?2 THEN CAST(role AS BLOB) END,
                length(CAST(metadata_storage AS BLOB)),
                CASE WHEN length(CAST(metadata_storage AS BLOB)) <= ?2
                     THEN CAST(metadata_storage AS BLOB) END,
                length(CAST(primary_import_folder AS BLOB)),
                CASE WHEN length(CAST(primary_import_folder AS BLOB)) <= ?2
                     THEN CAST(primary_import_folder AS BLOB) END
             FROM sources
             ORDER BY sort_order ASC, id ASC
             LIMIT ?3",
        )
        .map_err(map_sql_error)?;
    let raw_rows = statement
        .query_map(
            [
                SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES as i64,
                SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES as i64,
                (SOURCE_REGISTRY_MAX_DESCRIPTORS + 1) as i64,
            ],
            |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<Vec<u8>>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, Option<Vec<u8>>>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                ))
            },
        )
        .map_err(map_sql_error)?;
    let mut rows = Vec::new();
    let mut aggregate = 0usize;
    for (index, raw_row) in raw_rows.enumerate() {
        if index >= SOURCE_REGISTRY_MAX_DESCRIPTORS {
            return Err(LibraryError::SourceRegistryTooManyDescriptors {
                limit: SOURCE_REGISTRY_MAX_DESCRIPTORS,
                actual: index.saturating_add(1),
            });
        }
        let raw_row = raw_row.map_err(map_sql_error)?;
        let source = parse_source_row(index, raw_row)?;
        let descriptor =
            validate_sources(std::slice::from_ref(&source)).map_err(map_registry_bound_error)?;
        aggregate =
            aggregate
                .checked_add(descriptor)
                .ok_or(LibraryError::SourceRegistryTooLarge {
                    limit: SOURCE_REGISTRY_MAX_BYTES,
                    actual: SOURCE_REGISTRY_MAX_BYTES.saturating_add(1),
                })?;
        if aggregate > SOURCE_REGISTRY_MAX_BYTES {
            return Err(LibraryError::SourceRegistryTooLarge {
                limit: SOURCE_REGISTRY_MAX_BYTES,
                actual: aggregate,
            });
        }
        rows.push(source);
    }
    validate_sources_encoded(rows.as_slice()).map_err(map_registry_bound_error)?;
    Ok(rows)
}

fn load_bounded_metadata(
    conn: &Connection,
    key: &'static str,
    limit: usize,
) -> Result<Option<String>, LibraryError> {
    let Some((actual, bytes)) = conn
        .query_row(
            "SELECT length(CAST(value AS BLOB)),
                    CASE
                        WHEN length(CAST(value AS BLOB)) <= ?2
                        THEN CAST(value AS BLOB)
                    END
             FROM metadata
             WHERE key = ?1",
            params![key, limit as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
        )
        .optional()
        .map_err(map_sql_error)?
    else {
        return Ok(None);
    };
    let actual = usize::try_from(actual).map_err(|_| LibraryError::MetadataTooLarge {
        key,
        limit,
        actual: usize::MAX,
    })?;
    if actual > limit {
        return Err(LibraryError::MetadataTooLarge { key, limit, actual });
    }
    let Some(bytes) = bytes else {
        return Err(LibraryError::MetadataInvalidUtf8 { key });
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| LibraryError::MetadataInvalidUtf8 { key })
}

pub(super) fn map_registry_bound_error(error: RegistryBoundError) -> LibraryError {
    match error {
        RegistryBoundError::TooManyDescriptors { actual } => {
            LibraryError::SourceRegistryTooManyDescriptors {
                limit: SOURCE_REGISTRY_MAX_DESCRIPTORS,
                actual,
            }
        }
        RegistryBoundError::AggregateTooLarge { actual } => LibraryError::SourceRegistryTooLarge {
            limit: SOURCE_REGISTRY_MAX_BYTES,
            actual,
        },
        RegistryBoundError::DescriptorTooLarge { index, actual } => {
            LibraryError::SourceRegistryFieldTooLarge {
                index,
                field: "descriptor",
                limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
                actual,
            }
        }
        RegistryBoundError::FieldTooLarge {
            index,
            field,
            actual,
        } => LibraryError::SourceRegistryFieldTooLarge {
            index,
            field,
            limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
            actual,
        },
        RegistryBoundError::SourceIdTooLarge { index, actual } => {
            LibraryError::SourceRegistrySourceIdTooLarge {
                index,
                limit: SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES,
                actual,
            }
        }
        RegistryBoundError::ArithmeticOverflow => LibraryError::SourceRegistryTooLarge {
            limit: SOURCE_REGISTRY_MAX_BYTES,
            actual: SOURCE_REGISTRY_MAX_BYTES.saturating_add(1),
        },
        RegistryBoundError::Serialization => LibraryError::SourceRegistryMalformed {
            field: "source_registry_json",
        },
    }
}

type RawSourceRow = (
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<Vec<u8>>,
);

fn parse_source_row(index: usize, row: RawSourceRow) -> Result<SampleSource, LibraryError> {
    let id = decode_source_text(
        row.0,
        row.1,
        index,
        "id",
        SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES,
    )?;
    let root = decode_source_text(
        row.2,
        row.3,
        index,
        "root",
        SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
    )?;
    let role = decode_source_text(
        row.4,
        row.5,
        index,
        "role",
        SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
    )?;
    let metadata_storage = decode_source_text(
        row.6,
        row.7,
        index,
        "metadata_storage",
        SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
    )?;
    let primary_import_folder = decode_source_text(
        row.8,
        row.9,
        index,
        "primary_import_folder",
        SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
    )?;
    Ok(normalized_source(SampleSource {
        id: SourceId::from_string(id),
        root: PathBuf::from(root),
        role: SourceRole::from_stored(&role),
        metadata_storage: SourceMetadataStorage::from_stored(&metadata_storage),
        primary_import_folder: primary_import_folder_path(primary_import_folder),
    }))
}

fn decode_source_text(
    actual: Option<i64>,
    bytes: Option<Vec<u8>>,
    index: usize,
    field: &'static str,
    limit: usize,
) -> Result<String, LibraryError> {
    let actual = actual
        .and_then(|value| usize::try_from(value).ok())
        .ok_or(LibraryError::SourceRegistryMalformed { field })?;
    if actual > limit {
        if field == "id" {
            return Err(LibraryError::SourceRegistrySourceIdTooLarge {
                index,
                limit,
                actual,
            });
        }
        return Err(LibraryError::SourceRegistryFieldTooLarge {
            index,
            field,
            limit,
            actual,
        });
    }
    let bytes = bytes.ok_or(LibraryError::SourceRegistryMalformed { field })?;
    String::from_utf8(bytes).map_err(|_| LibraryError::SourceRegistryMalformed { field })
}

fn validate_known_source_mappings(mappings: &[KnownSourceMapping]) -> Result<(), LibraryError> {
    if mappings.len() > SOURCE_REGISTRY_MAX_DESCRIPTORS {
        return Err(LibraryError::SourceRegistryTooManyDescriptors {
            limit: SOURCE_REGISTRY_MAX_DESCRIPTORS,
            actual: mappings.len(),
        });
    }
    let mut aggregate = 0usize;
    for (index, mapping) in mappings.iter().enumerate() {
        let _encoded = bounded_json_at_limit(mapping, SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES)
            .map_err(|error| match error {
                RegistryBoundError::AggregateTooLarge { actual } => {
                    LibraryError::SourceRegistryFieldTooLarge {
                        index,
                        field: "descriptor",
                        limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
                        actual,
                    }
                }
                other => map_registry_bound_error(other),
            })?;
        let source_id = mapping.source_id.len();
        if source_id > SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES {
            return Err(LibraryError::SourceRegistrySourceIdTooLarge {
                index,
                limit: SOURCE_REGISTRY_MAX_SOURCE_ID_BYTES,
                actual: source_id,
            });
        }
        let fields = [
            ("root", mapping.root.len()),
            ("role", mapping.role.as_deref().unwrap_or_default().len()),
            (
                "metadata_storage",
                mapping
                    .metadata_storage
                    .as_deref()
                    .unwrap_or_default()
                    .len(),
            ),
            (
                "primary_import_folder",
                mapping
                    .primary_import_folder
                    .as_deref()
                    .unwrap_or_default()
                    .len(),
            ),
        ];
        for (field, actual) in fields {
            if actual > SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES {
                return Err(LibraryError::SourceRegistryFieldTooLarge {
                    index,
                    field,
                    limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
                    actual,
                });
            }
        }
        let descriptor = source_id
            .checked_add(mapping.root.len())
            .and_then(|value| value.checked_add(mapping.role.as_deref().unwrap_or_default().len()))
            .and_then(|value| {
                value.checked_add(
                    mapping
                        .metadata_storage
                        .as_deref()
                        .unwrap_or_default()
                        .len(),
                )
            })
            .and_then(|value| {
                value.checked_add(
                    mapping
                        .primary_import_folder
                        .as_deref()
                        .unwrap_or_default()
                        .len(),
                )
            })
            .and_then(|value| value.checked_add(128))
            .ok_or(LibraryError::SourceRegistryTooLarge {
                limit: SOURCE_REGISTRY_MAX_BYTES,
                actual: SOURCE_REGISTRY_MAX_BYTES.saturating_add(1),
            })?;
        if descriptor > SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES {
            return Err(LibraryError::SourceRegistryFieldTooLarge {
                index,
                field: "descriptor",
                limit: SOURCE_REGISTRY_MAX_DESCRIPTOR_BYTES,
                actual: descriptor,
            });
        }
        aggregate =
            aggregate
                .checked_add(descriptor)
                .ok_or(LibraryError::SourceRegistryTooLarge {
                    limit: SOURCE_REGISTRY_MAX_BYTES,
                    actual: SOURCE_REGISTRY_MAX_BYTES.saturating_add(1),
                })?;
        if aggregate > SOURCE_REGISTRY_MAX_BYTES {
            return Err(LibraryError::SourceRegistryTooLarge {
                limit: SOURCE_REGISTRY_MAX_BYTES,
                actual: aggregate,
            });
        }
    }
    Ok(())
}

pub(super) fn normalized_source(mut source: SampleSource) -> SampleSource {
    if source.role == SourceRole::Protected {
        source.metadata_storage = SourceMetadataStorage::AppData;
    }
    if source.role == SourceRole::Primary {
        source.metadata_storage = SourceMetadataStorage::SourceFolder;
    }
    source.primary_import_folder =
        primary_import_folder_path(source.primary_import_folder.to_string_lossy().to_string());
    source
}

fn primary_import_folder_path(value: String) -> PathBuf {
    let path = PathBuf::from(value.trim());
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return default_primary_import_folder();
    }
    path
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KnownSourceMapping {
    root: String,
    source_id: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    metadata_storage: Option<String>,
    #[serde(default)]
    primary_import_folder: Option<String>,
}

impl KnownSourceMapping {
    fn try_into_sample_source(self) -> Result<SampleSource, LibraryError> {
        let root = PathBuf::from(&self.root);
        let role = match self.role.as_deref() {
            None | Some("normal") => SourceRole::Normal,
            Some("protected") => SourceRole::Protected,
            Some("primary") => SourceRole::Primary,
            Some(value) => {
                return Err(LibraryError::InvalidRetainedSourceDescriptor {
                    root,
                    field: "role",
                    value: value.to_string(),
                });
            }
        };
        let metadata_storage = match self.metadata_storage.as_deref() {
            None | Some("source_folder") => SourceMetadataStorage::SourceFolder,
            Some("app_data") => SourceMetadataStorage::AppData,
            Some(value) => {
                return Err(LibraryError::InvalidRetainedSourceDescriptor {
                    root,
                    field: "metadata_storage",
                    value: value.to_string(),
                });
            }
        };
        Ok(normalized_source(SampleSource {
            id: SourceId::from_string(self.source_id),
            root,
            role,
            metadata_storage,
            primary_import_folder: primary_import_folder_path(
                self.primary_import_folder
                    .unwrap_or_else(|| default_primary_import_folder().to_string_lossy().into()),
            ),
        }))
    }
}

fn upsert_known_source_mapping(mappings: &mut Vec<KnownSourceMapping>, source: &SampleSource) {
    let normalized = normalize_path(&source.root);
    let root = normalized.to_string_lossy().to_string();
    if let Some(existing) = mappings.iter_mut().find(|entry| entry.root == root) {
        existing.source_id = source.id.as_str().to_string();
        existing.role = Some(source.role.as_str().to_string());
        existing.metadata_storage = Some(source.metadata_storage.as_str().to_string());
        existing.primary_import_folder =
            Some(source.primary_import_folder.to_string_lossy().to_string());
        return;
    }
    mappings.push(KnownSourceMapping {
        root,
        source_id: source.id.as_str().to_string(),
        role: Some(source.role.as_str().to_string()),
        metadata_storage: Some(source.metadata_storage.as_str().to_string()),
        primary_import_folder: Some(source.primary_import_folder.to_string_lossy().to_string()),
    });
}
