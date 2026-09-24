//! Bounded read-only projection of persisted similarity layout points.

use std::collections::{HashMap, HashSet};

use rusqlite::{OptionalExtension, params_from_iter};
use thiserror::Error;

use super::{SourceDatabase, SourceDbError};
use crate::sample_sources::SourceId;

/// Maximum number of visible samples in one layout read.
pub const STARMAP_LAYOUT_MAX_SAMPLES: usize = 20_000;
/// Maximum combined UTF-8 bytes in source, model, version, and sample identities.
pub const STARMAP_LAYOUT_MAX_REQUEST_BYTES: usize = 32 * 1024 * 1024;
const QUERY_CHUNK_SIZE: usize = 256;

/// One visible browser file and its persisted analysis identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StarmapLayoutSample {
    /// Browser-owned file identity returned with the layout point.
    pub file_id: String,
    /// Source-local analysis identity, in `source_id::relative_path` form.
    pub sample_id: String,
}

/// A source-revision-scoped background read request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StarmapLayoutRequest {
    /// Opaque browser generation; the caller must compare it on completion.
    pub signature: u64,
    /// Source that owns every requested sample identity.
    pub source_id: SourceId,
    /// Source lifecycle generation represented by the browser rows.
    pub lifecycle_generation: u64,
    /// Source revision represented by the browser rows.
    pub expected_source_revision: u64,
    /// Exact persisted similarity model identifier.
    pub model_id: String,
    /// Exact persisted UMAP layout version.
    pub umap_version: String,
    /// Browser rows whose layout points are requested.
    pub samples: Vec<StarmapLayoutSample>,
}

/// A normalized point inside the reference Starmap domain.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StarmapLayoutPoint {
    /// Horizontal position in the range 0.04–0.96.
    pub x: f32,
    /// Vertical position in the range 0.06–0.94.
    pub y: f32,
    /// Optional cluster grouping from the matching model and layout version.
    pub cluster_id: Option<i32>,
}

/// One read result. A stale or missing layout is never represented as current points.
#[derive(Clone, Debug, PartialEq)]
pub enum StarmapLayoutLoad {
    /// The requested source revision matched and at least one point was found.
    /// Analysis readiness must still be checked by the caller.
    Current {
        /// Browser generation copied from the request.
        signature: u64,
        /// Source identity copied from the request.
        source_id: SourceId,
        /// Source lifecycle generation copied from the request.
        lifecycle_generation: u64,
        /// Source revision observed in the same read transaction as the points.
        source_revision: u64,
        /// Points keyed by browser-owned file identity.
        points: HashMap<String, StarmapLayoutPoint>,
        /// Requested browser files that have no matching persisted layout point.
        missing_count: usize,
    },
    /// No matching layout rows exist for these samples/model/version.
    Missing {
        /// Browser generation copied from the request.
        signature: u64,
        /// Source identity copied from the request.
        source_id: SourceId,
        /// Source lifecycle generation copied from the request.
        lifecycle_generation: u64,
        /// Source revision observed in the same read transaction.
        source_revision: u64,
    },
    /// The browser's source revision was superseded before the read.
    Stale {
        /// Browser generation copied from the request.
        signature: u64,
        /// Source identity copied from the request.
        source_id: SourceId,
        /// Source lifecycle generation copied from the request.
        lifecycle_generation: u64,
        /// Source revision observed in the read transaction.
        source_revision: u64,
    },
}

