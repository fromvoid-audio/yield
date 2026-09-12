//! Browser-owned orchestration for bounded catalog rating-decay maintenance.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui::Context;
use wavecrate_library::sample_sources::{
    BrowserMetadataCursor, SourceId, reconciliation::RootIdentity,
};
use wavecrate_session::{
    CatalogCommand, CatalogError, CatalogEvent, CatalogOperation, CatalogSubmitError,
    MAX_RATING_DECAY_WEEKS, MIN_RATING_DECAY_WEEKS,
};

use super::{Browser, catalog, catalog_metadata::CatalogMetadataContext};

#[derive(Clone, Debug, Eq, PartialEq)]
struct DecayPageRequest {
    source_id: SourceId,
    lifecycle_generation: u64,
    root_identity: RootIdentity,
    directory_generation: u64,
    expected_revision: u64,
    request_id: u64,
    rating_decay_weeks: u16,
    now_epoch_seconds: i64,
    after_cursor: Option<BrowserMetadataCursor>,
    admitted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SnapshotRefresh {
    source_id: SourceId,
    lifecycle_generation: u64,
    root_identity: RootIdentity,
    expected_revision: Option<u64>,
    submitted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompletionKey {
    source_id: SourceId,
    lifecycle_generation: u64,
    root_identity: RootIdentity,
    directory_generation: u64,
    revision: u64,
    rating_decay_weeks: u16,
    audit_generation: u64,
}

/// Browser state for one bounded rating-decay maintenance run.
#[derive(Default)]
pub(super) struct RatingDecayState {
    next_request_id: u64,
    audit_generation: u64,
    pending: Option<DecayPageRequest>,
    refresh: Option<SnapshotRefresh>,
    last_completed: Option<CompletionKey>,
    processed_count: usize,
    updated_count: usize,
    status: Option<String>,
}

impl RatingDecayState {
    fn next_request_id(&mut self) -> u64 {
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == 0 {
            self.next_request_id = 1;
        }
        self.next_request_id
    }

    fn active(&self) -> bool {
        self.pending.is_some() || self.refresh.is_some()
    }

    fn should_start(
        &self,
        context: &CatalogMetadataContext,
        lifecycle_generation: u64,
        rating_decay_weeks: u16,
    ) -> bool {
        if self.active()
            || !(MIN_RATING_DECAY_WEEKS..=MAX_RATING_DECAY_WEEKS).contains(&rating_decay_weeks)
        {
            return false;
        }
        !self.last_completed.as_ref().is_some_and(|completed| {
            completed.source_id == context.source_id
                && completed.lifecycle_generation == lifecycle_generation
                && completed.root_identity == context.root_identity
                && completed.directory_generation == context.directory_generation
                && completed.revision == context.revision
                && completed.rating_decay_weeks == rating_decay_weeks
                && completed.audit_generation == self.audit_generation
        })
    }

    fn begin(
        &mut self,
        context: &CatalogMetadataContext,
        lifecycle_generation: u64,
        rating_decay_weeks: u16,
        now_epoch_seconds: i64,
    ) -> bool {
        if !self.should_start(context, lifecycle_generation, rating_decay_weeks) {
            return false;
        }
        self.processed_count = 0;
        self.updated_count = 0;
        self.status = Some("Rating decay maintenance queued…".to_string());
        self.pending = Some(DecayPageRequest {
            source_id: context.source_id.clone(),
            lifecycle_generation,
            root_identity: context.root_identity.clone(),
            directory_generation: context.directory_generation,
            expected_revision: context.revision,
            request_id: self.next_request_id(),
            rating_decay_weeks,
            now_epoch_seconds,
            after_cursor: None,
            admitted: false,
        });
        true
    }

    fn note_audit_committed(&mut self) {
        self.audit_generation = self.audit_generation.wrapping_add(1);
    }

    fn cancel(&mut self) {
        self.pending = None;
        self.refresh = None;
        self.processed_count = 0;
        self.updated_count = 0;
        self.status = None;
    }

    fn cancel_for_preference_change(&mut self) {
        // An admitted page may still commit. Stop further writes but retain a read after
        // that page so a failed preference save cannot leave the projection stale.
        let pending = self.pending.take();
        if self.refresh.is_none()
            && let Some(page) = pending
            && (page.admitted || self.updated_count > 0)
        {
            self.queue_refresh(
                page.source_id,
                page.lifecycle_generation,
                page.root_identity,
                None,
            );
        }
        self.processed_count = 0;
        self.updated_count = 0;
        self.status = None;
    }

    fn queue_refresh(
        &mut self,
        source_id: SourceId,
        lifecycle_generation: u64,
        root_identity: RootIdentity,
        expected_revision: Option<u64>,
    ) {
        self.refresh = Some(SnapshotRefresh {
            source_id,
            lifecycle_generation,
            root_identity,
            expected_revision,
            submitted: false,
        });
    }

    fn page_event_matches(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
        request_id: u64,
    ) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.source_id == *source_id
                && pending.lifecycle_generation == lifecycle_generation
                && pending.request_id == request_id
        })
    }
}

