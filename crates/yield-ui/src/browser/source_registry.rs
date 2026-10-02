//! GPUI ownership boundary for the persisted source registry.

use std::sync::mpsc::TryRecvError;

use gpui::{BorrowAppContext, Global, Subscription};
use yield_library::sample_sources::{SampleSource, SourceId};
use yield_session::{
    CanonicalSourceRoot, CatalogCancelError, CatalogCommand, CatalogDelivery, CatalogEvent,
    CatalogStatus, CatalogSubmitError, PreferenceEvent, PreferencePatch, PreferenceRequestId,
    ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularyLookup,
    ProfileTagVocabularyPage, ProfileTagVocabularyPageCursor, ProfileTagVocabularySetReceipt,
    RegistryEvent, RegistryRevision, RequestId, SourceRegistryService, SubmitError,
    VocabularyRequest, VocabularyStatus, VocabularySubmitError,
};

/// The app-owned source registry service.
///
/// Browser entities borrow this global to submit bounded commands and drain events. Keeping the
/// service here means a Browser window can be released without joining the profile-owned worker.
pub(crate) struct SourceRegistryGlobal {
    service: Option<SourceRegistryService>,
    start_error: Option<String>,
    quit_subscription: Option<Subscription>,
}

impl Global for SourceRegistryGlobal {}

impl SourceRegistryGlobal {
    /// Start the GPUI development-profile service, retaining startup failures for the UI.
    pub(crate) fn start() -> Self {
        match SourceRegistryService::start() {
            Ok(service) => Self {
                service: Some(service),
                start_error: None,
                quit_subscription: None,
            },
            Err(error) => Self {
                service: None,
                start_error: Some(error.to_string()),
                quit_subscription: None,
            },
        }
    }

    /// Return a startup failure, if the worker could not be created.
    pub(crate) fn start_error(&self) -> Option<&str> {
        self.start_error.as_deref()
    }

    /// Submit one canonical source root for persistence.
    pub(crate) fn add_root(&self, root: CanonicalSourceRoot) -> Result<RequestId, SubmitError> {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .add_root(root)
    }

    /// Submit removal of one canonical source root.
    pub(crate) fn remove_root(&self, root: CanonicalSourceRoot) -> Result<RequestId, SubmitError> {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .remove_root(root)
    }

    pub(crate) fn set_source_primary(
        &self,
        source_id: SourceId,
        primary: bool,
        revision: RegistryRevision,
    ) -> Result<RequestId, SubmitError> {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .set_source_primary(source_id, primary, revision)
    }

    pub(crate) fn reorder_source(
        &self,
        source_id: SourceId,
        target_id: SourceId,
        after: bool,
        revision: RegistryRevision,
    ) -> Result<RequestId, SubmitError> {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .reorder_source(source_id, target_id, after, revision)
    }

    /// Submit one typed preference patch to the profile-owned session service.
    pub(crate) fn patch_preferences(
        &self,
        patch: PreferencePatch,
    ) -> Result<PreferenceRequestId, SubmitError> {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .patch_preferences(patch)
    }

    /// Submit the explicit preference reset command.
    pub(crate) fn reset_preferences(&self) -> Result<PreferenceRequestId, SubmitError> {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .reset_preferences()
    }

    /// Query storage through the profile-owned maintenance service.
    pub(crate) fn request_storage_usage(
        &self,
    ) -> Result<yield_session::MaintenanceRequest<yield_session::GlobalStorageUsage>, SubmitError>
    {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .request_storage_usage()
    }

    /// Clear the captured disposable cache namespace through its owner.
    pub(crate) fn clear_rebuildable_cache(
        &self,
    ) -> Result<yield_session::MaintenanceRequest<yield_session::CacheClearOutcome>, SubmitError>
    {
        self.service
            .as_ref()
            .ok_or(SubmitError::Closed)?
            .clear_rebuildable_cache()
    }

    /// Return the current state of the independently-owned source catalog.
    pub(crate) fn catalog_status(&self) -> CatalogStatus {
        self.service
            .as_ref()
            .map(SourceRegistryService::catalog_status)
            .unwrap_or(CatalogStatus::Closed)
    }

