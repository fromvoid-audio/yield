//! Canonical original rows using bounded scalar references and borrowed text, no SQLite sort.
use super::{LegacyObservationError, Result, refused};
use rusqlite::{Connection, Statement, StatementStatus, types::ValueRef};
use std::{cmp::Ordering, path::PathBuf, time::Instant};

const MAX_METADATA_ROWS: usize = 128;
const MAX_AUDIO_ROWS: usize = 1024;
const MAX_PATH_BYTES: usize = 4096;
const METADATA_SQL: &str = "SELECT rowid, CASE WHEN length(CAST(key AS BLOB))<=4096 THEN key END, CASE WHEN length(CAST(value AS BLOB))<=65536 THEN value END FROM metadata INDEXED BY sqlite_autoindex_metadata_1 ORDER BY key COLLATE BINARY LIMIT 129";
const AUDIO_SQL: &str = "SELECT rowid, CASE WHEN length(CAST(path AS BLOB))<=4096 THEN path END, file_size, modified_ns FROM wav_files INDEXED BY sqlite_autoindex_wav_files_1 ORDER BY path COLLATE BINARY LIMIT 1025";
const METADATA_PLAN: &str = "EXPLAIN QUERY PLAN SELECT rowid, CASE WHEN length(CAST(key AS BLOB))<=4096 THEN key END, CASE WHEN length(CAST(value AS BLOB))<=65536 THEN value END FROM metadata INDEXED BY sqlite_autoindex_metadata_1 ORDER BY key COLLATE BINARY LIMIT 129";
const AUDIO_PLAN: &str = "EXPLAIN QUERY PLAN SELECT rowid, CASE WHEN length(CAST(path AS BLOB))<=4096 THEN path END, file_size, modified_ns FROM wav_files INDEXED BY sqlite_autoindex_wav_files_1 ORDER BY path COLLATE BINARY LIMIT 1025";
const METADATA_LOOKUP: &str = "SELECT CASE WHEN length(CAST(key AS BLOB))<=4096 THEN key END, CASE WHEN length(CAST(value AS BLOB))<=65536 THEN value END FROM metadata WHERE rowid=?1";
const AUDIO_LOOKUP: &str = "SELECT CASE WHEN length(CAST(path AS BLOB))<=4096 THEN path END, file_size, modified_ns FROM wav_files WHERE rowid=?1";
const METADATA_LOOKUP_PLAN: &str = "EXPLAIN QUERY PLAN SELECT CASE WHEN length(CAST(key AS BLOB))<=4096 THEN key END, CASE WHEN length(CAST(value AS BLOB))<=65536 THEN value END FROM metadata WHERE rowid=?1";
const AUDIO_LOOKUP_PLAN: &str = "EXPLAIN QUERY PLAN SELECT CASE WHEN length(CAST(path AS BLOB))<=4096 THEN path END, file_size, modified_ns FROM wav_files WHERE rowid=?1";
type AudioReference = (i64, i64, i64); // rowid, size, modified; no owned path.

// Actual fixed scalar capacity plus transient original path validation. Include conservative
// Vec and three prepared-statement headers even though those headers live on the stack.
// Ordinary prepare uses static SQL, an empty named-parameter cache and no statement-cache key;
// positional integer bindings add no Rust heap backing in pinned rusqlite 0.37.
// Native SQLite statement/text/cache allocations and process RSS are separate.
pub(crate) const SCRATCH_BYTES: usize = MAX_AUDIO_ROWS * std::mem::size_of::<AudioReference>()
    + MAX_PATH_BYTES
    + std::mem::size_of::<Vec<AudioReference>>()
    + 3 * std::mem::size_of::<Statement<'static>>();