impl Browser {
    /// Start maintenance after an authoritative, audit-ready projection is available.
    pub(super) fn maybe_start_rating_decay(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.preferences.ready_for_maintenance() || self.curation_state.has_pending_outcomes() {
            return false;
        }
        let Some(context) = self.catalog_metadata.context() else {
            return false;
        };
        let Some(activation) = self.catalog_activation.as_ref() else {
            return false;
        };
        if !activation.audit_finished
            || activation.audit_error.is_some()
            || activation.source_id != context.source_id
            || activation.lifecycle_generation != context.lifecycle_generation
            || activation.root != context.root
            || activation.root_identity.as_ref() != Some(&context.root_identity)
        {
            return false;
        }
        let rating_decay_weeks = self.preferences.effective().rating_decay_weeks;
        if !self.rating_decay.begin(
            &context,
            activation.lifecycle_generation,
            rating_decay_weeks,
            current_epoch_seconds(),
        ) {
            return false;
        }
        self.scan_status = "Applying rating decay maintenance…".to_string();
        let _ = retry(self, cx);
        self.start_source_registry_poll(cx);
        true
    }
}

/// Cancel a run whose source lifecycle is being replaced or removed.
pub(super) fn cancel_for_source_change(browser: &mut Browser) {
    browser.rating_decay.cancel();
}

/// Cancel a run while preferences are transitioning through an optimistic patch.
pub(super) fn cancel_for_preference_change(browser: &mut Browser) {
    browser.rating_decay.cancel_for_preference_change();
}

/// Record a fresh audit boundary used to permit a later same-revision run.
pub(super) fn note_audit_committed(browser: &mut Browser) {
    browser.rating_decay.note_audit_committed();
}

/// Retry bounded command admission and any authoritative post-run snapshot refresh.
pub(super) fn retry(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if let Some(request) = browser
        .rating_decay
        .pending
        .as_ref()
        .filter(|request| !request.admitted)
        .cloned()
    {
        if !active_identity_matches(browser, &request.source_id, request.lifecycle_generation) {
            browser.rating_decay.cancel();
            return true;
        }
        let result = catalog::submit_catalog_command(
            CatalogCommand::ApplyRatingDecay {
                source_id: request.source_id,
                lifecycle_generation: request.lifecycle_generation,
                expected_revision: request.expected_revision,
                root_identity: request.root_identity,
                request_id: request.request_id,
                rating_decay_weeks: request.rating_decay_weeks,
                now_epoch_seconds: request.now_epoch_seconds,
                after_cursor: request.after_cursor,
            },
            cx,
        );
        return match result {
            Ok(()) => {
                if let Some(pending) = browser.rating_decay.pending.as_mut()
                    && pending.request_id == request.request_id
                {
                    pending.admitted = true;
                }
                true
            }
            Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => false,
            Err(error) => {
                browser.rating_decay.pending = None;
                browser.rating_decay.status =
                    Some(format!("Rating decay maintenance unavailable: {error}"));
                true
            }
        };
    }

    let Some(refresh) = browser
        .rating_decay
        .refresh
        .as_ref()
        .filter(|refresh| !refresh.submitted)
        .cloned()
    else {
        return false;
    };
    if !active_identity_matches(browser, &refresh.source_id, refresh.lifecycle_generation) {
        browser.rating_decay.cancel();
        return true;
    }
    let result = catalog::submit_catalog_command(
        CatalogCommand::ReadSnapshot {
            source_id: refresh.source_id.clone(),
            expected_revision: refresh.expected_revision,
        },
        cx,
    );
    match result {
        Ok(()) => {
            if let Some(pending) = browser.rating_decay.refresh.as_mut()
                && pending.source_id == refresh.source_id
                && pending.lifecycle_generation == refresh.lifecycle_generation
            {
                pending.submitted = true;
            }
            true
        }
        Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => false,
        Err(error) => {
            browser.rating_decay.refresh = None;
            browser.rating_decay.status =
                Some(format!("Rating decay refresh unavailable: {error}"));
            true
        }
    }
}

