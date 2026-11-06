//! Source-owned persistence for the current waveform's explicit paint tempo.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::Context;
use yield_library::sample_sources::{SourcePublicationFence, readiness::SourceFeatureInputError};
use yield_media::WaveformFileRevision;
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogPublicationStatus, PlaymarkGridTempoOperation,
    PlaymarkTempoFailure, SourceContentDigestFailure, SourceContentDigestRequest,
    SourceContentDigestScope, SourcePlaymarkGridTempo, SourcePlaymarkGridTempoRequest,
};

use super::{Browser, catalog, source_digest};

const PENDING_DIGEST: PlaymarkTempoFailure =
    PlaymarkTempoFailure::Input(SourceFeatureInputError::ContentPending);

struct PendingRequest {
    request: SourcePlaymarkGridTempoRequest,
    cancellation: Arc<AtomicBool>,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
    }
}

struct Binding {
    read: PendingRequest,
    waveform_generation: u64,
    file_revision: WaveformFileRevision,
    prior: Option<Result<Option<SourcePlaymarkGridTempo>, PlaymarkTempoFailure>>,
    desired: Option<SourcePlaymarkGridTempo>,
    writing: Option<PendingRequest>,
    digest: Option<Arc<source_digest::PendingSourceDigest>>,
}

impl Binding {
    fn matches(&self, browser: &Browser) -> bool {
        self.matches_waveform(browser, Some(self.read.request.revision()))
    }

    fn matches_waveform(&self, browser: &Browser, revision: Option<u64>) -> bool {
        catalog::audited_waveform_source(browser).is_some_and(|(entry, detail, source)| {
            self.waveform_generation == browser.waveform_generation
                && self.file_revision == detail.file_revision
                && self.read.request.source_id() == &source.source_id
                && self.read.request.lifecycle_generation() == source.lifecycle_generation
                && revision.is_none_or(|revision| revision == source.revision)
                && self.read.request.relative_path() == entry.relative_path
        })
    }

    fn awaits_digest(&self, scope: &SourceContentDigestScope) -> bool {
        self.desired.is_some()
            && self.writing.is_none()
            && self.read.request.digest_scope() == *scope
    }
}

#[derive(Default)]
pub(super) struct GridTempoPersistence {
    binding: Option<Binding>,
    next_id: u64,
    local: bool,
}

impl GridTempoPersistence {
    pub(super) fn invalidate(&mut self) {
        self.binding = None;
        self.local = false;
    }
}

pub(super) fn load_for_waveform(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if browser
        .bpm_snap_entry
        .persistence
        .binding
        .as_ref()
        .is_some_and(|binding| binding.matches(browser))
    {
        return false;
    }
    let pending = browser
        .bpm_snap_entry
        .persistence
        .binding
        .take()
        .is_some_and(|binding| binding.desired.is_some());
    if pending {
        browser.set_playback_status(
            "BPM Snap tempo applies to this session. Source revision changed.",
        );
    }
    let Some((entry, detail, source)) = catalog::audited_waveform_source(browser) else {
        return false;
    };
    let Some(id) = browser.bpm_snap_entry.persistence.next_id.checked_add(1) else {
        return false;
    };
    let Ok(request) = SourcePlaymarkGridTempoRequest::new(
        &source.source_id,
        source.lifecycle_generation,
        source.revision,
        id,
        &entry.relative_path,
        PlaymarkGridTempoOperation::Read,
    ) else {
        return false;
    };
    let file_revision = detail.file_revision.clone();
    let Some(read) = submit(request, cx) else {
        return false;
    };
    let state = &mut browser.bpm_snap_entry.persistence;
    state.next_id = id;
    state.binding = Some(Binding {
        read,
        waveform_generation: browser.waveform_generation,
        file_revision,
        prior: None,
        desired: None,
        writing: None,
        digest: None,
    });
    browser.start_source_registry_poll(cx);
    true
}

