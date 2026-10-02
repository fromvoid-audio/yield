//! Exact inverse descriptors paired with publication; bounded supersession proof only.
use super::*;
use crate::sample_sources::db::SourceReferenceFence;
use crate::sample_sources::readiness::{ReadinessStage, ReadinessTarget};
use std::time::{Duration, Instant};

fn invalid() -> ReadinessError {
    ReadinessError::InvalidFeatureArtifact("reverse ownership conflict or unconfirmed effect")
}

pub(super) fn record(
    connection: &Connection,
    key: &FeatureArtifactKey,
) -> Result<(), ReadinessError> {
    let existing: Option<(String, String, String, String)> = connection.query_row(
        "SELECT substr(source_id,1,1025),substr(file_identity,1,1025),substr(content_generation,1,1025),substr(required_version,1,1025)
         FROM source_feature_reverse_owners WHERE target_key=?1", [&key.target_key],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)),
    ).optional()?;
    let wanted = (
        &key.target.source_id,
        &key.target.scope_id,
        &key.target.content_generation,
        &key.target.required_version,
    );
    if let Some(existing) = existing {
        if (&existing.0, &existing.1, &existing.2, &existing.3) != wanted {
            return Err(invalid());
        }
        return Ok(());
    }
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM (SELECT 1 FROM source_feature_reverse_owners LIMIT 4097)",
        [],
        |r| r.get(0),
    )?;
    if count >= 4096 {
        return Err(ReadinessError::FeatureArtifactCapacity);
    }
    let before = connection.total_changes();
    let inserted = connection.execute(
        "INSERT INTO source_feature_reverse_owners VALUES (?1,?2,?3,?4,?5)",
        params![key.target_key, wanted.0, wanted.1, wanted.2, wanted.3],
    )?;
    if inserted != 1 || connection.total_changes().checked_sub(before) != Some(1) {
        return Err(invalid());
    }
    // A trigger must not substitute a different descriptor after insertion.
    let exact: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM source_feature_reverse_owners WHERE target_key=?1 AND source_id=?2 AND file_identity=?3 AND content_generation=?4 AND required_version=?5)",
        params![key.target_key,wanted.0,wanted.1,wanted.2,wanted.3],|r|r.get(0))?;
    if !exact {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) struct Page {
    pub observed: u32,
    pub removed: u32,
    pub next: Option<String>,
}

