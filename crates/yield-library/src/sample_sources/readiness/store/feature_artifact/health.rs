//! Bounded health and exact owned rebuild admission; ambiguous provenance is never erased.
use super::super::super::{FeatureArtifactRebuildAdmission, ReadinessTarget};
use super::*;
use crate::sample_sources::db::SourceReferenceFence;

pub(in crate::sample_sources::readiness::store) enum Health {
    Missing,
    Healthy(FeatureArtifactKey, FeatureArtifactPayload),
    CorruptOwned,
    Ambiguous,
}

pub(in crate::sample_sources::readiness::store) fn health(
    connection: &Connection,
    target: &ReadinessTarget,
) -> Result<Health, ReadinessError> {
    let probe = FeatureArtifactKey::for_payload(
        target,
        &FeatureArtifactPayload::from_v1_vector(&[0.0; 183])?,
    )?;
    let row=connection.query_row("SELECT
        CASE WHEN typeof(artifact_ref)='text' AND length(CAST(artifact_ref AS BLOB))<=160 THEN artifact_ref END,
        CASE WHEN typeof(payload)='blob' AND length(payload)=732 THEN payload END,
        CASE WHEN typeof(digest)='blob' AND length(digest)=32 THEN digest END,
        CASE WHEN typeof(claim_generation)='integer' AND claim_generation BETWEEN 1 AND 4294967295 THEN claim_generation END
        FROM source_feature_artifacts WHERE target_key=?1",[&probe.target_key],|r|Ok((
            r.get::<_,Option<String>>(0)?,r.get::<_,Option<Vec<u8>>>(1)?,r.get::<_,Option<Vec<u8>>>(2)?,r.get::<_,Option<i64>>(3)?))).optional()?;
    let Some((reference, bytes, digest, generation)) = row else {
        return Ok(Health::Missing);
    };
    let prefix = format!("source-feature-v1:{}:", probe.target_key);
    let known = reference
        .as_deref()
        .and_then(|r| r.strip_prefix(&prefix))
        .is_some_and(|suffix| {
            suffix.len() == 64
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
    if !known || generation.is_none() {
        return Ok(Health::Ambiguous);
    }
    let Some(payload) = bytes.and_then(FeatureArtifactPayload::decode) else {
        return Ok(Health::CorruptOwned);
    };
    let key = FeatureArtifactKey::for_payload(target, &payload)?;
    if reference.as_deref() == Some(key.reference())
        && digest.as_deref() == Some(key.digest.as_slice())
    {
        Ok(Health::Healthy(key, payload))
    } else {
        Ok(Health::CorruptOwned)
    }
}

pub(in crate::sample_sources::readiness::store) fn marker_matches(
    connection: &Connection,
    key: &FeatureArtifactKey,
) -> Result<bool, ReadinessError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM source_readiness_artifacts WHERE source_id=?1
        AND scope_kind='file' AND scope_id=?2 AND stage='analysis_features' AND artifact_version=?3
        AND content_generation=?4 AND artifact_ref=?5)",
        params![
            key.target.source_id,
            key.target.scope_id,
            key.target.required_version,
            key.target.content_generation,
            key.reference
        ],
        |r| r.get(0),
    )?)
}

fn exact_target(connection: &Connection, target: &ReadinessTarget) -> Result<bool, ReadinessError> {
    let exact:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM source_readiness_targets t
        JOIN source_readiness_sources s ON s.source_id=t.source_id JOIN wav_files w ON w.path=t.relative_path
        WHERE t.source_id=?1 AND t.scope_kind='file' AND t.scope_id=?2 AND t.stage='analysis_features'
        AND t.required_version=?3 AND t.content_generation=?4 AND t.source_generation=?5
        AND t.relative_path=?6 AND t.eligibility='eligible' AND s.availability='active'
        AND s.source_generation=?5 AND s.contract_version='yield-audited-feature-matrix-v1'
        AND w.missing=0 AND w.file_identity=t.scope_id AND w.content_hash=t.content_generation)",
        params![target.source_id,target.scope_id,target.required_version,target.content_generation,
            target.source_generation,target.relative_path],|r|r.get(0))?;
    Ok(exact)
}

