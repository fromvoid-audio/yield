//! Deferred, revision-fenced loading of missing catalog WAV metadata.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui::{Context, Task, Timer};
use yield_library::sample_sources::{SourceId, reconciliation::RootIdentity};
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogSubmitError, MAX_WAV_METADATA_REQUEST_PATHS,
};

use super::{Browser, catalog};

const METADATA_DEBOUNCE: Duration = Duration::from_millis(45);

/// The immutable catalog projection identity used to fence lazy metadata requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CatalogMetadataContext {
    pub(super) source_id: SourceId,
    pub(super) lifecycle_generation: u64,
    pub(super) root: PathBuf,
    pub(super) root_identity: RootIdentity,
    pub(super) directory_generation: u64,
    pub(super) revision: u64,
    pub(super) entry_indices: Arc<HashMap<PathBuf, usize>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CatalogMetadataRequest {
    context: CatalogMetadataContext,
    request_id: u64,
    paths: Vec<PathBuf>,
}

/// UI state for one bounded, deferred catalog metadata lane.
pub(super) struct CatalogMetadataState {
    context: Option<CatalogMetadataContext>,
    latest_demand: Vec<PathBuf>,
    terminal_paths: BTreeSet<PathBuf>,
    pending: Option<CatalogMetadataRequest>,
    retry_ready: bool,
    paused: bool,
    next_request_id: u64,
    debounce_generation: u64,
    debounce_task: Option<Task<()>>,
    error: Option<String>,
}

impl Default for CatalogMetadataState {
    fn default() -> Self {
        Self {
            context: None,
            latest_demand: Vec::new(),
            terminal_paths: BTreeSet::new(),
            pending: None,
            retry_ready: false,
            paused: false,
            next_request_id: 0,
            debounce_generation: 0,
            debounce_task: None,
            error: None,
        }
    }
}

impl CatalogMetadataState {
    pub(super) fn reset(&mut self) {
        self.context = None;
        self.latest_demand.clear();
        self.terminal_paths.clear();
        self.pending = None;
        self.retry_ready = false;
        self.paused = false;
        self.debounce_generation = self.debounce_generation.wrapping_add(1);
        self.debounce_task.take();
        self.error = None;
    }

    pub(super) fn install_projection(&mut self, context: CatalogMetadataContext) {
        self.reset();
        self.context = Some(context);
    }

    /// Pause lazy metadata requests while a curation operation is awaiting its receipt.
    pub(super) fn pause_for_curation(&mut self) {
        self.latest_demand.clear();
        self.pending = None;
        self.retry_ready = false;
        self.paused = true;
        self.debounce_generation = self.debounce_generation.wrapping_add(1);
        self.debounce_task.take();
    }

    /// Resume lazy reads when a staged edit was rejected before catalog admission.
    pub(super) fn resume_after_rejected_curation(&mut self) {
        self.paused = false;
    }

    /// Reinstall the same audited metadata mapping at a newly committed revision.
    pub(super) fn advance_projection_revision(&mut self, revision: u64) -> bool {
        let Some(mut context) = self.context.clone() else {
            return false;
        };
        if revision < context.revision {
            return false;
        }
        context.revision = revision;
        self.install_projection(context);
        true
    }

    pub(super) fn context(&self) -> Option<CatalogMetadataContext> {
        self.context.clone()
    }

    pub(super) fn observe_visible_demand(
        &mut self,
        context: CatalogMetadataContext,
        paths: Vec<PathBuf>,
    ) -> bool {
        if self.paused || !self.context_matches(&context) {
            return false;
        }
        let mut latest = Vec::with_capacity(paths.len());
        for path in paths {
            if context.entry_indices.contains_key(&path) && !latest.contains(&path) {
                latest.push(path);
            }
        }
        self.latest_demand = latest;
        self.error.is_none()
            && self.pending.is_none()
            && self.debounce_task.is_none()
            && !self.retry_ready
            && !self.next_paths().is_empty()
    }

    fn context_matches(&self, context: &CatalogMetadataContext) -> bool {
        self.context.as_ref().is_some_and(|current| {
            current.source_id == context.source_id
                && current.lifecycle_generation == context.lifecycle_generation
                && current.root == context.root
                && current.root_identity == context.root_identity
                && current.revision == context.revision
                && Arc::ptr_eq(&current.entry_indices, &context.entry_indices)
        })
    }