/// Fail-closed errors for malformed requests or unreadable layout data.
#[derive(Debug, Error)]
pub enum StarmapLayoutError {
    /// The source database could not be read.
    #[error(transparent)]
    Database(#[from] SourceDbError),
    /// The request exceeded the bounded read contract.
    #[error("starmap request contains more than {STARMAP_LAYOUT_MAX_SAMPLES} samples")]
    TooManySamples,
    /// The request identities exceeded the bounded read contract.
    #[error("starmap request identities exceed {STARMAP_LAYOUT_MAX_REQUEST_BYTES} bytes")]
    TooManyIdentityBytes,
    /// The request contains a missing or mismatched identity.
    #[error("starmap request has an invalid model, layout, source, or sample identity")]
    InvalidIdentity,
    /// A requested layout row contains a coordinate that cannot be shown truthfully.
    #[error("starmap layout has a nonfinite coordinate")]
    NonfiniteCoordinate,
}

impl SourceDatabase {
    /// Read persisted Starmap positions through an already-open background read connection.
    ///
    /// Open this database with `SourceDatabaseConnectionRole::BackgroundRead` on a worker.
    /// The revision check and point rows share one SQLite read transaction. The returned
    /// signature still needs comparison with the browser's current generation before display.
    pub fn load_starmap_layout(
        &self,
        request: &StarmapLayoutRequest,
    ) -> Result<StarmapLayoutLoad, StarmapLayoutError> {
        validate_request(request)?;
        let transaction = self
            .connection
            .unchecked_transaction()
            .map_err(SourceDbError::from)?;
        let source_revision = transaction
            .query_row(
                "SELECT value FROM metadata WHERE key = 'revision'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(SourceDbError::from)?
            .map(|raw| raw.parse::<u64>().map_err(|_| SourceDbError::Unexpected))
            .transpose()?
            .unwrap_or_default();
        if source_revision != request.expected_source_revision {
            transaction.rollback().map_err(SourceDbError::from)?;
            return Ok(StarmapLayoutLoad::Stale {
                signature: request.signature,
                source_id: request.source_id.clone(),
                lifecycle_generation: request.lifecycle_generation,
                source_revision,
            });
        }

        let mut raw_points = HashMap::new();
        let file_by_sample = request
            .samples
            .iter()
            .map(|sample| (sample.sample_id.as_str(), sample.file_id.as_str()))
            .collect::<HashMap<_, _>>();
        for chunk in request.samples.chunks(QUERY_CHUNK_SIZE) {
            let mut query = String::from(
                "SELECT layout_umap.sample_id, layout_umap.x, layout_umap.y, hdbscan_clusters.cluster_id
                 FROM layout_umap
                 LEFT JOIN hdbscan_clusters
                   ON layout_umap.sample_id = hdbscan_clusters.sample_id
                  AND hdbscan_clusters.model_id = ?1
                  AND hdbscan_clusters.method = 'umap'
                  AND hdbscan_clusters.umap_version = ?2
                 WHERE layout_umap.model_id = ?1
                   AND layout_umap.umap_version = ?2
                   AND layout_umap.sample_id IN (",
            );
            query.push_str(
                &std::iter::repeat_n("?", chunk.len())
                    .collect::<Vec<_>>()
                    .join(","),
            );
            query.push(')');
            let mut params = Vec::with_capacity(chunk.len() + 2);
            params.push(request.model_id.as_str());
            params.push(request.umap_version.as_str());
            params.extend(chunk.iter().map(|sample| sample.sample_id.as_str()));
            let mut statement = transaction.prepare(&query).map_err(SourceDbError::from)?;
            let rows = statement
                .query_map(params_from_iter(params), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, f32>(1)?,
                        row.get::<_, f32>(2)?,
                        row.get::<_, Option<i32>>(3)?,
                    ))
                })
                .map_err(SourceDbError::from)?;
            for row in rows {
                let (sample_id, x, y, cluster_id) = row.map_err(SourceDbError::from)?;
                let Some(file_id) = file_by_sample.get(sample_id.as_str()) else {
                    continue;
                };
                if !x.is_finite() || !y.is_finite() {
                    return Err(StarmapLayoutError::NonfiniteCoordinate);
                }
                raw_points.insert((*file_id).to_owned(), (x, y, cluster_id));
            }
        }
        transaction.rollback().map_err(SourceDbError::from)?;
        if raw_points.is_empty() {
            return Ok(StarmapLayoutLoad::Missing {
                signature: request.signature,
                source_id: request.source_id.clone(),
                lifecycle_generation: request.lifecycle_generation,
                source_revision,
            });
        }
        Ok(StarmapLayoutLoad::Current {
            signature: request.signature,
            source_id: request.source_id.clone(),
            lifecycle_generation: request.lifecycle_generation,
            source_revision,
            missing_count: request.samples.len().saturating_sub(raw_points.len()),
            points: normalize_points(raw_points),
        })
    }
}

fn validate_request(request: &StarmapLayoutRequest) -> Result<(), StarmapLayoutError> {
    if request.samples.len() > STARMAP_LAYOUT_MAX_SAMPLES {
        return Err(StarmapLayoutError::TooManySamples);
    }
    let mut identity_bytes = request
        .source_id
        .as_str()
        .len()
        .saturating_add(request.model_id.len())
        .saturating_add(request.umap_version.len());
    if identity_bytes > STARMAP_LAYOUT_MAX_REQUEST_BYTES {
        return Err(StarmapLayoutError::TooManyIdentityBytes);
    }
    for sample in &request.samples {
        identity_bytes = identity_bytes
            .saturating_add(sample.file_id.len())
            .saturating_add(sample.sample_id.len());
        if identity_bytes > STARMAP_LAYOUT_MAX_REQUEST_BYTES {
            return Err(StarmapLayoutError::TooManyIdentityBytes);
        }
    }
    if request.source_id.as_str().is_empty()
        || request.lifecycle_generation == 0
        || request.model_id.trim().is_empty()
        || request.umap_version.trim().is_empty()
    {
        return Err(StarmapLayoutError::InvalidIdentity);
    }
    let prefix = format!("{}::", request.source_id.as_str());
    let mut file_ids = HashSet::new();
    let mut sample_ids = HashSet::new();
    for sample in &request.samples {
        if sample.file_id.is_empty()
            || !sample.sample_id.starts_with(&prefix)
            || sample.sample_id.len() == prefix.len()
            || !file_ids.insert(&sample.file_id)
            || !sample_ids.insert(&sample.sample_id)
        {
            return Err(StarmapLayoutError::InvalidIdentity);
        }
    }
    Ok(())
}

