//! Source-local disposable cache maintenance; no filesystem absence or retirement authority.
use super::*;
use std::sync::{Weak, atomic::AtomicBool};

/// Private owner/revision-bound keyset position. It cannot select a deletion key.
#[derive(Clone, Debug)]
pub struct FeaturePruneCursor {
    revision: SourceRevision,
    owner: Weak<()>,
    after: String,
}

/// Bounded committed cache observations, never physical reclaimed bytes.
#[derive(Clone, Debug)]
pub struct FeaturePrunePage {
    /// Candidates inspected, including retained ambiguous evidence.
    pub observed: u32,
    /// Proven superseded payload rows removed in the committed transaction.
    pub removed: u32,
    /// Logical payload bytes removed; SQLite pages and WAL may remain allocated.
    pub logical_payload_bytes_removed: u64,
    /// Continuation issued by this exact owner for this audited revision.
    pub next: Option<FeaturePruneCursor>,
}

impl SourceWriter {
    /// Inspect at most 64 payloads and prune only proven superseded source-local content.
    /// Missing/unknown/offline evidence and current aliases are retained. No readiness or
    /// user metadata is removed. A fresh audit/revision requires restarting enumeration.
    pub fn prune_superseded_feature_artifacts(
        &mut self,
        expected: &SourceRevision,
        cursor: Option<&FeaturePruneCursor>,
        limit: u32,
        cancel: &AtomicBool,
    ) -> Result<FeaturePrunePage, SourceWriterError> {
        let started = Instant::now();
        let result = (|| {
            self.validate_readiness_revision(expected)?;
            if !(1..=64).contains(&limit) {
                return Err(SourceWriterError::InvalidRequest(
                    "feature maintenance page bound",
                ));
            }
            if let Some(cursor) = cursor {
                if &cursor.revision != expected
                    || !cursor
                        .owner
                        .upgrade()
                        .is_some_and(|owner| Arc::ptr_eq(&owner, &self.reference_liveness))
                {
                    return Err(SourceWriterError::StaleFence);
                }
            }
            let fence = self
                .reference_read_fence()
                .map_err(|_| SourceWriterError::StaleFence)?;
            let result = self.database.readiness_store()?.prune_superseded_features(
                &fence,
                cursor.map_or("", |cursor| cursor.after.as_str()),
                limit,
                cancel,
            )?;
            Ok(FeaturePrunePage {
                observed: result.observed,
                removed: result.removed,
                logical_payload_bytes_removed: u64::from(result.removed) * 732,
                next: result.next.map(|after| FeaturePruneCursor {
                    revision: expected.clone(),
                    owner: Arc::downgrade(&self.reference_liveness),
                    after,
                }),
            })
        })();
        self.feature_maintenance_metrics
            .record(&result, started.elapsed());
        result
    }
    /// Fixed-size diagnostics under strict current read-only owner/revision validation.
    /// No lease renewal, SQL mutation, recount or cleanup authority is supplied.
    pub fn feature_maintenance_metrics(
        &self,
        expected: &SourceRevision,
    ) -> Result<FeatureMaintenanceMetrics, SourceWriterError> {
        self.validate_readiness_revision(expected)?;
        Ok(self.feature_maintenance_metrics)
    }
}