    /// Return the current profile tag vocabulary lifecycle state.
    pub(crate) fn vocabulary_status(&self) -> VocabularyStatus {
        self.service
            .as_ref()
            .map(SourceRegistryService::vocabulary_status)
            .unwrap_or(VocabularyStatus::Closed)
    }

    /// Submit an exact profile tag vocabulary lookup.
    pub(crate) fn lookup_profile_tag_categories(
        &self,
        labels: Vec<String>,
    ) -> Result<VocabularyRequest<ProfileTagVocabularyLookup>, VocabularySubmitError> {
        self.service
            .as_ref()
            .ok_or(VocabularySubmitError::Closed)?
            .lookup_profile_tag_categories(labels)
    }

    /// Submit one bounded literal-prefix profile vocabulary page.
    pub(crate) fn profile_tag_vocabulary_page(
        &self,
        expected_revision: Option<u64>,
        prefix: String,
        after_cursor: Option<ProfileTagVocabularyPageCursor>,
        requested_limit: usize,
    ) -> Result<VocabularyRequest<ProfileTagVocabularyPage>, VocabularySubmitError> {
        self.service
            .as_ref()
            .ok_or(VocabularySubmitError::Closed)?
            .profile_tag_vocabulary_page(expected_revision, prefix, after_cursor, requested_limit)
    }

    /// Submit one revision-fenced profile tag category assignment.
    pub(crate) fn set_profile_tag_category(
        &self,
        expected_revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<VocabularyRequest<ProfileTagVocabularySetReceipt>, VocabularySubmitError> {
        self.service
            .as_ref()
            .ok_or(VocabularySubmitError::Closed)?
            .set_profile_tag_category(expected_revision, label, category)
    }

    /// Submit deletion of one exact profile vocabulary label at a matching revision.
    pub(crate) fn delete_profile_tag_vocabulary(
        &self,
        expected_revision: u64,
        label: String,
    ) -> Result<VocabularyRequest<ProfileTagVocabularyDeleteReceipt>, VocabularySubmitError> {
        self.service
            .as_ref()
            .ok_or(VocabularySubmitError::Closed)?
            .delete_profile_tag_vocabulary(expected_revision, label)
    }

    /// Submit one typed catalog command through the profile-owned session service.
    pub(crate) fn submit_catalog(&self, command: CatalogCommand) -> Result<(), CatalogSubmitError> {
        self.service
            .as_ref()
            .ok_or(CatalogSubmitError::Closed)?
            .submit_catalog(command)
    }

    pub(crate) fn cancel_catalog_audit(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
    ) -> Result<(), CatalogCancelError> {
        self.service
            .as_ref()
            .ok_or(CatalogCancelError::Closed)?
            .cancel_catalog_audit(source_id, lifecycle_generation)
    }

    pub(crate) fn catalog_audit_is_running(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
    ) -> bool {
        self.service.as_ref().is_some_and(|service| {
            service.catalog_audit_is_running(source_id, lifecycle_generation)
        })
    }

    /// Drain catalog events without waiting for the background worker.
    #[allow(dead_code)]
    pub(crate) fn drain_catalog_events(&self, events: &mut Vec<CatalogEvent>) -> bool {
        let Some(service) = self.service.as_ref() else {
            return false;
        };
        const MAX_EVENTS_PER_TICK: usize = 64;
        for _ in 0..MAX_EVENTS_PER_TICK {
            match service.try_recv_catalog() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return false,
            }
        }
        true
    }

    /// Drain catalog deliveries while retaining each source-publication fence for the browser.
    pub(crate) fn drain_catalog_deliveries(&self, deliveries: &mut Vec<CatalogDelivery>) -> bool {
        let Some(service) = self.service.as_ref() else {
            return false;
        };
        const MAX_EVENTS_PER_TICK: usize = 64;
        for _ in 0..MAX_EVENTS_PER_TICK {
            match service.try_recv_catalog_delivery() {
                Ok(delivery) => deliveries.push(delivery),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return false,
            }
        }
        true
    }