pub(in crate::sample_sources::readiness::store) fn admit(
    connection: &mut Connection,
    ownership: &ReadinessOwnership<'_>,
    fence: &SourceReferenceFence,
    target: &ReadinessTarget,
    now: i64,
    cancel: &AtomicBool,
) -> Result<FeatureArtifactRebuildAdmission, ReadinessError> {
    ownership.validate_publication_fence_identity(Some(fence.publication_fence()))?;
    ownership.validate(connection)?;
    fence
        .validate()
        .map_err(|_| ReadinessError::InvalidFeatureArtifact("rebuild source fence changed"))?;
    if cancel.load(Ordering::Acquire) {
        return Err(ReadinessError::Cancelled);
    }
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ownership.validate(&tx)?;
    super::super::persistence::validate_audit(&tx, fence)?;
    let probe = FeatureArtifactKey::for_payload(
        target,
        &FeatureArtifactPayload::from_v1_vector(&[0.0; 183])?,
    )?;
    if !exact_target(&tx, target)? {
        return Ok(FeatureArtifactRebuildAdmission::RejectedStale);
    }
    let current_health = health(&tx, target)?;
    if let Health::Healthy(ref key, _) = current_health {
        if marker_matches(&tx, key)? {
            return Ok(FeatureArtifactRebuildAdmission::Ready);
        }
    }
    if matches!(current_health, Health::Ambiguous) {
        return Ok(FeatureArtifactRebuildAdmission::Ambiguous);
    }
    // Preserve an active exact job and unknown/malformed ownership-control state.
    let mut statement=tx.prepare("SELECT CASE WHEN typeof(status)='text' AND length(CAST(status AS BLOB))<=16 THEN status END,
        typeof(lease_expires_at),CASE WHEN typeof(lease_expires_at)='integer' THEN lease_expires_at END,
        CASE WHEN typeof(readiness_claim_generation)='integer' AND readiness_claim_generation BETWEEN 0 AND 4294967294 THEN readiness_claim_generation END
        FROM analysis_jobs WHERE sample_id=?1 AND job_type='readiness_analysis_features_v1' LIMIT 2")?;
    // Inspect the actual unique upsert slot, including legacy or other-version jobs. Never
    // overwrite another version/content or a legacy owner's evidence as a repair side effect.
    let mut rows = statement.query(params![target.scope_id])?;
    if let Some(row) = rows.next()? {
        let status: Option<String> = row.get(0)?;
        let lease_type: String = row.get(1)?;
        let lease: Option<i64> = row.get(2)?;
        let generation: Option<i64> = row.get(3)?;
        let Some(status) = status else {
            return Ok(FeatureArtifactRebuildAdmission::Ambiguous);
        };
        if generation.is_none()
            || !matches!(status.as_str(), "pending" | "running" | "failed" | "done")
        {
            return Ok(FeatureArtifactRebuildAdmission::Ambiguous);
        }
        let matches:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM analysis_jobs WHERE sample_id=?1 AND job_type='readiness_analysis_features_v1'
            AND source_id=?2 AND readiness_managed=1 AND readiness_scope_kind='file'
            AND readiness_scope_id=?1 AND readiness_stage='analysis_features' AND artifact_version=?3 AND content_generation=?4)",
            params![target.scope_id,target.source_id,target.required_version,target.content_generation],|r|r.get(0))?;
        if !matches {
            return Ok(if matches!(current_health, Health::Missing) {
                FeatureArtifactRebuildAdmission::Unchanged
            } else {
                FeatureArtifactRebuildAdmission::Ambiguous
            });
        }
        if status == "running" {
            if lease_type != "integer" {
                return Ok(FeatureArtifactRebuildAdmission::Ambiguous);
            }
            if lease.is_some_and(|until| until > now) {
                return Ok(FeatureArtifactRebuildAdmission::Deferred);
            }
        }
        if rows.next()?.is_some() {
            return Ok(FeatureArtifactRebuildAdmission::Ambiguous);
        }
        // Failed/retry work retains its policy. It has no finished payload to rebuild yet.
        if status == "failed" || (matches!(current_health, Health::Missing) && status == "pending")
        {
            return Ok(FeatureArtifactRebuildAdmission::Unchanged);
        }
    }
    drop(rows);
    drop(statement);
    let removed = matches!(current_health, Health::CorruptOwned);
    if removed {
        let before = tx.total_changes();
        let removed_rows = tx.execute(
            "DELETE FROM source_feature_artifacts WHERE target_key=?1",
            [&probe.target_key],
        )?;
        if removed_rows != 1 || tx.total_changes().checked_sub(before) != Some(1) {
            return Err(ReadinessError::InvalidFeatureArtifact(
                "corrupt removal has unconfirmed SQL effects",
            ));
        }
    }
    tx.execute("DELETE FROM source_readiness_artifacts WHERE source_id=?1 AND scope_kind='file'
        AND scope_id=?2 AND stage='analysis_features' AND artifact_version=?3 AND content_generation=?4",
        params![target.source_id,target.scope_id,target.required_version,target.content_generation])?;
    // Reuse the existing exact-target queue contract in the same transaction, not a new writer.
    super::super::persistence::persist_feature_rebuild_deficit(&tx, target, now)?;
    ownership.validate(&tx)?;
    fence
        .validate()
        .map_err(|_| ReadinessError::InvalidFeatureArtifact("rebuild source fence changed"))?;
    if cancel.load(Ordering::Acquire) {
        return Err(ReadinessError::Cancelled);
    }
    super::super::persistence::validate_audit(&tx, fence)?;
    if !exact_target(&tx, target)? {
        return Ok(FeatureArtifactRebuildAdmission::RejectedStale);
    }
    if removed && !matches!(health(&tx, target)?, Health::Missing) {
        return Err(ReadinessError::InvalidFeatureArtifact(
            "corrupt removal was not retained",
        ));
    }
    tx.commit()?;
    Ok(FeatureArtifactRebuildAdmission::Admitted {
        removed_corrupt_payload: removed,
    })
}