pub(super) fn commit_tempo(
    browser: &mut Browser,
    tempo: SourcePlaymarkGridTempo,
    cx: &mut Context<Browser>,
) {
    let entry = &mut browser.bpm_snap_entry;
    entry.tempo = Some(tempo);
    entry.persistence.local = true;
    if catalog::audited_waveform_source(browser).is_none() {
        browser.set_playback_status(format!("BPM Snap tempo set to {} BPM", tempo.bpm()));
        return;
    }
    if browser
        .bpm_snap_entry
        .persistence
        .binding
        .as_ref()
        .is_some_and(|binding| {
            binding
                .prior
                .is_some_and(|prior| prior.is_err_and(|error| error != PENDING_DIGEST))
        })
    {
        browser.bpm_snap_entry.persistence.binding = None;
    }
    load_for_waveform(browser, cx);
    let Some(binding) = browser.bpm_snap_entry.persistence.binding.as_mut() else {
        browser.set_playback_status(
            "BPM Snap tempo applies to this session. Source save unavailable.",
        );
        return;
    };
    binding.desired = Some(tempo);
    browser.set_playback_status("Saving BPM Snap tempo…");
    save_if_ready(browser, cx);
}

fn submit(
    request: SourcePlaymarkGridTempoRequest,
    cx: &mut Context<Browser>,
) -> Option<PendingRequest> {
    let cancellation = Arc::new(AtomicBool::new(false));
    catalog::submit_catalog_command(
        CatalogCommand::PlaymarkGridTempo {
            request: request.clone(),
            cancellation: Arc::clone(&cancellation),
        },
        cx,
    )
    .ok()?;
    Some(PendingRequest {
        request,
        cancellation,
    })
}

fn save_if_ready(browser: &mut Browser, cx: &mut Context<Browser>) {
    let state = &browser.bpm_snap_entry.persistence;
    let Some(binding) = state
        .binding
        .as_ref()
        .filter(|binding| binding.writing.is_none())
    else {
        return;
    };
    let (Some(desired), Some(prior)) = (binding.desired, binding.prior) else {
        return;
    };
    let prior = match prior {
        Ok(prior) => prior,
        Err(PENDING_DIGEST) => {
            request_digest(browser, cx);
            return;
        }
        Err(error) => {
            fail(browser, error);
            return;
        }
    };
    if prior == Some(desired) {
        settle(browser, desired);
        return;
    }
    let Some(id) = state.next_id.checked_add(1) else {
        fail(browser, PlaymarkTempoFailure::InvalidInput);
        return;
    };
    let read = &binding.read.request;
    let operation = PlaymarkGridTempoOperation::Replace {
        previous: prior,
        next: Some(desired),
    };
    let request = SourcePlaymarkGridTempoRequest::new(
        read.source_id(),
        read.lifecycle_generation(),
        read.revision(),
        id,
        read.relative_path(),
        operation,
    );
    let Some(writing) = request.ok().and_then(|request| submit(request, cx)) else {
        fail(browser, PlaymarkTempoFailure::Store);
        return;
    };
    let state = &mut browser.bpm_snap_entry.persistence;
    state.next_id = id;
    if let Some(binding) = state.binding.as_mut() {
        binding.writing = Some(writing);
    }
}

fn request_digest(browser: &mut Browser, cx: &mut Context<Browser>) {
    let Some(binding) = browser
        .bpm_snap_entry
        .persistence
        .binding
        .as_ref()
        .filter(|binding| binding.matches(browser))
    else {
        fail(browser, PlaymarkTempoFailure::StaleSource);
        return;
    };
    if binding.digest.is_none() {
        let scope = binding.read.request.digest_scope();
        let Some(digest) = source_digest::acquire(browser, &scope, cx) else {
            fail(browser, PlaymarkTempoFailure::Store);
            return;
        };
        if let Some(binding) = browser.bpm_snap_entry.persistence.binding.as_mut() {
            binding.digest = Some(digest);
        }
    }
    browser.set_playback_status("Verifying Source audio for BPM Snap tempo…");
}

pub(super) fn adopt_digest_revision(
    browser: &mut Browser,
    scope: &SourceContentDigestScope,
    revision: u64,
    cx: &mut Context<Browser>,
) {
    let state = &browser.bpm_snap_entry.persistence;
    let Some(binding) = state
        .binding
        .as_ref()
        .filter(|binding| binding.awaits_digest(scope))
    else {
        return;
    };
    if !binding.matches_waveform(browser, Some(revision)) {
        fail(browser, PlaymarkTempoFailure::StaleSource);
        cx.notify();
        return;
    }
    let desired = browser
        .bpm_snap_entry
        .persistence
        .binding
        .take()
        .and_then(|binding| binding.desired);
    if load_for_waveform(browser, cx)
        && let Some(binding) = browser.bpm_snap_entry.persistence.binding.as_mut()
    {
        binding.desired = desired;
        browser.set_playback_status("Saving BPM Snap tempo…");
    } else {
        browser.set_playback_status(
            "BPM Snap tempo applies to this session. Source save unavailable.",
        );
    }
    cx.notify();
}