    /// Drain all currently available worker events without waiting.
    pub(crate) fn drain_events(&self, events: &mut Vec<RegistryEvent>) {
        let Some(service) = self.service.as_ref() else {
            return;
        };
        loop {
            match service.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Drain all currently available preference events without waiting.
    pub(crate) fn drain_preference_events(&self, events: &mut Vec<PreferenceEvent>) {
        let Some(service) = self.service.as_ref() else {
            return;
        };
        loop {
            match service.try_recv_preference() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Request the profile worker to drain and close during application quit.
    pub(crate) fn shutdown(&mut self) {
        if let Some(mut service) = self.service.take() {
            let shutdown_result = service.shutdown();
            log_shutdown_curation_failures(&service);
            if let Err(error) = shutdown_result {
                eprintln!("yield source registry shutdown failed: {error}");
            }
        }
    }

    /// Retain the quit observer for the lifetime of the app global.
    pub(crate) fn retain_quit_subscription(&mut self, subscription: Subscription) {
        self.quit_subscription = Some(subscription);
    }
}

fn log_shutdown_curation_failures(service: &SourceRegistryService) {
    loop {
        match service.try_recv_catalog() {
            Ok(CatalogEvent::CurationOperationCompleted {
                source_id,
                request_id,
                outcome,
                ..
            }) => {
                if let Some(failure) = outcome.failure() {
                    eprintln!(
                        "yield curation operation failed during shutdown: source={source_id} request={request_id} committed_batches={} error={}",
                        outcome.committed_batches().len(),
                        failure.error(),
                    );
                }
            }
            Ok(_) => {}
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
}

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{
    Browser, PendingRegistryMutation, SourceSession, catalog, curation_queue, source_label,
    tag_chips, tag_completion, tag_filter, tag_library, tags,
};
use gpui::Context;

impl Browser {
    pub(super) fn start_source_registry_poll(&mut self, cx: &mut Context<Self>) {
        if self.source_registry_task.is_some() || !cx.has_global::<SourceRegistryGlobal>() {
            return;
        }
        if let Some(error) = cx.global::<SourceRegistryGlobal>().start_error() {
            self.source_registry_loaded = true;
            self.set_source_registry_status(format!("Source persistence unavailable: {error}"));
        }
        self.source_registry_task = Some(cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(100)).await;
                let keep_polling = this
                    .update(cx, |view, cx| {
                        let keep_polling = view.poll_source_registry(cx);
                        if !keep_polling {
                            view.source_registry_task = None;
                        }
                        keep_polling
                    })
                    .unwrap_or(false);
                if !keep_polling {
                    break;
                }
            }
        }));
    }

    fn poll_source_registry(&mut self, cx: &mut Context<Self>) -> bool {
        self.sync_display_labels_projection();
        let mut events = Vec::new();
        let _ = cx
            .update_global::<SourceRegistryGlobal, _>(|global, _| global.drain_events(&mut events));
        let mut changed = false;
        for event in events {
            changed |= self.apply_source_registry_event(event, cx);
        }
        let mut catalog_deliveries = Vec::new();
        let catalog_events_capped = cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
            global.drain_catalog_deliveries(&mut catalog_deliveries)
        });
        for delivery in catalog_deliveries {
            changed |= self.apply_catalog_delivery(delivery, cx);
        }
        changed |= tags::poll(self, cx);
        changed |= tag_chips::poll(self, cx);
        changed |= self.retry_catalog_commands(cx);
        changed |= super::source_recovery::poll(self, cx);
        changed |= tag_completion::poll(self, cx);
        changed |= tag_filter::poll(self, cx);
        changed |= curation_queue::poll(self, cx);
        changed |= tag_library::poll(self, cx);
        changed |= self.poll_display_labels(cx);
        changed |= self.poll_file_drag();
        if changed {
            cx.notify();
        }
        !self.source_registry_loaded
            || !self.source_registry_pending_adds.is_empty()
            || !self.source_registry_pending_removes.is_empty()
            || self.source_registry_pending_edit.is_some()
            || catalog_events_capped
            || tags::has_pending(self)
            || tag_chips::has_pending(self)
            || tag_completion::has_pending(self)
            || tag_filter::has_pending(self)
            || curation_queue::has_pending(self)
            || tag_library::has_pending(self)
            || self.has_pending_display_labels()
            || self.file_drag_poll_needed()
            || catalog::catalog_poll_needed(self)
    }

