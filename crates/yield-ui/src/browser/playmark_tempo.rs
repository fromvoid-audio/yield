//! Read the authored playmark of the exact waveform currently displayed.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::Context;
use yield_library::sample_sources::SourcePublicationFence;
use yield_media::WaveformFileRevision;
use yield_session::{
    CatalogCommand, CatalogEvent, CatalogPublicationStatus, PlaymarkTempoOperation,
    SourcePlaymarkTempoRequest,
};

use super::{Browser, catalog, waveform::WaveformPlayRegion};

struct Load {
    request: SourcePlaymarkTempoRequest,
    generation: u64,
    file_revision: WaveformFileRevision,
    cancellation: Arc<AtomicBool>,
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
    }
}

pub(super) fn load_for_waveform(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    if browser.playmark_tempo.restore_blocked
        || browser.waveform_play_region.is_some()
        || browser.waveform_region_drag.is_some()
        || browser
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
    let may_restore = browser
        .playmark_tempo
        .load
        .as_ref()
        .is_some_and(|load| load.request == *request && load.matches(browser));
    if !may_restore
        || browser.playmark_tempo.restore_blocked
        || *publication != CatalogPublicationStatus::Current
        || !fence.is_some_and(SourcePublicationFence::is_live)
        || browser.waveform_play_region.is_some()
        || browser.waveform_region_drag.is_some()
    {
        return true;
    }
    if let Ok(Some(tempo)) = result
        && let Some(preview) = browser.waveform.as_ref()
    {
        let (total, sample_rate, beats) = tempo.dimensions();
        if total == preview.total_frames as u64
            && sample_rate == preview.sample_rate
            && beats <= crate::beat_count_input::MAX_BEAT_COUNT
        {
            let (first, end) = tempo.frame_range();
            if let Some(region) =
                WaveformPlayRegion::new(first as f64 / total as f64, end as f64 / total as f64)
            {
                browser.set_waveform_play_region(Some(region));
                browser.retarget_committed_playmark_resize(region, cx);
                browser
                    .beat_count_input
                    .update(cx, |input, cx| input.set_count(beats, cx));
                cx.notify();
            }
        }
    }
    true
}
