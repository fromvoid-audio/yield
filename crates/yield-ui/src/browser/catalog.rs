//! Browser integration for the profile-owned source catalog.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{BorrowAppContext, Context};
use yield_core::{AudioFile, AudioFileCuration, AudioFileId, FolderTree, PlaybackType};
use yield_library::sample_sources::{
    BrowserFileMetadata, SourceId, SourcePlaybackType, SourcePublicationFence,
    reconciliation::RootIdentity,
};
use yield_session::{
    CatalogCommand, CatalogDelivery, CatalogEvent, CatalogOperation, CatalogPublicationStatus,
    CatalogStatus, CatalogSubmitError, VerifiedSourceRoot,
};

use super::source_registry::SourceRegistryGlobal;
use super::{Browser, SourceSession, catalog_metadata};

/// Filesystem facts retained with a catalog projection for playback invalidation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CatalogFileFacts {
    pub(super) relative_path: PathBuf,
    pub(super) file_size: u64,
    pub(super) modified_ns: i64,
    pub(super) file_identity: Option<String>,
    pub(super) content_hash: Option<String>,
}

/// The source lifecycle currently owned by the browser projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CatalogActivation {
    pub(super) source_id: SourceId,
    pub(super) root: PathBuf,
    pub(super) lifecycle_generation: u64,
    /// Whether the worker published a matching Registered event for this generation.
    pub(super) registered: bool,
    /// Whether the worker accepted this activation's Register command.
    ///
    /// A remove is only meaningful after registration admission. Keeping this
    /// separate from the lifecycle generation prevents failed or superseded
    /// activations from accumulating remove commands for sources that were
    /// never registered with the worker.
    pub(super) registration_admitted: bool,
    /// Last authoritative or verified physical root identity for this activation.
    pub(super) root_identity: Option<RootIdentity>,
    pub(super) audit_finished: bool,
    pub(super) audit_error: Option<String>,
    pub(super) audit_retryable: bool,
    pub(super) force_reload: bool,
    pub(super) fallback_to_scan: bool,
}

/// A catalog command waiting for bounded worker admission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CatalogCommandIntent {
    Register {
        source_id: SourceId,
        root: PathBuf,
        lifecycle_generation: u64,
    },
    RegisterVerified {
        source_id: SourceId,
        root: VerifiedSourceRoot,
        lifecycle_generation: u64,
    },
    Audit {
        source_id: SourceId,
        lifecycle_generation: u64,
    },
    Remove {
        source_id: SourceId,
        lifecycle_generation: u64,
    },
}

impl CatalogCommandIntent {
    fn source_id(&self) -> &SourceId {
        match self {
            Self::Register { source_id, .. }
            | Self::RegisterVerified { source_id, .. }
            | Self::Audit { source_id, .. }
            | Self::Remove { source_id, .. } => source_id,
        }
    }

    fn lifecycle_generation(&self) -> u64 {
        match self {
            Self::Register {
                lifecycle_generation,
                ..
            }
            | Self::RegisterVerified {
                lifecycle_generation,
                ..
            }
            | Self::Audit {
                lifecycle_generation,
                ..
            }
            | Self::Remove {
                lifecycle_generation,
                ..
            } => *lifecycle_generation,
        }
    }

    fn into_command(self) -> CatalogCommand {
        match self {
            Self::Register {
                source_id,
                root,
                lifecycle_generation,
            } => CatalogCommand::Register {
                source_id,
                canonical_root: root,
                lifecycle_generation,
            },
            Self::RegisterVerified {
                source_id,
                root,
                lifecycle_generation,
            } => CatalogCommand::RegisterVerified {
                source_id,
                root,
                lifecycle_generation,
            },
            Self::Audit {
                source_id,
                lifecycle_generation,
            } => CatalogCommand::RequestAudit {
                source_id,
                lifecycle_generation,
            },
            Self::Remove {
                source_id,
                lifecycle_generation,
            } => CatalogCommand::Remove {
                source_id,
                lifecycle_generation,
            },
        }
    }
}

struct CatalogProjection {
    source_id: SourceId,
    lifecycle_generation: u64,
    root: PathBuf,
    root_identity: RootIdentity,
    directory_generation: u64,
    committed_revision: u64,
    entries: Vec<AudioFile>,
    tree: FolderTree,
    facts: HashMap<PathBuf, CatalogFileFacts>,
    entry_indices: Arc<HashMap<PathBuf, usize>>,
}