// Schema capture finishes and drops its bounded strings/sort backing BEFORE rows begin.
const _: () = assert!(
    5 * (16 + 128 + 128 + 4096)
        + (8 + 5) * std::mem::size_of::<(String, String, String, Option<String>)>()
        <= SCRATCH_BYTES
);
const _: () = assert!(
    MAX_METADATA_ROWS * std::mem::size_of::<i64>()
        + std::mem::size_of::<Vec<i64>>()
        + 3 * std::mem::size_of::<Statement<'static>>()
        + 20
        <= SCRATCH_BYTES
);

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Rows {
    pub revision: u64,
    pub metadata_rows: usize,
    pub audio_rows: usize,
    pub digest: [u8; 32],
}
fn check_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        Err(LegacyObservationError("legacy row deadline elapsed"))
    } else {
        Ok(())
    }
}
fn text(value: ValueRef<'_>) -> Result<&str> {
    match value {
        ValueRef::Text(bytes) => std::str::from_utf8(bytes).map_err(refused),
        _ => Err(LegacyObservationError("legacy row is not valid UTF-8 text")),
    }
}
fn no_sort(statement: &Statement<'_>) -> Result<()> {
    if statement.get_status(StatementStatus::Sort) != 0 {
        Err(LegacyObservationError(
            "legacy statement used a SQLite sorter",
        ))
    } else {
        Ok(())
    }
}
fn verify_plan(connection: &Connection, sql: &str, expected: &str) -> Result<()> {
    let mut statement = connection.prepare(sql).map_err(refused)?;
    let mut rows = match statement.parameter_count() {
        0 => statement.query([]),
        1 => statement.query([0_i64]),
        _ => return Err(LegacyObservationError("unexpected legacy plan parameters")),
    }
    .map_err(refused)?;
    let row = rows
        .next()
        .map_err(refused)?
        .ok_or(LegacyObservationError("missing legacy index plan"))?;
    if text(row.get_ref(3).map_err(refused)?)? != expected
        || rows.next().map_err(refused)?.is_some()
    {
        return Err(LegacyObservationError(
            "legacy plan is not the exact non-sorting scan or lookup",
        ));
    }
    drop(rows);
    no_sort(&statement)
}
fn count(connection: &Connection, sql: &str, maximum: usize) -> Result<usize> {
    let mut statement = connection.prepare(sql).map_err(refused)?;
    let n: i64 = statement.query_row([], |r| r.get(0)).map_err(refused)?;
    no_sort(&statement)?;
    let n = usize::try_from(n).map_err(refused)?;
    if n > maximum {
        return Err(LegacyObservationError("legacy row count bound"));
    }
    Ok(n)
}
fn hash_text(hash: &mut blake3::Hasher, value: &str) {
    hash.update(&(value.len() as u64).to_le_bytes());
    hash.update(value.as_bytes());
}

