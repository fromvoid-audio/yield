use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{SourceDbError, SourceWriterError, util::map_sql_error};

/// Maximum retained frame indices in each persistent transient result.
pub const TRANSIENT_CACHE_MAX_MARKERS: usize = 2_048;
const MAX_PAYLOAD_BYTES: usize = 4 * (TRANSIENT_CACHE_MAX_MARKERS + 1);
const MAX_ENTRIES: usize = 128;

/// Complete, strictly ordered frame positions for one analyzed input.
#[derive(Debug, PartialEq, Eq)]
pub struct CachedTransientMarkers {
    source_frames: u32,
    frames: Vec<u32>,
}

impl CachedTransientMarkers {
    /// Validate positions before they can be written to the Source cache.
    pub fn new(source_frames: u32, frames: Vec<u32>) -> Option<Self> {
        if source_frames == 0
            || frames.len() > TRANSIENT_CACHE_MAX_MARKERS
            || frames.last().is_some_and(|&frame| frame >= source_frames)
            || frames.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return None;
        }
        Some(Self {
            source_frames,
            frames,
        })
    }

    /// Number of native frames in the analyzed input.
    pub fn source_frames(&self) -> u32 {
        self.source_frames
    }

    /// Ordered zero-based source frames.
    pub fn frames(&self) -> &[u32] {
        &self.frames
    }

    /// Move the payload's frame positions without copying.
    pub fn into_frames(self) -> Vec<u32> {
        self.frames
    }

    fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(4 * (self.frames.len() + 1));
        bytes.extend_from_slice(&self.source_frames.to_le_bytes());
        for frame in &self.frames {
            bytes.extend_from_slice(&frame.to_le_bytes());
        }
        bytes
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_PAYLOAD_BYTES || !bytes.len().is_multiple_of(4) {
            return None;
        }
        let mut words = bytes.chunks_exact(4).map(|chunk| {
            // Every element produced by chunks_exact has exactly four bytes.
            u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])
        });
        Self::new(words.next()?, words.collect())
    }
}

#[derive(Clone, Copy)]
pub(super) struct DetectorConfig<'a> {
    version: &'a str,
    preset: &'a str,
}

impl<'a> DetectorConfig<'a> {
    pub(super) fn new(version: &'a str, preset: &'a str) -> Result<Self, SourceWriterError> {
        let valid_key = |key: &str, limit| {
            !key.is_empty()
                && key.len() <= limit
                && key
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        };
        if !valid_key(version, 64) || !valid_key(preset, 32) {
            return Err(SourceWriterError::InvalidRequest(
                "invalid transient cache key",
            ));
        }
        Ok(Self { version, preset })
    }
}

pub(super) fn lookup(
    connection: &Connection,
    content: &str,
    config: DetectorConfig<'_>,
) -> Result<Option<CachedTransientMarkers>, SourceDbError> {
    let stored = connection
        .query_row(
            "SELECT substr(payload, 1, ?4), substr(digest, 1, 33)
             FROM source_transient_cache WHERE content_hash = ?1 AND detector_version = ?2 AND preset = ?3",
            params![content, config.version, config.preset, MAX_PAYLOAD_BYTES + 1],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()
        .map_err(map_sql_error)?;
    Ok(stored.and_then(|(bytes, digest)| {
        if payload_digest(content, config, &bytes)
            .as_bytes()
            .as_slice()
            != digest
        {
            return None;
        }
        CachedTransientMarkers::decode(&bytes)
    }))
}

pub(super) fn store(
    transaction: &Transaction<'_>,
    content: &str,
    config: DetectorConfig<'_>,
    markers: &CachedTransientMarkers,
) -> Result<(), SourceDbError> {
    let payload = markers.encode();
    let digest = payload_digest(content, config, &payload);
    transaction
        .execute(
            "INSERT OR REPLACE INTO source_transient_cache
             (content_hash, detector_version, preset, payload, digest) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                content,
                config.version,
                config.preset,
                payload,
                digest.as_bytes().as_slice()
            ],
        )
        .map_err(map_sql_error)?;
    transaction
        .execute(
            "DELETE FROM source_transient_cache WHERE insertion_order <
             (SELECT insertion_order FROM source_transient_cache
              ORDER BY insertion_order DESC LIMIT 1 OFFSET ?1)",
            [MAX_ENTRIES - 1],
        )
        .map_err(map_sql_error)?;
    Ok(())
}

fn payload_digest(content: &str, config: DetectorConfig<'_>, payload: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for part in [content, config.version, config.preset] {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    hasher.update(payload).finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_requires_complete_sorted_in_bounds_frames() {
        let empty = CachedTransientMarkers::new(48_000, Vec::new()).unwrap();
        assert_eq!(CachedTransientMarkers::decode(&empty.encode()), Some(empty));
        let markers = CachedTransientMarkers::new(48_000, vec![0, 17, 47_999]).unwrap();
        assert_eq!(
            CachedTransientMarkers::decode(&markers.encode()),
            Some(markers)
        );
        for frames in [
            vec![4, 3],
            vec![1, 1],
            vec![48_000],
            vec![0; TRANSIENT_CACHE_MAX_MARKERS + 1],
        ] {
            assert!(CachedTransientMarkers::new(48_000, frames).is_none());
        }
        assert!(CachedTransientMarkers::new(0, Vec::new()).is_none());
        assert!(CachedTransientMarkers::decode(&[0; 5]).is_none());
    }

    #[test]
    fn cache_prunes_old_payloads_and_rejects_corrupt_digest() {
        let mut connection = Connection::open_in_memory().unwrap();
        super::super::schema::apply_schema_fast(&connection).unwrap();
        let config = DetectorConfig::new("detector-v1", "normal").unwrap();
        let markers = CachedTransientMarkers::new(100, vec![24, 81]).unwrap();
        let transaction = connection.transaction().unwrap();
        for id in 0..=MAX_ENTRIES {
            store(&transaction, &format!("{id:064x}"), config, &markers).unwrap();
        }
        transaction.commit().unwrap();
        assert!(
            lookup(&connection, &format!("{:064x}", 0), config)
                .unwrap()
                .is_none()
        );
        let newest = format!("{MAX_ENTRIES:064x}");
        assert_eq!(lookup(&connection, &newest, config).unwrap(), Some(markers));
        let count: usize = connection
            .query_row("SELECT count(*) FROM source_transient_cache", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, MAX_ENTRIES);
        connection
            .execute(
                "UPDATE source_transient_cache SET digest = zeroblob(32) WHERE content_hash = ?1",
                [&newest],
            )
            .unwrap();
        assert!(lookup(&connection, &newest, config).unwrap().is_none());
    }
}