    pub(super) fn begin_debounce(&mut self) -> Option<u64> {
        if self.paused
            || self.debounce_task.is_some()
            || self.pending.is_some()
            || self.retry_ready
            || self.error.is_some()
            || self.next_paths().is_empty()
        {
            return None;
        }
        self.debounce_generation = self.debounce_generation.wrapping_add(1);
        Some(self.debounce_generation)
    }

    pub(super) fn set_debounce_task(&mut self, task: Task<()>) {
        self.debounce_task = Some(task);
    }

    pub(super) fn settle_debounce(&mut self, generation: u64) -> bool {
        if self.debounce_generation != generation {
            return false;
        }
        self.debounce_task.take();
        self.retry_ready = true;
        true
    }

    fn begin_request(&mut self) -> Option<CatalogMetadataRequest> {
        if self.paused || self.pending.is_some() || !self.retry_ready || self.error.is_some() {
            return None;
        }
        let context = self.context.clone()?;
        let paths = self
            .next_paths()
            .into_iter()
            .take(MAX_WAV_METADATA_REQUEST_PATHS)
            .collect::<Vec<_>>();
        if paths.is_empty() {
            self.retry_ready = false;
            return None;
        }
        self.next_request_id = self.next_request_id.wrapping_add(1);
        if self.next_request_id == 0 {
            self.next_request_id = 1;
        }
        let request = CatalogMetadataRequest {
            context,
            request_id: self.next_request_id,
            paths,
        };
        self.retry_ready = false;
        self.pending = Some(request.clone());
        Some(request)
    }

    pub(super) fn admission_failed(
        &mut self,
        request_id: u64,
        retryable: bool,
        error: Option<String>,
    ) -> bool {
        if self
            .pending
            .as_ref()
            .is_none_or(|request| request.request_id != request_id)
        {
            return false;
        }
        self.pending = None;
        self.retry_ready = retryable;
        self.error = error;
        true
    }

    fn take_ready(
        &mut self,
        source_id: &SourceId,
        lifecycle_generation: u64,
        root_identity: &RootIdentity,
        revision: u64,
        request_id: u64,
    ) -> Option<CatalogMetadataRequest> {
        let request = self.pending.as_ref()?;
        (request.request_id == request_id
            && request.context.source_id == *source_id
            && request.context.lifecycle_generation == lifecycle_generation
            && request.context.root_identity == *root_identity
            && request.context.revision == revision)
            .then(|| self.pending.take().expect("pending request was checked"))
    }

    fn ready_request(
        &self,
        source_id: &SourceId,
        lifecycle_generation: u64,
        root_identity: &RootIdentity,
        revision: u64,
        request_id: u64,
    ) -> Option<CatalogMetadataRequest> {
        let request = self.pending.as_ref()?;
        (request.request_id == request_id
            && request.context.source_id == *source_id
            && request.context.lifecycle_generation == lifecycle_generation
            && request.context.root_identity == *root_identity
            && request.context.revision == revision)
            .then(|| request.clone())
    }

    fn take_failed(
        &mut self,
        source_id: &SourceId,
        lifecycle_generation: u64,
        revision: u64,
        request_id: u64,
    ) -> Option<CatalogMetadataRequest> {
        let request = self.pending.as_ref()?;
        (request.request_id == request_id
            && request.context.source_id == *source_id
            && request.context.lifecycle_generation == lifecycle_generation
            && request.context.revision == revision)
            .then(|| self.pending.take().expect("pending request was checked"))
    }

    fn complete(&mut self, request: &CatalogMetadataRequest, requested_paths: &[PathBuf]) {
        for path in requested_paths {
            if request.paths.contains(path) {
                self.terminal_paths.insert(path.clone());
            }
        }
        self.error = None;
        self.retry_ready = !self.next_paths().is_empty();
    }

    fn fail(&mut self, _request: &CatalogMetadataRequest, error: String) {
        self.retry_ready = false;
        self.error = Some(error);
    }

    fn next_paths(&self) -> Vec<PathBuf> {
        let pending_paths = self
            .pending
            .as_ref()
            .map(|request| request.paths.as_slice())
            .unwrap_or_default();
        self.latest_demand
            .iter()
            .filter(|path| !self.terminal_paths.contains(*path) && !pending_paths.contains(path))
            .cloned()
            .collect()
    }

    pub(super) fn poll_needed(&self) -> bool {
        self.pending.is_some() || self.retry_ready || self.debounce_task.is_some()
    }

