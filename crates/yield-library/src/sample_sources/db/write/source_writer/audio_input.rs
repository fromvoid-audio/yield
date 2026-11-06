use std::{
    path::Path,
    sync::{Arc, Weak},
};

use rusqlite::params;

use super::{
    RootRelativePath, SourceRevision, SourceWriter, SourceWriterError, map_sql_error,
    normalize_relative_path,
};
use crate::sample_sources::{
    db::SourceIndexInspectionError,
    readiness::{
        FEATURE_INPUT_MAX_BYTES, FeatureArtifactClaim, InputPermit, PendingSourceContentInput,
        PreparedSourceContentDigest, SourceFeatureInput, SourceFeatureInputError as InputError,
        TRANSIENT_STREAM_MAX_BYTES,
    },
};

enum DigestRequirement<'a> {
    Claim { identity: &'a str, hash: &'a str },
    Verified,
    Pending,
}

impl SourceWriter {
    /// Prepare held input from the exact current manifest/claim without data reads or SQL mutation.
    pub fn prepare_feature_input(
        &self,
        expected: &SourceRevision,
        claim: &FeatureArtifactClaim,
    ) -> Result<SourceFeatureInput, SourceWriterError> {
        self.validate_feature_artifact_claim(expected, claim)?;
        let target = claim.readiness_claim().target();
        let path = target
            .relative_path
            .as_deref()
            .ok_or(SourceWriterError::InvalidRequest("feature path missing"))?;
        self.hold_audio_input(
            expected,
            path,
            DigestRequirement::Claim {
                identity: &target.scope_id,
                hash: &target.content_generation,
            },
            FEATURE_INPUT_MAX_BYTES,
        )
    }

    /// Hold one current audited audio entry for bounded read-only analysis.
    ///
    /// This opens no audio data and creates no feature job. Compute outside the
    /// owner and call `validate_audio_input` on the retained owner before publishing.
    pub fn prepare_audio_input(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
    ) -> Result<SourceFeatureInput, SourceWriterError> {
        self.prepare_audited_audio_input(
            expected,
            relative,
            DigestRequirement::Verified,
            FEATURE_INPUT_MAX_BYTES,
        )
    }

    /// Hold one current audited entry under the larger bounded streaming admission.
    /// Consume it through the verified reader. Whole-file allocation keeps its normal limit.
    pub fn prepare_transient_stream_input(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
    ) -> Result<SourceFeatureInput, SourceWriterError> {
        self.prepare_audited_audio_input(
            expected,
            relative,
            DigestRequirement::Verified,
            TRANSIENT_STREAM_MAX_BYTES,
        )
    }

    /// Hold one exact current audited entry that still lacks its scanner digest.
    pub fn prepare_pending_content_digest(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
    ) -> Result<PendingSourceContentInput, SourceWriterError> {
        Ok(PendingSourceContentInput {
            input: self.prepare_audited_audio_input(
                expected,
                relative,
                DigestRequirement::Pending,
                TRANSIENT_STREAM_MAX_BYTES,
            )?,
            relative_path: relative.clone(),
        })
    }

    /// Revalidate a digest observation against the original writer and exact pending row.
    pub fn validate_pending_content_digest(
        &self,
        digest: &PreparedSourceContentDigest,
    ) -> Result<(), SourceWriterError> {
        self.validate_audio_input(&digest.input)?;
        let path = normalize_relative_path(digest.relative_path().as_path())?;
        let current = self
            .database
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM wav_files WHERE path = ?1 AND missing = 0
             AND file_identity = ?2 AND file_size = ?3 AND modified_ns = ?4
             AND content_hash IS NULL)",
                params![
                    path,
                    digest.input.identity,
                    digest.input.size,
                    digest.input.modified
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(map_sql_error)?;
        if !current {
            return Err(InputError::Changed.into());
        }
        Ok(())
    }

    fn prepare_audited_audio_input(
        &self,
        expected: &SourceRevision,
        relative: &RootRelativePath,
        requirement: DigestRequirement<'_>,
        byte_limit: u64,
    ) -> Result<SourceFeatureInput, SourceWriterError> {
        self.validate_readiness_revision(expected)?;
        let path = normalize_relative_path(relative.as_path())?;
        if Path::new(&path) != relative.as_path() {
            return Err(SourceWriterError::InvalidRequest(
                "audio input path differs from its manifest key",
            ));
        }
        self.hold_audio_input(expected, &path, requirement, byte_limit)
    }

    /// Confirm that a computed read still belongs to this exact owner, audit and file.
    pub fn validate_audio_input(
        &self,
        input: &SourceFeatureInput,
    ) -> Result<(), SourceWriterError> {
        if !Weak::ptr_eq(&input.owner, &Arc::downgrade(&self.reference_liveness)) {
            return Err(SourceWriterError::StaleFence);
        }
        self.validate_readiness_revision(&input.revision)?;
        Ok(input.validate()?)
    }

    fn hold_audio_input(
        &self,
        revision: &SourceRevision,
        path: &str,
        requirement: DigestRequirement<'_>,
        byte_limit: u64,
    ) -> Result<SourceFeatureInput, SourceWriterError> {
        let relative = RootRelativePath::try_from_path(path.into())
            .map_err(|_| SourceWriterError::InvalidRequest("invalid audio input path"))?;
        let (expected_identity, expected_content) = match &requirement {
            DigestRequirement::Claim { identity, hash } => (Some(*identity), Some(*hash)),
            _ => (None, None),
        };
        let (size, modified, identity, content): (i64, i64, String, Option<String>) = self
            .database
            .connection
            .query_row(
                "SELECT file_size, modified_ns, file_identity, content_hash FROM wav_files
                 WHERE path = ?1 AND missing = 0
                 AND (?2 IS NULL OR file_identity = ?2) AND (?3 IS NULL OR content_hash = ?3)
                 AND length(file_identity) <= 512
                 AND (content_hash IS NULL OR length(content_hash) = 64)",
                params![path, expected_identity, expected_content],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(map_sql_error)?;
        let size = u64::try_from(size)
            .map_err(|_| SourceWriterError::InvalidRequest("invalid audio input size"))?;
        if size > byte_limit {
            return Err(InputError::ByteLimit.into());
        }
        match (&requirement, &content) {
            (DigestRequirement::Pending, Some(_)) => {
                return Err(SourceWriterError::InvalidRequest(
                    "Source already has a content digest",
                ));
            }
            (DigestRequirement::Verified | DigestRequirement::Claim { .. }, None) => {
                return Err(InputError::ContentPending.into());
            }
            _ => {}
        }
        let permit = InputPermit::acquire()?;
        let fence = self.database.source_publication_fence()?;
        let root = self
            .database
            .source_lease
            .as_ref()
            .ok_or(SourceWriterError::StaleFence)?
            .media_root_binding();
        let held = root
            .inspect_index_file(relative.as_path())
            .map_err(|error| match error {
                SourceIndexInspectionError::Unsupported => InputError::UnsupportedPlatform,
                _ => InputError::Changed,
            })?;
        let input = SourceFeatureInput {
            held,
            root,
            fence,
            owner: Arc::downgrade(&self.reference_liveness),
            revision: revision.clone(),
            identity,
            size,
            modified,
            content,
            _permit: permit,
        };
        input.validate()?;
        Ok(input)
    }
}