pub(super) fn reject_digest_revision(
    browser: &mut Browser,
    scope: &SourceContentDigestScope,
    cx: &mut Context<Browser>,
) {
    if browser
        .bpm_snap_entry
        .persistence
        .binding
        .as_ref()
        .is_some_and(|binding| binding.awaits_digest(scope))
    {
        fail(browser, PlaymarkTempoFailure::StaleSource);
        cx.notify();
    }
}

fn settle(browser: &mut Browser, tempo: SourcePlaymarkGridTempo) {
    let state = &mut browser.bpm_snap_entry.persistence;
    if let Some(binding) = state.binding.as_mut() {
        binding.desired = None;
    }
    state.local = false;
    browser.set_playback_status(format!("BPM Snap tempo saved at {} BPM", tempo.bpm()));
}

fn fail(browser: &mut Browser, error: PlaymarkTempoFailure) {
    let Some(binding) = browser.bpm_snap_entry.persistence.binding.as_mut() else {
        return;
    };
    binding.prior = Some(Err(error));
    binding.writing = None;
    binding.digest = None;
    if binding.desired.take().is_some() {
        let message = match error {
            PlaymarkTempoFailure::Conflict => {
                "BPM Snap tempo changed in the Source. Enter the BPM again to retry."
            }
            _ => "BPM Snap tempo applies to this session. Source save unavailable.",
        };
        browser.set_playback_status(message);
    }
}

pub(super) fn fail_digest(
    browser: &mut Browser,
    request: &SourceContentDigestRequest,
    error: SourceContentDigestFailure,
    cx: &mut Context<Browser>,
) {
    let current = browser
        .bpm_snap_entry
        .persistence
        .binding
        .as_ref()
        .is_some_and(|binding| {
            binding.matches_waveform(browser, None)
                && binding
                    .digest
                    .as_ref()
                    .is_some_and(|digest| digest.request == *request)
        });
    if current {
        let failure = match error {
            SourceContentDigestFailure::Input(error) => PlaymarkTempoFailure::Input(error),
            SourceContentDigestFailure::StaleSource => PlaymarkTempoFailure::StaleSource,
            _ => PlaymarkTempoFailure::Store,
        };
        fail(browser, failure);
        if error == SourceContentDigestFailure::Busy {
            browser.set_playback_status(
                "BPM Snap tempo applies to this session. Source audio verification is busy.",
            );
        } else if error == SourceContentDigestFailure::Input(SourceFeatureInputError::ByteLimit) {
            browser.set_playback_status("BPM Snap tempo applies to this session. Source audio exceeds the verification limit.");
        }
        cx.notify();
    }
}

pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    fence: Option<&SourcePublicationFence>,
    cx: &mut Context<Browser>,
) -> bool {
    let CatalogEvent::PlaymarkGridTempoCompleted {
        request,
        publication,
        result,
        ..
    } = event
    else {
        return false;
    };
    let state = &browser.bpm_snap_entry.persistence;
    let Some(binding) = state.binding.as_ref() else {
        return true;
    };
    let read = binding.read.request == *request && binding.prior.is_none();
    let write = binding
        .writing
        .as_ref()
        .is_some_and(|writing| writing.request == *request);
    if !read && !write {
        return true;
    }
    if !binding.matches(browser)
        || *publication != CatalogPublicationStatus::Current
        || !fence.is_some_and(SourcePublicationFence::is_live)
    {
        fail(browser, PlaymarkTempoFailure::StaleSource);
        cx.notify();
        return true;
    }
    let result = match (request.operation(), result) {
        (PlaymarkGridTempoOperation::Replace { next, .. }, Ok(actual)) if *actual != next => {
            Err(PlaymarkTempoFailure::Store)
        }
        (_, result) => *result,
    };
    if let Err(error) = result
        && !(read && error == PENDING_DIGEST)
    {
        fail(browser, error);
    } else {
        let state = &mut browser.bpm_snap_entry.persistence;
        if let Some(binding) = state.binding.as_mut() {
            binding.prior = Some(result);
            binding.writing = None;
        }
        if read
            && !state.local
            && let Ok(tempo) = result
        {
            browser.bpm_snap_entry.tempo = tempo;
        }
        save_if_ready(browser, cx);
    }
    cx.notify();
    true
}
