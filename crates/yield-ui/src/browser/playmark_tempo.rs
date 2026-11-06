//! Read and commit the authored playmark of the exact waveform currently displayed.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::Context;
use yield_library::sample_sources::SourcePublicationFence;
use yield_media::WaveformFileRevision;
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogPublicationStatus, PlaymarkTempoFailure,
    PlaymarkTempoOperation, SourcePlaymarkTempo, SourcePlaymarkTempoRequest,
};

use super::{Browser, catalog, similar_sections, waveform::WaveformPlayRegion};

struct Save {
    next: SourcePlaymarkTempo,
    request: Option<SourcePlaymarkTempoRequest>,
    cancellation: Arc<AtomicBool>,
}

impl Drop for Save {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
    }
}

struct Load {
    request: SourcePlaymarkTempoRequest,
    generation: u64,
    file_revision: WaveformFileRevision,
    cancellation: Arc<AtomicBool>,
    result: Option<Result<Option<SourcePlaymarkTempo>, PlaymarkTempoFailure>>,
    save: Option<Save>,
}

impl Load {
    fn matches(&self, browser: &Browser) -> bool {
        catalog::audited_waveform_source(browser).is_some_and(|(entry, detail, source)| {
            self.generation == browser.waveform_generation
                && self.file_revision == detail.file_revision
                && self.request.source_id() == &source.source_id
                && self.request.lifecycle_generation() == source.lifecycle_generation
                && self.request.revision() == source.revision
                && self.request.relative_path() == entry.relative_path
        })
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
    }
}

#[derive(Default)]
pub(super) struct PlaymarkTempoState {
    load: Option<Load>,
    next_request_id: u64,
    restore_blocked: bool,
}

impl PlaymarkTempoState {
    pub(super) fn invalidate(&mut self) {
        self.load = None;
        self.restore_blocked = false;
    }

    pub(super) fn preserve_local_edit(&mut self) {
        self.restore_blocked = true;
        if self.load.as_ref().is_some_and(|load| load.save.is_some()) {
            self.load = None;
        }
    }
}

pub(super) fn load_for_waveform(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if browser.playmark_tempo.restore_blocked
        || browser.waveform_play_region.is_some()
        || browser.waveform_region_drag.is_some()
    {
        return false;
    }
    ensure_load(browser, cx)
}

fn ensure_load(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if browser
        .playmark_tempo
        .load
        .as_ref()
        .is_some_and(|load| load.matches(browser))
    {
        return false;
    }
    browser.playmark_tempo.load = None;
    let Some((entry, detail, source)) = catalog::audited_waveform_source(browser) else {
        return false;
    };
    let Some(id) = browser.playmark_tempo.next_request_id.checked_add(1) else {
        return false;
    };
    let Ok(request) = SourcePlaymarkTempoRequest::new(
        &source.source_id,
        source.lifecycle_generation,
        source.revision,
        id,
        &entry.relative_path,
        PlaymarkTempoOperation::Read,
    ) else {
        return false;
    };
    let load = Load {
        request,
        generation: browser.waveform_generation,
        file_revision: detail.file_revision.clone(),
        cancellation: Arc::new(AtomicBool::new(false)),
        result: None,
        save: None,
    };
    if catalog::submit_catalog_command(
        CatalogCommand::PlaymarkTempo {
            request: load.request.clone(),
            cancellation: Arc::clone(&load.cancellation),
        },
        cx,
    )
    .is_err()
    {
        return false;
    }
    browser.playmark_tempo.next_request_id = id;
    browser.playmark_tempo.load = Some(load);
    true
}

/// Handle the release when the loaded waveform belongs to an audited Source.
pub(super) fn save_released_tempo(
    browser: &mut Browser,
    tempo: SourcePlaymarkTempo,
    cx: &mut Context<Browser>,
) -> bool {
    if catalog::audited_waveform_source(browser).is_none() {
        return false;
    }
    browser.playmark_tempo.restore_blocked = true;
    ensure_load(browser, cx);
    let Some(load) = browser.playmark_tempo.load.as_mut() else {
        browser.set_playback_status("Playmark tempo save unavailable");
        return true;
    };
    if load.save.is_some() {
        browser.set_playback_status("A playmark tempo save is already pending");
        return true;
    }
    load.save = Some(Save {
        next: tempo,
        request: None,
        cancellation: Arc::new(AtomicBool::new(false)),
    });
    browser.set_playback_status("Saving playmark tempo…");
    submit_ready_save(browser, cx);
    true
}

