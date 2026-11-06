//! Bounded readiness admission from one retained, fully audited source manifest.
use super::super::super::util::map_sql_error;
use super::{SourceRevision, SourceWriter, SourceWriterError};
use crate::sample_sources::readiness::{
    ReadinessEligibility, ReadinessMembership, ReadinessStage, ReadinessTarget,
    ReadinessTargetPublication, SourceAvailability,
};
use crate::sample_sources::reconciliation::RootRelativePath;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

const CONTRACT: &str = "yield-audited-feature-matrix-v1";

fn checkpoint(cancel: &AtomicBool) -> Result<(), SourceWriterError> {
    if cancel.load(Ordering::Acquire) {
        Err(crate::sample_sources::readiness::ReadinessError::Cancelled.into())
    } else {
        Ok(())
    }
}

impl SourceWriter {
    /// Maximum supported manifest rows admitted by this initial complete-matrix command.
    pub const FEATURE_READINESS_MAX_FILES: usize = 256;
    /// Maximum combined manifest path/identity/content bytes before target construction.
    pub const FEATURE_READINESS_MAX_INPUT_BYTES: usize = 128 * 1024;

    /// Publish current audited targets, using bounded streaming when small-source admission
    /// exceeds its row/text budget. Both paths preserve the complete-matrix atomic contract.
    pub fn prepare_feature_readiness_target(
        &mut self,
        expected: &SourceRevision,
        relative_path: &Path,
        now: i64,
        cancel: &AtomicBool,
    ) -> Result<ReadinessTarget, SourceWriterError> {
        match self.prepare_bounded_feature_readiness_target(expected, relative_path, now, cancel) {
            Err(SourceWriterError::InvalidRequest(
                "feature manifest row limit" | "feature manifest byte limit",
            )) => {
                self.prepare_streamed_feature_readiness_target(expected, relative_path, now, cancel)
            }
            result => result,
        }
    }

    /// Stream a complete source generation from 64-row/128-KiB owner-held pages.
    /// At most 16,384 files and two seconds of SQLite work are admitted per atomic attempt;
    /// over-budget/cancelled/stale attempts retain the prior visible generation for retry.
    pub fn prepare_streamed_feature_readiness_target(
        &mut self,
        expected: &SourceRevision,
        relative_path: &Path,
        now: i64,
        cancel: &AtomicBool,
    ) -> Result<ReadinessTarget, SourceWriterError> {
        self.stream_feature_readiness_with_checkpoint(
            expected,
            relative_path,
            now,
            cancel,
            &mut |_| {},
        )
    }

    #[cfg(test)]
    pub(crate) fn stream_feature_readiness_with_checkpoint_for_test(
        &mut self,
        expected: &SourceRevision,
        relative_path: &Path,
        now: i64,
        cancel: &AtomicBool,
        checkpoint_fn: &mut dyn FnMut(usize),
    ) -> Result<ReadinessTarget, SourceWriterError> {
        self.stream_feature_readiness_with_checkpoint(
            expected,
            relative_path,
            now,
            cancel,
            checkpoint_fn,
        )
    }

    fn stream_feature_readiness_with_checkpoint(
        &mut self,
        expected: &SourceRevision,
        relative_path: &Path,
        now: i64,
        cancel: &AtomicBool,
        checkpoint_fn: &mut dyn FnMut(usize),
    ) -> Result<ReadinessTarget, SourceWriterError> {
        checkpoint(cancel)?;
        self.validate_readiness_revision(expected)?;
        let fence = self
            .reference_read_fence()
            .map_err(|_| SourceWriterError::StaleFence)?;
        if fence.source_revision() != expected {
            return Err(SourceWriterError::StaleFence);
        }
        RootRelativePath::try_from_path(relative_path.to_path_buf())
            .map_err(|_| SourceWriterError::InvalidRequest("invalid feature target path"))?;
        if relative_path.to_str().is_none_or(|p| p.len() > 1024) {
            return Err(SourceWriterError::InvalidRequest(
                "feature target path limit",
            ));
        }
        Ok(self
            .database
            .readiness_store()?
            .publish_streamed_feature_manifest(&fence, relative_path, now, cancel, checkpoint_fn)?)
    }

