//! Bounded complete Harvest content observations through the retained global connection.
use super::{LibraryError, reference_participant::ReferenceFileKey};
use rusqlite::{Connection, params, types::ValueRef};

const FILE_COLUMNS: &str = "source_id,relative_path,file_size,modified_ns,content_hash,harvest_state,discovered_at,seen_at,touched_at,done_at,ignored_at,note";
const EDGE_COLUMNS: &str = "id,parent_source_id,parent_relative_path,parent_file_size,parent_modified_ns,parent_content_hash,child_source_id,child_relative_path,child_file_size,child_modified_ns,child_content_hash,operation,source_range_start,source_range_end,output_duration_seconds,destination_folder,inherited_rating,inherited_tags_json,inherited_playback_type,tool_version,created_at";
const FILE_TYPES: &[u8] = b"TTiitTIiiiit";
const EDGE_TYPES: &[u8] = b"ITTiitTTiitTrrrtiTtTI";
const ENDPOINT: &str = "(parent_source_id=?1 AND parent_relative_path=?2) OR (child_source_id=?1 AND child_relative_path=?2)";

/// Content CAS observation, bound to both exact keys; never ownership or completion authority.
#[derive(Clone, Debug)]
pub struct ReferenceSnapshot {
    pub(super) old: ReferenceFileKey,
    pub(super) new: ReferenceFileKey,
    pub(super) identity: [u8; 32],
    pub(super) missing: bool,
    pub(super) occupied: bool,
}
impl ReferenceSnapshot {
    /// Versioned hash of complete bounded old state/edges and destination occupancy.
    pub fn identity(&self) -> &[u8; 32] {
        &self.identity
    }
}

pub(super) fn read(
    connection: &Connection,
    old: &ReferenceFileKey,
    new: &ReferenceFileKey,
) -> Result<ReferenceSnapshot, LibraryError> {
    let args = params![old.source, old.path.as_str()];
    let files: i64 = connection.query_row("SELECT COUNT(*) FROM (SELECT 1 FROM harvest_files WHERE source_id=?1 AND relative_path=?2 LIMIT 2)", args, |r|r.get(0))?;
    let edges: i64 = connection.query_row(
        &format!(
            "SELECT COUNT(*) FROM (SELECT 1 FROM harvest_derivations WHERE {ENDPOINT} LIMIT 65)"
        ),
        args,
        |r| r.get(0),
    )?;
    if files > 1 || edges > 64 {
        return Err(LibraryError::ReferenceSnapshotInvalid);
    }
    // Inspect scalar SQLite type/byte-length predicates before selecting field values.
    // This keeps an oversized field out of the materialized read, even on corrupt tables.
    for (table, columns, types, predicate) in [
        (
            "harvest_files",
            FILE_COLUMNS,
            FILE_TYPES,
            "source_id=?1 AND relative_path=?2",
        ),
        ("harvest_derivations", EDGE_COLUMNS, EDGE_TYPES, ENDPOINT),
    ] {
        let checks = columns
            .split(',')
            .zip(types)
            .map(|(column, kind)| {
                let shape = match kind.to_ascii_uppercase() {
                    b'T' => {
                        format!("typeof({column})='text' AND length(CAST({column} AS BLOB))<=4096")
                    }
                    b'I' => format!("typeof({column})='integer'"),
                    b'R' => format!("typeof({column}) IN ('integer','real')"),
                    _ => unreachable!(),
                };
                if kind.is_ascii_lowercase() {
                    format!("({column} IS NULL OR ({shape}))")
                } else {
                    format!("({shape})")
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ");
        let invalid: bool = connection.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE ({predicate}) AND NOT ({checks}))"),
            args,
            |r| r.get(0),
        )?;
        if invalid {
            return Err(LibraryError::ReferenceSnapshotInvalid);
        }
    }
    let occupied: bool = connection.query_row(&format!("SELECT EXISTS(SELECT 1 FROM harvest_files WHERE source_id=?1 AND relative_path=?2) OR EXISTS(SELECT 1 FROM harvest_derivations WHERE {ENDPOINT})"), params![new.source,new.path.as_str()], |r|r.get(0))?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"yield.harvest-reference.snapshot.v1\0");
    hash.update(&files.to_le_bytes());
    hash.update(&edges.to_le_bytes());
    hash.update(&[u8::from(occupied)]);
    let mut budget = 17usize;
    for (sql, types) in [
        (
            format!(
                "SELECT {FILE_COLUMNS} FROM harvest_files WHERE source_id=?1 AND relative_path=?2 LIMIT 2"
            ),
            FILE_TYPES,
        ),
        (
            format!(
                "SELECT {EDGE_COLUMNS} FROM harvest_derivations WHERE {ENDPOINT} ORDER BY id LIMIT 65"
            ),
            EDGE_TYPES,
        ),
    ] {
        let mut statement = connection.prepare(&sql)?;
        let mut rows = statement.query(args)?;
        while let Some(row) = rows.next()? {
            for (i, kind) in types.iter().enumerate() {
                // ValueRef borrows SQLite memory: check type/length/aggregate before any Rust
                // field allocation. The fingerprint streams exact SQLite typed values.
                let value = row.get_ref(i)?;
                let len = match value {
                    ValueRef::Null if kind.is_ascii_lowercase() => 0,
                    ValueRef::Integer(_) if *kind == b'I' || *kind == b'i' => 8,
                    ValueRef::Real(v) if *kind == b'r' && v.is_finite() => 8,
                    ValueRef::Integer(_) if *kind == b'r' => 8,
                    ValueRef::Text(bytes) if *kind == b'T' || *kind == b't' => {
                        if bytes.len() > 4096 || std::str::from_utf8(bytes).is_err() {
                            return Err(LibraryError::ReferenceSnapshotInvalid);
                        }
                        bytes.len()
                    }
                    _ => return Err(LibraryError::ReferenceSnapshotInvalid),
                };
                budget = budget
                    .checked_add(9 + len)
                    .ok_or(LibraryError::ReferenceSnapshotInvalid)?;
                if budget > 65536 {
                    return Err(LibraryError::ReferenceSnapshotInvalid);
                }
                let tag = match value {
                    ValueRef::Null => 0,
                    ValueRef::Integer(_) => 1,
                    ValueRef::Real(_) => 2,
                    ValueRef::Text(_) => 3,
                    _ => unreachable!(),
                };
                hash.update(&[tag]);
                hash.update(&(len as u64).to_le_bytes());
                match value {
                    ValueRef::Integer(v) => {
                        hash.update(&v.to_le_bytes());
                    }
                    ValueRef::Real(v) => {
                        hash.update(&v.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(v) => {
                        hash.update(v);
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(ReferenceSnapshot {
        old: old.clone(),
        new: new.clone(),
        identity: *hash.finalize().as_bytes(),
        missing: files == 0 && edges == 0,
        occupied,
    })
}
