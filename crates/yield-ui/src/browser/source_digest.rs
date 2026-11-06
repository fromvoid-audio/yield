//! Shared digest demand for metadata writes on the same live waveform.

use std::sync::{Arc, Weak};

use gpui::Context;
use yield_session::{
    CatalogCommand, FeatureAnalysisCancellation, SourceContentDigestRequest,
    SourceContentDigestScope,
};

use super::{Browser, catalog};

#[derive(Default)]
pub(super) struct SourceDigestRequests {
    next_id: u64,
    current: Weak<PendingSourceDigest>,
}

#[cfg(test)]
impl SourceDigestRequests {
    pub(super) fn active_users(&self) -> usize {
        self.current.strong_count()
    }
}

pub(super) struct PendingSourceDigest {
    pub(super) request: SourceContentDigestRequest,
    cancellation: FeatureAnalysisCancellation,
}

impl Drop for PendingSourceDigest {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Join a current request for the same audited file or submit one bounded verification.
pub(super) fn acquire(
    browser: &mut Browser,
    scope: &SourceContentDigestScope,
    cx: &mut Context<Browser>,
) -> Option<Arc<PendingSourceDigest>> {
    let state = &mut browser.source_digest_requests;
    if let Some(current) = state.current.upgrade()
        && current.request.scope() == scope
        && !current.cancellation.is_cancelled()
    {
        return Some(current);
    }
    let id = state.next_id.checked_add(1)?;
    let request = SourceContentDigestRequest::new(
        &scope.source_id,
        scope.lifecycle_generation,
        scope.previous_revision,
        id,
        scope.relative_path.as_path(),
    )
    .ok()?;
    let pending = Arc::new(PendingSourceDigest {
        request,
        cancellation: FeatureAnalysisCancellation::new(),
    });
    catalog::submit_catalog_command(
        CatalogCommand::RequestSourceContentDigest {
            request: pending.request.clone(),
            cancellation: pending.cancellation.clone(),
        },
        cx,
    )
    .ok()?;
    state.next_id = id;
    state.current = Arc::downgrade(&pending);
    browser.start_source_registry_poll(cx);
    Some(pending)
}