/// Return whether this lane has work that should keep the catalog poll alive.
pub(super) fn poll_needed(browser: &Browser) -> bool {
    browser.rating_decay.active()
}

/// Observe an authoritative snapshot used to finish a post-maintenance refresh.
pub(super) fn observe_snapshot(browser: &mut Browser, event: &CatalogEvent) {
    let snapshot = match event {
        CatalogEvent::SnapshotReady { snapshot } | CatalogEvent::AuditCommitted { snapshot } => {
            snapshot
        }
        _ => return,
    };
    let should_clear = browser
        .rating_decay
        .refresh
        .as_ref()
        .is_some_and(|refresh| {
            refresh.submitted
                && refresh.source_id == snapshot.source_id
                && refresh.lifecycle_generation == snapshot.lifecycle_generation
                && refresh.root_identity == snapshot.root_identity
                && refresh
                    .expected_revision
                    .is_none_or(|revision| snapshot.committed_revision >= revision)
        });
    if should_clear {
        browser.rating_decay.refresh = None;
        browser.rating_decay.status = Some("Rating decay maintenance refreshed".to_string());
    }
}

/// Consume one rating-decay event, applying only the matching source lifecycle request.
pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    cx: &mut Context<Browser>,
) -> bool {
    match event {
        CatalogEvent::RatingDecayProgress {
            source_id,
            lifecycle_generation,
            root_identity,
            revision,
            request_id,
            processed_count,
            updated_count,
            next_cursor,
            receipt,
        } => apply_progress(
            browser,
            cx,
            source_id,
            *lifecycle_generation,
            root_identity,
            *revision,
            *request_id,
            *processed_count,
            *updated_count,
            next_cursor,
            receipt.as_ref(),
        ),
        CatalogEvent::RatingDecayFailed {
            source_id,
            lifecycle_generation,
            expected_revision,
            request_id,
            error,
        } => apply_failure(
            browser,
            cx,
            source_id,
            *lifecycle_generation,
            *expected_revision,
            *request_id,
            error,
        ),
        CatalogEvent::Failed {
            source_id: Some(source_id),
            lifecycle_generation: Some(lifecycle_generation),
            operation: CatalogOperation::ReadSnapshot,
            error,
        } => apply_refresh_failure(browser, source_id, *lifecycle_generation, error),
        _ => false,
    }
}

