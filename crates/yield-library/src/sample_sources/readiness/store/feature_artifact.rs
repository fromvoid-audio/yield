//! Real source-owned feature payload publication and validated lookup.
mod health;
pub(super) use health::{Health, admit, health, marker_matches};

use super::super::{
    ArtifactPublishOutcome, ClaimedReadinessWork, FeatureArtifactKey, FeatureArtifactPayload,
    FeatureArtifactPublication, ReadinessError,
};
use crate::sample_sources::db::ReadinessOwnership;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS source_feature_artifacts (
    target_key TEXT PRIMARY KEY CHECK(length(target_key) = 64),
    artifact_ref TEXT NOT NULL UNIQUE CHECK(length(artifact_ref) <= 160),
    payload BLOB NOT NULL CHECK(length(payload) = 732),
    digest BLOB NOT NULL CHECK(length(digest) = 32),
    claim_generation INTEGER NOT NULL CHECK(claim_generation > 0)
) WITHOUT ROWID;";

fn current_target(
    connection: &Connection,
    key: &FeatureArtifactKey,
) -> Result<bool, ReadinessError> {
    Ok(connection.query_row(
        "SELECT EXISTS(
        SELECT 1 FROM source_readiness_targets t
        JOIN source_readiness_sources s ON s.source_id = t.source_id
        WHERE t.source_id = ?1 AND t.scope_kind = 'file' AND t.scope_id = ?2
        AND t.stage = 'analysis_features' AND t.required_version = ?3
        AND t.content_generation = ?4 AND t.eligibility = 'eligible'
        AND s.availability = 'active'
        AND (s.contract_version <> 'yield-audited-feature-matrix-v1' OR EXISTS(
            SELECT 1 FROM wav_files w WHERE w.path=t.relative_path AND w.missing=0
            AND w.file_identity=t.scope_id AND w.content_hash=t.content_generation)))",
        params![
            key.target.source_id,
            key.target.scope_id,
            key.target.required_version,
            key.target.content_generation
        ],
        |row| row.get(0),
    )?)
}

// Current file content is the dependency of a file-scoped feature artifact. A source revision
// bump alone (for example curation of a different file) must not throw away unchanged features.
pub(super) fn manifest_matches(
    connection: &Connection,
    target: &super::super::ReadinessTarget,
) -> Result<bool, ReadinessError> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM wav_files WHERE path=?1 AND missing=0 AND file_identity=?2 AND content_hash=?3)",
        params![target.relative_path,target.scope_id,target.content_generation],|row|row.get(0))?)
}