// In-place, fallible heapsort: bounded stack and no comparator error swallowed or allocation.
fn sort_references<T>(
    items: &mut [T],
    mut compare: impl FnMut(&T, &T) -> Result<Ordering>,
) -> Result<()> {
    fn sift<T>(
        items: &mut [T],
        mut root: usize,
        end: usize,
        compare: &mut impl FnMut(&T, &T) -> Result<Ordering>,
    ) -> Result<()> {
        while root < end / 2 {
            let mut child = root * 2 + 1;
            if child + 1 < end && compare(&items[child], &items[child + 1])? == Ordering::Less {
                child += 1;
            }
            if compare(&items[root], &items[child])? != Ordering::Less {
                break;
            }
            items.swap(root, child);
            root = child;
        }
        Ok(())
    }
    for root in (0..items.len() / 2).rev() {
        sift(items, root, items.len(), &mut compare)?;
    }
    for end in (1..items.len()).rev() {
        items.swap(0, end);
        sift(items, 0, end, &mut compare)?;
    }
    Ok(())
}
fn compare_text(
    left: &mut Statement<'_>,
    right: &mut Statement<'_>,
    a: i64,
    b: i64,
    deadline: Instant,
) -> Result<Ordering> {
    check_deadline(deadline)?;
    let mut l = left.query([a]).map_err(refused)?;
    let mut r = right.query([b]).map_err(refused)?;
    let a = l
        .next()
        .map_err(refused)?
        .ok_or(LegacyObservationError("missing original legacy rowid"))?;
    let b = r
        .next()
        .map_err(refused)?
        .ok_or(LegacyObservationError("missing original legacy rowid"))?;
    let order = text(a.get_ref(0).map_err(refused)?)?.cmp(text(b.get_ref(0).map_err(refused)?)?);
    if l.next().map_err(refused)?.is_some() || r.next().map_err(refused)?.is_some() {
        return Err(LegacyObservationError("ambiguous original legacy rowid"));
    }
    drop(l);
    drop(r);
    no_sort(left)?;
    no_sort(right)?;
    Ok(order)
}
pub(super) fn capture(connection: &Connection, deadline: Instant) -> Result<Rows> {
    check_deadline(deadline)?;
    // Caller proved exact compiled rowid v11 DDL in this SAME original read transaction.
    // BINARY index order differs across UTF-8/UTF-16: scan it, but canonicalize borrowed UTF-8
    // through scalar rowids rather than treating encoded order as historical Rust order.
    for (sql, expected) in [
        (
            METADATA_PLAN,
            "SCAN metadata USING INDEX sqlite_autoindex_metadata_1",
        ),
        (
            AUDIO_PLAN,
            "SCAN wav_files USING INDEX sqlite_autoindex_wav_files_1",
        ),
        (
            METADATA_LOOKUP_PLAN,
            "SEARCH metadata USING INTEGER PRIMARY KEY (rowid=?)",
        ),
        (
            AUDIO_LOOKUP_PLAN,
            "SEARCH wav_files USING INTEGER PRIMARY KEY (rowid=?)",
        ),
    ] {
        verify_plan(connection, sql, expected)?;
    }
    let metadata_rows = count(
        connection,
        "SELECT COUNT(*) FROM (SELECT 1 FROM metadata NOT INDEXED LIMIT 129)",
        MAX_METADATA_ROWS,
    )?;
    let audio_rows = count(
        connection,
        "SELECT COUNT(*) FROM (SELECT 1 FROM wav_files NOT INDEXED LIMIT 1025)",
        MAX_AUDIO_ROWS,
    )?;
    let mut hash = blake3::Hasher::new();
    hash.update(b"legacy-metadata");
    hash.update(&(metadata_rows as u64).to_le_bytes());
    let revision = metadata(connection, metadata_rows, deadline, &mut hash)?;
    hash.update(b"legacy-audio");
    hash.update(&(audio_rows as u64).to_le_bytes());
    audio(connection, audio_rows, deadline, &mut hash)?;
    check_deadline(deadline)?;
    Ok(Rows {
        revision,
        metadata_rows,
        audio_rows,
        digest: *hash.finalize().as_bytes(),
    })
}
fn metadata(
    connection: &Connection,
    expected: usize,
    deadline: Instant,
    hash: &mut blake3::Hasher,
) -> Result<u64> {
    let mut references = Vec::with_capacity(MAX_METADATA_ROWS);
    {
        let mut statement = connection.prepare(METADATA_SQL).map_err(refused)?;
        let mut rows = statement.query([]).map_err(refused)?;
        let mut bytes = 0usize;
        while let Some(row) = rows.next().map_err(refused)? {
            check_deadline(deadline)?;
            let id: i64 = row.get(0).map_err(refused)?;
            let key = text(row.get_ref(1).map_err(refused)?)?;
            let value = text(row.get_ref(2).map_err(refused)?)?;
            bytes = bytes
                .checked_add(key.len())
                .and_then(|b| b.checked_add(value.len()))
                .and_then(|b| b.checked_add(std::mem::size_of::<(String, String)>()))
                .ok_or(LegacyObservationError("metadata byte overflow"))?;
            // Historical key guard is encoded SQL BLOB bytes, not UTF-8 key length.
            if references.len() == MAX_METADATA_ROWS
                || key.is_empty()
                || key == "source.legacy_adoption.v1"
                || bytes > 65536
                || references.contains(&id)
            {
                return Err(LegacyObservationError(
                    "legacy metadata byte/key/count/rowid bound",
                ));
            }
            references.push(id);
        }
        drop(rows);
        no_sort(&statement)?;
    }
    if references.len() != expected {
        return Err(LegacyObservationError(
            "legacy metadata index count differs",
        ));
    }
    let mut left = connection.prepare(METADATA_LOOKUP).map_err(refused)?;
    let mut right = connection.prepare(METADATA_LOOKUP).map_err(refused)?;
    sort_references(&mut references, |a, b| {
        compare_text(&mut left, &mut right, *a, *b, deadline)
    })?;
    let mut previous = None;
    let mut revision = None;
    for id in references {
        check_deadline(deadline)?;
        if let Some(previous) = previous {
            if compare_text(&mut left, &mut right, previous, id, deadline)? != Ordering::Less {
                return Err(LegacyObservationError("ambiguous legacy metadata"));
            }
        }
        let mut rows = left.query([id]).map_err(refused)?;
        let row = rows
            .next()
            .map_err(refused)?
            .ok_or(LegacyObservationError("missing original legacy metadata"))?;
        let key = text(row.get_ref(0).map_err(refused)?)?;
        let value = text(row.get_ref(1).map_err(refused)?)?;
        if key == "revision" {
            let parsed = value.parse::<u64>().map_err(refused)?;
            if parsed > i64::MAX as u64 || parsed.to_string() != value {
                return Err(LegacyObservationError("noncanonical legacy revision"));
            }
            revision = Some(parsed);
        }
        hash_text(hash, key);
        hash_text(hash, value);
        if rows.next().map_err(refused)?.is_some() {
            return Err(LegacyObservationError("ambiguous original legacy metadata"));
        }
        previous = Some(id);
    }
    no_sort(&left)?;
    no_sort(&right)?;
    revision.ok_or(LegacyObservationError("missing legacy revision"))
}
fn compare_audio(
    left: &mut Statement<'_>,
    right: &mut Statement<'_>,
    a: &AudioReference,
    b: &AudioReference,
    deadline: Instant,
) -> Result<Ordering> {
    Ok(compare_text(left, right, a.0, b.0, deadline)?.then_with(|| (a.1, a.2).cmp(&(b.1, b.2))))
}
fn audio(
    connection: &Connection,
    expected: usize,
    deadline: Instant,
    hash: &mut blake3::Hasher,
) -> Result<()> {
    let mut references: Vec<AudioReference> = Vec::with_capacity(MAX_AUDIO_ROWS);
    {
        let mut statement = connection.prepare(AUDIO_SQL).map_err(refused)?;
        let mut rows = statement.query([]).map_err(refused)?;
        let mut bytes = 0usize;
        while let Some(row) = rows.next().map_err(refused)? {
            check_deadline(deadline)?;
            let id: i64 = row.get(0).map_err(refused)?;
            let path = text(row.get_ref(1).map_err(refused)?)?;
            let size: i64 = row.get(2).map_err(refused)?;
            let modified: i64 = row.get(3).map_err(refused)?;
            bytes = bytes
                .checked_add(path.len())
                .and_then(|b| b.checked_add(std::mem::size_of::<(String, i64, i64)>()))
                .ok_or(LegacyObservationError("audio byte overflow"))?;
            if references.len() == MAX_AUDIO_ROWS
                || path.len() > MAX_PATH_BYTES
                || size < 0
                || bytes > 4 * 1024 * 1024
                || references.iter().any(|r| r.0 == id)
                || crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
                    PathBuf::from(path),
                )
                .is_err()
            {
                return Err(LegacyObservationError(
                    "legacy audio path/size/count/rowid bound",
                ));
            }
            references.push((id, size, modified));
        }
        drop(rows);
        no_sort(&statement)?;
    }
    if references.len() != expected {
        return Err(LegacyObservationError("legacy audio index count differs"));
    }
    let mut left = connection.prepare(AUDIO_LOOKUP).map_err(refused)?;
    let mut right = connection.prepare(AUDIO_LOOKUP).map_err(refused)?;
    sort_references(&mut references, |a, b| {
        compare_audio(&mut left, &mut right, a, b, deadline)
    })?;
    for (id, size, modified) in references {
        check_deadline(deadline)?;
        let mut rows = left.query([id]).map_err(refused)?;
        let row = rows
            .next()
            .map_err(refused)?
            .ok_or(LegacyObservationError("missing original legacy audio"))?;
        let path = text(row.get_ref(0).map_err(refused)?)?;
        if row.get::<_, i64>(1).map_err(refused)? != size
            || row.get::<_, i64>(2).map_err(refused)? != modified
        {
            return Err(LegacyObservationError("original legacy scalar row changed"));
        }
        hash_text(hash, path);
        hash.update(&size.to_le_bytes());
        hash.update(&modified.to_le_bytes());
        if rows.next().map_err(refused)?.is_some() {
            return Err(LegacyObservationError("ambiguous original legacy audio"));
        }
    }
    no_sort(&left)?;
    no_sort(&right)
}

#[cfg(test)]
mod tests;
