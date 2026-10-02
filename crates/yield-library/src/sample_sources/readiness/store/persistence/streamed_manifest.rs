//! Owner-held, bounded-page manifest publication. SQL transaction rollback is the staging ledger.
use super::*;
use crate::sample_sources::db::{
    META_LAST_AUTHORITATIVE_AUDIT_REVISION, META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
    SourceReferenceFence,
};
use crate::sample_sources::reconciliation::RootRelativePath;
use std::{
    path::Path,
    time::{Duration, Instant},
};

const CONTRACT: &str = "yield-audited-feature-matrix-v1";
const PAGE_ROWS: usize = 64;
const PAGE_BYTES: usize = 128 * 1024;
const MAX_FILES: usize = 16_384;
const MAX_ELAPSED: Duration = Duration::from_secs(2);

fn checkpoint(cancel: &AtomicBool, deadline: Instant) -> Result<(), ReadinessError> {
    cancellation_checkpoint(Some(cancel))?;
    if Instant::now() >= deadline {
        return Err(ReadinessError::AdmissionBudget("elapsed work"));
    }
    Ok(())
}
struct SqlDeadline<'a>(&'a Connection);
impl<'a> SqlDeadline<'a> {
    fn install(connection: &'a Connection, deadline: Instant) -> Self {
        connection.progress_handler(1000, Some(move || Instant::now() >= deadline));
        Self(connection)
    }
}
impl Drop for SqlDeadline<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}

// Check both the externally retained owner's audit and the transaction's own metadata. The
// latter also catches corrupted triggers changing revision/audit inside this transaction.
pub(in crate::sample_sources::readiness::store) fn validate_audit(
    tx: &Connection,
    fence: &SourceReferenceFence,
) -> Result<(), ReadinessError> {
    fence
        .validate()
        .map_err(|_| invalid_persisted_value("audit", "retained audited owner changed"))?;
    let mut statement = tx.prepare("SELECT CASE WHEN typeof(key)='text' AND length(CAST(key AS BLOB))<=64 THEN key END, CASE WHEN typeof(value)='text' AND length(CAST(value AS BLOB)) BETWEEN 1 AND CASE WHEN key=?1 THEN 24576 ELSE 20 END THEN value END FROM metadata WHERE key IN (?1, ?2, 'revision') ORDER BY key LIMIT 4")?;
    let mut rows = statement.query(params![
        META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY,
        META_LAST_AUTHORITATIVE_AUDIT_REVISION
    ])?;
    let mut root = None;
    let mut audit = None;
    let mut revision = None;
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        let key: String = row
            .get::<_, Option<String>>(0)?
            .ok_or_else(|| invalid_persisted_value("audit", "invalid audit key"))?;
        let value: Option<String> = row.get(1)?;
        let slot = match key.as_str() {
            META_LAST_AUTHORITATIVE_AUDIT_ROOT_IDENTITY => &mut root,
            META_LAST_AUTHORITATIVE_AUDIT_REVISION => &mut audit,
            "revision" => &mut revision,
            _ => return Err(invalid_persisted_value("audit", "unknown audit field")),
        };
        if slot.is_some() || value.is_none() {
            return Err(invalid_persisted_value(
                "audit",
                "missing or ambiguous audit",
            ));
        }
        *slot = value;
    }
    let expected = fence.source_revision();
    let parse_revision = |value: Option<String>| value.and_then(|v| v.parse::<u64>().ok());
    let root: Vec<u8> = serde_json::from_str(&root.unwrap_or_default())
        .map_err(|_| invalid_persisted_value("audit", "invalid root identity"))?;
    if count != 3
        || root.as_slice() != expected.root_identity().as_bytes()
        || parse_revision(audit) != Some(expected.revision())
        || parse_revision(revision) != Some(expected.revision())
    {
        return Err(invalid_persisted_value(
            "audit",
            "source audit/revision changed",
        ));
    }
    Ok(())
}