fn apply_progress(
    browser: &mut Browser,
    cx: &mut Context<Browser>,
    source_id: &SourceId,
    lifecycle_generation: u64,
    root_identity: &RootIdentity,
    revision: u64,
    request_id: u64,
    processed_count: usize,
    updated_count: usize,
    next_cursor: &Option<BrowserMetadataCursor>,
    receipt: Option<&wavecrate_session::SourceCurationCommitReceipt>,
) -> bool {
    let Some(request) = browser.rating_decay.pending.as_ref().cloned() else {
        return true;
    };
    if !browser
        .rating_decay
        .page_event_matches(source_id, lifecycle_generation, request_id)
    {
        return true;
    }
    let expected_next_revision = request.expected_revision.checked_add(1);
    let revision_is_valid = revision == request.expected_revision
        || (receipt.is_some() && expected_next_revision == Some(revision));
    let receipt_is_valid = receipt.is_none_or(|receipt| {
        receipt.source_revision() == revision
            && receipt.root_identity() == root_identity
            && receipt.directory_generation() == request.directory_generation
    });
    if !request.admitted
        || request.root_identity != *root_identity
        || !revision_is_valid
        || !receipt_is_valid
    {
        browser.rating_decay.pending = None;
        browser.rating_decay.status = Some("Rating decay lineage became stale; refreshing".into());
        browser.rating_decay.queue_refresh(
            request.source_id,
            request.lifecycle_generation,
            request.root_identity,
            None,
        );
        let _ = retry(browser, cx);
        return true;
    }

    browser.rating_decay.processed_count = browser
        .rating_decay
        .processed_count
        .saturating_add(processed_count);
    browser.rating_decay.updated_count = browser
        .rating_decay
        .updated_count
        .saturating_add(updated_count);
    if let Some(cursor) = next_cursor {
        if request.after_cursor.as_ref() == Some(cursor) {
            browser.rating_decay.pending = None;
            browser.rating_decay.status = Some("Rating decay cursor stalled; refreshing".into());
            browser.rating_decay.queue_refresh(
                request.source_id,
                request.lifecycle_generation,
                request.root_identity,
                None,
            );
            let _ = retry(browser, cx);
            return true;
        }
        let next_request_id = browser.rating_decay.next_request_id();
        browser.rating_decay.pending = Some(DecayPageRequest {
            source_id: request.source_id,
            lifecycle_generation: request.lifecycle_generation,
            root_identity: request.root_identity,
            directory_generation: request.directory_generation,
            expected_revision: revision,
            request_id: next_request_id,
            rating_decay_weeks: request.rating_decay_weeks,
            now_epoch_seconds: request.now_epoch_seconds,
            after_cursor: Some(cursor.clone()),
            admitted: false,
        });
        browser.scan_status = format!(
            "Rating decay maintenance: {} updated across {} files…",
            browser.rating_decay.updated_count, browser.rating_decay.processed_count
        );
        let _ = retry(browser, cx);
        return true;
    }

    browser.rating_decay.pending = None;
    let final_revision = revision;
    let key = CompletionKey {
        source_id: request.source_id.clone(),
        lifecycle_generation: request.lifecycle_generation,
        root_identity: request.root_identity.clone(),
        directory_generation: request.directory_generation,
        revision: final_revision,
        rating_decay_weeks: request.rating_decay_weeks,
        audit_generation: browser.rating_decay.audit_generation,
    };
    let updated_total = browser.rating_decay.updated_count;
    browser.rating_decay.last_completed = Some(key);
    browser.rating_decay.status = Some(format!(
        "Rating decay complete: {} updated across {} files",
        updated_total, browser.rating_decay.processed_count
    ));
    browser.scan_status = browser.rating_decay.status.clone().unwrap_or_default();
    if updated_total > 0 {
        browser.rating_decay.queue_refresh(
            request.source_id,
            request.lifecycle_generation,
            request.root_identity,
            Some(final_revision),
        );
        let _ = retry(browser, cx);
    }
    true
}

fn apply_failure(
    browser: &mut Browser,
    cx: &mut Context<Browser>,
    source_id: &SourceId,
    lifecycle_generation: u64,
    expected_revision: u64,
    request_id: u64,
    error: &CatalogError,
) -> bool {
    let Some(request) = browser.rating_decay.pending.as_ref().cloned() else {
        return true;
    };
    if !browser
        .rating_decay
        .page_event_matches(source_id, lifecycle_generation, request_id)
    {
        return true;
    }
    browser.rating_decay.pending = None;
    browser.rating_decay.status = Some(format!("Rating decay maintenance failed: {error}"));
    if request.expected_revision != expected_revision {
        browser.rating_decay.status = Some("Rating decay lineage became stale; refreshing".into());
    }
    browser.rating_decay.queue_refresh(
        request.source_id,
        request.lifecycle_generation,
        request.root_identity,
        None,
    );
    let _ = retry(browser, cx);
    true
}

fn apply_refresh_failure(
    browser: &mut Browser,
    source_id: &SourceId,
    lifecycle_generation: u64,
    error: &CatalogError,
) -> bool {
    let matches = browser
        .rating_decay
        .refresh
        .as_ref()
        .is_some_and(|refresh| {
            refresh.submitted
                && refresh.source_id == *source_id
                && refresh.lifecycle_generation == lifecycle_generation
        });
    if !matches {
        return false;
    }
    let refresh = browser
        .rating_decay
        .refresh
        .as_mut()
        .expect("matching refresh remains installed");
    if matches!(error, CatalogError::StaleRevision { .. }) && refresh.expected_revision.is_some() {
        refresh.expected_revision = None;
        refresh.submitted = false;
        browser.rating_decay.status = Some("Refreshing rating-decay projection…".into());
    } else {
        browser.rating_decay.refresh = None;
        browser.rating_decay.status = Some(format!("Rating decay refresh failed: {error}"));
    }
    true
}