struct Deadline<'a>(&'a Connection);
impl Drop for Deadline<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}
fn checkpoint(cancel: &AtomicBool, deadline: Instant) -> Result<(), ReadinessError> {
    if cancel.load(Ordering::Acquire) {
        return Err(ReadinessError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(ReadinessError::AdmissionBudget(
            "feature maintenance elapsed work",
        ));
    }
    Ok(())
}

pub(in crate::sample_sources::readiness::store) fn prune(
    connection: &mut Connection,
    ownership: &ReadinessOwnership<'_>,
    fence: &SourceReferenceFence,
    after: &str,
    limit: u32,
    cancel: &AtomicBool,
) -> Result<Page, ReadinessError> {
    if !(1..=64).contains(&limit) || after.len() > 64 {
        return Err(invalid());
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    checkpoint(cancel, deadline)?;
    ownership.validate(connection)?;
    ownership.validate_publication_fence_identity(Some(fence.publication_fence()))?;
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.progress_handler(1000, Some(move || Instant::now() >= deadline));
    let guard = Deadline(&tx);
    ownership.validate(&tx)?;
    super::super::persistence::validate_audit(&tx, fence)?;
    let source = fence.source_revision().source_id().as_str();
    let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM source_readiness_sources WHERE source_id=?1 AND availability='active' AND contract_version='yield-audited-feature-matrix-v1' AND typeof(source_generation)='integer' AND source_generation=?2 AND typeof(readiness_revision)='integer' AND readiness_revision>=0)",
        params![source,fence.source_revision().revision()],|r|r.get(0))?;
    if !active {
        return Err(invalid());
    }
    // Enumerate payload keys, including descriptor-less legacy evidence, with no caller key API.
    let keys = {
        let mut statement=tx.prepare("SELECT CASE WHEN typeof(target_key)='text' AND length(CAST(target_key AS BLOB))=64 THEN target_key END FROM source_feature_artifacts WHERE target_key>?1 ORDER BY target_key LIMIT ?2")?;
        let rows =
            statement.query_map(params![after, limit + 1], |r| r.get::<_, Option<String>>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut page = Page {
        observed: 0,
        removed: 0,
        next: None,
    };
    for key in keys.iter().take(limit as usize) {
        checkpoint(cancel, deadline)?;
        page.observed += 1;
        let Some(key) = key else {
            return Err(invalid());
        };
        if proven_superseded(&tx, source, key)? {
            // Disposable cleanup also emits WAL frames. It cannot bypass the existing
            // retained hard/unknown gate, and has no required/recovery exception permit.
            crate::sqlite_wal::enforce_wal_admission(&tx)?;
            let before = tx.total_changes();
            let removed = tx.execute(
                "DELETE FROM source_feature_artifacts WHERE target_key=?1",
                [key],
            )?;
            let inverse = tx.execute(
                "DELETE FROM source_feature_reverse_owners WHERE target_key=?1",
                [key],
            )?;
            let remaining:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM source_feature_artifacts WHERE target_key=?1) OR EXISTS(SELECT 1 FROM source_feature_reverse_owners WHERE target_key=?1)",[key],|r|r.get(0))?;
            if removed != 1
                || inverse != 1
                || tx.total_changes().checked_sub(before) != Some(2)
                || remaining
            {
                return Err(invalid());
            }
            page.removed += 1;
        }
    }
    if keys.len() > limit as usize {
        page.next = keys[limit as usize - 1].clone();
    }
    checkpoint(cancel, deadline)?;
    ownership.validate(&tx)?;
    super::super::persistence::validate_audit(&tx, fence)?;
    checkpoint(cancel, deadline)?;
    drop(guard);
    tx.commit()?;
    Ok(page)
}

fn proven_superseded(
    connection: &Connection,
    source: &str,
    target_key: &str,
) -> Result<bool, ReadinessError> {
    let descriptor:Option<(Option<String>,Option<String>,Option<String>)>=connection.query_row(
        "SELECT CASE WHEN typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512 THEN file_identity END,
         CASE WHEN typeof(content_generation)='text' AND length(CAST(content_generation AS BLOB))=64 THEN content_generation END,
         CASE WHEN typeof(required_version)='text' AND length(CAST(required_version AS BLOB)) BETWEEN 1 AND 1024 THEN required_version END
         FROM source_feature_reverse_owners WHERE target_key=?1 AND source_id=?2",params![target_key,source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((Some(identity), Some(content), Some(version))) = descriptor else {
        return Ok(false);
    };
    if !valid_hash(&content) || version != "features-v1" {
        return Ok(false);
    }
    let row:Option<(Option<String>,Option<Vec<u8>>,Option<Vec<u8>>)>=connection.query_row(
        "SELECT CASE WHEN typeof(artifact_ref)='text' AND length(CAST(artifact_ref AS BLOB))<=160 THEN artifact_ref END,
         CASE WHEN typeof(payload)='blob' AND length(payload)=732 THEN payload END,
         CASE WHEN typeof(digest)='blob' AND length(digest)=32 THEN digest END
         FROM source_feature_artifacts WHERE target_key=?1 AND typeof(claim_generation)='integer' AND claim_generation BETWEEN 1 AND 4294967295",[target_key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((Some(reference), Some(bytes), Some(digest))) = row else {
        return Ok(false);
    };
    let Some(payload) = FeatureArtifactPayload::decode(bytes) else {
        return Ok(false);
    };
    let target = ReadinessTarget::file(
        source,
        &identity,
        "",
        ReadinessStage::AnalysisFeatures,
        &version,
        0,
        &content,
    );
    let recomputed = FeatureArtifactKey::for_payload(&target, &payload)?;
    if recomputed.target_key != target_key
        || recomputed.reference != reference
        || recomputed.digest.as_slice() != digest
    {
        return Ok(false);
    }
    // Preserve running or malformed control state for this exact immutable artifact.
    let uncertain: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM analysis_jobs WHERE source_id=?1 AND readiness_scope_id=?2 AND (
          status='running' OR typeof(status)<>'text' OR status NOT IN ('pending','failed','done','cancelled')
          OR typeof(artifact_version)<>'text' OR length(CAST(artifact_version AS BLOB)) NOT BETWEEN 1 AND 1024
          OR typeof(content_generation)<>'text' OR length(CAST(content_generation AS BLOB))<>64 OR content_generation GLOB '*[^0-9a-f]*'
          OR typeof(readiness_managed)<>'integer' OR readiness_managed<>1))
        OR EXISTS(SELECT 1 FROM analysis_jobs WHERE sample_id=?2 AND job_type='readiness_analysis_features_v1' AND (
          status='running' OR typeof(status)<>'text' OR status NOT IN ('pending','failed','done','cancelled')
          OR typeof(source_id)<>'text' OR source_id<>?1 OR typeof(readiness_managed)<>'integer' OR readiness_managed<>1
          OR typeof(artifact_version)<>'text' OR length(CAST(artifact_version AS BLOB)) NOT BETWEEN 1 AND 1024
          OR typeof(content_generation)<>'text' OR length(CAST(content_generation AS BLOB))<>64 OR content_generation GLOB '*[^0-9a-f]*'))",
        params![source, identity], |r| r.get(0),
    )?;
    if uncertain {
        return Ok(false);
    }
    // Any accepted current alias preserves the bytes. Unknown/missing hashes never prove absence.
    let mut statement=connection.prepare("SELECT CASE WHEN missing=0 AND typeof(content_hash)='text' AND length(CAST(content_hash AS BLOB))=64 THEN content_hash END FROM wav_files WHERE file_identity=?1 ORDER BY path LIMIT 65")?;
    let mut rows = statement.query([&identity])?;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        let hash: Option<String> = row.get(0)?;
        if count > 64
            || hash
                .as_deref()
                .is_none_or(|hash| !valid_hash(hash) || hash == content)
        {
            return Ok(false);
        }
    }
    Ok(count > 0)
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
