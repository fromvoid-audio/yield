//! Typed bounded lookahead and in-memory canonical ordering; no SQLite sort/temp files.
use super::{LegacyObservationError, Result, refused};
use rusqlite::Connection;
use std::path::PathBuf;

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Rows {
    pub revision: u64,
    pub metadata_rows: usize,
    pub audio_rows: usize,
    pub digest: [u8; 32],
}
pub(super) fn capture(connection: &Connection) -> Result<Rows> {
    let mut query = connection.prepare("SELECT CASE WHEN length(CAST(key AS BLOB))<=4096 THEN key END, CASE WHEN length(CAST(value AS BLOB))<=65536 THEN value END FROM metadata LIMIT 129").map_err(refused)?;
    let mut metadata = Vec::new();
    let captured = query
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(refused)?;
    let mut bytes = 0usize;
    for row in captured {
        let (key, value) = row.map_err(refused)?;
        if metadata.len() == 128 {
            return Err(LegacyObservationError("legacy metadata row bound"));
        }
        bytes = bytes
            .checked_add(key.len())
            .and_then(|b| b.checked_add(value.len()))
            .and_then(|b| b.checked_add(std::mem::size_of::<(String, String)>()))
            .ok_or(LegacyObservationError("metadata byte overflow"))?;
        if key.is_empty() || key == "source.legacy_adoption.v1" || bytes > 65536 {
            return Err(LegacyObservationError("legacy metadata byte/key bound"));
        }
        metadata.push((key, value));
    }
    metadata.sort();
    if metadata.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err(LegacyObservationError("ambiguous legacy metadata"));
    }
    let revision_text = metadata
        .iter()
        .find(|(k, _)| k == "revision")
        .ok_or(LegacyObservationError("missing legacy revision"))?
        .1
        .as_str();
    let revision = revision_text.parse::<u64>().map_err(refused)?;
    if revision > i64::MAX as u64 || revision.to_string() != revision_text {
        return Err(LegacyObservationError("noncanonical legacy revision"));
    }
    let mut query = connection.prepare("SELECT CASE WHEN length(CAST(path AS BLOB))<=4096 THEN path END, file_size, modified_ns FROM wav_files LIMIT 1025").map_err(refused)?;
    let mut audio = Vec::new();
    let captured = query
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })
        .map_err(refused)?;
    bytes = 0;
    for row in captured {
        let (path, size, modified) = row.map_err(refused)?;
        if audio.len() == 1024 {
            return Err(LegacyObservationError("legacy audio row bound"));
        }
        bytes = bytes
            .checked_add(path.len())
            .and_then(|b| b.checked_add(std::mem::size_of::<(String, i64, i64)>()))
            .ok_or(LegacyObservationError("audio byte overflow"))?;
        if size < 0
            || bytes > 4 * 1024 * 1024
            || path.len() > 4096
            || crate::sample_sources::reconciliation::RootRelativePath::try_from_path(
                PathBuf::from(&path),
            )
            .is_err()
        {
            return Err(LegacyObservationError("legacy audio path/size bound"));
        }
        audio.push((path, size, modified));
    }
    audio.sort();
    let mut digest = blake3::Hasher::new();
    digest.update(b"legacy-metadata");
    digest.update(&(metadata.len() as u64).to_le_bytes());
    for (key, value) in &metadata {
        for text in [key, value] {
            digest.update(&(text.len() as u64).to_le_bytes());
            digest.update(text.as_bytes());
        }
    }
    digest.update(b"legacy-audio");
    digest.update(&(audio.len() as u64).to_le_bytes());
    for (path, size, modified) in &audio {
        digest.update(&(path.len() as u64).to_le_bytes());
        digest.update(path.as_bytes());
        digest.update(&size.to_le_bytes());
        digest.update(&modified.to_le_bytes());
    }
    Ok(Rows {
        revision,
        metadata_rows: metadata.len(),
        audio_rows: audio.len(),
        digest: *digest.finalize().as_bytes(),
    })
}