fn normalize_points(
    raw: HashMap<String, (f32, f32, Option<i32>)>,
) -> HashMap<String, StarmapLayoutPoint> {
    let min_x = raw
        .values()
        .map(|(x, _, _)| *x)
        .fold(f32::INFINITY, f32::min);
    let max_x = raw
        .values()
        .map(|(x, _, _)| *x)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_y = raw
        .values()
        .map(|(_, y, _)| *y)
        .fold(f32::INFINITY, f32::min);
    let max_y = raw
        .values()
        .map(|(_, y, _)| *y)
        .fold(f32::NEG_INFINITY, f32::max);
    let center_x = (min_x + max_x) * 0.5;
    let center_y = (min_y + max_y) * 0.5;
    let units_per_normalized_unit = ((max_x - min_x) / 0.92).max((max_y - min_y) / 0.88);
    raw.into_iter()
        .map(|(file_id, (x, y, cluster_id))| {
            let (x, y) = if !units_per_normalized_unit.is_finite()
                || units_per_normalized_unit <= f32::EPSILON
            {
                (0.5, 0.5)
            } else {
                (
                    (0.5 + (x - center_x) / units_per_normalized_unit).clamp(0.04, 0.96),
                    (0.5 + (y - center_y) / units_per_normalized_unit).clamp(0.06, 0.94),
                )
            };
            (file_id, StarmapLayoutPoint { x, y, cluster_id })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sample_sources::db::{DB_FILE_NAME, SourceDatabaseConnectionRole};

    fn request(expected_source_revision: u64) -> StarmapLayoutRequest {
        StarmapLayoutRequest {
            signature: 41,
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 2,
            expected_source_revision,
            model_id: "model".to_owned(),
            umap_version: "layout-v1".to_owned(),
            samples: vec![
                StarmapLayoutSample {
                    file_id: "a.wav".to_owned(),
                    sample_id: "source::a.wav".to_owned(),
                },
                StarmapLayoutSample {
                    file_id: "b.wav".to_owned(),
                    sample_id: "source::b.wav".to_owned(),
                },
            ],
        }
    }

    #[test]
    fn seeded_background_read_returns_normalized_persisted_points() {
        let root = tempfile::tempdir().expect("source root");
        drop(SourceDatabase::open_for_source_write(root.path()).expect("create source db"));
        let connection =
            rusqlite::Connection::open(root.path().join(DB_FILE_NAME)).expect("seed connection");
        connection
            .execute_batch(
                "INSERT INTO samples (sample_id, content_hash, size, mtime_ns)
                 VALUES ('source::a.wav', 'hash-a', 1, 1),
                        ('source::b.wav', 'hash-b', 1, 1);
                 INSERT INTO layout_umap
                    (sample_id, model_id, umap_version, x, y, created_at)
                 VALUES
                    ('source::a.wav', 'model', 'layout-v1', 0, 0, 1),
                    ('source::b.wav', 'model', 'layout-v1', 10, 20, 1);
                 INSERT INTO hdbscan_clusters
                    (sample_id, model_id, method, umap_version, cluster_id, created_at)
                 VALUES ('source::a.wav', 'model', 'umap', 'layout-v1', 3, 1);",
            )
            .expect("seed layout");
        drop(connection);
        let reader = SourceDatabase::open_with_role(
            root.path(),
            SourceDatabaseConnectionRole::BackgroundRead,
        )
        .expect("background read");
        let revision = reader.get_revision().expect("source revision");
        let result = reader
            .load_starmap_layout(&request(revision))
            .expect("layout read");
        let StarmapLayoutLoad::Current {
            signature,
            source_id,
            lifecycle_generation,
            source_revision,
            points,
            missing_count,
        } = result
        else {
            panic!("expected current points, got {result:?}");
        };
        assert_eq!(signature, 41);
        assert_eq!(source_id.as_str(), "source");
        assert_eq!(lifecycle_generation, 2);
        assert_eq!(source_revision, revision);
        assert_eq!(points.len(), 2);
        assert_eq!(missing_count, 0);
        let a = points["a.wav"];
        let b = points["b.wav"];
        assert!((a.x - 0.28).abs() < 0.001);
        assert!((a.y - 0.06).abs() < 0.001);
        assert!((b.x - 0.72).abs() < 0.001);
        assert!((b.y - 0.94).abs() < 0.001);
        assert_eq!(a.cluster_id, Some(3));
        assert_eq!(b.cluster_id, None);
        let mut partial = request(revision);
        partial.samples.push(StarmapLayoutSample {
            file_id: "c.wav".to_owned(),
            sample_id: "source::c.wav".to_owned(),
        });
        assert!(matches!(
            reader
                .load_starmap_layout(&partial)
                .expect("partial layout"),
            StarmapLayoutLoad::Current {
                missing_count: 1,
                ..
            }
        ));
        assert!(matches!(
            reader
                .load_starmap_layout(&request(revision + 1))
                .expect("stale read"),
            StarmapLayoutLoad::Stale {
                signature: 41,
                source_revision,
                ..
            } if source_revision == revision
        ));
        let mut wrong_version = request(revision);
        wrong_version.umap_version = "other".to_owned();
        assert!(matches!(
            reader
                .load_starmap_layout(&wrong_version)
                .expect("missing layout"),
            StarmapLayoutLoad::Missing {
                signature: 41,
                source_revision,
                ..
            } if source_revision == revision
        ));
    }

    #[test]
    fn request_rejects_cross_source_and_duplicate_identities() {
        let mut invalid = request(0);
        invalid.samples[0].sample_id = "other::a.wav".to_owned();
        assert!(matches!(
            validate_request(&invalid),
            Err(StarmapLayoutError::InvalidIdentity)
        ));
        let mut duplicate = request(0);
        duplicate.samples[1].file_id = "a.wav".to_owned();
        assert!(matches!(
            validate_request(&duplicate),
            Err(StarmapLayoutError::InvalidIdentity)
        ));
        let mut oversized = request(0);
        oversized.samples = vec![
            StarmapLayoutSample {
                file_id: "a.wav".to_owned(),
                sample_id: "source::a.wav".to_owned(),
            };
            STARMAP_LAYOUT_MAX_SAMPLES + 1
        ];
        assert!(matches!(
            validate_request(&oversized),
            Err(StarmapLayoutError::TooManySamples)
        ));
    }

    #[test]
    fn request_rejects_unbounded_identity_bytes_before_reading() {
        let mut oversized = request(0);
        oversized.model_id = "m".repeat(STARMAP_LAYOUT_MAX_REQUEST_BYTES + 1);
        assert!(matches!(
            validate_request(&oversized),
            Err(StarmapLayoutError::TooManyIdentityBytes)
        ));
    }

    #[test]
    fn unreadable_coordinate_never_becomes_a_current_point() {
        let root = tempfile::tempdir().expect("source root");
        drop(SourceDatabase::open_for_source_write(root.path()).expect("create source db"));
        let connection =
            rusqlite::Connection::open(root.path().join(DB_FILE_NAME)).expect("seed connection");
        connection
            .execute(
                "INSERT INTO samples (sample_id, content_hash, size, mtime_ns)
                 VALUES ('source::a.wav', 'hash-a', 1, 1)",
                [],
            )
            .expect("seed sample");
        connection
            .execute(
                "INSERT INTO layout_umap
                    (sample_id, model_id, umap_version, x, y, created_at)
                 VALUES ('source::a.wav', 'model', 'layout-v1', ?1, 0, 1)",
                [f64::INFINITY],
            )
            .expect("seed invalid layout");
        drop(connection);
        let reader = SourceDatabase::open_with_role(
            root.path(),
            SourceDatabaseConnectionRole::BackgroundRead,
        )
        .expect("background read");
        let result =
            reader.load_starmap_layout(&request(reader.get_revision().expect("source revision")));
        assert!(matches!(
            result,
            Err(StarmapLayoutError::NonfiniteCoordinate)
        ));
    }

    #[test]
    fn normalization_centers_coincident_points() {
        let points = normalize_points(HashMap::from([
            ("a".to_owned(), (5.0, -2.0, None)),
            ("b".to_owned(), (5.0, -2.0, Some(1))),
        ]));
        assert_eq!(points["a"].x, 0.5);
        assert_eq!(points["a"].y, 0.5);
        assert_eq!(points["b"].cluster_id, Some(1));
    }

    #[test]
    fn background_result_can_cross_the_worker_boundary() {
        fn assert_send<T: Send>() {}
        assert_send::<StarmapLayoutLoad>();
        assert_send::<StarmapLayoutError>();
    }
}