impl Browser {
    /// Begin a catalog-backed source activation when the session worker is available.
    pub(super) fn begin_catalog_activation(
        &mut self,
        index: usize,
        force_reload: bool,
        preserve_transport: bool,
        fallback_to_scan: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::cancel_for_source_change(self);
        super::tags::cancel_for_source_change(self);
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        self.collection_rename_editor = None;
        self.display_labels.cancel_for_source_change();
        self.reset_browser_focus_state();
        self.context_menu = None;
        let Some((source_id, root, label)) = self
            .source_sessions
            .get(index)
            .and_then(|session| session.source_id.clone().map(|id| (id, session)))
            .map(|(source_id, session)| (source_id, session.root.clone(), session.label.clone()))
        else {
            return false;
        };
        if !matches!(
            catalog_status(cx),
            Some(CatalogStatus::Loading | CatalogStatus::Ready)
        ) {
            return false;
        }

        let same_active_source = self.active_source == Some(index)
            && self.root.as_deref() == Some(root.as_path())
            && self
                .active_source
                .and_then(|active| self.source_sessions.get(active))
                .and_then(|session| session.source_id.as_ref())
                == Some(&source_id);
        if self.catalog_activation.is_some() {
            self.deactivate_catalog_source(cx);
        }
        if !same_active_source {
            // A catalog source change invalidates a reveal captured from the previous source.
            // History handoff marks the destination active before calling this method, so its
            // same-source activation preserves the reveal while keeping transport alive.
            let _ = self.view_model.clear_reveal(&self.entries);
            self.playback_policy.clear_file();
            self.selection.clear();
            self.reset_collection_filter();
        }

        self.reset_focus();
        self.reset_scroll_edges();
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, Ordering::Release);
            self.scan_task.take();
            self.scan_generation = self.scan_generation.wrapping_add(1);
        }
        if preserve_transport {
            self.restore_selection_id = None;
            self.restore_selection_position = None;
        } else {
            self.restore_selection_id = if same_active_source {
                self.selected_id()
            } else {
                self.source_sessions
                    .get(index)
                    .and_then(|session| session.selected_audio_id.clone())
            };
            self.restore_selection_position = if same_active_source {
                self.selected_visible_position()
            } else {
                None
            };
            self.stop_playback();
            self.clear_waveform_region();
            self.cancel_waveform_load();
            self.waveform_pending = false;
            self.waveform = None;
            self.waveform_error = None;
            self.selected = None;
            self.invalidate_waveform_generation();
        }

        self.active_source = Some(index);
        self.pending_source_label = None;
        self.pending_source_return_index = None;
        self.pending_source_return_root = None;
        self.root = Some(root.clone());
        self.entries = self.source_sessions[index].entries.clone();
        self.folder_tree = Some(self.source_sessions[index].tree.clone());
        self.selected_folder = self.source_sessions[index].selected_folder.clone();
        self.expanded_folders = self.source_sessions[index].expanded_folders.clone();
        self.folder_scope = self.source_sessions[index].folder_scope;
        self.rebuild_folder_rows_cache();
        let focus_id = self.selected_id();
        let focus_position = self.selected_visible_position();
        super::tag_filter::preserve_focus_for_projection(
            self,
            focus_id,
            focus_position,
            super::tag_filter::FilterOrigin::PassiveMetadata,
        );
        self.view_model.set_entries(&self.entries);
        self.reschedule_playback_age_refresh(cx);
        self.view_model.set_curation_queue(
            &self.entries,
            self.source_sessions[index].curation_queue_mode,
        );
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        super::tag_filter::reinstall_after_projection(self, cx);
        super::curation_queue::reinstall_after_projection(self, cx);
        self.refresh_collection_filter_counts();
        self.reconcile_selection();

        self.catalog_next_generation = self.catalog_next_generation.wrapping_add(1);
        if self.catalog_next_generation == 0 {
            self.catalog_next_generation = 1;
        }
        let lifecycle_generation = self.catalog_next_generation;
        self.source_sessions[index].catalog_lifecycle_generation = Some(lifecycle_generation);
        self.catalog_activation = Some(CatalogActivation {
            source_id: source_id.clone(),
            root: root.clone(),
            lifecycle_generation,
            registered: false,
            registration_admitted: false,
            root_identity: None,
            audit_finished: false,
            audit_error: None,
            audit_retryable: false,
            force_reload,
            fallback_to_scan,
        });
        self.catalog_pending_command = Some(CatalogCommandIntent::Register {
            source_id,
            root,
            lifecycle_generation,
        });
        self.set_scan_status(format!("Loading catalog {label}…"));
        self.start_source_registry_poll(cx);
        cx.notify();
        true
    }

    /// Activate the durable catalog after a newly scanned source receives its stable ID.
    pub(super) fn ensure_active_catalog_activation(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.active_source else {
            return;
        };
        let Some(session) = self.source_sessions.get(index) else {
            return;
        };
        let Some(source_id) = session.source_id.clone() else {
            return;
        };
        let root = session.root.clone();
        if self
            .catalog_activation
            .as_ref()
            .is_some_and(|activation| activation.source_id == source_id && activation.root == root)
        {
            return;
        }
        let preserve_transport = self.catalog_activation.is_none();
        let _ = self.begin_catalog_activation(index, false, preserve_transport, false, cx);
    }

    /// Queue removal of the browser's current catalog projection.
    pub(super) fn deactivate_catalog_source(&mut self, cx: &mut Context<Self>) {
        self.job_action = None;
        self.job_action_error = None;
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::cancel_for_source_change(self);
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        self.display_labels.cancel_for_source_change();
        self.reset_catalog_metadata();
        let activation = self.catalog_activation.take();
        if let Some(activation) = activation {
            if activation.registration_admitted {
                self.queue_catalog_remove(
                    activation.source_id.clone(),
                    activation.lifecycle_generation,
                );
            }
            if let Some(index) = self.active_source
                && let Some(session) = self.source_sessions.get_mut(index)
                && session.source_id.as_ref() == Some(&activation.source_id)
                && session.catalog_lifecycle_generation == Some(activation.lifecycle_generation)
            {
                session.catalog_lifecycle_generation = None;
            }
        }
        self.curation_state.prune_completed_except(None);
        self.catalog_pending_command = None;
        self.cancel_catalog_projection();
        if !self.catalog_pending_removes.is_empty() {
            self.start_source_registry_poll(cx);
        }
    }

    /// Queue removal when a source row is forgotten or removed by registry reconciliation.
    pub(super) fn deactivate_catalog_for_session(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(session) = self.source_sessions.get(index) else {
            return;
        };
        let Some(source_id) = session.source_id.clone() else {
            return;
        };
        if self
            .catalog_activation
            .as_ref()
            .is_some_and(|activation| activation.source_id == source_id)
        {
            self.deactivate_catalog_source(cx);
        } else if !self.catalog_pending_removes.is_empty() {
            self.start_source_registry_poll(cx);
        }
        if let Some(session) = self.source_sessions.get_mut(index) {
            session.catalog_lifecycle_generation = None;
        }
    }

    fn queue_catalog_remove(&mut self, source_id: SourceId, lifecycle_generation: u64) {
        let intent = CatalogCommandIntent::Remove {
            source_id: source_id.clone(),
            lifecycle_generation,
        };
        coalesce_catalog_remove(&mut self.catalog_pending_removes, source_id, intent);
    }

    pub(super) fn cancel_catalog_projection(&mut self) {
        if let Some(cancellation) = self.catalog_projection_cancel.take() {
            cancellation.store(true, Ordering::Release);
        }
        self.catalog_projection_task.take();
    }

    pub(super) fn retry_catalog_commands(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(status) = catalog_status(cx) else {
            return false;
        };
        match status {
            CatalogStatus::Loading => return false,
            CatalogStatus::Unavailable { reason } => {
                return self.handle_catalog_unavailable(reason, cx);
            }
            CatalogStatus::Closed => {
                let changed = self.catalog_activation.is_some()
                    || self.catalog_pending_command.is_some()
                    || !self.catalog_pending_removes.is_empty();
                self.catalog_activation = None;
                self.catalog_pending_command = None;
                self.catalog_pending_removes.clear();
                super::source_recovery::cancel_for_source_change(self);
                super::rating_decay::cancel_for_source_change(self);
                super::tag_chips::cancel_for_source_change(self);
                super::tag_completion::cancel_for_source_change(self, cx);
                super::tag_filter::cancel_for_source_change(self);
                self.display_labels.cancel_for_source_change();
                self.cancel_catalog_projection();
                self.reset_catalog_metadata();
                self.curation_state.prune_completed_except(None);
                return changed;
            }
            CatalogStatus::Ready => {}
        }

        if let Some(intent) = self.catalog_pending_removes.values().next().cloned() {
            match submit_catalog(intent.clone(), cx) {
                Ok(()) => {
                    self.catalog_pending_removes.remove(intent.source_id());
                    return true;
                }
                Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => return false,
                Err(CatalogSubmitError::Unavailable(reason)) => {
                    return self.handle_catalog_unavailable(reason, cx);
                }
                Err(
                    CatalogSubmitError::Closed
                    | CatalogSubmitError::InvalidWavMetadataRequest(_)
                    | CatalogSubmitError::InvalidCurationRequest(_)
                    | CatalogSubmitError::InvalidTagPageRequest(_)
                    | CatalogSubmitError::InvalidTagFilterRequest(_)
                    | CatalogSubmitError::InvalidDisplayLabelsRequest(_)
                    | CatalogSubmitError::InvalidCurationTagFactsRequest(_)
                    | CatalogSubmitError::InvalidRatingDecayRequest(_),
                ) => {
                    return false;
                }
            }
        }
        if let Some(intent) = self.catalog_pending_command.clone() {
            let register_identity = match &intent {
                CatalogCommandIntent::Register {
                    source_id,
                    lifecycle_generation,
                    ..
                }
                | CatalogCommandIntent::RegisterVerified {
                    source_id,
                    lifecycle_generation,
                    ..
                } => Some((source_id.clone(), *lifecycle_generation)),
                CatalogCommandIntent::Audit { .. } | CatalogCommandIntent::Remove { .. } => None,
            };
            match submit_catalog(intent, cx) {
                Ok(()) => {
                    if let Some((source_id, lifecycle_generation)) = register_identity
                        && let Some(activation) = self.catalog_activation.as_mut()
                        && activation.source_id == source_id
                        && activation.lifecycle_generation == lifecycle_generation
                    {
                        activation.registration_admitted = true;
                        super::source_recovery::note_registration_admitted(
                            self,
                            &source_id,
                            lifecycle_generation,
                        );
                    }
                    self.catalog_pending_command = None;
                    return true;
                }
                Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => return false,
                Err(CatalogSubmitError::Unavailable(reason)) => {
                    return self.handle_catalog_unavailable(reason, cx);
                }
                Err(
                    CatalogSubmitError::Closed
                    | CatalogSubmitError::InvalidWavMetadataRequest(_)
                    | CatalogSubmitError::InvalidCurationRequest(_)
                    | CatalogSubmitError::InvalidTagPageRequest(_)
                    | CatalogSubmitError::InvalidTagFilterRequest(_)
                    | CatalogSubmitError::InvalidDisplayLabelsRequest(_)
                    | CatalogSubmitError::InvalidCurationTagFactsRequest(_)
                    | CatalogSubmitError::InvalidRatingDecayRequest(_),
                ) => return false,
            }
        }
        let mut changed = self.retry_catalog_metadata(cx);
        changed |= super::rating_decay::retry(self, cx);
        changed
    }

    fn handle_catalog_unavailable(&mut self, reason: String, cx: &mut Context<Self>) -> bool {
        let Some(activation) = self.catalog_activation.clone() else {
            self.catalog_pending_command = None;
            self.catalog_pending_removes.clear();
            super::source_recovery::cancel_for_source_change(self);
            super::rating_decay::cancel_for_source_change(self);
            super::tag_chips::cancel_for_source_change(self);
            super::tag_completion::cancel_for_source_change(self, cx);
            self.display_labels.cancel_for_source_change();
            self.reset_catalog_metadata();
            self.curation_state.prune_completed_except(None);
            return true;
        };
        self.catalog_pending_command = None;
        self.catalog_pending_removes.clear();
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::cancel_for_source_change(self);
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        self.display_labels.cancel_for_source_change();
        self.cancel_catalog_projection();
        self.reset_catalog_metadata();
        self.curation_state.prune_completed_except(None);
        self.catalog_activation = None;
        if activation.fallback_to_scan {
            self.set_scan_status(format!(
                "Catalog unavailable for {}; scanning source…",
                activation.root.display()
            ));
            self.begin_source_scan(activation.root, self.active_source, cx);
        } else {
            self.set_scan_status(format!("Catalog unavailable: {reason}"));
        }
        true
    }

    /// Apply one queued catalog delivery after its cheap source-publication liveness check.
    pub(super) fn apply_catalog_delivery(
        &mut self,
        delivery: CatalogDelivery,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some((event, publication_fence)) = delivery.into_parts_if_live() else {
            return false;
        };
        self.apply_catalog_event_with_fence(event, publication_fence, cx)
    }

    #[allow(dead_code)]
    pub(super) fn apply_catalog_event(
        &mut self,
        event: CatalogEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        self.apply_catalog_event_with_fence(event, None, cx)
    }

    fn apply_catalog_event_with_fence(
        &mut self,
        event: CatalogEvent,
        publication_fence: Option<SourcePublicationFence>,
        cx: &mut Context<Self>,
    ) -> bool {
        super::rating_decay::observe_snapshot(self, &event);
        if super::rating_decay::apply_catalog_event(self, &event, cx) {
            return true;
        }
        if matches!(
            &event,
            CatalogEvent::TagPageReady { .. } | CatalogEvent::TagPageFailed { .. }
        ) && (super::tag_completion::apply_catalog_event(self, &event, cx)
            || super::tags::apply_catalog_event(self, &event, cx)
            || super::tag_library::apply_catalog_event(self, &event, cx)
            || super::tag_chips::apply_catalog_event(self, &event))
        {
            return true;
        }
        if matches!(
            &event,
            CatalogEvent::DisplayLabelsReady { .. } | CatalogEvent::DisplayLabelsFailed { .. }
        ) && self.apply_display_labels_event(&event)
        {
            return true;
        }
        if matches!(
            &event,
            CatalogEvent::TagFilterReady { .. } | CatalogEvent::TagFilterFailed { .. }
        ) {
            return super::tag_filter::apply_catalog_event(self, event, cx);
        }
        if matches!(
            &event,
            CatalogEvent::CurationTagFactsReady { .. }
                | CatalogEvent::CurationTagFactsFailed { .. }
        ) {
            return super::curation_queue::apply_catalog_event(self, &event);
        }
        if matches!(event, CatalogEvent::CurationOperationCompleted { .. }) {
            // Compound outcomes belong to their source lifecycle even after the browser has
            // switched to another source. Consume them before the active projection fence so a
            // later receipt cannot be mistaken for a new source's metadata.
            let consumed = self.apply_curation_event(&event, cx);
            if consumed
                && matches!(
                    &event,
                    CatalogEvent::CurationOperationCompleted {
                        publication: CatalogPublicationStatus::Current,
                        ..
                    }
                )
            {
                let _ = self.maybe_start_rating_decay(cx);
            }
            return consumed;
        }
        if let CatalogEvent::Failed {
            operation: CatalogOperation::Register,
            ..
        } = &event
            && let Some((source_id, lifecycle_generation)) = catalog_event_identity(&event)
            && let Some(activation) = self.catalog_activation.as_mut()
            && activation.source_id == source_id
            && activation.lifecycle_generation == lifecycle_generation
        {
            activation.registered = false;
        }
        match super::source_recovery::before_catalog_event(self, &event, cx) {
            super::source_recovery::CatalogEventDisposition::Consume => return true,
            super::source_recovery::CatalogEventDisposition::Pass => {}
        }
        let Some((source_id, lifecycle_generation)) = catalog_event_identity(&event) else {
            if matches!(event, CatalogEvent::Closed) {
                self.job_action = None;
                self.job_action_error = None;
                super::source_recovery::cancel_for_source_change(self);
                super::rating_decay::cancel_for_source_change(self);
                super::tag_chips::cancel_for_source_change(self);
                super::tag_completion::cancel_for_source_change(self, cx);
                super::tag_filter::cancel_for_source_change(self);
                self.display_labels.cancel_for_source_change();
                self.catalog_activation = None;
                self.catalog_pending_command = None;
                self.catalog_pending_removes.clear();
                self.cancel_catalog_projection();
                self.reset_catalog_metadata();
                self.set_scan_status("Source catalog closed".to_string());
                return true;
            }
            return false;
        };
        if !self.catalog_activation.as_ref().is_some_and(|activation| {
            activation.source_id == source_id
                && activation.lifecycle_generation == lifecycle_generation
        }) {
            return false;
        }
        match &event {
            CatalogEvent::Registered { .. } => {
                if let Some(activation) = self.catalog_activation.as_mut() {
                    activation.registered = true;
                }
                self.catalog_pending_command = Some(CatalogCommandIntent::Audit {
                    source_id,
                    lifecycle_generation,
                });
                self.set_scan_status("Auditing source catalog…".to_string());
                true
            }
            CatalogEvent::SnapshotReady { .. } => {
                self.start_catalog_projection(event, publication_fence, cx);
                true
            }
            CatalogEvent::AuditCommitted { .. } => {
                self.job_action = None;
                self.job_action_error = None;
                super::rating_decay::note_audit_committed(self);
                if let Some(activation) = self.catalog_activation.as_mut() {
                    activation.audit_finished = true;
                    activation.audit_error = None;
                    activation.audit_retryable = false;
                }
                if let CatalogEvent::AuditCommitted { snapshot } = &event
                    && let Some(activation) = self.catalog_activation.as_mut()
                {
                    activation.root_identity = Some(snapshot.root_identity.clone());
                }
                self.set_scan_status("Catalog audit committed…".to_string());
                self.start_catalog_projection(event, publication_fence, cx);
                true
            }
            CatalogEvent::AuditCanceled { .. } => {
                self.job_action = None;
                self.job_action_error = None;
                if let Some(activation) = self.catalog_activation.as_mut() {
                    activation.audit_finished = true;
                    activation.audit_error = Some("Audit canceled; Retry when ready".into());
                    activation.audit_retryable = true;
                }
                self.set_scan_status("Source audit canceled; Retry when ready");
                true
            }
            CatalogEvent::WavMetadataReady { .. } | CatalogEvent::WavMetadataFailed { .. } => {
                self.apply_catalog_metadata_event(&event)
            }
            CatalogEvent::CurationCommitted {
                source_id,
                lifecycle_generation,
                publication: CatalogPublicationStatus::NonProjectable,
                ..
            } => {
                self.invalidate_source_publication(
                    cx,
                    source_id,
                    *lifecycle_generation,
                    "Curation committed, but the source changed; refresh the catalog",
                );
                true
            }
            CatalogEvent::CurationCommitted { .. }
            | CatalogEvent::CurationFailed { .. }
            | CatalogEvent::CurationOperationCompleted { .. }
            | CatalogEvent::TagPageReady { .. }
            | CatalogEvent::TagPageFailed { .. }
            | CatalogEvent::TagFilterReady { .. }
            | CatalogEvent::TagFilterFailed { .. }
            | CatalogEvent::DisplayLabelsReady { .. }
            | CatalogEvent::DisplayLabelsFailed { .. }
            | CatalogEvent::CurationTagFactsReady { .. }
            | CatalogEvent::CurationTagFactsFailed { .. }
            | CatalogEvent::RatingDecayProgress { .. }
            | CatalogEvent::RatingDecayFailed { .. } => false,
            CatalogEvent::Failed {
                error, operation, ..
            } => {
                if *operation == CatalogOperation::Audit {
                    self.job_action = None;
                }
                super::tag_chips::cancel_for_source_change(self);
                super::tag_completion::cancel_for_source_change(self, cx);
                super::tag_filter::cancel_for_source_change(self);
                self.display_labels.cancel_for_source_change();
                self.reset_catalog_metadata();
                if let Some(activation) = self.catalog_activation.as_mut() {
                    activation.audit_finished = true;
                    activation.audit_error = Some(error.to_string());
                    activation.audit_retryable =
                        *operation == CatalogOperation::Audit && error.audit_retryable();
                }
                self.set_scan_status(format!("Catalog update failed: {error}"));
                if let Some(index) = self.active_source
                    && let Some(session) = self.source_sessions.get_mut(index)
                {
                    session.last_error = Some(error.to_string());
                }
                true
            }
            CatalogEvent::Removed { .. } => {
                super::tag_chips::cancel_for_source_change(self);
                super::tag_completion::cancel_for_source_change(self, cx);
                super::tag_filter::cancel_for_source_change(self);
                false
            }
            CatalogEvent::Closed => false,
        }
    }

    fn start_catalog_projection(
        &mut self,
        event: CatalogEvent,
        publication_fence: Option<SourcePublicationFence>,
        cx: &mut Context<Self>,
    ) {
        let Some(activation) = self.catalog_activation.as_ref() else {
            return;
        };
        let source_id = activation.source_id.clone();
        let lifecycle_generation = activation.lifecycle_generation;
        let force_reload = activation.force_reload;
        let root = activation.root.clone();
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        self.reset_catalog_metadata();
        self.cancel_catalog_projection();
        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        let worker_publication_fence = publication_fence.clone();
        self.catalog_projection_cancel = Some(Arc::clone(&cancellation));
        self.catalog_projection_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    project_catalog_event_with_fence(
                        event,
                        root,
                        &worker_cancellation,
                        worker_publication_fence,
                    )
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                let current_token = view
                    .catalog_projection_cancel
                    .as_ref()
                    .filter(|flag| Arc::ptr_eq(flag, &cancellation))
                    .filter(|flag| !flag.load(Ordering::Acquire));
                if current_token.is_none() {
                    return;
                }
                view.catalog_projection_cancel = None;
                view.catalog_projection_task = None;
                if publication_fence
                    .as_ref()
                    .is_some_and(|fence| !fence.is_live())
                {
                    return;
                }
                if !view.catalog_activation.as_ref().is_some_and(|activation| {
                    activation.source_id == source_id
                        && activation.lifecycle_generation == lifecycle_generation
                }) {
                    return;
                }
                match result {
                    Ok(projection) => view.apply_catalog_projection(projection, force_reload, cx),
                    Err(error) => {
                        view.set_scan_status(format!("Catalog projection failed: {error}"));
                        if let Some(index) = view.active_source
                            && let Some(session) = view.source_sessions.get_mut(index)
                        {
                            session.last_error = Some(error);
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    fn apply_catalog_projection(
        &mut self,
        projection: CatalogProjection,
        force_reload: bool,
        cx: &mut Context<Self>,
    ) {
        self.reset_browser_focus_state();
        self.context_menu = None;
        let Some(index) = self.active_source.filter(|index| {
            self.source_sessions.get(*index).is_some_and(|session| {
                session.source_id.as_ref() == Some(&projection.source_id)
                    && session.catalog_lifecycle_generation == Some(projection.lifecycle_generation)
                    && session.root == projection.root
            })
        }) else {
            return;
        };
        let audit_ready = self.catalog_activation.as_ref().is_some_and(|activation| {
            activation.audit_finished && activation.audit_error.is_none()
        });
        if audit_ready {
            if let Some(activation) = self.catalog_activation.as_mut() {
                activation.root_identity = Some(projection.root_identity.clone());
            }
            self.curation_state.install_projection(
                super::curation::CurationLifecycleKey {
                    source_id: projection.source_id.clone(),
                    lifecycle_generation: projection.lifecycle_generation,
                },
                projection.root_identity.clone(),
                projection.directory_generation,
                projection.committed_revision,
                &projection.entries,
            );
            self.catalog_metadata
                .install_projection(catalog_metadata::CatalogMetadataContext {
                    source_id: projection.source_id.clone(),
                    lifecycle_generation: projection.lifecycle_generation,
                    root: projection.root.clone(),
                    root_identity: projection.root_identity.clone(),
                    directory_generation: projection.directory_generation,
                    revision: projection.committed_revision,
                    entry_indices: Arc::clone(&projection.entry_indices),
                });
            self.display_labels
                .install_projection(super::display_labels::DisplayLabelsContext {
                    source_id: projection.source_id.clone(),
                    lifecycle_generation: projection.lifecycle_generation,
                    root: projection.root.clone(),
                    root_identity: projection.root_identity.clone(),
                    directory_generation: projection.directory_generation,
                    revision: projection.committed_revision,
                    entry_indices: Arc::clone(&projection.entry_indices),
                });
        } else {
            self.reset_catalog_metadata();
        }
        let previous_id = self.selected_id();
        let previous_position = self.selected_visible_position();
        let restore_id = self.restore_selection_id.take().or(previous_id.clone());
        let restore_position = self.restore_selection_position.take().or(previous_position);
        super::tag_filter::preserve_focus_for_projection(
            self,
            restore_id.clone(),
            restore_position,
            super::tag_filter::FilterOrigin::PassiveMetadata,
        );
        super::curation_queue::preserve_focus_for_projection(self);
        let focused_changed = restore_id.as_ref().is_some_and(|id| {
            focused_manifest_changed(id, &projection, &self.source_sessions[index])
        });
        let apply_force_reload = force_reload
            && self
                .catalog_activation
                .as_ref()
                .is_some_and(|activation| activation.force_reload);
        let should_reload = apply_force_reload || focused_changed;
        if should_reload {
            self.stop_playback();
            self.cancel_waveform_load();
            self.waveform_pending = false;
            self.waveform = None;
            self.waveform_error = None;
            self.selected = None;
            self.invalidate_waveform_generation();
        }

        let (previous_folder, previous_expanded, previous_scope) = {
            let session = &self.source_sessions[index];
            (
                session.selected_folder.clone(),
                session.expanded_folders.clone(),
                session.folder_scope,
            )
        };
        let state = projection
            .tree
            .reconcile_state(Some(&previous_folder), &previous_expanded);
        let count = projection.entries.len();
        let entries = projection.entries;
        let tree = projection.tree;
        let facts = projection.facts;
        {
            let session = &mut self.source_sessions[index];
            session.entries = entries.clone();
            session.tree = tree.clone();
            session.catalog_facts = facts;
            session.selected_folder = state.selected.clone();
            session.expanded_folders = state.expanded.clone();
            session.folder_scope = previous_scope;
            session.last_error = self
                .catalog_activation
                .as_ref()
                .and_then(|activation| activation.audit_error.clone());
        }
        self.entries = entries;
        self.folder_tree = Some(tree);
        self.selected_folder = state.selected;
        self.expanded_folders = state.expanded;
        self.folder_scope = previous_scope;
        self.rebuild_folder_rows_cache();
        self.view_model.set_entries(&self.entries);
        self.reschedule_playback_age_refresh(cx);
        self.view_model.set_curation_queue(
            &self.entries,
            self.source_sessions[index].curation_queue_mode,
        );
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        super::tag_filter::reinstall_after_projection(self, cx);
        super::curation_queue::reinstall_after_projection(self, cx);
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        super::tag_chips::invalidate(self);
        self.start_source_registry_poll(cx);
        if let Some(activation) = self.catalog_activation.as_mut()
            && apply_force_reload
        {
            activation.force_reload = false;
        }
        self.set_scan_status(
            if let Some(error) = self
                .catalog_activation
                .as_ref()
                .and_then(|activation| activation.audit_error.as_ref())
            {
                format!("Catalog update failed: {error}")
            } else {
                format!(
                    "{count} audio file{} · catalog ready",
                    if count == 1 { "" } else { "s" }
                )
            },
        );
        self.restore_projection_focus(restore_id, restore_position, cx);
        if audit_ready {
            let _ = self.maybe_start_rating_decay(cx);
        }
    }

    /// Retire source-scoped projections after a durable result loses its publication fence.
    pub(super) fn invalidate_source_publication(
        &mut self,
        cx: &mut Context<Browser>,
        source_id: &SourceId,
        lifecycle_generation: u64,
        status: &str,
    ) {
        if !self.catalog_activation.as_ref().is_some_and(|activation| {
            activation.source_id == *source_id
                && activation.lifecycle_generation == lifecycle_generation
        }) {
            return;
        }
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::invalidate_for_source_publication(self, status);
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        self.display_labels.cancel_for_source_change();
        self.catalog_pending_command = None;
        self.cancel_catalog_projection();
        self.reset_catalog_metadata();
        if let Some(activation) = self.catalog_activation.as_mut() {
            activation.audit_finished = false;
            activation.audit_error = Some(status.to_string());
            activation.audit_retryable = false;
        }
        self.set_scan_status(status.to_string());
    }
}

pub(super) fn catalog_status(cx: &Context<Browser>) -> Option<CatalogStatus> {
    cx.has_global::<SourceRegistryGlobal>()
        .then(|| cx.global::<SourceRegistryGlobal>().catalog_status())
}

fn submit_catalog(
    intent: CatalogCommandIntent,
    cx: &mut Context<Browser>,
) -> Result<(), CatalogSubmitError> {
    submit_catalog_command(intent.into_command(), cx)
}

pub(super) fn submit_catalog_command(
    command: CatalogCommand,
    cx: &mut Context<Browser>,
) -> Result<(), CatalogSubmitError> {
    if !cx.has_global::<SourceRegistryGlobal>() {
        return Err(CatalogSubmitError::Closed);
    }
    cx.update_global::<SourceRegistryGlobal, _>(|global, _| global.submit_catalog(command))
}

fn catalog_event_identity(event: &CatalogEvent) -> Option<(SourceId, u64)> {
    match event {
        CatalogEvent::Registered {
            source_id,
            lifecycle_generation,
        }
        | CatalogEvent::Removed {
            source_id,
            lifecycle_generation,
        }
        | CatalogEvent::AuditCanceled {
            source_id,
            lifecycle_generation,
        } => Some((source_id.clone(), *lifecycle_generation)),
        CatalogEvent::SnapshotReady { snapshot } | CatalogEvent::AuditCommitted { snapshot } => {
            Some((snapshot.source_id.clone(), snapshot.lifecycle_generation))
        }
        CatalogEvent::WavMetadataReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::WavMetadataFailed {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::TagPageReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::TagPageFailed {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::TagFilterReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::DisplayLabelsReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::DisplayLabelsFailed {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::CurationTagFactsReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::CurationTagFactsFailed {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::TagFilterFailed {
            source_id,
            lifecycle_generation,
            ..
        } => Some((source_id.clone(), *lifecycle_generation)),
        CatalogEvent::CurationCommitted {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::CurationFailed {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::RatingDecayProgress {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::RatingDecayFailed {
            source_id,
            lifecycle_generation,
            ..
        } => Some((source_id.clone(), *lifecycle_generation)),
        CatalogEvent::CurationOperationCompleted {
            source_id,
            lifecycle_generation,
            ..
        } => Some((source_id.clone(), *lifecycle_generation)),
        CatalogEvent::Failed {
            source_id: Some(source_id),
            lifecycle_generation: Some(lifecycle_generation),
            ..
        } => Some((source_id.clone(), *lifecycle_generation)),
        CatalogEvent::Failed { .. } | CatalogEvent::Closed => None,
    }
}

#[allow(dead_code)]
fn project_catalog_event(
    event: CatalogEvent,
    root: PathBuf,
    cancellation: &AtomicBool,
) -> Result<CatalogProjection, String> {
    project_catalog_event_with_fence(event, root, cancellation, None)
}

fn project_catalog_event_with_fence(
    event: CatalogEvent,
    root: PathBuf,
    cancellation: &AtomicBool,
    publication_fence: Option<SourcePublicationFence>,
) -> Result<CatalogProjection, String> {
    project_catalog_event_with_fence_observer(event, root, cancellation, publication_fence, || {})
}

fn project_catalog_event_with_fence_observer<F>(
    event: CatalogEvent,
    root: PathBuf,
    cancellation: &AtomicBool,
    publication_fence: Option<SourcePublicationFence>,
    after_initial_validation: F,
) -> Result<CatalogProjection, String>
where
    F: FnOnce(),
{
    let snapshot = match event {
        CatalogEvent::SnapshotReady { snapshot } | CatalogEvent::AuditCommitted { snapshot } => {
            snapshot
        }
        _ => return Err("catalog event did not contain a snapshot".to_string()),
    };
    if cancellation.load(Ordering::Acquire) {
        return Err("catalog projection canceled".to_string());
    }
    if let Some(fence) = publication_fence.as_ref() {
        fence
            .validate()
            .map_err(|error| format!("catalog publication fence invalid: {error}"))?;
    }
    after_initial_validation();
    let source_id = snapshot.source_id;
    let lifecycle_generation = snapshot.lifecycle_generation;
    let root_identity = snapshot.root_identity;
    let directory_generation = snapshot.directory_generation;
    let committed_revision = snapshot.committed_revision;
    let manifest = snapshot.manifest;
    let directories = snapshot.directories;
    let wav_metadata = snapshot.wav_metadata;
    let curation_metadata = snapshot.curation_metadata;
    let mut entries = Vec::with_capacity(manifest.len());
    let mut facts = HashMap::with_capacity(manifest.len());
    let mut entry_indices = HashMap::with_capacity(manifest.len());
    for manifest in manifest {
        if cancellation.load(Ordering::Acquire) {
            return Err("catalog projection canceled".to_string());
        }
        validate_relative_path(&manifest.relative_path)?;
        let entry_index = entries.len();
        entry_indices.insert(manifest.relative_path.clone(), entry_index);
        let path = root.join(&manifest.relative_path);
        let (duration_seconds, sample_rate) = wav_metadata
            .get(&manifest.relative_path)
            .and_then(|metadata| {
                (metadata.sample_rate > 0).then(|| {
                    (
                        Some(metadata.frames as f32 / metadata.sample_rate as f32),
                        Some(metadata.sample_rate),
                    )
                })
            })
            .unwrap_or((None, None));
        entries.push(AudioFile {
            path,
            relative_path: manifest.relative_path.clone(),
            byte_len: manifest.file_size,
            duration_seconds,
            sample_rate,
            playback_type: curation_metadata
                .get(&manifest.relative_path)
                .and_then(playback_type_from_metadata),
            curation: curation_metadata
                .get(&manifest.relative_path)
                .and_then(curation_from_metadata),
        });
        facts.insert(
            manifest.relative_path.clone(),
            CatalogFileFacts {
                relative_path: manifest.relative_path,
                file_size: manifest.file_size,
                modified_ns: manifest.modified_ns,
                file_identity: manifest.file_identity,
                content_hash: manifest.content_hash,
            },
        );
    }
    let mut directory_paths = Vec::with_capacity(directories.len());
    for directory in directories {
        if cancellation.load(Ordering::Acquire) {
            return Err("catalog projection canceled".to_string());
        }
        validate_relative_path(&directory.relative_path)?;
        directory_paths.push(directory.relative_path);
    }
    if cancellation.load(Ordering::Acquire) {
        return Err("catalog projection canceled".to_string());
    }
    let tree = FolderTree::from_scan(&directory_paths, &entries);
    if let Some(fence) = publication_fence.as_ref() {
        fence
            .validate()
            .map_err(|error| format!("catalog publication fence invalid: {error}"))?;
    }
    Ok(CatalogProjection {
        source_id,
        lifecycle_generation,
        root,
        root_identity,
        directory_generation,
        committed_revision,
        entries,
        tree,
        facts,
        entry_indices: Arc::new(entry_indices),
    })
}

fn playback_type_from_metadata(metadata: &BrowserFileMetadata) -> Option<PlaybackType> {
    match metadata.playback_type {
        Some(SourcePlaybackType::OneShot) => Some(PlaybackType::OneShot),
        Some(SourcePlaybackType::Loop) => Some(PlaybackType::Loop),
        None => None,
    }
}

fn curation_from_metadata(metadata: &BrowserFileMetadata) -> Option<AudioFileCuration> {
    let collection_mask = metadata.collections.iter().fold(0_u8, |mask, collection| {
        let slot = usize::from(collection.index());
        (slot < yield_core::COLLECTION_COUNT)
            .then(|| mask | (1 << slot))
            .unwrap_or(mask)
    });
    AudioFileCuration::from_parts(
        metadata.rating.val(),
        metadata.locked,
        collection_mask,
        metadata.last_played_at,
        metadata.last_curated_at,
    )
}

fn validate_relative_path(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(format!("invalid catalog relative path: {}", path.display()));
    }
    if path.components().any(|component| {
        matches!(
            component,
            Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(format!("invalid catalog relative path: {}", path.display()));
    }
    Ok(())
}

fn coalesce_catalog_remove(
    pending: &mut HashMap<SourceId, CatalogCommandIntent>,
    source_id: SourceId,
    intent: CatalogCommandIntent,
) {
    match pending.entry(source_id) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            if entry.get().lifecycle_generation() < intent.lifecycle_generation() {
                entry.insert(intent);
            }
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(intent);
        }
    }
}

fn focused_manifest_changed(
    focused_id: &AudioFileId,
    projection: &CatalogProjection,
    session: &SourceSession,
) -> bool {
    let Ok(relative) = focused_id.path().strip_prefix(&projection.root) else {
        return true;
    };
    match (
        session.catalog_facts.get(relative),
        projection.facts.get(relative),
    ) {
        (Some(previous), Some(current)) => previous != current,
        (Some(_), None) => true,
        (None, Some(_)) | (None, None) => true,
    }
}

pub(super) fn is_catalog_pending(browser: &Browser, root: &Path) -> bool {
    browser
        .catalog_activation
        .as_ref()
        .is_some_and(|activation| {
            activation.root == root
                && (!activation.audit_finished
                    || browser.catalog_pending_command.is_some()
                    || browser.catalog_projection_task.is_some())
        })
}

pub(super) fn is_catalog_busy(browser: &Browser) -> bool {
    browser.catalog_projection_task.is_some()
        || browser.catalog_pending_command.is_some()
        || !browser.catalog_pending_removes.is_empty()
        || browser
            .catalog_activation
            .as_ref()
            .is_some_and(|activation| !activation.audit_finished)
}

pub(super) fn catalog_poll_needed(browser: &Browser) -> bool {
    // Native watcher audits can publish after the browser becomes idle. Keep draining the
    // bounded event queue; unchanged polls do not notify GPUI or mark the catalog busy.
    browser.catalog_activation.is_some()
        || is_catalog_busy(browser)
        || browser.catalog_metadata.poll_needed()
        || browser.curation_state.has_pending_outcomes()
        || super::rating_decay::poll_needed(browser)
}

#[cfg(test)]
mod tests {
    use super::{
        CatalogActivation, CatalogCommandIntent, CatalogFileFacts, project_catalog_event,
        project_catalog_event_with_fence_observer, validate_relative_path,
    };
    use super::{
        CatalogProjection, catalog_event_identity, coalesce_catalog_remove,
        focused_manifest_changed,
    };
    use std::collections::{BTreeMap, HashMap};
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use tempfile::tempdir;
    use yield_catalog::CatalogWavMetadata;
    use yield_core::{AudioFile, FolderTree, PlaybackType};
    use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use yield_library::sample_sources::{
        BrowserFileMetadata, Rating, SampleCollection, SourceDatabase, SourceId,
        SourceManifestEntry, SourcePlaybackType, db::SourceDirectoryEntry,
    };

    #[test]
    fn command_intents_preserve_source_and_generation() {
        let source_id = SourceId::from_string("source");
        let intent = CatalogCommandIntent::Remove {
            source_id: source_id.clone(),
            lifecycle_generation: 7,
        };
        assert_eq!(intent.source_id(), &source_id);
        assert_eq!(intent.lifecycle_generation(), 7);
    }

    #[test]
    fn catalog_events_are_fenced_by_source_and_lifecycle_generation() {
        let source_id = SourceId::from_string("source");
        let event = yield_session::CatalogEvent::Registered {
            source_id: source_id.clone(),
            lifecycle_generation: 4,
        };
        assert_eq!(catalog_event_identity(&event), Some((source_id, 4)));
        let stale = yield_session::CatalogEvent::Removed {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
        };
        assert_ne!(
            catalog_event_identity(&stale),
            catalog_event_identity(&event)
        );
    }

    #[test]
    fn non_snapshot_events_fail_projection_without_membership() {
        let event = yield_session::CatalogEvent::Registered {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 1,
        };
        let cancellation = std::sync::atomic::AtomicBool::new(false);
        let error = project_catalog_event(event, PathBuf::from("/library"), &cancellation)
            .err()
            .expect("registration has no snapshot to project");
        assert!(error.contains("did not contain a snapshot"));
    }

    #[test]
    fn relative_paths_reject_ambiguous_components() {
        assert!(validate_relative_path(PathBuf::from("kick.wav").as_path()).is_ok());
        assert!(validate_relative_path(PathBuf::from("./kick.wav").as_path()).is_err());
        assert!(validate_relative_path(PathBuf::from("../kick.wav").as_path()).is_err());
        assert!(validate_relative_path(PathBuf::from("/tmp/kick.wav").as_path()).is_err());
    }

    #[test]
    fn catalog_projection_uses_authoritative_snapshot_membership() {
        let root = PathBuf::from("/library");
        let source_id = SourceId::from_string("ui-catalog-source");
        let snapshot = yield_catalog::CatalogSnapshot {
            source_id,
            root_identity: yield_library::sample_sources::reconciliation::RootIdentity::from_bytes(
                b"root-identity".to_vec(),
            ),
            lifecycle_generation: 1,
            committed_revision: 4,
            directory_generation: 2,
            manifest: vec![yield_library::sample_sources::SourceManifestEntry {
                relative_path: PathBuf::from("drums/kicks/kick é.wav"),
                file_identity: Some(String::from("file-identity")),
                content_hash: Some(String::from("content-hash")),
                file_size: 16,
                modified_ns: 20,
            }],
            directories: vec![
                SourceDirectoryEntry {
                    relative_path: PathBuf::from("empty"),
                    directory_identity: String::from("empty-identity"),
                },
                SourceDirectoryEntry {
                    relative_path: PathBuf::from("drums"),
                    directory_identity: String::from("drums-identity"),
                },
                SourceDirectoryEntry {
                    relative_path: PathBuf::from("drums/kicks"),
                    directory_identity: String::from("kicks-identity"),
                },
            ],
            wav_metadata: BTreeMap::from([(
                PathBuf::from("drums/kicks/kick é.wav"),
                CatalogWavMetadata {
                    frames: 48_000,
                    sample_rate: 48_000,
                },
            )]),
            curation_metadata: BTreeMap::from([(
                PathBuf::from("drums/kicks/kick é.wav"),
                BrowserFileMetadata {
                    relative_path: PathBuf::from("drums/kicks/kick é.wav"),
                    file_size: 16,
                    modified_ns: 20,
                    missing: false,
                    rating: Rating::new(2),
                    locked: true,
                    collections: vec![
                        SampleCollection::new(0).expect("valid collection"),
                        SampleCollection::new(4).expect("valid collection"),
                    ],
                    last_played_at: Some(10),
                    last_curated_at: Some(11),
                    playback_type: Some(SourcePlaybackType::Loop),
                },
            )]),
        };
        let event = yield_session::CatalogEvent::AuditCommitted { snapshot };
        let cancellation = std::sync::atomic::AtomicBool::new(false);
        let projection =
            project_catalog_event(event, root.clone(), &cancellation).expect("project snapshot");

        assert_eq!(projection.entries.len(), 1);
        let entry = &projection.entries[0];
        assert_eq!(entry.path, root.join("drums/kicks/kick é.wav"));
        assert_eq!(entry.relative_path, PathBuf::from("drums/kicks/kick é.wav"));
        assert_eq!(entry.byte_len, 16);
        assert_eq!(entry.duration_seconds, Some(1.0));
        assert_eq!(entry.sample_rate, Some(48_000));
        assert_eq!(entry.playback_type, Some(PlaybackType::Loop));
        let curation = entry.curation.expect("catalog curation metadata");
        assert_eq!(curation.rating().value(), 2);
        assert!(curation.rating().locked());
        assert_eq!(curation.collection_mask(), 0b0001_0001);
        assert_eq!(curation.last_played_at(), Some(10));
        assert!(projection.tree.contains(Path::new("empty")));
        assert!(projection.tree.contains(Path::new("drums/kicks")));
        assert!(
            !projection
                .entries
                .iter()
                .any(|entry| entry.relative_path == PathBuf::from("added-after-audit.wav"))
        );
    }

    #[test]
    fn canceled_catalog_projection_does_not_publish_membership() {
        let snapshot = yield_catalog::CatalogSnapshot {
            source_id: SourceId::from_string("source"),
            root_identity: yield_library::sample_sources::reconciliation::RootIdentity::from_bytes(
                b"root".to_vec(),
            ),
            lifecycle_generation: 1,
            committed_revision: 1,
            directory_generation: 1,
            manifest: Vec::new(),
            directories: Vec::new(),
            wav_metadata: BTreeMap::new(),
            curation_metadata: BTreeMap::new(),
        };
        let event = yield_session::CatalogEvent::SnapshotReady { snapshot };
        let cancellation = std::sync::atomic::AtomicBool::new(true);
        let error = match project_catalog_event(event, PathBuf::from("/library"), &cancellation) {
            Ok(_) => panic!("canceled projection must not publish"),
            Err(error) => error,
        };
        assert!(error.contains("canceled"));
    }

    #[test]
    fn catalog_projection_rechecks_retired_fence_after_background_work() {
        let config_root = tempdir().expect("temporary config root");
        let _base = ConfigBaseGuard::set(
            config_root
                .path()
                .canonicalize()
                .expect("canonical config root"),
        );
        let _profile = PersistenceProfileGuard::named("ui-catalog-publication-fence");
        let profile_guard =
            WritableProfileGuard::acquire_current().expect("fixture profile ownership");
        let source_root = tempdir().expect("temporary source root");
        let source_id = SourceId::from_string("ui-catalog-publication-fence");
        let snapshot = || yield_catalog::CatalogSnapshot {
            source_id: source_id.clone(),
            root_identity: yield_library::sample_sources::reconciliation::RootIdentity::from_bytes(
                b"projection-root".to_vec(),
            ),
            lifecycle_generation: 1,
            committed_revision: 0,
            directory_generation: 0,
            manifest: vec![SourceManifestEntry {
                relative_path: PathBuf::from("one.wav"),
                file_identity: Some(String::from("file-identity")),
                content_hash: None,
                file_size: 16,
                modified_ns: 1,
            }],
            directories: Vec::new(),
            wav_metadata: BTreeMap::new(),
            curation_metadata: BTreeMap::new(),
        };

        let live_database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &profile_guard,
            Path::new("catalog-projection-live"),
        )
        .expect("live catalog database");
        let live_fence = live_database
            .source_publication_fence()
            .expect("live publication fence");
        let cancellation = AtomicBool::new(false);
        let live_projection = project_catalog_event_with_fence_observer(
            yield_session::CatalogEvent::SnapshotReady {
                snapshot: snapshot(),
            },
            source_root.path().to_path_buf(),
            &cancellation,
            Some(live_fence),
            || {},
        )
        .expect("live projection remains deliverable");
        assert_eq!(live_projection.entries.len(), 1);
        drop(live_database);

        let retired_database = SourceDatabase::open_for_catalog_with_profile_guard(
            source_root.path(),
            &profile_guard,
            Path::new("catalog-projection-retired"),
        )
        .expect("retired catalog database");
        let retired_fence = retired_database
            .source_publication_fence()
            .expect("retired publication fence");
        let error = match project_catalog_event_with_fence_observer(
            yield_session::CatalogEvent::SnapshotReady {
                snapshot: snapshot(),
            },
            source_root.path().to_path_buf(),
            &cancellation,
            Some(retired_fence.clone()),
            move || drop(retired_database),
        ) {
            Ok(_) => panic!("retired publication must fail the final background check"),
            Err(error) => error,
        };
        assert!(error.contains("catalog publication fence invalid"));
        assert!(!retired_fence.is_live());
    }

    #[test]
    fn unadmitted_activation_does_not_produce_a_remove() {
        let source_id = SourceId::from_string("source");
        let mut activation = CatalogActivation {
            source_id,
            root: PathBuf::from("/library"),
            lifecycle_generation: 1,
            registered: false,
            registration_admitted: false,
            root_identity: None,
            audit_finished: false,
            audit_error: None,
            audit_retryable: false,
            force_reload: false,
            fallback_to_scan: false,
        };
        assert!(!activation.registration_admitted);
        activation.registration_admitted = true;
        assert!(activation.registration_admitted);
    }

    #[test]
    fn remove_intents_coalesce_to_the_newest_generation() {
        let source_id = SourceId::from_string("source");
        let mut pending = HashMap::new();
        coalesce_catalog_remove(
            &mut pending,
            source_id.clone(),
            CatalogCommandIntent::Remove {
                source_id: source_id.clone(),
                lifecycle_generation: 2,
            },
        );
        coalesce_catalog_remove(
            &mut pending,
            source_id.clone(),
            CatalogCommandIntent::Remove {
                source_id: source_id.clone(),
                lifecycle_generation: 1,
            },
        );
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[&source_id].lifecycle_generation(), 2);
    }

    #[test]
    fn focused_manifest_facts_preserve_or_invalidate_playback() {
        let root = PathBuf::from("/library");
        let path = root.join("kick.wav");
        let id = AudioFile {
            path: path.clone(),
            relative_path: PathBuf::from("kick.wav"),
            byte_len: 10,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
        .stable_id();
        let mut session = super::super::SourceSession::new(root.clone());
        let facts = CatalogFileFacts {
            relative_path: PathBuf::from("kick.wav"),
            file_size: 10,
            modified_ns: 20,
            file_identity: Some(String::from("inode-1")),
            content_hash: Some(String::from("hash-1")),
        };
        session
            .catalog_facts
            .insert(facts.relative_path.clone(), facts.clone());
        let mut projection_facts = HashMap::new();
        projection_facts.insert(facts.relative_path.clone(), facts.clone());
        let projection = CatalogProjection {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 1,
            root,
            root_identity: yield_library::sample_sources::reconciliation::RootIdentity::from_bytes(
                b"root".to_vec(),
            ),
            directory_generation: 1,
            committed_revision: 1,
            entries: vec![AudioFile {
                path: id.path().to_path_buf(),
                relative_path: PathBuf::from("kick.wav"),
                byte_len: 10,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }],
            tree: FolderTree::default(),
            facts: projection_facts,
            entry_indices: Arc::new(HashMap::from([(PathBuf::from("kick.wav"), 0)])),
        };
        assert!(!focused_manifest_changed(&id, &projection, &session));
        let mut changed = projection;
        changed
            .facts
            .get_mut(&PathBuf::from("kick.wav"))
            .expect("fact")
            .modified_ns += 1;
        assert!(focused_manifest_changed(&id, &changed, &session));
    }
}