    fn apply_source_registry_event(
        &mut self,
        event: RegistryEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        match event {
            RegistryEvent::Loading { revision } => {
                if revision < self.source_registry_revision {
                    return false;
                }
                self.source_registry_revision = revision;
                self.set_source_registry_status("Loading sources…".to_string());
                true
            }
            RegistryEvent::Snapshot { revision, snapshot } => {
                if revision < self.source_registry_revision {
                    return false;
                }
                self.source_registry_revision = revision;
                if self
                    .source_reorder_drag
                    .as_ref()
                    .is_some_and(|drag| drag.revision != revision)
                {
                    self.source_reorder_drag = None;
                }
                self.source_registry_loaded = true;
                self.apply_source_registry_snapshot(snapshot, cx);
                self.set_source_registry_status("Sources loaded".to_string());
                true
            }
            RegistryEvent::Persisted { revision } => {
                if revision < self.source_registry_persisted_revision {
                    return false;
                }
                self.source_registry_persisted_revision = revision;
                self.clear_persisted_registry_mutations();
                self.set_source_registry_status(
                    if self.source_registry_pending_adds.is_empty()
                        && self.source_registry_pending_removes.is_empty()
                        && self.source_registry_pending_edit.is_none()
                    {
                        "Sources saved".to_string()
                    } else {
                        "Saving source…".to_string()
                    },
                );
                true
            }
            RegistryEvent::RequestApplied { request, revision } => {
                if let Some((pending, applied)) = self.source_registry_pending_edit.as_mut()
                    && *pending == request
                {
                    *applied = Some(revision);
                }
                if let Some(mutation) = self.source_registry_pending_adds.get_mut(&request) {
                    mutation.revision = Some(revision);
                }
                let remove_root = self
                    .source_registry_pending_removes
                    .get(&request)
                    .map(|mutation| mutation.root.clone());
                if let Some(mutation) = self.source_registry_pending_removes.get_mut(&request) {
                    mutation.revision = Some(revision);
                }
                if let Some(root) = remove_root {
                    self.source_registry_pending_adds
                        .retain(|_, mutation| mutation.root != root);
                    self.remove_source_session(&root, cx);
                }
                if revision > self.source_registry_revision {
                    self.source_registry_revision = revision;
                }
                self.clear_persisted_registry_mutations();
                self.set_source_registry_status(
                    if self.source_registry_pending_adds.is_empty()
                        && self.source_registry_pending_removes.is_empty()
                        && self.source_registry_pending_edit.is_none()
                    {
                        "Sources saved".to_string()
                    } else {
                        "Saving source…".to_string()
                    },
                );
                true
            }
            RegistryEvent::RequestFailed { request, error } => {
                let message = error.to_string();
                if self
                    .source_registry_pending_edit
                    .as_ref()
                    .is_some_and(|(pending, _)| *pending == request)
                {
                    // A rejected request with no applied revision changed no role and can retry.
                    if self
                        .source_registry_pending_edit
                        .as_ref()
                        .is_some_and(|(_, revision)| revision.is_none())
                    {
                        self.source_primary_unconfirmed = false;
                    }
                    self.source_registry_pending_edit = None;
                }
                if let Some(mutation) = self.source_registry_pending_adds.remove(&request) {
                    self.set_source_error(&mutation.root, message.clone());
                }
                if let Some(mutation) = self.source_registry_pending_removes.remove(&request) {
                    self.source_registry_removed_roots.remove(&mutation.root);
                    self.set_source_error(&mutation.root, message.clone());
                }
                let suffix = if self.source_primary_unconfirmed {
                    "; primary changes are unconfirmed"
                } else {
                    ""
                };
                self.set_source_registry_status(format!(
                    "Source registry error: {message}{suffix}"
                ));
                true
            }
            RegistryEvent::PersistenceUnavailable { revision, error } => {
                self.source_registry_loaded = true;
                self.source_registry_revision = revision;
                self.source_reorder_drag = None;
                self.fail_pending_registry_mutations(error.to_string());
                self.set_source_registry_status(format!("Source persistence unavailable: {error}"));
                true
            }
            RegistryEvent::Closed => {
                self.source_registry_loaded = true;
                self.source_reorder_drag = None;
                self.fail_pending_registry_mutations("Source registry closed".to_string());
                self.set_source_registry_status("Source registry closed".to_string());
                true
            }
        }
    }