pub(super) fn publish(
    connection: &mut Connection,
    ownership: &ReadinessOwnership<'_>,
    claim: &ClaimedReadinessWork,
    payload: &FeatureArtifactPayload,
    now: i64,
    cancel: &AtomicBool,
) -> Result<FeatureArtifactPublication, ReadinessError> {
    let key = FeatureArtifactKey::for_payload(claim.target(), payload)?;
    super::work::validate_claim_ownership(connection, ownership, claim)?;
    if cancel.load(Ordering::Acquire) {
        return Err(ReadinessError::Cancelled);
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    super::work::validate_claim_ownership(&tx, ownership, claim)?;
    if !current_target(&tx, &key)? {
        return Ok(FeatureArtifactPublication::RejectedStale);
    }
    let stored = tx
        .query_row(
            "SELECT substr(artifact_ref, 1, 161), substr(payload, 1, 733), substr(digest, 1, 33), claim_generation
        FROM source_feature_artifacts WHERE target_key = ?1",
            [&key.target_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((reference, bytes, digest, generation)) = stored {
        if reference != key.reference || bytes != payload.bytes() || digest != key.digest {
            return Err(ReadinessError::InvalidFeatureArtifact(
                "immutable payload conflict",
            ));
        }
        let exact: bool = tx.query_row(
            "SELECT EXISTS(
            SELECT 1 FROM analysis_jobs j JOIN source_readiness_artifacts a
            ON a.source_id = j.source_id AND a.scope_kind = j.readiness_scope_kind
            AND a.scope_id = j.readiness_scope_id AND a.stage = j.readiness_stage
            WHERE j.source_id = ?1 AND j.readiness_scope_id = ?2
            AND j.readiness_stage = 'analysis_features' AND j.readiness_managed = 1
            AND j.artifact_version = ?3 AND j.content_generation = ?4
            AND j.status = 'done' AND j.readiness_claim_generation = ?5
            AND a.artifact_ref = ?6 AND a.artifact_version = ?3 AND a.content_generation = ?4)",
            params![
                key.target.source_id,
                key.target.scope_id,
                key.target.required_version,
                key.target.content_generation,
                claim.claim_generation(),
                key.reference
            ],
            |r| r.get(0),
        )?;
        if generation != i64::from(claim.claim_generation()) || !exact {
            let outcome = super::work::complete_readiness_work_in_transaction(
                &tx,
                ownership,
                claim,
                now,
                Some(key.reference()),
            )?;
            if outcome != ArtifactPublishOutcome::Recorded {
                return Ok(FeatureArtifactPublication::RejectedStale);
            }
            tx.execute(
                "UPDATE source_feature_artifacts SET claim_generation=?2 WHERE target_key=?1",
                params![key.target_key, claim.claim_generation()],
            )?;
            super::work::validate_claim_ownership(&tx, ownership, claim)?;
            if cancel.load(Ordering::Acquire) {
                return Err(ReadinessError::Cancelled);
            }
            if !current_target(&tx, &key)? {
                return Ok(FeatureArtifactPublication::RejectedStale);
            }
            tx.commit()?;
            return Ok(FeatureArtifactPublication::Recorded(key));
        }
        super::work::validate_claim_ownership(&tx, ownership, claim)?;
        if cancel.load(Ordering::Acquire) {
            return Err(ReadinessError::Cancelled);
        }
        if !current_target(&tx, &key)? {
            return Ok(FeatureArtifactPublication::RejectedStale);
        }
        tx.commit()?;
        return Ok(FeatureArtifactPublication::Duplicate(key));
    }
    let count: i64 = tx.query_row("SELECT count(*) FROM source_feature_artifacts", [], |r| {
        r.get(0)
    })?;
    if count >= 4096 {
        return Err(ReadinessError::FeatureArtifactCapacity);
    }
    let outcome = super::work::complete_readiness_work_in_transaction(
        &tx,
        ownership,
        claim,
        now,
        Some(key.reference()),
    )?;
    if outcome != ArtifactPublishOutcome::Recorded {
        return Ok(FeatureArtifactPublication::RejectedStale);
    }
    tx.execute(
        "INSERT INTO source_feature_artifacts VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            key.target_key,
            key.reference,
            payload.bytes(),
            key.digest.as_slice(),
            claim.claim_generation()
        ],
    )?;
    super::work::validate_claim_ownership(&tx, ownership, claim)?;
    if cancel.load(Ordering::Acquire) {
        return Err(ReadinessError::Cancelled);
    }
    if !current_target(&tx, &key)? {
        return Ok(FeatureArtifactPublication::RejectedStale);
    }
    tx.commit()?;
    Ok(FeatureArtifactPublication::Recorded(key))
}

pub(super) fn lookup(
    connection: &Connection,
    ownership: &ReadinessOwnership<'_>,
    key: &FeatureArtifactKey,
) -> Result<Option<FeatureArtifactPayload>, ReadinessError> {
    ownership.validate_artifact_read_only(connection)?;
    if !current_target(connection, key)? {
        return Ok(None);
    }
    let row = connection.query_row("SELECT payload, digest FROM source_feature_artifacts
        WHERE target_key = ?1 AND artifact_ref = ?2 AND length(payload) = 732 AND length(digest) = 32", params![key.target_key, key.reference],
        |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))).optional()?;
    let payload = row.and_then(|(bytes, digest)| {
        (digest == key.digest && *blake3::hash(&bytes).as_bytes() == key.digest)
            .then(|| FeatureArtifactPayload::decode(bytes))
            .flatten()
    });
    ownership.validate_artifact_read_only(connection)?;
    Ok(payload)
}

pub(super) fn lookup_target(
    connection: &Connection,
    ownership: &ReadinessOwnership<'_>,
    target: &super::super::ReadinessTarget,
) -> Result<Option<(FeatureArtifactKey, FeatureArtifactPayload)>, ReadinessError> {
    let seed = FeatureArtifactPayload::from_v1_vector(&[0.0; 183])?;
    let probe = FeatureArtifactKey::for_payload(target, &seed)?;
    ownership.validate_artifact_read_only(connection)?;
    if !current_target(connection, &probe)? {
        return Ok(None);
    }
    let result = match health(connection, target)? {
        Health::Healthy(key, payload) if marker_matches(connection, &key)? => Some((key, payload)),
        _ => None,
    };
    ownership.validate_artifact_read_only(connection)?;
    Ok(result)
}

#[cfg(any(test, feature = "test-support"))]
pub(super) fn inject_fixture_fault(
    connection: &Connection,
    ownership: &ReadinessOwnership<'_>,
    target: &super::super::ReadinessTarget,
    fault: super::super::FeatureArtifactFixtureFault,
) -> Result<(), ReadinessError> {
    ownership.validate(connection)?;
    use super::super::FeatureArtifactFixtureFault;
    let key = FeatureArtifactKey::for_payload(
        target,
        &FeatureArtifactPayload::from_v1_vector(&[0.0; 183])?,
    )?;
    if !current_target(connection, &key)? {
        return Err(ReadinessError::InvalidFeatureArtifact(
            "fixture target changed",
        ));
    }
    match fault {
        FeatureArtifactFixtureFault::MissingPayload => {
            connection.execute(
                "DELETE FROM source_feature_artifacts WHERE target_key=?1",
                [&key.target_key],
            )?;
        }
        FeatureArtifactFixtureFault::CorruptPayload => {
            connection.execute(
                "UPDATE source_feature_artifacts SET payload=zeroblob(732) WHERE target_key=?1",
                [&key.target_key],
            )?;
        }
        FeatureArtifactFixtureFault::MissingMarker => {
            connection.execute("DELETE FROM source_readiness_artifacts WHERE source_id=?1 AND scope_kind='file' AND scope_id=?2 AND stage='analysis_features' AND artifact_version=?3 AND content_generation=?4", params![target.source_id,target.scope_id,target.required_version,target.content_generation])?;
        }
    }
    Ok(())
}