    pub(super) fn status_label(&self) -> Option<String> {
        self.error
            .as_ref()
            .map(|error| format!("Metadata: {error}"))
    }
}

impl Browser {
    pub(super) fn observe_catalog_metadata(
        &mut self,
        context: CatalogMetadataContext,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.catalog_metadata.observe_visible_demand(context, paths) {
            self.schedule_catalog_metadata_debounce(cx);
        }
        if self.catalog_metadata.poll_needed() {
            self.start_source_registry_poll(cx);
        }
    }

    fn schedule_catalog_metadata_debounce(&mut self, cx: &mut Context<Self>) {
        let Some(generation) = self.catalog_metadata.begin_debounce() else {
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            Timer::after(METADATA_DEBOUNCE).await;
            let _ = this.update(cx, |view, cx| {
                if view.catalog_metadata.settle_debounce(generation) {
                    if view.retry_catalog_metadata(cx) {
                        cx.notify();
                    }
                }
            });
        });
        self.catalog_metadata.set_debounce_task(task);
    }

    pub(super) fn retry_catalog_metadata(&mut self, cx: &mut Context<Self>) -> bool {
        let audit_ready = self.catalog_activation.as_ref().is_some_and(|activation| {
            activation.audit_finished && activation.audit_error.is_none()
        });
        if !audit_ready || self.catalog_projection_task.is_some() {
            return false;
        }
        let Some(request) = self.catalog_metadata.begin_request() else {
            return false;
        };
        let request_id = request.request_id;
        let result = catalog::submit_catalog_command(request.into_command(), cx);
        match result {
            Ok(()) => true,
            Err(CatalogSubmitError::Full | CatalogSubmitError::NotReady) => self
                .catalog_metadata
                .admission_failed(request_id, true, None),
            Err(error) => self.catalog_metadata.admission_failed(
                request_id,
                false,
                Some(format!("could not load visible WAV metadata: {error}")),
            ),
        }
    }

    pub(super) fn apply_catalog_metadata_event(&mut self, event: &CatalogEvent) -> bool {
        match event {
            CatalogEvent::WavMetadataReady {
                source_id,
                lifecycle_generation,
                root_identity,
                revision,
                request_id,
                requested_paths,
                metadata,
            } => {
                let Some(candidate) = self.catalog_metadata.ready_request(
                    source_id,
                    *lifecycle_generation,
                    root_identity,
                    *revision,
                    *request_id,
                ) else {
                    return false;
                };
                let Some(active_index) = self.active_source.filter(|index| {
                    self.source_sessions.get(*index).is_some_and(|session| {
                        session.source_id.as_ref() == Some(source_id)
                            && session.root == candidate.context.root
                            && session.catalog_lifecycle_generation
                                == Some(candidate.context.lifecycle_generation)
                    })
                }) else {
                    return false;
                };
                let Some(request) = self.catalog_metadata.take_ready(
                    source_id,
                    *lifecycle_generation,
                    root_identity,
                    *revision,
                    *request_id,
                ) else {
                    return false;
                };
                let requested = request.paths.iter().collect::<BTreeSet<_>>();
                for (path, wav_metadata) in metadata {
                    if !requested.contains(&path) || wav_metadata.sample_rate == 0 {
                        continue;
                    }
                    let Some(index) = request.context.entry_indices.get(path).copied() else {
                        continue;
                    };
                    let browser_entry_matches = self
                        .entries
                        .get(index)
                        .is_some_and(|entry| entry.relative_path == *path);
                    let session_entry_matches = self
                        .source_sessions
                        .get(active_index)
                        .and_then(|session| session.entries.get(index))
                        .is_some_and(|entry| entry.relative_path == *path);
                    if !browser_entry_matches || !session_entry_matches {
                        continue;
                    }
                    if let Some(entry) = self.entries.get_mut(index) {
                        apply_wav_metadata(entry, wav_metadata.frames, wav_metadata.sample_rate);
                    }
                    if let Some(entry) = self.source_sessions[active_index].entries.get_mut(index) {
                        apply_wav_metadata(entry, wav_metadata.frames, wav_metadata.sample_rate);
                    }
                }
                self.catalog_metadata.complete(&request, requested_paths);
                true
            }
            CatalogEvent::WavMetadataFailed {
                source_id,
                lifecycle_generation,
                expected_revision,
                request_id,
                error,
            } => {
                let Some(request) = self.catalog_metadata.take_failed(
                    source_id,
                    *lifecycle_generation,
                    *expected_revision,
                    *request_id,
                ) else {
                    return false;
                };
                self.catalog_metadata.fail(
                    &request,
                    format!("could not load visible WAV metadata: {error}"),
                );
                true
            }
            _ => false,
        }
    }

    pub(super) fn reset_catalog_metadata(&mut self) {
        self.catalog_metadata.reset();
        self.display_labels.reset_for_projection();
    }
}

