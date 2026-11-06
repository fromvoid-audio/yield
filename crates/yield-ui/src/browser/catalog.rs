//! Browser integration for the profile-owned source catalog.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui::{BorrowAppContext, Context};
use yield_core::{AudioFile, AudioFileCuration, AudioFileId, FolderTree, PlaybackType};
use yield_library::sample_sources::{
    BrowserFileMetadata, SourceId, SourceManifestEntry, SourcePlaybackType, SourcePublicationFence,
    reconciliation::RootIdentity,
};
use yield_media::WaveformDetailContext;
use yield_session::{
    CatalogCommand, CatalogDelivery, CatalogEvent, CatalogOperation, CatalogPublicationStatus,
    CatalogStatus, CatalogSubmitError, SourceContentDigestOrigin, SourceContentDigestScope,
    VerifiedSourceRoot,
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
    ReadSnapshot {
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
            | Self::ReadSnapshot { source_id, .. }
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
            | Self::ReadSnapshot {
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
            Self::ReadSnapshot { source_id, .. } => CatalogCommand::ReadSnapshot {
                source_id,
                expected_revision: None,
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
    facts: Arc<HashMap<PathBuf, CatalogFileFacts>>,
    entry_indices: Arc<HashMap<PathBuf, usize>>,
}

/// Shared immutable predecessor captured on the UI; materialize only on the worker.
struct CatalogReplayBase {
    source_id: SourceId,
    lifecycle_generation: u64,
    root: PathBuf,
    root_identity: RootIdentity,
    directory_generation: u64,
    committed_revision: u64,
    entries: Arc<Vec<AudioFile>>,
    tree: Arc<FolderTree>,
    facts: Arc<HashMap<PathBuf, CatalogFileFacts>>,
    entry_indices: Arc<HashMap<PathBuf, usize>>,
}

impl CatalogReplayBase {
    fn capture(
        context: catalog_metadata::CatalogMetadataContext,
        session: &SourceSession,
    ) -> Option<Self> {
        if session.source_id.as_ref() != Some(&context.source_id)
            || session.catalog_lifecycle_generation != Some(context.lifecycle_generation)
            || session.root != context.root
        {
            return None;
        }
        Some(Self {
            source_id: context.source_id,
            lifecycle_generation: context.lifecycle_generation,
            root: context.root,
            root_identity: context.root_identity,
            directory_generation: context.directory_generation,
            committed_revision: context.revision,
            entries: Arc::clone(&session.entries),
            tree: Arc::clone(&session.tree),
            facts: Arc::clone(&session.catalog_facts),
            entry_indices: context.entry_indices,
        })
    }

    fn materialize(self) -> CatalogProjection {
        CatalogProjection {
            source_id: self.source_id,
            lifecycle_generation: self.lifecycle_generation,
            root: self.root,
            root_identity: self.root_identity,
            directory_generation: self.directory_generation,
            committed_revision: self.committed_revision,
            entries: self.entries.as_ref().clone(),
            tree: self.tree.as_ref().clone(),
            facts: self.facts,
            entry_indices: self.entry_indices,
        }
    }
}

/// Prepare both retained and displayed views off render, rather than cloning at delivery.
struct PreparedCatalogProjection {
    projection: CatalogProjection,
    session_entries: Arc<Vec<AudioFile>>,
    session_tree: Arc<FolderTree>,
}

impl PreparedCatalogProjection {
    fn new(projection: CatalogProjection) -> Self {
        Self {
            session_entries: Arc::new(projection.entries.clone()),
            session_tree: Arc::new(projection.tree.clone()),
            projection,
        }
    }
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
        self.similarity_list.cancel();
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::cancel_for_source_change(self);
        super::unsupported_entries::cancel_for_source_change(self, cx);
        super::tags::cancel_for_source_change(self);
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        super::harvest_filter::cancel_for_source_change(self);
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
        self.entries = self.source_sessions[index].entries.as_ref().clone();
        self.folder_tree = Some(self.source_sessions[index].tree.as_ref().clone());
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
        super::harvest_filter::reinstall_after_projection(self, cx);
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
        if let Some(activation) = self.catalog_activation.as_ref() {
            let key = super::curation::CurationLifecycleKey {
                source_id: activation.source_id.clone(),
                lifecycle_generation: activation.lifecycle_generation,
            };
            if !self.flush_latest_curation(&key, cx) {
                self.set_scan_status(String::from(
                    "Latest rating or listen value was not saved before changing source",
                ));
            }
        }
        self.job_action = None;
        self.job_action_error = None;
        self.similarity_list.cancel();
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::cancel_for_source_change(self);
        super::unsupported_entries::cancel_for_source_change(self, cx);
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        super::harvest_filter::cancel_for_source_change(self);
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
        if self.expire_collection_preparation() {
            cx.notify();
        }
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
                self.similarity_list.cancel();
                super::source_recovery::cancel_for_source_change(self);
                super::rating_decay::cancel_for_source_change(self);
                super::unsupported_entries::cancel_for_source_change(self, cx);
                super::tag_chips::cancel_for_source_change(self);
                super::tag_completion::cancel_for_source_change(self, cx);
                super::tag_filter::cancel_for_source_change(self);
                self.tag_dictionary.close_preview();
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
                    | CatalogSubmitError::InvalidFeatureRequest
                    | CatalogSubmitError::InvalidWavMetadataRequest(_)
                    | CatalogSubmitError::InvalidCurationRequest(_)
                    | CatalogSubmitError::InvalidTagPageRequest(_)
                    | CatalogSubmitError::InvalidTagFilterRequest(_)
                    | CatalogSubmitError::InvalidDisplayLabelsRequest(_)
                    | CatalogSubmitError::InvalidCurationTagFactsRequest(_)
                    | CatalogSubmitError::InvalidIndexPageRequest(_)
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
                CatalogCommandIntent::Audit { .. }
                | CatalogCommandIntent::ReadSnapshot { .. }
                | CatalogCommandIntent::Remove { .. } => None,
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
                    | CatalogSubmitError::InvalidFeatureRequest
                    | CatalogSubmitError::InvalidWavMetadataRequest(_)
                    | CatalogSubmitError::InvalidCurationRequest(_)
                    | CatalogSubmitError::InvalidTagPageRequest(_)
                    | CatalogSubmitError::InvalidTagFilterRequest(_)
                    | CatalogSubmitError::InvalidDisplayLabelsRequest(_)
                    | CatalogSubmitError::InvalidCurationTagFactsRequest(_)
                    | CatalogSubmitError::InvalidIndexPageRequest(_)
                    | CatalogSubmitError::InvalidRatingDecayRequest(_),
                ) => return false,
            }
        }
        let mut changed = self.retry_catalog_metadata(cx);
        changed |= super::playmark_tempo::load_for_waveform(self, cx);
        changed |= super::rating_decay::retry(self, cx);
        changed |= super::unsupported_entries::retry(self, cx);
        changed
    }

    fn handle_catalog_unavailable(&mut self, reason: String, cx: &mut Context<Self>) -> bool {
        let Some(activation) = self.catalog_activation.clone() else {
            self.catalog_pending_command = None;
            self.catalog_pending_removes.clear();
            self.similarity_list.cancel();
            super::source_recovery::cancel_for_source_change(self);
            super::rating_decay::cancel_for_source_change(self);
            super::unsupported_entries::cancel_for_source_change(self, cx);
            super::tag_chips::cancel_for_source_change(self);
            super::tag_completion::cancel_for_source_change(self, cx);
            self.display_labels.cancel_for_source_change();
            self.reset_catalog_metadata();
            self.curation_state.prune_completed_except(None);
            return true;
        };
        self.catalog_pending_command = None;
        self.catalog_pending_removes.clear();
        self.similarity_list.cancel();
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::cancel_for_source_change(self);
        super::unsupported_entries::cancel_for_source_change(self, cx);
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
        mut event: CatalogEvent,
        publication_fence: Option<SourcePublicationFence>,
        cx: &mut Context<Self>,
    ) -> bool {
        if super::playmark_tempo::apply_catalog_event(self, &event, publication_fence.as_ref(), cx)
        {
            return true;
        }
        if super::transient_markers::apply_catalog_event(
            self,
            &mut event,
            publication_fence.clone(),
            cx,
        ) {
            return true;
        }
        if super::similarity_list::apply_catalog_event(
            self,
            &mut event,
            publication_fence.clone(),
            cx,
        ) {
            return true;
        }
        if super::analysis::apply_catalog_event(self, &event, cx) {
            return true;
        }
        if super::unsupported_entries::apply_catalog_event(self, &mut event, cx) {
            return true;
        }
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
            CatalogEvent::TagAssignmentPreviewReady { .. }
                | CatalogEvent::TagAssignmentPreviewFailed { .. }
        ) && super::tag_dictionary::apply_preview_event(self, &event)
        {
            cx.notify();
            return true;
        }
        if matches!(
            &event,
            CatalogEvent::DisplayLabelsReady { .. } | CatalogEvent::DisplayLabelsFailed { .. }
        ) && self.apply_display_labels_event(&event)
        {
            return true;
        }
        if matches!(&event, CatalogEvent::HarvestFilterCompleted { .. }) {
            return super::harvest_filter::apply_catalog_event(self, event, cx);
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
        if matches!(&event, CatalogEvent::CollectionHistoryGroupCompleted { .. }) {
            if publication_fence.as_ref().is_some_and(|f| !f.is_live())
                && let CatalogEvent::CollectionHistoryGroupCompleted { result, .. } = &mut event
            {
                *result = Err(yield_session::CollectionHistoryFailure::StaleSource);
            }
            return self.apply_collection_preparation_event(&event, cx);
        }
        if matches!(event, CatalogEvent::CurationOperationCompleted { .. }) {
            if publication_fence.as_ref().is_some_and(|f| !f.is_live())
                && let CatalogEvent::CurationOperationCompleted { publication, .. } = &mut event
            {
                *publication = CatalogPublicationStatus::NonProjectable;
            }
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
                self.similarity_list.cancel();
                super::source_recovery::cancel_for_source_change(self);
                super::rating_decay::cancel_for_source_change(self);
                super::unsupported_entries::cancel_for_source_change(self, cx);
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
            CatalogEvent::SourceContentDigestCommitted {
                scope,
                origin,
                committed_revision,
                entry,
            } => {
                if publication_fence
                    .as_ref()
                    .is_some_and(SourcePublicationFence::is_live)
                {
                    self.apply_digest_revision(
                        scope,
                        match origin {
                            SourceContentDigestOrigin::Transients(id) => Some(*id),
                            SourceContentDigestOrigin::DigestOnly(_) => None,
                        },
                        *committed_revision,
                        entry,
                        cx,
                    );
                }
                true
            }
            CatalogEvent::ReplayCommitted { .. } => {
                super::unsupported_entries::cancel_for_source_change(self, cx);
                self.job_action = None;
                self.job_action_error = None;
                self.start_catalog_projection(event, publication_fence, cx);
                true
            }
            CatalogEvent::AuditCommitted { .. } => {
                super::unsupported_entries::cancel_for_source_change(self, cx);
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
                super::unsupported_entries::cancel_for_source_change(self, cx);
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
            CatalogEvent::FeatureAnalysisState { .. }
            | CatalogEvent::CurationCommitted { .. }
            | CatalogEvent::CurationFailed { .. }
            | CatalogEvent::CurationOperationCompleted { .. }
            | CatalogEvent::IndexPageReady { .. }
            | CatalogEvent::IndexPageFailed { .. }
            | CatalogEvent::IndexInspectionCompleted { .. }
            | CatalogEvent::TagPageReady { .. }
            | CatalogEvent::TagPageFailed { .. }
            | CatalogEvent::TagAssignmentsReady { .. }
            | CatalogEvent::TagAssignmentsFailed { .. }
            | CatalogEvent::TagAssignmentPreviewReady { .. }
            | CatalogEvent::TagAssignmentPreviewFailed { .. }
            | CatalogEvent::TagFilterReady { .. }
            | CatalogEvent::TagFilterFailed { .. }
            | CatalogEvent::DisplayLabelsReady { .. }
            | CatalogEvent::DisplayLabelsFailed { .. }
            | CatalogEvent::CurationTagFactsReady { .. }
            | CatalogEvent::CurationTagFactsFailed { .. }
            | CatalogEvent::HarvestFilterCompleted { .. }
            | CatalogEvent::HarvestFactsCompleted { .. }
            | CatalogEvent::RatingDecayProgress { .. }
            | CatalogEvent::RatingDecayFailed { .. } => false,
            CatalogEvent::Failed {
                error, operation, ..
            } => {
                if *operation == CatalogOperation::Audit {
                    super::unsupported_entries::cancel_for_source_change(self, cx);
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
            CatalogEvent::CollectionHistoryCompleted { .. }
            | CatalogEvent::CollectionHistoryGroupCompleted { .. }
            | CatalogEvent::SimilarityCompleted { .. }
            | CatalogEvent::PlaymarkTempoCompleted { .. }
            | CatalogEvent::SourceContentDigestFailed { .. }
            | CatalogEvent::TransientMarkersCompleted { .. }
            | CatalogEvent::Closed => false,
        }
    }

    fn apply_digest_revision(
        &mut self,
        scope: &SourceContentDigestScope,
        transient_request_id: Option<u64>,
        revision: u64,
        entry: &SourceManifestEntry,
        cx: &mut Context<Self>,
    ) {
        let previous = self.catalog_metadata.context();
        if let Some(context) = &previous {
            if context.revision == revision {
                super::playmark_tempo::adopt_digest_revision(self, scope, revision, cx);
                return;
            }
            if context.revision > revision {
                super::playmark_tempo::reject_digest_revision(self, scope, cx);
                return;
            }
        }
        let key = super::curation::CurationLifecycleKey {
            source_id: scope.source_id.clone(),
            lifecycle_generation: scope.lifecycle_generation,
        };
        let exact = previous.filter(|context| {
            context.source_id == key.source_id
                && context.lifecycle_generation == key.lifecycle_generation
                && context.revision == scope.previous_revision
                && context.revision.checked_add(1) == Some(revision)
        });
        let keep_tag_mask = super::tag_filter::capture_ready_mask(self).is_some();
        let session = self
            .active_source
            .and_then(|index| self.source_sessions.get_mut(index));
        let patched = exact
            .as_ref()
            .zip(session)
            .is_some_and(|(context, session)| {
                let Some(facts) = session.catalog_facts.get(&entry.relative_path) else {
                    return false;
                };
                if session.root != context.root
                    || session.source_id.as_ref() != Some(&key.source_id)
                    || session.catalog_lifecycle_generation != Some(key.lifecycle_generation)
                    || entry.relative_path != scope.relative_path.as_path()
                    || facts.file_size != entry.file_size
                    || facts.modified_ns != entry.modified_ns
                    || facts.file_identity != entry.file_identity
                    || facts.content_hash.is_some()
                    || entry.content_hash.is_none()
                    || !self
                        .curation_state
                        .advance_digest_revision(&key, context, revision)
                {
                    return false;
                }
                if let Some(facts) =
                    Arc::make_mut(&mut session.catalog_facts).get_mut(&entry.relative_path)
                {
                    facts.content_hash = entry.content_hash.clone();
                }
                true
            });
        let Some(previous) = exact.filter(|_| patched) else {
            super::playmark_tempo::reject_digest_revision(self, scope, cx);
            self.catalog_pending_command
                .get_or_insert(CatalogCommandIntent::ReadSnapshot {
                    source_id: key.source_id,
                    lifecycle_generation: key.lifecycle_generation,
                });
            return;
        };
        self.catalog_metadata.advance_projection_revision(revision);
        let pending = self.curation_state.has_pending_for_lifecycle(&key);
        if pending {
            self.catalog_metadata.pause_for_curation();
        }
        let display_context = self
            .catalog_metadata
            .context()
            .map(super::display_labels::display_context_from_metadata);
        if !display_context.clone().is_some_and(|context| {
            self.display_labels.rebase_after_curation(
                context,
                previous.revision,
                revision,
                &previous.root_identity,
                previous.directory_generation,
                pending,
            )
        }) {
            self.display_labels
                .invalidate_after_curation(display_context, pending);
        }
        if !keep_tag_mask
            || !super::tag_filter::rebase_ready_context(
                self,
                revision,
                &previous.root_identity,
                previous.directory_generation,
            )
        {
            super::tag_filter::invalidate_after_curation(
                self,
                super::tag_filter::FilterOrigin::PassiveMetadata,
                cx,
            );
        }
        super::transient_markers::accept_digest_revision(
            self,
            scope,
            transient_request_id,
            revision,
            cx,
        );
        super::playmark_tempo::adopt_digest_revision(self, scope, revision, cx);
        cx.notify();
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
        let is_replay = matches!(&event, CatalogEvent::ReplayCommitted { .. });
        let replay_base = if is_replay {
            let Some(context) = self.catalog_metadata.context() else {
                self.set_scan_status("Replay committed; an audited projection is required");
                self.catalog_pending_command = Some(CatalogCommandIntent::Audit {
                    source_id,
                    lifecycle_generation,
                });
                return;
            };
            let Some(session) = self
                .active_source
                .and_then(|index| self.source_sessions.get(index))
            else {
                return;
            };
            let Some(base) = CatalogReplayBase::capture(context, session) else {
                self.catalog_pending_command = Some(CatalogCommandIntent::Audit {
                    source_id,
                    lifecycle_generation,
                });
                return;
            };
            Some(base)
        } else {
            None
        };
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        super::harvest_filter::cancel_for_source_change(self);
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
                    if worker_cancellation.load(Ordering::Acquire) {
                        return Err("catalog projection canceled".into());
                    }
                    if let Some(base) = replay_base {
                        return project_catalog_replay_event_with_fence(
                            event,
                            base.materialize(),
                            &worker_cancellation,
                            worker_publication_fence,
                        )
                        .map(PreparedCatalogProjection::new);
                    }
                    project_catalog_event_with_fence(
                        event,
                        root,
                        &worker_cancellation,
                        worker_publication_fence,
                    )
                    .map(PreparedCatalogProjection::new)
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
                        if is_replay {
                            // A missing predecessor/lost projection is a recovery audit, not
                            // permission to invent a replay patch from a persisted cursor.
                            view.catalog_pending_command = Some(CatalogCommandIntent::Audit {
                                source_id: source_id.clone(),
                                lifecycle_generation,
                            });
                        }
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
        prepared: PreparedCatalogProjection,
        force_reload: bool,
        cx: &mut Context<Self>,
    ) {
        let PreparedCatalogProjection {
            projection,
            session_entries,
            session_tree,
        } = prepared;
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
            session.entries = session_entries;
            session.tree = session_tree;
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
        super::harvest_filter::reinstall_after_projection(self, cx);
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
        super::transient_markers::schedule(self, cx);
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
        self.similarity_list.cancel();
        self.transient_markers.invalidate();
        super::source_recovery::cancel_for_source_change(self);
        super::rating_decay::invalidate_for_source_publication(self, status);
        super::unsupported_entries::cancel_for_source_change(self, cx);
        super::tag_chips::cancel_for_source_change(self);
        super::tag_completion::cancel_for_source_change(self, cx);
        super::tag_filter::cancel_for_source_change(self);
        super::curation_queue::cancel_for_source_change(self);
        super::harvest_filter::cancel_for_source_change(self);
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

pub(super) fn audited_waveform_source(
    browser: &Browser,
) -> Option<(
    &AudioFile,
    &WaveformDetailContext,
    &catalog_metadata::CatalogMetadataContext,
)> {
    let selected = browser.selected?;
    let entry = browser.entries.get(selected)?;
    let detail = browser.waveform.as_ref()?.detail_context.as_ref()?;
    let source = browser.catalog_metadata.context_ref()?;
    let activation = browser.catalog_activation.as_ref()?;
    if entry.path != detail.path
        || browser.root.as_ref() != Some(&source.root)
        || source.entry_indices.get(&entry.relative_path) != Some(&selected)
        || activation.source_id != source.source_id
        || activation.lifecycle_generation != source.lifecycle_generation
        || activation.root_identity.as_ref() != Some(&source.root_identity)
    {
        return None;
    }
    Some((entry, detail, source))
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
        CatalogEvent::HarvestFilterCompleted { request, .. } => Some((
            request.context().source_id().clone(),
            request.context().lifecycle_generation(),
        )),
        CatalogEvent::HarvestFactsCompleted { request, .. } => {
            Some((request.source_id().clone(), request.lifecycle_generation()))
        }
        CatalogEvent::CollectionHistoryCompleted { request, .. } => {
            Some((request.source_id().clone(), request.lifecycle_generation()))
        }
        CatalogEvent::CollectionHistoryGroupCompleted { request, .. } => {
            Some((request.source_id().clone(), request.lifecycle_generation()))
        }
        CatalogEvent::SimilarityCompleted { request, .. } => {
            Some((request.source_id().clone(), request.lifecycle_generation()))
        }
        CatalogEvent::TransientMarkersCompleted { request, .. } => {
            Some((request.source_id().clone(), request.lifecycle_generation()))
        }
        CatalogEvent::SourceContentDigestCommitted { scope, .. } => {
            Some((scope.source_id.clone(), scope.lifecycle_generation))
        }
        CatalogEvent::SourceContentDigestFailed { request, .. } => {
            let scope = request.scope();
            Some((scope.source_id.clone(), scope.lifecycle_generation))
        }
        CatalogEvent::PlaymarkTempoCompleted { request, .. } => {
            Some((request.source_id().clone(), request.lifecycle_generation()))
        }
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
        CatalogEvent::FeatureAnalysisState {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::ReplayCommitted {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::WavMetadataReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::WavMetadataFailed {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::IndexPageReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::IndexInspectionCompleted {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::IndexPageFailed {
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
        | CatalogEvent::TagAssignmentsReady {
            source_id,
            lifecycle_generation,
            ..
        }
        | CatalogEvent::TagAssignmentsFailed {
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
        CatalogEvent::TagAssignmentPreviewReady { .. }
        | CatalogEvent::TagAssignmentPreviewFailed { .. }
        | CatalogEvent::Failed { .. }
        | CatalogEvent::Closed => None,
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
        facts: Arc::new(facts),
        entry_indices: Arc::new(entry_indices),
    })
}

/// Patch only actual committed rows into the retained projection. No SQL, path inspection,
/// metadata read, or whole-source hydration is permitted in this background transformation.
fn project_catalog_replay_event_with_fence(
    event: CatalogEvent,
    mut base: CatalogProjection,
    cancellation: &AtomicBool,
    publication_fence: Option<SourcePublicationFence>,
) -> Result<CatalogProjection, String> {
    let CatalogEvent::ReplayCommitted {
        source_id,
        lifecycle_generation,
        root_identity,
        expected_revision,
        receipt,
        delta,
    } = event
    else {
        return Err("expected a replay delta".into());
    };
    if cancellation.load(Ordering::Acquire)
        || base.source_id != source_id
        || base.lifecycle_generation != lifecycle_generation
        || base.root_identity != root_identity
        || base.committed_revision != expected_revision
        || receipt.prior().source_id() != &source_id
        || receipt.prior().root_identity() != &root_identity
        || receipt.prior().watcher_generation()
            != yield_library::sample_sources::reconciliation::WatcherGeneration::new(
                lifecycle_generation,
            )
        || receipt.source_revision() != delta.revision
        || delta.revision <= expected_revision
        || delta.authoritative_snapshot.is_some()
        || delta.touched_path_changes.len() > yield_session::MAX_CATALOG_REPLAY_DELTA_ENTRIES
        || delta.touched_directory_changes.len() > 128
    {
        return Err("replay delta does not match the retained projection".into());
    }
    if let Some(fence) = &publication_fence {
        fence
            .validate()
            .map_err(|e| format!("replay publication fence invalid: {e}"))?;
    }
    base = patch_catalog_replay_projection(base, delta, cancellation)?;
    if let Some(fence) = &publication_fence {
        fence
            .validate()
            .map_err(|e| format!("replay publication fence invalid: {e}"))?;
    }
    Ok(base)
}

fn patch_catalog_replay_projection(
    mut base: CatalogProjection,
    delta: yield_session::CatalogReplayDelta,
    cancellation: &AtomicBool,
) -> Result<CatalogProjection, String> {
    // Validate the bounded batch first, then visit the retained rows once. Scanning
    // or shifting the whole list separately for each path multiplies large-source
    // work by the batch size. Duplicate paths cannot supply an ambiguous patch.
    let mut changes = HashMap::with_capacity(delta.touched_path_changes.len());
    for (path, entry) in delta.touched_path_changes {
        if cancellation.load(Ordering::Acquire) {
            return Err("replay projection canceled".into());
        }
        validate_relative_path(&path)?;
        if entry.as_ref().is_some_and(|e| e.relative_path != path) {
            return Err("replay manifest path mismatch".into());
        }
        if changes.insert(path, entry).is_some() {
            return Err("duplicate replay manifest path".into());
        }
    }
    if !changes.is_empty() {
        let facts = Arc::make_mut(&mut base.facts);
        for (path, entry) in &changes {
            match entry {
                Some(entry) => {
                    facts.insert(
                        path.clone(),
                        CatalogFileFacts {
                            relative_path: path.clone(),
                            file_size: entry.file_size,
                            modified_ns: entry.modified_ns,
                            file_identity: entry.file_identity.clone(),
                            content_hash: entry.content_hash.clone(),
                        },
                    );
                }
                None => {
                    facts.remove(path);
                }
            }
        }
        let mut canceled = false;
        base.entries.retain_mut(|audio| {
            if cancellation.load(Ordering::Acquire) {
                canceled = true;
                return true;
            }
            match changes.remove(&audio.relative_path) {
                Some(Some(entry)) => {
                    audio.byte_len = entry.file_size;
                    audio.duration_seconds = None;
                    audio.sample_rate = None;
                    // Source-owned curation/playback type survive the byte change.
                    true
                }
                Some(None) => false,
                None => true,
            }
        });
        if canceled {
            return Err("replay projection canceled".into());
        }
        for (path, entry) in changes {
            if cancellation.load(Ordering::Acquire) {
                return Err("replay projection canceled".into());
            }
            if let Some(entry) = entry {
                base.entries.push(AudioFile {
                    path: base.root.join(&path),
                    relative_path: path,
                    byte_len: entry.file_size,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                });
            }
        }
    }
    let mut directories = base
        .tree
        .paths()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    for (path, entry) in delta.touched_directory_changes {
        validate_relative_path(&path)?;
        match entry {
            Some(entry) if entry.relative_path == path => {
                directories.insert(path);
            }
            Some(_) => return Err("replay directory path mismatch".into()),
            None => {
                directories.remove(&path);
            }
        }
    }
    if let Some(generation) = delta.directory_generation {
        base.directory_generation = generation;
    }
    base.entries
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    base.entry_indices = Arc::new(
        base.entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.relative_path.clone(), i))
            .collect(),
    );
    base.tree = FolderTree::from_scan(&directories.into_iter().collect::<Vec<_>>(), &base.entries);
    base.committed_revision = delta.revision;
    if cancellation.load(Ordering::Acquire) {
        return Err("replay projection canceled".into());
    }
    Ok(base)
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
        CatalogProjection, RootIdentity, catalog_event_identity, coalesce_catalog_remove,
        focused_manifest_changed, patch_catalog_replay_projection,
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

    fn replay_projection_fixture() -> CatalogProjection {
        let root = PathBuf::from("/retained-source");
        let entries = ["changed.wav", "keep.wav", "removed.wav"]
            .into_iter()
            .map(|path| AudioFile {
                path: root.join(path),
                relative_path: path.into(),
                byte_len: 10,
                duration_seconds: Some(2.0),
                sample_rate: Some(48000),
                playback_type: Some(PlaybackType::Loop),
                curation: None,
            })
            .collect::<Vec<_>>();
        CatalogProjection {
            source_id: SourceId::from_string("replay-projection"),
            lifecycle_generation: 7,
            root,
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 3,
            committed_revision: 4,
            tree: FolderTree::from_scan(&["old-empty".into(), "keep-empty".into()], &entries),
            facts: entries
                .iter()
                .map(|entry| {
                    (
                        entry.relative_path.clone(),
                        CatalogFileFacts {
                            relative_path: entry.relative_path.clone(),
                            file_size: 10,
                            modified_ns: 2,
                            file_identity: Some("original".into()),
                            content_hash: None,
                        },
                    )
                })
                .collect::<HashMap<_, _>>()
                .into(),
            entries,
            entry_indices: Arc::new(HashMap::new()),
        }
    }

    fn retained_replay_fixture() -> (
        super::super::SourceSession,
        super::catalog_metadata::CatalogMetadataContext,
    ) {
        let prepared = super::PreparedCatalogProjection::new(replay_projection_fixture());
        let projection = prepared.projection;
        let mut session = super::super::SourceSession::new(projection.root.clone());
        session.source_id = Some(projection.source_id.clone());
        session.catalog_lifecycle_generation = Some(projection.lifecycle_generation);
        session.entries = prepared.session_entries;
        session.tree = prepared.session_tree;
        session.catalog_facts = Arc::clone(&projection.facts);
        let context = super::catalog_metadata::CatalogMetadataContext {
            source_id: projection.source_id,
            lifecycle_generation: projection.lifecycle_generation,
            root: projection.root,
            root_identity: projection.root_identity,
            directory_generation: projection.directory_generation,
            revision: projection.committed_revision,
            entry_indices: projection.entry_indices,
        };
        (session, context)
    }

    #[test]
    fn replay_capture_shares_original_data_and_isolates_later_metadata_edits() {
        let (mut session, context) = retained_replay_fixture();
        let base = super::CatalogReplayBase::capture(context, &session).unwrap();
        assert!(Arc::ptr_eq(&base.entries, &session.entries));
        assert!(Arc::ptr_eq(&base.tree, &session.tree));
        assert!(Arc::ptr_eq(&base.facts, &session.catalog_facts));
        Arc::make_mut(&mut session.entries)[1].byte_len = 123;
        Arc::make_mut(&mut session.catalog_facts)
            .get_mut(&PathBuf::from("keep.wav"))
            .unwrap()
            .modified_ns = 99;
        session.tree = Arc::new(FolderTree::default());
        let patched = patch_catalog_replay_projection(
            base.materialize(),
            replay_projection_delta(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            patched
                .entries
                .iter()
                .find(|e| e.relative_path == PathBuf::from("keep.wav"))
                .unwrap()
                .byte_len,
            10
        );
        assert_eq!(patched.facts[&PathBuf::from("keep.wav")].modified_ns, 2);
        assert!(patched.tree.contains(&PathBuf::from("keep-empty")));
        assert_eq!(session.entries[1].byte_len, 123);
        assert_eq!(
            session.catalog_facts[&PathBuf::from("keep.wav")].modified_ns,
            99
        );
    }

    #[test]
    fn replay_capture_refuses_foreign_source_lifecycle_and_root() {
        let (mut session, context) = retained_replay_fixture();
        session.catalog_lifecycle_generation = Some(context.lifecycle_generation + 1);
        assert!(super::CatalogReplayBase::capture(context.clone(), &session).is_none());
        session.catalog_lifecycle_generation = Some(context.lifecycle_generation);
        session.root = PathBuf::from("/foreign-root");
        assert!(super::CatalogReplayBase::capture(context.clone(), &session).is_none());
        session.root = context.root.clone();
        session.source_id = Some(SourceId::from_string("foreign-source"));
        assert!(super::CatalogReplayBase::capture(context, &session).is_none());
    }

    #[test]
    fn prepared_display_mutations_preserve_retained_replay_rows() {
        let mut prepared = super::PreparedCatalogProjection::new(replay_projection_fixture());
        prepared.projection.entries[0].byte_len = 777;
        assert_eq!(prepared.session_entries[0].byte_len, 10);
        assert_eq!(
            prepared.session_entries.len(),
            prepared.projection.entries.len()
        );
    }

    fn replay_projection_delta() -> yield_session::CatalogReplayDelta {
        yield_session::CatalogReplayDelta {
            revision: 5,
            touched_path_changes: vec![
                (
                    "changed.wav".into(),
                    Some(SourceManifestEntry {
                        relative_path: "changed.wav".into(),
                        file_identity: Some("changed-file".into()),
                        content_hash: None,
                        file_size: 20,
                        modified_ns: 30,
                    }),
                ),
                ("removed.wav".into(), None),
            ],
            authoritative_snapshot: None,
            source_index_commit: None,
            directory_generation: Some(3),
            touched_directory_changes: vec![
                ("old-empty".into(), None),
                (
                    "new-empty".into(),
                    Some(SourceDirectoryEntry {
                        relative_path: "new-empty".into(),
                        directory_identity: "new-dir".into(),
                    }),
                ),
            ],
        }
    }

    #[test]
    fn replay_projection_patches_retained_rows_and_empty_directories_without_io() {
        // Passive projection data is not a native producer, receipt or mutation grant.
        let projection = patch_catalog_replay_projection(
            replay_projection_fixture(),
            replay_projection_delta(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(projection.committed_revision, 5);
        assert_eq!(projection.entries.len(), 2);
        let changed = &projection.entries[projection.entry_indices[Path::new("changed.wav")]];
        assert_eq!(changed.byte_len, 20);
        assert_eq!(changed.duration_seconds, None);
        assert_eq!(changed.sample_rate, None);
        assert_eq!(changed.playback_type, Some(PlaybackType::Loop));
        let unchanged = &projection.entries[projection.entry_indices[Path::new("keep.wav")]];
        assert_eq!(unchanged.duration_seconds, Some(2.0));
        assert_eq!(projection.facts[Path::new("keep.wav")].modified_ns, 2);
        assert!(projection.tree.contains(Path::new("new-empty")));
        assert!(projection.tree.contains(Path::new("keep-empty")));
        assert!(!projection.tree.contains(Path::new("old-empty")));
        assert!(!projection.facts.contains_key(Path::new("removed.wav")));
        assert_eq!(projection.facts[Path::new("changed.wav")].modified_ns, 30);
    }

    #[test]
    fn replay_projection_mixed_batch_preserves_large_predecessor_and_row_metadata() {
        let mut base = replay_projection_fixture();
        let prototype = base.entries[0].clone();
        base.entries = (0..20_000)
            .map(|i| {
                let mut entry = prototype.clone();
                entry.relative_path = format!("sample-{i:05}.wav").into();
                entry.path = base.root.join(&entry.relative_path);
                entry.curation = super::AudioFileCuration::from_parts(3, true, 1, None, None);
                entry
            })
            .collect();
        base.facts = Arc::new(
            base.entries
                .iter()
                .map(|entry| {
                    (
                        entry.relative_path.clone(),
                        CatalogFileFacts {
                            relative_path: entry.relative_path.clone(),
                            file_size: 10,
                            modified_ns: 2,
                            file_identity: Some("original".into()),
                            content_hash: None,
                        },
                    )
                })
                .collect(),
        );
        let retained_facts = Arc::clone(&base.facts);
        let curation = base.entries[0].curation;
        let mut delta = replay_projection_delta();
        delta.touched_path_changes.clear();
        for i in 0..yield_session::MAX_CATALOG_REPLAY_DELTA_ENTRIES {
            let path: PathBuf = if i % 3 == 2 {
                format!("new-{i:05}.wav").into()
            } else {
                format!("sample-{:05}.wav", i * 300).into()
            };
            let entry = (i % 3 != 1).then(|| SourceManifestEntry {
                relative_path: path.clone(),
                file_identity: Some("replacement".into()),
                content_hash: None,
                file_size: 90,
                modified_ns: 91,
            });
            delta.touched_path_changes.push((path, entry));
        }
        delta.touched_path_changes.reverse();
        let projection =
            patch_catalog_replay_projection(base, delta, &AtomicBool::new(false)).unwrap();
        let bound = yield_session::MAX_CATALOG_REPLAY_DELTA_ENTRIES;
        let removed = (0..bound).filter(|i| i % 3 == 1).count();
        let inserted = (0..bound).filter(|i| i % 3 == 2).count();
        assert_eq!(projection.entries.len(), 20_000 - removed + inserted);
        for i in 0..20_000 {
            let path = PathBuf::from(format!("sample-{i:05}.wav"));
            let touched = (i % 300 == 0 && i / 300 < bound).then_some(i / 300);
            if touched.is_some_and(|i| i % 3 == 1) {
                assert!(!projection.entry_indices.contains_key(&path));
                assert!(!projection.facts.contains_key(&path));
                continue;
            }
            let entry = &projection.entries[projection.entry_indices[&path]];
            let changed = touched.is_some_and(|i| i % 3 == 0);
            assert_eq!(entry.byte_len, if changed { 90 } else { 10 });
            assert_eq!(
                entry.duration_seconds,
                if changed { None } else { Some(2.0) }
            );
            assert_eq!(entry.sample_rate, if changed { None } else { Some(48000) });
            assert_eq!(entry.curation, curation);
            assert_eq!(entry.playback_type, Some(PlaybackType::Loop));
            assert_eq!(
                projection.facts[&path].modified_ns,
                if changed { 91 } else { 2 }
            );
            assert_eq!(retained_facts[&path].modified_ns, 2);
        }
        for i in (0..bound).filter(|i| i % 3 == 2) {
            let path = PathBuf::from(format!("new-{i:05}.wav"));
            let entry = &projection.entries[projection.entry_indices[&path]];
            assert_eq!(entry.path, projection.root.join(&path));
            assert_eq!(entry.byte_len, 90);
            assert_eq!(entry.curation, None);
            assert_eq!(entry.playback_type, None);
            assert!(!retained_facts.contains_key(&path));
        }
        assert_eq!(retained_facts.len(), 20_000);
        assert!(
            projection
                .entries
                .windows(2)
                .all(|pair| pair[0].relative_path < pair[1].relative_path)
        );
    }

    #[test]
    fn replay_projection_rejects_duplicate_path_before_changing_shared_facts() {
        for replacement in [
            None,
            replay_projection_delta().touched_path_changes[0].1.clone(),
        ] {
            let base = replay_projection_fixture();
            let retained_facts = Arc::clone(&base.facts);
            let mut delta = replay_projection_delta();
            delta
                .touched_path_changes
                .push(("changed.wav".into(), replacement));
            assert!(patch_catalog_replay_projection(base, delta, &AtomicBool::new(false)).is_err());
            assert_eq!(retained_facts[Path::new("changed.wav")].file_size, 10);
            assert!(retained_facts.contains_key(Path::new("removed.wav")));
        }
    }

    #[test]
    fn replay_projection_rejects_cancellation_and_foreign_relative_paths() {
        assert!(
            patch_catalog_replay_projection(
                replay_projection_fixture(),
                replay_projection_delta(),
                &AtomicBool::new(true)
            )
            .is_err()
        );
        let mut delta = replay_projection_delta();
        delta.touched_path_changes[0].0 = "../escape.wav".into();
        assert!(
            patch_catalog_replay_projection(
                replay_projection_fixture(),
                delta,
                &AtomicBool::new(false)
            )
            .is_err()
        );
        let mut delta = replay_projection_delta();
        delta.touched_directory_changes[1]
            .1
            .as_mut()
            .unwrap()
            .relative_path = "foreign".into();
        assert!(
            patch_catalog_replay_projection(
                replay_projection_fixture(),
                delta,
                &AtomicBool::new(false)
            )
            .is_err()
        );
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
    fn similarity_dispatch_event_uses_exact_captured_source_lifecycle() {
        use yield_session::{
            SimilarityAspect, SimilarityCancellation, SimilarityDispatchFailure,
            SourceSimilarityRequest,
        };
        let source = SourceId::from_string("similarity-event");
        let request = SourceSimilarityRequest::new(
            source.clone(),
            8,
            20,
            30,
            "anchor.wav".into(),
            &["candidate.wav".into()],
            SimilarityAspect::Timbre,
        )
        .unwrap();
        let event = yield_session::CatalogEvent::SimilarityCompleted {
            request,
            cancellation: SimilarityCancellation::new(),
            result: Err(SimilarityDispatchFailure::AnchorUnavailable),
        };
        assert_eq!(catalog_event_identity(&event), Some((source.clone(), 8)));
        assert_ne!(catalog_event_identity(&event), Some((source, 7)));
        assert!(
            project_catalog_event(event, PathBuf::from("/unused"), &AtomicBool::new(false))
                .is_err()
        ); // transport alone invents no rendered result
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
        Arc::make_mut(&mut session.catalog_facts)
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
            facts: Arc::new(projection_facts),
            entry_indices: Arc::new(HashMap::from([(PathBuf::from("kick.wav"), 0)])),
        };
        assert!(!focused_manifest_changed(&id, &projection, &session));
        let mut changed = projection;
        Arc::make_mut(&mut changed.facts)
            .get_mut(&PathBuf::from("kick.wav"))
            .expect("fact")
            .modified_ns += 1;
        assert!(focused_manifest_changed(&id, &changed, &session));
    }
}