    pub(super) fn set_context_source_primary(
        &mut self,
        target: super::directory_open::DirectoryOpenTarget,
        primary: bool,
        revision: RegistryRevision,
        cx: &mut Context<Self>,
    ) {
        if target.kind != super::directory_open::DirectoryOpenKind::Source
            || !super::directory_open::directory_target_is_current(self, &target)
            || !self.source_registry_loaded
            || revision != self.source_registry_revision
            || self.source_registry_pending_edit.is_some()
            || self.source_primary_unconfirmed
            || !self.source_registry_pending_adds.is_empty()
            || !self.source_registry_pending_removes.is_empty()
            || self.source_sessions[target.source_index].role
                == yield_library::sample_sources::SourceRole::Protected
        {
            self.set_source_registry_status(
                "Primary source change canceled: source changed or is not writable/settled"
                    .to_string(),
            );
            cx.notify();
            return;
        }
        let result = if cx.has_global::<SourceRegistryGlobal>() {
            cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
                global.set_source_primary(target.source_id, primary, revision)
            })
        } else {
            Err(SubmitError::Closed)
        };
        match result {
            Ok(request) => {
                self.source_registry_pending_edit = Some((request, None));
                self.source_primary_unconfirmed = true;
                self.set_source_registry_status("Saving primary source…".to_string());
                self.start_source_registry_poll(cx);
            }
            Err(error) => self
                .set_source_registry_status(format!("Primary source change unavailable: {error}")),
        }
        cx.notify();
    }

    fn clear_persisted_registry_mutations(&mut self) {
        let persisted = self.source_registry_persisted_revision;
        if self
            .source_registry_pending_edit
            .as_ref()
            .is_some_and(|(_, revision)| revision.is_some_and(|revision| revision <= persisted))
        {
            self.source_registry_pending_edit = None;
            self.source_primary_unconfirmed = false;
        }
        self.source_registry_pending_adds.retain(|_, mutation| {
            !mutation
                .revision
                .is_some_and(|revision| revision <= persisted)
        });
        let removed_roots: Vec<PathBuf> = self
            .source_registry_pending_removes
            .iter()
            .filter(|(_, mutation)| {
                mutation
                    .revision
                    .is_some_and(|revision| revision <= persisted)
            })
            .map(|(_, mutation)| mutation.root.clone())
            .collect();
        self.source_registry_pending_removes.retain(|_, mutation| {
            !mutation
                .revision
                .is_some_and(|revision| revision <= persisted)
        });
        for root in removed_roots {
            self.source_registry_removed_roots.remove(&root);
        }
    }

    fn fail_pending_registry_mutations(&mut self, message: String) {
        self.source_registry_pending_edit = None;
        let add_roots: Vec<PathBuf> = self
            .source_registry_pending_adds
            .drain()
            .map(|(_, mutation)| mutation.root)
            .collect();
        let remove_roots: Vec<PathBuf> = self
            .source_registry_pending_removes
            .drain()
            .map(|(_, mutation)| mutation.root)
            .collect();
        for root in add_roots {
            self.set_source_error(&root, message.clone());
        }
        for root in remove_roots {
            self.source_registry_removed_roots.remove(&root);
            self.set_source_error(&root, message.clone());
        }
    }

    fn remove_source_session(&mut self, root: &Path, cx: &mut Context<Self>) {
        if self
            .compare_anchor
            .as_ref()
            .is_some_and(|anchor| anchor.source_root() == root)
        {
            self.compare_anchor_generation = self.compare_anchor_generation.wrapping_add(1);
            self.compare_anchor_task.take();
            self.compare_anchor_play_pending = false;
            self.compare_anchor = None;
            self.set_playback_status("Compare anchor source is no longer available");
        }
        let Some(index) = self
            .source_sessions
            .iter()
            .position(|session| session.root == root)
        else {
            return;
        };
        self.deactivate_catalog_for_session(index, cx);
        if self.active_source == Some(index) {
            self.clear_active_source_projection(cx);
        }
        self.source_sessions.remove(index);
        if let Some(active) = self.active_source {
            if active > index {
                self.active_source = Some(active - 1);
            }
        }
    }

    fn apply_source_registry_snapshot(
        &mut self,
        snapshot: yield_library::sample_sources::SourceRegistrySnapshot,
        cx: &mut Context<Self>,
    ) {
        let active_id = self
            .active_source
            .and_then(|index| self.source_sessions.get(index))
            .and_then(|session| session.source_id.clone());
        let active_root = self
            .active_source
            .and_then(|index| self.source_sessions.get(index))
            .map(|session| session.root.clone());
        let snapshot_roots: BTreeSet<PathBuf> = snapshot
            .as_slice()
            .iter()
            .map(|source| source.root.clone())
            .collect();
        self.source_registry_known_roots = snapshot_roots.clone();

        let mut previous = std::mem::take(&mut self.source_sessions);
        let in_flight_root = self
            .scan_cancel
            .is_some()
            .then(|| self.root.as_deref())
            .flatten()
            .map(Path::to_path_buf);
        if let Some(root) = in_flight_root.as_ref()
            && !previous.iter().any(|session| session.root == *root)
        {
            previous.push(SourceSession::new(root.clone()));
        }
        let mut sessions = Vec::with_capacity(snapshot.len());
        for source in snapshot.as_slice() {
            if self.source_registry_removed_roots.contains(&source.root) {
                continue;
            }
            let previous_index = previous
                .iter()
                .position(|session| source_session_matches(session, source));
            let mut session = previous_index
                .map(|index| previous.remove(index))
                .unwrap_or_else(|| SourceSession::from_source(source));
            let root_changed = session.root != source.root;
            session.source_id = Some(source.id.clone());
            session.role = source.role;
            session.root = source.root.clone();
            session.label = source_label(&source.root);
            if root_changed {
                session.entries.clear();
                session.tree = yield_core::FolderTree::default();
                session.catalog_facts.clear();
                session.selected_audio_id = None;
            }
            sessions.push(session);
        }

        let pending_add_roots: BTreeSet<PathBuf> = self
            .source_registry_pending_adds
            .values()
            .map(|mutation| mutation.root.clone())
            .collect();
        for session in previous {
            if should_retain_pending_session(
                &session,
                &pending_add_roots,
                &self.source_registry_removed_roots,
                in_flight_root.as_deref(),
            ) {
                sessions.push(session);
            }
        }
        self.source_sessions = sessions;
        if let Some(anchor) = self.compare_anchor.as_ref()
            && !self.compare_anchor_path_is_current(&anchor.path())
        {
            self.compare_anchor_generation = self.compare_anchor_generation.wrapping_add(1);
            self.compare_anchor_task.take();
            self.compare_anchor_play_pending = false;
            self.compare_anchor = None;
            self.set_playback_status("Compare anchor source is no longer available");
        }
        if let Some(root) = self.pending_source_return_root.as_ref() {
            self.pending_source_return_index = self
                .source_sessions
                .iter()
                .position(|session| &session.root == root);
        }

        for mutation in self.source_registry_pending_adds.values_mut() {
            if let Some(source) = snapshot
                .as_slice()
                .iter()
                .find(|source| source.root == mutation.root)
            {
                if self.source_registry_removed_roots.contains(&mutation.root) {
                    continue;
                }
                if let Some(session) = self
                    .source_sessions
                    .iter_mut()
                    .find(|session| session.root == source.root)
                {
                    session.source_id = Some(source.id.clone());
                    session.last_error = None;
                }
            }
        }
        let next_active = active_id
            .as_ref()
            .and_then(|id| {
                self.source_sessions
                    .iter()
                    .position(|session| session.source_id.as_ref() == Some(id))
            })
            .or_else(|| {
                active_root.as_ref().and_then(|root| {
                    self.source_sessions
                        .iter()
                        .position(|session| &session.root == root)
                })
            });
        if self.active_source.is_some() && next_active.is_none() {
            self.deactivate_catalog_source(cx);
            self.clear_active_source_projection(cx);
        } else {
            self.active_source = next_active;
        }

        let removed_roots: BTreeSet<PathBuf> = self
            .source_registry_pending_removes
            .values()
            .filter(|mutation| mutation.revision.is_some())
            .map(|mutation| mutation.root.clone())
            .collect();
        if !removed_roots.is_empty() {
            self.source_registry_pending_adds
                .retain(|_, mutation| !removed_roots.contains(&mutation.root));
            self.source_sessions
                .retain(|session| !removed_roots.contains(&session.root));
        }
        self.ensure_active_catalog_activation(cx);
    }

    pub(super) fn clear_active_source_projection(&mut self, cx: &mut Context<Self>) {
        tags::cancel_for_source_change(self);
        tag_chips::cancel_for_source_change(self);
        tag_completion::cancel_for_source_change(self, cx);
        tag_filter::cancel_for_source_change(self);
        curation_queue::cancel_for_source_change(self);
        self.collection_rename_editor = None;
        self.display_labels.cancel_for_source_change();
        super::source_recovery::cancel_for_source_change(self);
        self.reset_browser_focus_state();
        self.context_menu = None;
        self.stop_playback();
        self.playback_policy.clear_file();
        self.cancel_waveform_load();
        self.reset_catalog_metadata();
        self.scan_generation = self.scan_generation.wrapping_add(1);
        self.invalidate_waveform_generation();
        self.active_source = None;
        self.root = None;
        self.entries.clear();
        self.view_model.set_entries(&self.entries);
        self.reschedule_playback_age_refresh(cx);
        self.view_model
            .set_curation_queue(&self.entries, yield_core::CurationQueueMode::default());
        tag_filter::reinstall_after_projection(self, cx);
        curation_queue::reinstall_after_projection(self, cx);
        self.reset_collection_filter();
        self.selected = None;
        self.folder_tree = None;
        self.folder_rows_cache.clear();
        self.waveform_pending = false;
        self.waveform = None;
        self.waveform_error = None;
        self.set_scan_status("Choose a source folder to begin".to_string());
        cx.notify();
    }

    fn set_source_error(&mut self, root: &Path, error: String) {
        if let Some(session) = self
            .source_sessions
            .iter_mut()
            .find(|session| session.root == root)
        {
            session.last_error = Some(error);
        } else if self.root.as_deref() == Some(root) {
            self.set_scan_status(error);
        }
    }

    pub(super) fn submit_source_add(&mut self, root: PathBuf, cx: &mut Context<Self>) {
        if self.source_registry_removed_roots.contains(&root) {
            return;
        }
        if self.source_registry_known_roots.contains(&root) {
            return;
        }
        if self
            .source_sessions
            .iter()
            .any(|session| session.root == root && session.source_id.is_some())
            || self
                .source_registry_pending_adds
                .values()
                .any(|mutation| mutation.root == root)
        {
            return;
        }
        let Ok(canonical_root) = CanonicalSourceRoot::new(root.clone()) else {
            self.set_source_error(&root, "Scanned source root was not canonical".to_string());
            return;
        };
        let result = if cx.has_global::<SourceRegistryGlobal>() {
            cx.update_global::<SourceRegistryGlobal, _>(|global, _| global.add_root(canonical_root))
        } else {
            Err(SubmitError::Closed)
        };
        match result {
            Ok(request) => {
                self.source_registry_pending_adds.insert(
                    request,
                    PendingRegistryMutation {
                        root,
                        revision: None,
                    },
                );
                self.set_source_registry_status("Saving source…".to_string());
                self.start_source_registry_poll(cx);
            }
            Err(error) => {
                self.set_source_error(&root, format!("Could not save source: {error}"));
                self.set_source_registry_status(format!("Source registry error: {error}"));
            }
        }
    }

    pub(super) fn submit_source_remove(&mut self, root: PathBuf, cx: &mut Context<Self>) -> bool {
        if self
            .source_registry_pending_removes
            .values()
            .any(|mutation| mutation.root == root)
        {
            return true;
        }
        let has_registered_source = self
            .source_sessions
            .iter()
            .any(|session| session.root == root && session.source_id.is_some());
        let has_pending_add = self
            .source_registry_pending_adds
            .values()
            .any(|mutation| mutation.root == root);
        if !has_registered_source && !has_pending_add {
            self.source_registry_removed_roots.remove(&root);
            return true;
        }
        let Ok(canonical_root) = CanonicalSourceRoot::new(root.clone()) else {
            self.set_source_error(&root, "Source root is no longer canonical".to_string());
            return false;
        };
        let result = if cx.has_global::<SourceRegistryGlobal>() {
            cx.update_global::<SourceRegistryGlobal, _>(|global, _| {
                global.remove_root(canonical_root)
            })
        } else {
            Err(SubmitError::Closed)
        };
        match result {
            Ok(request) => {
                self.source_registry_removed_roots.insert(root.clone());
                self.source_registry_pending_removes.insert(
                    request,
                    PendingRegistryMutation {
                        root,
                        revision: None,
                    },
                );
                self.set_source_registry_status("Saving source removal…".to_string());
                self.start_source_registry_poll(cx);
                true
            }
            Err(error) => {
                self.set_source_error(&root, format!("Could not remove source: {error}"));
                self.set_source_registry_status(format!("Source registry error: {error}"));
                false
            }
        }
    }
}