fn submit_ready_save(browser: &mut Browser, cx: &mut Context<Browser>) {
    let Some(load) = browser.playmark_tempo.load.as_ref() else {
        return;
    };
    let Some(save) = load.save.as_ref().filter(|save| save.request.is_none()) else {
        return;
    };
    let Some(result) = load.result else {
        return;
    };
    let previous = match result {
        Ok(previous) => previous,
        Err(error) => {
            fail_save(browser, error);
            return;
        }
    };
    let Some(id) = browser.playmark_tempo.next_request_id.checked_add(1) else {
        fail_save(browser, PlaymarkTempoFailure::InvalidInput);
        return;
    };
    let request = SourcePlaymarkTempoRequest::new(
        load.request.source_id(),
        load.request.lifecycle_generation(),
        load.request.revision(),
        id,
        load.request.relative_path(),
        PlaymarkTempoOperation::Replace {
            previous,
            next: Some(save.next),
        },
    );
    let Ok(request) = request else {
        fail_save(browser, PlaymarkTempoFailure::InvalidInput);
        return;
    };
    if catalog::submit_catalog_command(
        CatalogCommand::PlaymarkTempo {
            request: request.clone(),
            cancellation: Arc::clone(&save.cancellation),
        },
        cx,
    )
    .is_err()
    {
        fail_save(browser, PlaymarkTempoFailure::Store);
        return;
    }
    browser.playmark_tempo.next_request_id = id;
    if let Some(save) = browser
        .playmark_tempo
        .load
        .as_mut()
        .and_then(|load| load.save.as_mut())
    {
        save.request = Some(request);
    }
}

fn fail_save(browser: &mut Browser, error: PlaymarkTempoFailure) {
    browser.playmark_tempo.load = None;
    let message = match error {
        PlaymarkTempoFailure::Conflict => {
            "Playmark tempo changed in the Source. Resize again to retry"
        }
        PlaymarkTempoFailure::Cancelled => "Playmark tempo save canceled",
        PlaymarkTempoFailure::StaleSource => "Playmark tempo save lost its current Source",
        _ => "Playmark tempo save unavailable",
    };
    browser.set_playback_status(message);
}

pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &CatalogEvent,
    fence: Option<&SourcePublicationFence>,
    cx: &mut Context<Browser>,
) -> bool {
    let CatalogEvent::PlaymarkTempoCompleted {
        request,
        publication,
        result,
        ..
    } = event
    else {
        return false;
    };
    let Some(load) = browser.playmark_tempo.load.as_ref() else {
        return true;
    };
    let read = load.request == *request && load.result.is_none();
    let save = load
        .save
        .as_ref()
        .filter(|save| save.request.as_ref() == Some(request));
    if !read && save.is_none() {
        return true;
    }
    if !load.matches(browser)
        || *publication != CatalogPublicationStatus::Current
        || !fence.is_some_and(SourcePublicationFence::is_live)
    {
        if load.save.is_some() {
            fail_save(
                browser,
                result.err().unwrap_or(PlaymarkTempoFailure::StaleSource),
            );
            cx.notify();
        }
        return true;
    }

    if let Some(save) = save {
        match result {
            Ok(Some(tempo)) if *tempo == save.next => {
                if let Some(load) = browser.playmark_tempo.load.as_mut() {
                    load.result = Some(*result);
                    load.save = None;
                }
                browser.set_playback_status("Playmark tempo saved");
                if browser.waveform_region_drag.is_none() {
                    apply_tempo(browser, *tempo, cx);
                }
            }
            Ok(_) => fail_save(browser, PlaymarkTempoFailure::Store),
            Err(error) => fail_save(browser, *error),
        }
        cx.notify();
        return true;
    }

    if let Some(load) = browser.playmark_tempo.load.as_mut() {
        load.result = Some(*result);
    }
    if browser
        .playmark_tempo
        .load
        .as_ref()
        .is_some_and(|load| load.save.is_some())
    {
        submit_ready_save(browser, cx);
        cx.notify();
    } else if !browser.playmark_tempo.restore_blocked
        && browser.waveform_play_region.is_none()
        && browser.waveform_region_drag.is_none()
        && let Ok(Some(tempo)) = result
    {
        apply_tempo(browser, *tempo, cx);
    }
    true
}

fn apply_tempo(browser: &mut Browser, tempo: SourcePlaymarkTempo, cx: &mut Context<Browser>) {
    let Some(preview) = browser.waveform.as_ref() else {
        return;
    };
    let (total, sample_rate, beats) = tempo.dimensions();
    if total != preview.total_frames as u64
        || sample_rate != preview.sample_rate
        || beats > crate::beat_count_input::MAX_BEAT_COUNT
    {
        return;
    }
    let (first, end) = tempo.frame_range();
    if let Some(region) =
        WaveformPlayRegion::new(first as f64 / total as f64, end as f64 / total as f64)
    {
        browser.set_waveform_play_region(Some(region));
        similar_sections::invalidate_if_stale(browser);
        browser.retarget_committed_playmark_resize(region, cx);
        browser.beat_guide_count = beats;
        browser
            .beat_count_input
            .update(cx, |input, cx| input.set_count(beats, cx));
        cx.notify();
    }
}