fn active_identity_matches(
    browser: &Browser,
    source_id: &SourceId,
    lifecycle_generation: u64,
) -> bool {
    browser
        .catalog_activation
        .as_ref()
        .is_some_and(|activation| {
            activation.source_id == *source_id
                && activation.lifecycle_generation == lifecycle_generation
        })
}

fn current_epoch_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().min(i64::MAX as u64) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn context(revision: u64, directory_generation: u64) -> CatalogMetadataContext {
        CatalogMetadataContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/library"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation,
            revision,
            entry_indices: Default::default(),
        }
    }

    #[test]
    fn run_captures_one_clock_value_and_allocates_unique_page_ids() {
        let mut state = RatingDecayState::default();
        let context = context(7, 2);
        assert!(state.begin(&context, 3, 4, 1234));
        let first = state.pending.clone().expect("first page");
        state.pending = Some(DecayPageRequest {
            after_cursor: None,
            expected_revision: first.expected_revision,
            request_id: state.next_request_id(),
            admitted: false,
            ..first.clone()
        });
        let second = state.pending.as_ref().expect("second page");
        assert_ne!(first.request_id, second.request_id);
        assert_eq!(first.now_epoch_seconds, second.now_epoch_seconds);
        assert_eq!(second.expected_revision, first.expected_revision);
    }

    #[test]
    fn completed_revision_and_weeks_are_deduplicated_until_a_new_audit() {
        let mut state = RatingDecayState::default();
        let context = context(7, 2);
        assert!(state.begin(&context, 3, 4, 1234));
        state.pending = None;
        state.last_completed = Some(CompletionKey {
            source_id: context.source_id.clone(),
            lifecycle_generation: 3,
            root_identity: context.root_identity.clone(),
            directory_generation: context.directory_generation,
            revision: context.revision,
            rating_decay_weeks: 4,
            audit_generation: state.audit_generation,
        });
        assert!(!state.should_start(&context, 3, 4));
        state.note_audit_committed();
        assert!(state.should_start(&context, 3, 4));
    }

    #[test]
    fn preference_change_refreshes_admitted_writes_without_continuing_them() {
        let mut state = RatingDecayState::default();
        let context = context(7, 2);
        assert!(state.begin(&context, 3, 4, 1234));
        state.pending.as_mut().unwrap().admitted = true;
        state.cancel_for_preference_change();
        assert!(state.pending.is_none());
        let refresh = state
            .refresh
            .as_mut()
            .expect("authoritative read after admitted page");
        assert_eq!(refresh.source_id, context.source_id);
        assert_eq!(refresh.expected_revision, None);
        assert!(!refresh.submitted);
        refresh.submitted = true;
        state.cancel_for_preference_change();
        assert!(state.refresh.as_ref().unwrap().submitted);
        assert!(!state.should_start(&context, 3, 4));
        state.cancel();
        assert!(!state.active());
    }

    #[test]
    fn preference_change_only_refreshes_a_queued_page_if_prior_pages_changed_data() {
        for updated_count in [0, 1] {
            let mut state = RatingDecayState::default();
            assert!(state.begin(&context(7, 2), 3, 4, 1234));
            state.updated_count = updated_count;
            state.cancel_for_preference_change();
            assert!(state.pending.is_none());
            assert_eq!(state.refresh.is_some(), updated_count > 0);
        }
    }

    #[test]
    fn source_change_cancels_page_and_refresh_continuations() {
        let mut state = RatingDecayState::default();
        let context = context(7, 2);
        assert!(state.begin(&context, 3, 4, 1234));
        state.queue_refresh(
            context.source_id.clone(),
            3,
            context.root_identity.clone(),
            Some(8),
        );
        state.cancel();
        assert!(!state.active());
        assert_eq!(state.processed_count, 0);
        assert_eq!(state.updated_count, 0);
    }
}