fn source_session_matches(session: &SourceSession, source: &SampleSource) -> bool {
    session.source_id.as_ref() == Some(&source.id) || session.root == source.root
}

fn should_retain_pending_session(
    session: &SourceSession,
    pending_add_roots: &BTreeSet<PathBuf>,
    removed_roots: &BTreeSet<PathBuf>,
    in_flight_root: Option<&Path>,
) -> bool {
    session.source_id.is_none()
        && !removed_roots.contains(&session.root)
        && (pending_add_roots.contains(&session.root)
            || in_flight_root.is_some_and(|root| root == session.root))
}

#[cfg(test)]
mod tests {
    use super::{should_retain_pending_session, source_session_matches};
    use crate::browser::SourceSession;
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use yield_library::sample_sources::{SampleSource, SourceId};

    #[gpui::test]
    fn primary_source_persistence_failure_keeps_role_unconfirmed(cx: &mut gpui::TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| crate::browser::Browser::new(cx));
        browser.update(cx, |view, cx| {
            view.source_primary_unconfirmed = true;
            assert!(view.apply_source_registry_event(
                yield_session::RegistryEvent::PersistenceUnavailable {
                    revision: yield_session::RegistryRevision::default(),
                    error: yield_session::SessionError::Library("injected save failure".into()),
                },
                cx
            ));
            assert!(view.source_primary_unconfirmed);
            assert!(view.source_registry_pending_edit.is_none());
        });
    }

    #[test]
    fn restored_session_matches_by_durable_id_before_root_fallback() {
        let old_root = PathBuf::from("/library/old");
        let new_root = PathBuf::from("/library/new");
        let source = SampleSource::new_with_id(SourceId::from_string("stable"), new_root);
        let mut session = SourceSession::new(old_root);
        session.source_id = Some(SourceId::from_string("stable"));

        assert!(source_session_matches(&session, &source));
    }

    #[test]
    fn pending_unregistered_session_is_removed_after_failed_or_removed_add() {
        let root = PathBuf::from("/library/pending");
        let session = SourceSession::new(root.clone());
        let pending = BTreeSet::from([root.clone()]);
        let removed = BTreeSet::new();
        assert!(should_retain_pending_session(
            &session, &pending, &removed, None,
        ));

        let removed = BTreeSet::from([root]);
        assert!(!should_retain_pending_session(
            &session, &pending, &removed, None,
        ));
    }

    #[test]
    fn in_flight_scan_session_survives_initial_registry_snapshot() {
        let root = PathBuf::from("/library/scanning");
        let session = SourceSession::new(root.clone());
        let pending = BTreeSet::new();
        let removed = BTreeSet::new();

        assert!(should_retain_pending_session(
            &session,
            &pending,
            &removed,
            Some(root.as_path()),
        ));
    }
}