fn apply_wav_metadata(entry: &mut yield_core::AudioFile, frames: u32, sample_rate: u32) {
    entry.duration_seconds = Some(frames as f32 / sample_rate as f32);
    entry.sample_rate = Some(sample_rate);
}

impl CatalogMetadataRequest {
    fn into_command(self) -> CatalogCommand {
        CatalogCommand::RequestWavMetadata {
            source_id: self.context.source_id,
            lifecycle_generation: self.context.lifecycle_generation,
            expected_revision: self.context.revision,
            request_id: self.request_id,
            relative_paths: self.paths,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yield_library::sample_sources::reconciliation::RootIdentity;

    fn context(paths: &[&str]) -> CatalogMetadataContext {
        CatalogMetadataContext {
            source_id: SourceId::from_string("source"),
            lifecycle_generation: 3,
            root: PathBuf::from("/library"),
            root_identity: RootIdentity::from_bytes(b"root".to_vec()),
            directory_generation: 2,
            revision: 7,
            entry_indices: Arc::new(
                paths
                    .iter()
                    .enumerate()
                    .map(|(index, path)| (PathBuf::from(path), index))
                    .collect(),
            ),
        }
    }

    #[test]
    fn visible_demand_is_coalesced_and_batched_at_sixteen_paths() {
        let paths = (0..20)
            .map(|index| format!("{index}.wav"))
            .collect::<Vec<_>>();
        let path_refs = paths.iter().map(String::as_str).collect::<Vec<_>>();
        let context = context(&path_refs);
        let mut state = CatalogMetadataState::default();
        state.install_projection(context.clone());
        assert!(
            state.observe_visible_demand(context, paths.iter().rev().map(PathBuf::from).collect(),)
        );
        let generation = state.begin_debounce().expect("debounce admitted");
        assert!(state.settle_debounce(generation));
        let request = state.begin_request().expect("first batch admitted");
        assert_eq!(request.paths.len(), 16);
        assert_eq!(request.paths[0], PathBuf::from("19.wav"));
        state.admission_failed(request.request_id, true, None);
        assert!(state.poll_needed());
    }

    #[test]
    fn terminal_omissions_suppress_retries_for_the_same_revision() {
        let context = context(&["missing.wav", "known.wav"]);
        let mut state = CatalogMetadataState::default();
        state.install_projection(context.clone());
        assert!(state.observe_visible_demand(
            context,
            vec![PathBuf::from("missing.wav"), PathBuf::from("known.wav")],
        ));
        let generation = state.begin_debounce().expect("debounce admitted");
        state.settle_debounce(generation);
        let request = state.begin_request().expect("request admitted");
        let root_identity = RootIdentity::from_bytes(b"root".to_vec());
        let accepted = state
            .take_ready(
                &request.context.source_id,
                request.context.lifecycle_generation,
                &root_identity,
                request.context.revision,
                request.request_id,
            )
            .expect("request completion is current");
        state.complete(&accepted, &accepted.paths);
        assert!(!state.poll_needed());
    }

    #[test]
    fn stale_source_or_revision_completion_is_rejected() {
        let context = context(&["sample.wav"]);
        let mut state = CatalogMetadataState::default();
        state.install_projection(context.clone());
        state.observe_visible_demand(context, vec![PathBuf::from("sample.wav")]);
        let generation = state.begin_debounce().expect("debounce admitted");
        state.settle_debounce(generation);
        let request = state.begin_request().expect("request admitted");
        let wrong_source = SourceId::from_string("other");
        let root = RootIdentity::from_bytes(b"root".to_vec());
        assert!(
            state
                .take_ready(&wrong_source, 3, &root, 7, request.request_id)
                .is_none()
        );
        assert!(
            state
                .take_ready(&request.context.source_id, 3, &root, 8, request.request_id)
                .is_none()
        );
        assert!(
            state
                .take_ready(&request.context.source_id, 3, &root, 7, request.request_id,)
                .is_some()
        );
    }

    #[test]
    fn reset_same_projection_rejects_an_old_request_id() {
        let context = context(&["sample.wav"]);
        let mut state = CatalogMetadataState::default();
        state.install_projection(context.clone());
        assert!(state.observe_visible_demand(context.clone(), vec![PathBuf::from("sample.wav")]));
        let generation = state.begin_debounce().expect("first debounce admitted");
        state.settle_debounce(generation);
        let old_request = state.begin_request().expect("first request admitted");

        state.install_projection(context.clone());
        assert!(state.observe_visible_demand(context, vec![PathBuf::from("sample.wav")]));
        let generation = state.begin_debounce().expect("second debounce admitted");
        state.settle_debounce(generation);
        let new_request = state.begin_request().expect("second request admitted");
        assert_ne!(old_request.request_id, new_request.request_id);
        assert!(
            state
                .take_ready(
                    &new_request.context.source_id,
                    new_request.context.lifecycle_generation,
                    &new_request.context.root_identity,
                    new_request.context.revision,
                    old_request.request_id,
                )
                .is_none()
        );
        assert!(
            state
                .take_ready(
                    &new_request.context.source_id,
                    new_request.context.lifecycle_generation,
                    &new_request.context.root_identity,
                    new_request.context.revision,
                    new_request.request_id,
                )
                .is_some()
        );
    }

    #[test]
    fn curation_pause_blocks_old_revision_demand_until_projection_advances() {
        let context = context(&["sample.wav"]);
        let mut state = CatalogMetadataState::default();
        state.install_projection(context.clone());
        state.pause_for_curation();
        assert!(!state.observe_visible_demand(context.clone(), vec![PathBuf::from("sample.wav")]));
        assert!(!state.poll_needed());

        assert!(state.advance_projection_revision(8));
        let advanced = state.context().expect("advanced projection context");
        assert_eq!(advanced.revision, 8);
        assert!(state.observe_visible_demand(advanced, vec![PathBuf::from("sample.wav")]));
    }

    #[test]
    fn rejected_staged_curation_resumes_demand_at_unchanged_revision() {
        let context = context(&["sample.wav"]);
        let mut state = CatalogMetadataState::default();
        state.install_projection(context.clone());
        state.pause_for_curation();
        assert!(!state.observe_visible_demand(context.clone(), vec![PathBuf::from("sample.wav")]));
        state.resume_after_rejected_curation();
        assert_eq!(
            state.context().expect("retained context").revision,
            context.revision
        );
        assert!(state.observe_visible_demand(context, vec![PathBuf::from("sample.wav")]));
    }

    #[test]
    fn empty_visible_demand_cancels_follow_on_batches() {
        let context = context(&["sample.wav", "other.wav"]);
        let mut state = CatalogMetadataState::default();
        state.install_projection(context.clone());
        assert!(state.observe_visible_demand(
            context.clone(),
            vec![PathBuf::from("sample.wav"), PathBuf::from("other.wav")],
        ));
        let generation = state.begin_debounce().expect("debounce admitted");
        state.settle_debounce(generation);
        let request = state.begin_request().expect("request admitted");
        assert!(!state.observe_visible_demand(context.clone(), Vec::new()));
        let accepted = state
            .take_ready(
                &request.context.source_id,
                request.context.lifecycle_generation,
                &request.context.root_identity,
                request.context.revision,
                request.request_id,
            )
            .expect("request completion is current");
        state.complete(&accepted, &accepted.paths);
        assert!(!state.poll_needed());
    }

    #[test]
    fn metadata_patch_preserves_audio_file_identity_and_path() {
        let mut entry = yield_core::AudioFile {
            path: PathBuf::from("/library/sample.wav"),
            relative_path: PathBuf::from("sample.wav"),
            byte_len: 12,
            duration_seconds: None,
            sample_rate: None,
            playback_type: Some(yield_core::PlaybackType::Loop),
            curation: yield_core::AudioFileCuration::from_parts(
                3,
                true,
                0b0010_0101,
                Some(11),
                Some(22),
            ),
        };
        let before = entry.clone();
        apply_wav_metadata(&mut entry, 48_000, 48_000);
        assert_eq!(entry.path, before.path);
        assert_eq!(entry.relative_path, before.relative_path);
        assert_eq!(entry.byte_len, before.byte_len);
        assert_eq!(entry.curation, before.curation);
        assert_eq!(entry.playback_type, before.playback_type);
        assert_eq!(entry.duration_seconds, Some(1.0));
        assert_eq!(entry.sample_rate, Some(48_000));
    }
}