    /// Publish the complete bounded manifest matrix and return one owner-derived WAV target.
    ///
    /// Requires a current complete audit, stable manifest identities and full BLAKE3 hashes.
    /// All manifest index completions and the durable wake share the target transaction.
    /// Features, embeddings and layout are never completed by manifest evidence. Unsupported
    /// formats and the unconnected embedding/layout adapters remain explicitly unsupported.
    /// The caller queues the returned feature target through `enqueue_readiness_jobs`; a crash
    /// between publication and that command can retry this publication without losing evidence.
    /// An incompatible existing readiness contract is rejected, never silently replaced.
    pub(crate) fn prepare_bounded_feature_readiness_target(
        &mut self,
        expected: &SourceRevision,
        relative_path: &Path,
        now: i64,
        cancel: &AtomicBool,
    ) -> Result<ReadinessTarget, SourceWriterError> {
        checkpoint(cancel)?;
        self.validate_readiness_revision(expected)?;
        // Strict bounded audit reader also rejects malformed/incomplete audit metadata.
        self.reference_read_fence()
            .map_err(|_| SourceWriterError::StaleFence)?;
        let requested = relative_path.to_str().filter(|p| p.len() <= 1024).ok_or(
            SourceWriterError::InvalidRequest("feature target path limit"),
        )?;
        RootRelativePath::try_from_path(relative_path.to_path_buf())
            .map_err(|_| SourceWriterError::InvalidRequest("invalid feature target path"))?;
        let generation = i64::try_from(expected.revision())
            .map_err(|_| SourceWriterError::InvalidRequest("feature source generation overflow"))?;
        let source = self.source_id.as_str().to_owned();
        let prior = self.database.readiness_store()?.source_state(&source)?;
        if prior
            .as_ref()
            .is_some_and(|s| s.contract_version != CONTRACT)
        {
            return Err(SourceWriterError::InvalidRequest(
                "incompatible feature readiness contract",
            ));
        }
        let readiness_revision = prior
            .map_or(Some(1), |s| s.readiness_revision.checked_add(1))
            .ok_or(SourceWriterError::InvalidRequest(
                "feature readiness revision overflow",
            ))?;
        let filter = crate::sample_sources::supported_audio_where_clause();
        let manifest = {
            let mut statement = self.database.connection.prepare(&format!(
                "SELECT CASE WHEN typeof(path)='text' AND length(CAST(path AS BLOB)) BETWEEN 1 AND 1024 THEN path END,
                        CASE WHEN typeof(file_identity)='text' AND length(CAST(file_identity AS BLOB)) BETWEEN 1 AND 512 THEN file_identity END,
                        CASE WHEN typeof(content_hash)='text' AND length(CAST(content_hash AS BLOB))=64 THEN content_hash END
                 FROM wav_files WHERE missing=0 AND {filter} ORDER BY path LIMIT 257"
            )).map_err(map_sql_error)?;
            let mut rows = statement.query([]).map_err(map_sql_error)?;
            let mut manifest = Vec::new();
            let mut used = 0usize;
            let mut identities = BTreeSet::new();
            while let Some(row) = rows.next().map_err(map_sql_error)? {
                checkpoint(cancel)?;
                if manifest.len() == Self::FEATURE_READINESS_MAX_FILES {
                    return Err(SourceWriterError::InvalidRequest(
                        "feature manifest row limit",
                    ));
                }
                let (path, identity, content) = (
                    row.get::<_, Option<String>>(0).map_err(map_sql_error)?,
                    row.get::<_, Option<String>>(1).map_err(map_sql_error)?,
                    row.get::<_, Option<String>>(2).map_err(map_sql_error)?,
                );
                let (path, identity, content) = path
                    .zip(identity)
                    .zip(content)
                    .map(|((p, i), c)| (p, i, c))
                    .ok_or(SourceWriterError::InvalidRequest(
                        "feature manifest identity or full hash unavailable",
                    ))?;
                if !content
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    || identity.trim().is_empty()
                    || !identities.insert(identity.clone())
                {
                    return Err(SourceWriterError::InvalidRequest(
                        "invalid or duplicate feature manifest identity",
                    ));
                }
                RootRelativePath::try_from_path(path.clone().into()).map_err(|_| {
                    SourceWriterError::InvalidRequest("invalid manifest relative path")
                })?;
                used = used
                    .checked_add(path.len() + identity.len() + content.len())
                    .filter(|n| *n <= Self::FEATURE_READINESS_MAX_INPUT_BYTES)
                    .ok_or(SourceWriterError::InvalidRequest(
                        "feature manifest byte limit",
                    ))?;
                manifest.push((path, identity, content));
            }
            manifest
        };
        let mut targets = Vec::with_capacity(manifest.len() * 3 + 1);
        let mut membership = ReadinessMembership::default();
        let mut selected = None;
        for (path, identity, content) in manifest {
            checkpoint(cancel)?;
            membership.add(&identity, &content);
            for (stage, version) in [
                (ReadinessStage::IndexedIdentity, "audited-identity-v1"),
                (ReadinessStage::AnalysisFeatures, "features-v1"),
                (ReadinessStage::EmbeddingAspects, "unconnected-embedding-v1"),
            ] {
                let mut target = ReadinessTarget::file(
                    &source, &identity, &path, stage, version, generation, &content,
                );
                let supported = crate::sample_sources::is_supported_audio(Path::new(&path));
                if stage == ReadinessStage::EmbeddingAspects
                    || (stage == ReadinessStage::AnalysisFeatures && !supported)
                {
                    target.eligibility = ReadinessEligibility::Unsupported;
                }
                if path == requested && stage == ReadinessStage::AnalysisFeatures {
                    if !supported {
                        return Err(SourceWriterError::InvalidRequest(
                            "feature input format unsupported",
                        ));
                    }
                    selected = Some(target.clone());
                }
                targets.push(target);
            }
        }
        let selected = selected.ok_or(SourceWriterError::InvalidRequest(
            "feature target absent from audited manifest",
        ))?;
        targets.push(
            ReadinessTarget::source(
                &source,
                ReadinessStage::SimilarityLayout,
                "unconnected-layout-v1",
                generation,
                membership.generation(),
            )
            .with_eligibility(ReadinessEligibility::Unsupported),
        );
        checkpoint(cancel)?;
        self.validate_readiness_revision(expected)?;
        let mut store = self.database.readiness_store()?;
        let origin = store.capture_publication_origin()?;
        store.publish_audited_manifest_targets(
            &origin,
            &ReadinessTargetPublication::new(
                &source,
                generation,
                readiness_revision,
                SourceAvailability::Active,
                CONTRACT,
                &targets,
                now,
            ),
            cancel,
        )?;
        Ok(selected)
    }
}