fn layout(source: &str, generation: i64, membership: &ReadinessMembership) -> ReadinessTarget {
    ReadinessTarget::source(
        source,
        ReadinessStage::SimilarityLayout,
        "unconnected-layout-v1",
        generation,
        membership.generation(),
    )
    .with_eligibility(ReadinessEligibility::Unsupported)
}

/// Derive every target from the same retained audited manifest snapshot. No caller-supplied
/// target iterator may omit rows or mint index evidence. All visible writes commit together.
#[allow(clippy::too_many_arguments)]
pub(in crate::sample_sources::readiness::store) fn publish(
    connection: &mut Connection,
    ownership: &ReadinessOwnership<'_>,
    origin: &ReadinessPublicationOrigin,
    fence: &SourceReferenceFence,
    requested: &Path,
    now: i64,
    cancel: &AtomicBool,
    observe_page: &mut dyn FnMut(usize),
) -> Result<ReadinessTarget, ReadinessError> {
    let deadline = Instant::now() + MAX_ELAPSED;
    checkpoint(cancel, deadline)?;
    ownership.validate_publication_fence_identity(Some(fence.publication_fence()))?;
    validate_publication_ownership(connection, ownership, origin)?;
    let expected = fence.source_revision();
    let source = expected.source_id().as_str();
    let generation = i64::try_from(expected.revision())
        .map_err(|_| ReadinessError::AdmissionBudget("source generation"))?;
    validate_publication_text_bounds(source, CONTRACT, &[], &[])?;
    validate_contract_version(CONTRACT)?;
    let tx = connection.transaction()?;
    let sql_deadline = SqlDeadline::install(&tx, deadline);
    validate_publication_ownership(&tx, ownership, origin)?;
    validate_audit(&tx, fence)?;
    let prior = read_delta_source_state(&tx, source)?;
    let readiness_revision = if let Some((prior_generation, prior_revision, contract, _, _)) = prior
    {
        if contract != CONTRACT {
            return Err(invalid_persisted_value(
                "contract",
                "incompatible feature contract",
            ));
        }
        if prior_generation > generation {
            return Err(ReadinessError::StaleSourceGeneration {
                source_id: source.into(),
                attempted: generation,
                current: prior_generation,
            });
        }
        prior_revision
            .checked_add(1)
            .ok_or(ReadinessError::AdmissionBudget("readiness revision"))?
    } else {
        1
    };
    tx.execute(
        "DELETE FROM source_readiness_targets WHERE source_id=?1",
        [source],
    )?;
    let filter = crate::sample_sources::supported_audio_where_clause();
    let mut cursor: Option<String> = None;
    let mut all_membership = ReadinessMembership::default();
    let mut files = 0usize;
    let mut selected = None;
    loop {
        checkpoint(cancel, deadline)?;
        validate_audit(&tx, fence)?;
        // Each page is fully bounded before materialization. No full manifest/identity map.
        let manifest = {
            let mut statement = tx.prepare(&format!("SELECT
                CASE WHEN typeof(path)='text' AND length(CAST(path AS BLOB)) BETWEEN 1 AND 1024 THEN path END,
                CASE WHEN typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512 THEN file_identity END,
                CASE WHEN typeof(content_hash)='text' AND length(CAST(content_hash AS BLOB))=64 THEN content_hash END
                FROM wav_files WHERE missing=0 AND {filter} AND (?1 IS NULL OR path>?1) ORDER BY path LIMIT 64"))?;
            let mut rows = statement.query([cursor.as_deref()])?;
            let mut page = Vec::with_capacity(PAGE_ROWS);
            let mut bytes = 0usize;
            while let Some(row) = rows.next()? {
                checkpoint(cancel, deadline)?;
                let path: Option<String> = row.get(0)?;
                let identity: Option<String> = row.get(1)?;
                let hash: Option<String> = row.get(2)?;
                let (path, identity, hash) = path
                    .zip(identity)
                    .zip(hash)
                    .map(|((p, i), h)| (p, i, h))
                    .ok_or_else(|| {
                        invalid_persisted_value(
                            "manifest",
                            "identity/full hash unavailable or oversized",
                        )
                    })?;
                bytes += path.len() + identity.len() + hash.len();
                if page.len() >= PAGE_ROWS || bytes > PAGE_BYTES {
                    return Err(ReadinessError::AdmissionBudget("manifest page"));
                }
                if identity.trim().is_empty()
                    || !hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(invalid_persisted_value(
                        "manifest",
                        "invalid identity/full hash",
                    ));
                }
                RootRelativePath::try_from_path(path.clone().into())
                    .map_err(|_| invalid_persisted_value("manifest", "invalid relative path"))?;
                page.push((path, identity, hash));
            }
            page
        };
        if manifest.is_empty() {
            break;
        }
        files = files
            .checked_add(manifest.len())
            .filter(|n| *n <= MAX_FILES)
            .ok_or(ReadinessError::AdmissionBudget("manifest files"))?;
        cursor = manifest.last().map(|(path, _, _)| path.clone());
        // At most 193 targets are resident; the source target validates this page's complete
        // stage shape only and is not inserted until all manifest pages have been consumed.
        let mut targets = Vec::with_capacity(manifest.len() * 3 + 1);
        for (path, identity, hash) in manifest {
            checkpoint(cancel, deadline)?;
            all_membership.add(&identity, &hash);
            let wav = Path::new(&path)
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("wav"));
            for (stage, version) in [
                (ReadinessStage::IndexedIdentity, "audited-identity-v1"),
                (ReadinessStage::AnalysisFeatures, "features-v1"),
                (ReadinessStage::EmbeddingAspects, "unconnected-embedding-v1"),
            ] {
                let mut target = ReadinessTarget::file(
                    source, &identity, &path, stage, version, generation, &hash,
                );
                if stage == ReadinessStage::EmbeddingAspects
                    || (stage == ReadinessStage::AnalysisFeatures && !wav)
                {
                    target.eligibility = ReadinessEligibility::Unsupported;
                }
                if path == requested.to_str().unwrap_or_default()
                    && stage == ReadinessStage::AnalysisFeatures
                {
                    if !wav {
                        return Err(invalid_persisted_value(
                            "input",
                            "unsupported feature format",
                        ));
                    }
                    selected = Some(target.clone());
                }
                targets.push(target);
            }
        }
        targets.push(layout(source, generation, &all_membership));
        validate_publication_text_bounds(source, CONTRACT, &targets, &[])?;
        validate_targets(source, generation, &targets, Some(cancel))?;
        targets.pop();
        for target in &targets {
            checkpoint(cancel, deadline)?;
            // The existing primary key rejects duplicate identities across pages, rolling back
            // the entire generation; there is no unbounded resident identity set.
            insert_target(&tx, target, now)?;
            refresh_work_metadata(&tx, target)?;
            if target.stage == ReadinessStage::IndexedIdentity {
                tx.execute("INSERT INTO source_readiness_artifacts(source_id,scope_kind,scope_id,relative_path,stage,artifact_version,source_generation,content_generation,artifact_ref,completed_at)
                    VALUES (?1,'file',?2,NULL,'indexed_identity',?3,?4,?5,NULL,?6)
                    ON CONFLICT(source_id,scope_kind,scope_id,stage) DO UPDATE SET relative_path=NULL,artifact_version=excluded.artifact_version,source_generation=excluded.source_generation,content_generation=excluded.content_generation,artifact_ref=NULL,completed_at=excluded.completed_at",
                    params![source,target.scope_id,target.required_version,generation,target.content_generation,now])?;
            }
        }
        observe_page(files);
        checkpoint(cancel, deadline)?;
        validate_audit(&tx, fence)?;
    }
    let selected = selected
        .ok_or_else(|| invalid_persisted_value("input", "requested WAV absent from manifest"))?;
    let source_target = layout(source, generation, &all_membership);
    validate_targets(
        source,
        generation,
        std::slice::from_ref(&source_target),
        Some(cancel),
    )?;
    insert_target(&tx, &source_target, now)?;
    // Validate the whole persisted matrix against the transaction's manifest, not only the
    // caller-derived page DTOs. EXISTS returns one scalar, keeping invalid-row evidence bounded.
    let mismatch: bool = tx.query_row(
        &format!(
            "SELECT EXISTS(
        SELECT 1 FROM source_readiness_targets AS target
        WHERE target.source_id=?1 AND target.scope_kind='file' AND (
            target.source_generation<>?2 OR target.required_version<>CASE target.stage
                WHEN 'indexed_identity' THEN 'audited-identity-v1'
                WHEN 'analysis_features' THEN 'features-v1'
                WHEN 'embedding_aspects' THEN 'unconnected-embedding-v1' ELSE '' END
            OR NOT EXISTS(SELECT 1 FROM wav_files WHERE missing=0 AND {filter}
                AND file_identity=target.scope_id AND path=target.relative_path
                AND content_hash=target.content_generation
                AND target.eligibility=CASE WHEN target.stage='embedding_aspects'
                    OR (target.stage='analysis_features' AND lower(substr(path,-4))<>'.wav')
                    THEN 'unsupported' ELSE 'eligible' END)))"
        ),
        params![source, generation],
        |r| r.get(0),
    )?;
    let layout_matches: i64 = tx.query_row(
        "SELECT count(*) FROM source_readiness_targets
        WHERE source_id=?1 AND scope_kind='source' AND scope_id=?1 AND stage='similarity_layout'
        AND source_generation=?2 AND required_version='unconnected-layout-v1'
        AND content_generation=?3 AND relative_path IS NULL AND eligibility='unsupported'",
        params![source, generation, all_membership.generation()],
        |r| r.get(0),
    )?;
    if mismatch || layout_matches != 1 {
        return Err(invalid_persisted_value(
            "matrix",
            "manifest target membership mismatch",
        ));
    }
    // Exact whole matrix: every file came once from the same audited manifest, every page
    // passed the original stage validator, and duplicate identity inserts failed atomically.
    let (digest, count) =
        digest_targets_in_transaction(&tx, source, Some(generation), Some(cancel))?;
    if count != (files as i64) * 3 + 1 {
        return Err(invalid_persisted_value("matrix", "incomplete target count"));
    }
    // Existing contract records eligible embedding membership. This feature-only publisher
    // leaves all embeddings unsupported; the layout token separately binds all manifest files.
    let membership = ReadinessMembership::default();
    tx.execute("INSERT INTO source_readiness_sources(source_id,source_generation,readiness_revision,availability,contract_version,membership_digest,membership_count,updated_at)
        VALUES(?1,?2,?3,'active',?4,?5,0,?6) ON CONFLICT(source_id) DO UPDATE SET source_generation=excluded.source_generation,readiness_revision=excluded.readiness_revision,availability=excluded.availability,contract_version=excluded.contract_version,membership_digest=excluded.membership_digest,membership_count=excluded.membership_count,updated_at=excluded.updated_at",
        params![source,generation,readiness_revision,CONTRACT,membership.digest.as_slice(),now])?;
    tx.execute("DELETE FROM analysis_jobs WHERE source_id=?1 AND readiness_managed=1 AND NOT EXISTS(
        SELECT 1 FROM source_readiness_targets AS target WHERE target.source_id=analysis_jobs.source_id AND target.scope_kind=analysis_jobs.readiness_scope_kind AND target.scope_id=analysis_jobs.readiness_scope_id AND target.stage=analysis_jobs.readiness_stage AND target.required_version=analysis_jobs.artifact_version AND target.content_generation=analysis_jobs.content_generation AND (target.scope_kind='file' OR target.source_generation=analysis_jobs.source_generation))",[source])?;
    persist_readiness_wake(
        &tx,
        source,
        generation,
        readiness_revision,
        CONTRACT,
        digest,
        count,
    )?;
    checkpoint(cancel, deadline)?;
    validate_audit(&tx, fence)?;
    validate_publication_ownership(&tx, ownership, origin)?;
    checkpoint(cancel, deadline)?;
    drop(sql_deadline);
    tx.commit()?;
    Ok(selected)
}
