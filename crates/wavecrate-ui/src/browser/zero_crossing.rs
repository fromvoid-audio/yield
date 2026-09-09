#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded, revision-fenced zero-crossing snapping for waveform selection gestures.
//!
//! The browser keeps the raw pointer region authoritative while a background request is in
//! flight.  A snapped range is usable only when its generation, source context, and raw bounds
//! still match the current gesture or released commit.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    AnyView, App, Bounds, ClickEvent, Context, Hsla, PathBuilder, Pixels, Render, Window, canvas,
    div, point, prelude::*, px,
};
use wavecrate_media::{
    WavZeroCrossingSnapRequest, WaveformDetailContext, snap_wav_range_to_zero_crossings_with_cancel,
};

use super::Browser;
use super::waveform::{WaveformPlayRegion, WaveformRegionDrag, WaveformSelectionKind};

const ZERO_CROSSING_ICON_SIZE: f32 = 16.0;
const ZERO_CROSSING_ICON_STROKE_WIDTH: f32 = 1.25;

/// One exact waveform selection intent sent to the zero-crossing worker.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ZeroCrossingIntent {
    /// Waveform generation that owns the source context.
    pub(super) generation: u64,
    /// Revision-fenced source request and its exact raw bounds.
    pub(super) request: WavZeroCrossingSnapRequest,
    /// Selection kind and raw normalized bounds shown before the snapped result is accepted.
    pub(super) raw: ZeroCrossingBounds,
    /// Captured edit resize effects used to rebuild the final settings after a late snap.
    pub(super) edit_resize_effects: Option<(
        wavecrate_core::WaveformEditRegion,
        wavecrate_core::WaveformEditSettings,
    )>,
}

/// Selection bounds shared by playback and edit snapping without clamping edit margins.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ZeroCrossingBounds {
    pub(super) kind: WaveformSelectionKind,
    pub(super) start: f64,
    pub(super) end: f64,
}

impl ZeroCrossingBounds {
    fn new(kind: WaveformSelectionKind, start: f64, end: f64) -> Option<Self> {
        let (start, end) = match kind {
            WaveformSelectionKind::Play => {
                let region = WaveformPlayRegion::new(start, end)?;
                (region.start, region.end)
            }
            WaveformSelectionKind::Edit => {
                let region = wavecrate_core::WaveformEditRegion::new(start, end)?;
                (region.start(), region.end())
            }
        };
        Some(Self { kind, start, end })
    }

    fn from_snapped(kind: WaveformSelectionKind, start: f64, end: f64) -> Option<Self> {
        Self::new(kind, start, end)
    }

    fn as_play(self) -> Option<WaveformPlayRegion> {
        (self.kind == WaveformSelectionKind::Play)
            .then(|| WaveformPlayRegion::new(self.start, self.end))
            .flatten()
    }

    fn as_edit(self) -> Option<wavecrate_core::WaveformEditRegion> {
        (self.kind == WaveformSelectionKind::Edit)
            .then(|| wavecrate_core::WaveformEditRegion::new(self.start, self.end))
            .flatten()
    }
}

impl ZeroCrossingIntent {
    fn new(
        generation: u64,
        context: WaveformDetailContext,
        kind: WaveformSelectionKind,
        start: f64,
        end: f64,
    ) -> Option<Self> {
        Self::new_with_edit_resize_effects(generation, context, kind, start, end, None)
    }

    fn new_with_edit_resize_effects(
        generation: u64,
        context: WaveformDetailContext,
        kind: WaveformSelectionKind,
        start: f64,
        end: f64,
        edit_resize_effects: Option<(
            wavecrate_core::WaveformEditRegion,
            wavecrate_core::WaveformEditSettings,
        )>,
    ) -> Option<Self> {
        let raw = ZeroCrossingBounds::new(kind, start, end)?;
        let request = WavZeroCrossingSnapRequest::new(context, raw.start, raw.end).ok()?;
        Some(Self {
            generation,
            request,
            raw,
            edit_resize_effects,
        })
    }

    fn matches_context_and_raw(
        &self,
        generation: u64,
        context: &WaveformDetailContext,
        raw: ZeroCrossingBounds,
        edit_resize_effects: Option<(
            wavecrate_core::WaveformEditRegion,
            wavecrate_core::WaveformEditSettings,
        )>,
    ) -> bool {
        self.generation == generation
            && self.raw == raw
            && self.request.context == *context
            && self.request.start_ratio == raw.start
            && self.request.end_ratio == raw.end
            && self.edit_resize_effects == edit_resize_effects
    }
}

struct ActiveZeroCrossing {
    id: u64,
    intent: ZeroCrossingIntent,
    cancellation: Arc<AtomicBool>,
}

/// One background zero-crossing request and its cancellation fence.
pub(super) struct ZeroCrossingJob {
    id: u64,
    pub(super) intent: ZeroCrossingIntent,
    pub(super) cancellation: Arc<AtomicBool>,
}

#[derive(Clone, Debug, PartialEq)]
struct ReadyZeroCrossing {
    intent: ZeroCrossingIntent,
    snapped: ZeroCrossingBounds,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ZeroCrossingStatus {
    Preparing,
    Unavailable,
}

/// UI-owned lifecycle state for one active snap, one ready result, and one released commit.
#[derive(Default)]
pub(super) struct ZeroCrossingState {
    enabled: bool,
    next_id: u64,
    active: Option<ActiveZeroCrossing>,
    ready: Option<ReadyZeroCrossing>,
    pending_commit: Option<ZeroCrossingIntent>,
    status: Option<ZeroCrossingStatus>,
}

impl ZeroCrossingState {
    /// Return whether the transient zero-crossing toggle is enabled.
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Set the transient toggle, canceling owned work when it is disabled.
    pub(super) fn set_enabled(&mut self, enabled: bool) -> bool {
        let changed = self.enabled != enabled;
        self.enabled = enabled;
        if !enabled {
            self.cancel_for_interruption();
        }
        changed
    }

    fn begin(&mut self, intent: ZeroCrossingIntent) -> Option<ZeroCrossingJob> {
        if !self.enabled || self.active.is_some() {
            return None;
        }
        self.next_id = self.next_id.wrapping_add(1);
        let cancellation = Arc::new(AtomicBool::new(false));
        self.active = Some(ActiveZeroCrossing {
            id: self.next_id,
            intent: intent.clone(),
            cancellation: Arc::clone(&cancellation),
        });
        self.status = Some(ZeroCrossingStatus::Preparing);
        Some(ZeroCrossingJob {
            id: self.next_id,
            intent,
            cancellation,
        })
    }

    fn active_matches(&self, intent: &ZeroCrossingIntent) -> bool {
        self.active.as_ref().is_some_and(|active| {
            active.intent == *intent && !active.cancellation.load(Ordering::Acquire)
        })
    }

    fn cancel_active(&mut self) -> bool {
        let Some(active) = self.active.as_ref() else {
            return false;
        };
        active.cancellation.store(true, Ordering::Release);
        true
    }

    /// Cancel current ownership while retaining the active job until its completion callback.
    pub(super) fn cancel_for_interruption(&mut self) -> bool {
        let canceled = self.cancel_active();
        self.ready = None;
        self.pending_commit = None;
        self.status = None;
        canceled
    }

    fn try_set_pending_commit(&mut self, intent: ZeroCrossingIntent) -> bool {
        if !self.enabled {
            return false;
        }
        self.pending_commit = Some(intent);
        self.status = Some(ZeroCrossingStatus::Preparing);
        self.ready = None;
        true
    }

    fn pending_commit_matches(&self, intent: &ZeroCrossingIntent) -> bool {
        self.pending_commit.as_ref() == Some(intent)
    }

    fn take_pending_commit(&mut self, intent: &ZeroCrossingIntent) -> bool {
        if !self.pending_commit_matches(intent) {
            return false;
        }
        self.pending_commit = None;
        true
    }

    fn set_ready(&mut self, intent: ZeroCrossingIntent, snapped: ZeroCrossingBounds) {
        self.ready = Some(ReadyZeroCrossing { intent, snapped });
        self.status = None;
    }

    fn take_ready_for(&mut self, intent: &ZeroCrossingIntent) -> Option<ZeroCrossingBounds> {
        if self
            .ready
            .as_ref()
            .is_some_and(|ready| &ready.intent == intent)
        {
            return self.ready.take().map(|ready| ready.snapped);
        }
        None
    }

    /// Return a snapped preview only for the exact current raw region and source context.
    pub(super) fn ready_for(
        &self,
        generation: u64,
        context: &WaveformDetailContext,
        kind: WaveformSelectionKind,
        start: f64,
        end: f64,
    ) -> Option<ZeroCrossingBounds> {
        self.ready_for_with_edit_resize_effects(generation, context, kind, start, end, None)
    }

    fn ready_for_with_edit_resize_effects(
        &self,
        generation: u64,
        context: &WaveformDetailContext,
        kind: WaveformSelectionKind,
        start: f64,
        end: f64,
        edit_resize_effects: Option<(
            wavecrate_core::WaveformEditRegion,
            wavecrate_core::WaveformEditSettings,
        )>,
    ) -> Option<ZeroCrossingBounds> {
        let raw = ZeroCrossingBounds::new(kind, start, end)?;
        self.ready.as_ref().and_then(|ready| {
            ready
                .intent
                .matches_context_and_raw(generation, context, raw, edit_resize_effects)
                .then_some(ready.snapped)
        })
    }

    fn finish(
        &mut self,
        job: ZeroCrossingJob,
        result: Result<wavecrate_media::WavZeroCrossingSnapRange, String>,
    ) -> Option<Result<ZeroCrossingBounds, String>> {
        let active = self.active.as_ref()?;
        if active.id != job.id
            || active.intent != job.intent
            || !Arc::ptr_eq(&active.cancellation, &job.cancellation)
        {
            return None;
        }
        self.active = None;
        if job.cancellation.load(Ordering::Acquire) {
            return None;
        }
        match result {
            Ok(snapped) => {
                let region = ZeroCrossingBounds::from_snapped(
                    job.intent.raw.kind,
                    snapped.start_ratio,
                    snapped.end_ratio,
                )
                .ok_or_else(|| String::from("zero-crossing snap returned invalid bounds"));
                if region.is_err() {
                    self.status = Some(ZeroCrossingStatus::Unavailable);
                } else {
                    self.status = None;
                }
                Some(region)
            }
            Err(error) if error.contains("canceled") => None,
            Err(_) => {
                self.status = Some(ZeroCrossingStatus::Unavailable);
                Some(Err(String::from("zero-crossing snap unavailable")))
            }
        }
    }

    /// Return a bounded status label for the browser status row.
    pub(super) fn status_label(&self) -> Option<&'static str> {
        match self.status {
            Some(ZeroCrossingStatus::Preparing) => Some("Snapping zero crossings…"),
            Some(ZeroCrossingStatus::Unavailable) => Some("Zero-crossing snap unavailable"),
            None => None,
        }
    }
}

fn icon_point(bounds: Bounds<Pixels>, x: f32, y: f32) -> gpui::Point<Pixels> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let scale = (width / ZERO_CROSSING_ICON_SIZE)
        .min(height / ZERO_CROSSING_ICON_SIZE)
        .max(0.0);
    let left = f32::from(bounds.origin.x) + (width - ZERO_CROSSING_ICON_SIZE * scale) / 2.0;
    let top = f32::from(bounds.origin.y) + (height - ZERO_CROSSING_ICON_SIZE * scale) / 2.0;
    point(px(left + x * scale), px(top + y * scale))
}

/// Render the fixed 16×16 zero-crossing waveform glyph used by the toolbar.
pub(super) fn render_icon(icon_color: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            let mut axis = PathBuilder::stroke(px(ZERO_CROSSING_ICON_STROKE_WIDTH));
            axis.move_to(icon_point(bounds, 1.0, 8.0));
            axis.line_to(icon_point(bounds, 15.0, 8.0));
            if let Ok(path) = axis.build() {
                window.paint_path(path, icon_color.opacity(0.65));
            }
            let mut wave = PathBuilder::stroke(px(ZERO_CROSSING_ICON_STROKE_WIDTH));
            wave.move_to(icon_point(bounds, 1.0, 8.0));
            wave.cubic_bezier_to(
                icon_point(bounds, 3.0, 2.0),
                icon_point(bounds, 5.0, 2.0),
                icon_point(bounds, 7.0, 8.0),
            );
            wave.cubic_bezier_to(
                icon_point(bounds, 9.0, 14.0),
                icon_point(bounds, 11.0, 14.0),
                icon_point(bounds, 13.0, 8.0),
            );
            wave.line_to(icon_point(bounds, 15.0, 8.0));
            if let Ok(path) = wave.build() {
                window.paint_path(path, icon_color);
            }
        },
    )
    .w(px(ZERO_CROSSING_ICON_SIZE))
    .h(px(ZERO_CROSSING_ICON_SIZE))
}

struct ZeroCrossingTooltip;

impl Render for ZeroCrossingTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .border(px(theme.spacing.border_width))
            .border_color(crate::theme::color(theme.colors.border_emphasis))
            .text_size(px(theme.typography.small_size))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .child("Snap selection edges to nearby zero crossings.")
    }
}

/// Build the zero-crossing tooltip for the compact toolbar control.
pub(super) fn tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| ZeroCrossingTooltip).into()
}

fn intent_for_raw(
    browser: &Browser,
    kind: WaveformSelectionKind,
    start: f64,
    end: f64,
) -> Option<ZeroCrossingIntent> {
    let context = browser.waveform.as_ref()?.detail_context.as_ref()?.clone();
    ZeroCrossingIntent::new(browser.waveform_generation, context, kind, start, end)
}

fn intent_for_raw_with_edit_resize_effects(
    browser: &Browser,
    kind: WaveformSelectionKind,
    start: f64,
    end: f64,
    edit_resize_effects: Option<(
        wavecrate_core::WaveformEditRegion,
        wavecrate_core::WaveformEditSettings,
    )>,
) -> Option<ZeroCrossingIntent> {
    let context = browser.waveform.as_ref()?.detail_context.as_ref()?.clone();
    ZeroCrossingIntent::new_with_edit_resize_effects(
        browser.waveform_generation,
        context,
        kind,
        start,
        end,
        edit_resize_effects,
    )
}

fn drag_intent(browser: &Browser, drag: WaveformRegionDrag) -> Option<ZeroCrossingIntent> {
    let bpm_snap = browser.bpm_snap_settings();
    if !drag.zero_crossing_applies(bpm_snap) {
        return None;
    }
    let (start, end) = drag.preview_bounds_with_bpm(bpm_snap)?;
    intent_for_raw_with_edit_resize_effects(
        browser,
        drag.selection_kind(),
        start,
        end,
        drag.edit_resize_effects(),
    )
}

/// Return the raw or exact snapped preview for a live waveform drag.
pub(super) fn preview_drag_region(
    browser: &Browser,
    drag: WaveformRegionDrag,
) -> Option<WaveformPlayRegion> {
    if drag.selection_kind() != WaveformSelectionKind::Play {
        return None;
    }
    let bpm_snap = browser.bpm_snap_settings();
    let raw = drag.preview_region_with_bpm(bpm_snap)?;
    if !drag.zero_crossing_applies(bpm_snap) {
        return Some(raw);
    }
    let Some(context) = browser.waveform.as_ref()?.detail_context.as_ref() else {
        return Some(raw);
    };
    Some(
        browser
            .zero_crossing
            .ready_for(
                browser.waveform_generation,
                context,
                WaveformSelectionKind::Play,
                raw.start,
                raw.end,
            )
            .and_then(ZeroCrossingBounds::as_play)
            .unwrap_or(raw),
    )
}

/// Return the raw or exact snapped edit preview for a live waveform drag.
pub(super) fn preview_drag_edit_region(
    browser: &Browser,
    drag: WaveformRegionDrag,
) -> Option<wavecrate_core::WaveformEditRegion> {
    if drag.selection_kind() != WaveformSelectionKind::Edit {
        return None;
    }
    let bpm_snap = browser.bpm_snap_settings();
    let raw = drag.preview_edit_region_with_bpm(bpm_snap)?;
    if !drag.zero_crossing_applies(bpm_snap) {
        return Some(raw);
    }
    let Some(context) = browser.waveform.as_ref()?.detail_context.as_ref() else {
        return Some(raw);
    };
    Some(
        browser
            .zero_crossing
            .ready_for_with_edit_resize_effects(
                browser.waveform_generation,
                context,
                WaveformSelectionKind::Edit,
                raw.start(),
                raw.end(),
                drag.edit_resize_effects(),
            )
            .and_then(ZeroCrossingBounds::as_edit)
            .unwrap_or(raw),
    )
}

/// Cancel zero-crossing ownership for a new or incompatible waveform action.
pub(super) fn cancel_for_interruption(browser: &mut Browser) -> bool {
    browser.zero_crossing_queued = None;
    browser.zero_crossing.cancel_for_interruption()
}

/// Queue the latest live drag intent while retaining an older canceled worker until completion.
pub(super) fn schedule_for_drag(browser: &mut Browser, cx: &mut Context<Browser>) {
    let Some(drag) = browser.waveform_region_drag else {
        return;
    };
    let Some(intent) = drag_intent(browser, drag) else {
        return;
    };
    schedule_intent(browser, intent, cx);
}

/// Queue a zero-crossing request for a committed keyboard move.
pub(super) fn schedule_for_committed_region(browser: &mut Browser, cx: &mut Context<Browser>) {
    if !browser.zero_crossing.enabled() {
        return;
    }
    let Some(raw) = browser
        .waveform_play_region
        .filter(|region| region.is_valid())
    else {
        return;
    };
    let Some(intent) = intent_for_raw(browser, WaveformSelectionKind::Play, raw.start, raw.end)
    else {
        return;
    };
    if !browser.zero_crossing.try_set_pending_commit(intent.clone()) {
        return;
    }
    schedule_intent(browser, intent, cx);
}

/// Commit a released raw region immediately, then refine it when an exact snap is ready.
pub(super) fn commit_or_schedule(
    browser: &mut Browser,
    raw: WaveformPlayRegion,
    cx: &mut Context<Browser>,
) {
    commit_or_schedule_bounds(
        browser,
        WaveformSelectionKind::Play,
        raw.start,
        raw.end,
        None,
        cx,
    );
}

/// Commit a released edit region immediately, then refine it when an exact snap is ready.
pub(super) fn commit_or_schedule_edit(
    browser: &mut Browser,
    raw: wavecrate_core::WaveformEditRegion,
    edit_resize_effects: Option<(
        wavecrate_core::WaveformEditRegion,
        wavecrate_core::WaveformEditSettings,
    )>,
    cx: &mut Context<Browser>,
) {
    commit_or_schedule_bounds(
        browser,
        WaveformSelectionKind::Edit,
        raw.start(),
        raw.end(),
        edit_resize_effects,
        cx,
    );
}

fn commit_or_schedule_bounds(
    browser: &mut Browser,
    kind: WaveformSelectionKind,
    start: f64,
    end: f64,
    edit_resize_effects: Option<(
        wavecrate_core::WaveformEditRegion,
        wavecrate_core::WaveformEditSettings,
    )>,
    cx: &mut Context<Browser>,
) {
    apply_bounds(browser, kind, start, end, None);
    if !browser.zero_crossing.enabled() {
        browser.zero_crossing_queued = None;
        browser.zero_crossing.cancel_for_interruption();
        return;
    }
    let Some(intent) =
        intent_for_raw_with_edit_resize_effects(browser, kind, start, end, edit_resize_effects)
    else {
        browser.zero_crossing.cancel_for_interruption();
        browser.zero_crossing_queued = None;
        return;
    };
    if let Some(snapped) = browser.zero_crossing.take_ready_for(&intent) {
        apply_bounds(
            browser,
            snapped.kind,
            snapped.start,
            snapped.end,
            intent.edit_resize_effects,
        );
        browser.zero_crossing.take_pending_commit(&intent);
        return;
    }
    if !browser.zero_crossing.try_set_pending_commit(intent.clone()) {
        return;
    }
    schedule_intent(browser, intent, cx);
}

fn apply_bounds(
    browser: &mut Browser,
    kind: WaveformSelectionKind,
    start: f64,
    end: f64,
    edit_resize_effects: Option<(
        wavecrate_core::WaveformEditRegion,
        wavecrate_core::WaveformEditSettings,
    )>,
) {
    match kind {
        WaveformSelectionKind::Play => {
            browser.waveform_play_region = WaveformPlayRegion::new(start, end);
        }
        WaveformSelectionKind::Edit => {
            let next = wavecrate_core::WaveformEditRegion::new(start, end);
            if let Some((baseline, settings)) = edit_resize_effects
                && let Some(next_region) = next
            {
                browser.waveform_edit_settings = settings.for_resized_region(baseline, next_region);
            }
            browser.waveform_edit_region = next;
        }
    }
}

fn schedule_intent(browser: &mut Browser, intent: ZeroCrossingIntent, cx: &mut Context<Browser>) {
    if !browser.zero_crossing.enabled() {
        return;
    }
    if browser.zero_crossing.active_matches(&intent) && browser.zero_crossing_queued.is_none() {
        return;
    }
    if browser.zero_crossing_queued.as_ref() == Some(&intent) {
        return;
    }
    if browser.zero_crossing_task.is_some() {
        browser.zero_crossing.cancel_active();
        browser.zero_crossing_queued = Some(intent);
        return;
    }
    launch(browser, intent, cx);
}

fn launch(browser: &mut Browser, intent: ZeroCrossingIntent, cx: &mut Context<Browser>) {
    let Some(job) = browser.zero_crossing.begin(intent) else {
        return;
    };
    browser.zero_crossing_task = Some(cx.spawn(async move |this, cx| {
        let (job, result) = cx
            .background_executor()
            .spawn(async move {
                let result = snap_wav_range_to_zero_crossings_with_cancel(
                    &job.intent.request,
                    Some(job.cancellation.as_ref()),
                );
                (job, result)
            })
            .await;
        let _ = this.update(cx, |browser, cx| {
            browser.zero_crossing_task = None;
            finish(browser, job, result, cx);
        });
    }));
}

fn finish(
    browser: &mut Browser,
    job: ZeroCrossingJob,
    result: Result<wavecrate_media::WavZeroCrossingSnapRange, String>,
    cx: &mut Context<Browser>,
) {
    let intent = job.intent.clone();
    match browser.zero_crossing.finish(job, result) {
        Some(Ok(snapped)) => {
            if browser.zero_crossing.pending_commit_matches(&intent) {
                apply_bounds(
                    browser,
                    snapped.kind,
                    snapped.start,
                    snapped.end,
                    intent.edit_resize_effects,
                );
                browser.zero_crossing.take_pending_commit(&intent);
            } else if browser
                .waveform_region_drag
                .and_then(|drag| drag_intent(browser, drag))
                .is_some_and(|current| current == intent)
            {
                browser.zero_crossing.set_ready(intent, snapped);
            }
        }
        Some(Err(_)) => {
            browser.zero_crossing.take_pending_commit(&intent);
        }
        None => {}
    }
    if let Some(next) = browser.zero_crossing_queued.take()
        && browser.zero_crossing.enabled()
    {
        launch(browser, next, cx);
    }
    cx.notify();
}

/// Toggle zero-crossing snapping from the compact waveform toolbar.
pub(super) fn toggle_from_toolbar(
    browser: &mut Browser,
    _: &ClickEvent,
    window: &mut Window,
    cx: &mut Context<Browser>,
) {
    if !browser.preferences_editable() {
        browser.mark_preferences_edit_blocked(cx);
        return;
    }
    window.focus(&browser.waveform_focus_handle);
    let enabled = !browser.zero_crossing.enabled();
    browser.zero_crossing.set_enabled(enabled);
    browser.zero_crossing_queued = None;
    if !enabled {
        browser.zero_crossing.cancel_for_interruption();
    } else {
        schedule_for_drag(browser, cx);
    }
    cx.notify();
}

/// Cancel zero-crossing work when the browser is dropped or its source is replaced.
pub(super) fn cancel_for_shutdown(browser: &mut Browser) {
    browser.zero_crossing_queued = None;
    browser.zero_crossing.cancel_for_interruption();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wavecrate_core::WaveformRegion;
    use wavecrate_media::{
        WavZeroCrossingSnapRange, WaveformFileRevision, WaveformPreview, load_waveform_preview,
    };

    fn context() -> WaveformDetailContext {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("sample.wav");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&40_u32.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&44_100_u32.to_le_bytes());
        bytes.extend_from_slice(&88_200_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&4_u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        fs::write(&path, bytes).expect("write temporary WAV");
        let mut context = load_waveform_preview(&path)
            .expect("temporary WAV preview")
            .detail_context
            .expect("temporary WAV context");
        context.path = std::path::Path::new("sample.wav").to_path_buf();
        context.file_revision = WaveformFileRevision {
            filesystem_identity: String::from("id"),
            change_marker: String::from("marker"),
            byte_len: 1,
            modified_time: None,
        };
        context.total_frames = 100;
        context
    }

    fn intent(start: f64, end: f64) -> ZeroCrossingIntent {
        ZeroCrossingIntent::new(7, context(), WaveformSelectionKind::Play, start, end)
            .expect("valid intent")
    }

    #[test]
    fn ready_preview_requires_exact_generation_context_and_raw_bounds() {
        let mut state = ZeroCrossingState {
            enabled: true,
            ..ZeroCrossingState::default()
        };
        let exact = intent(0.2, 0.6);
        let snapped =
            ZeroCrossingBounds::new(WaveformSelectionKind::Play, 0.21, 0.59).expect("valid snap");
        state.set_ready(exact.clone(), snapped);
        assert_eq!(
            state.ready_for(7, &context(), WaveformSelectionKind::Play, 0.2, 0.6),
            Some(snapped)
        );
        assert_eq!(
            state.ready_for(8, &context(), WaveformSelectionKind::Play, 0.2, 0.6),
            None
        );
        assert_eq!(
            state.ready_for(7, &context(), WaveformSelectionKind::Play, 0.2, 0.7),
            None
        );
        let mut changed = context();
        changed.file_revision.change_marker = String::from("new");
        assert_eq!(
            state.ready_for(7, &changed, WaveformSelectionKind::Play, 0.2, 0.6),
            None
        );
    }

    #[test]
    fn ready_edit_snap_requires_the_original_resize_effects() {
        let context = context();
        let baseline = wavecrate_core::WaveformEditRegion::new(0.1, 0.7).unwrap();
        let settings = wavecrate_core::WaveformEditSettings::new(
            0.5,
            Some(wavecrate_core::WaveformEditFade::new(0.25, 0.5, 0.1, 0.8).unwrap()),
            None,
        )
        .unwrap();
        let metadata = Some((baseline, settings));
        let exact = ZeroCrossingIntent::new_with_edit_resize_effects(
            7,
            context.clone(),
            WaveformSelectionKind::Edit,
            0.2,
            0.6,
            metadata,
        )
        .unwrap();
        let snapped = ZeroCrossingBounds::new(WaveformSelectionKind::Edit, 0.21, 0.59).unwrap();
        let mut state = ZeroCrossingState::default();
        state.set_ready(exact.clone(), snapped);
        assert_eq!(
            state.ready_for_with_edit_resize_effects(
                7,
                &context,
                WaveformSelectionKind::Edit,
                0.2,
                0.6,
                metadata
            ),
            Some(snapped)
        );
        let changed = Some((
            baseline,
            wavecrate_core::WaveformEditSettings::new(0.75, settings.fade_in(), None).unwrap(),
        ));
        assert_eq!(
            state.ready_for_with_edit_resize_effects(
                7,
                &context,
                WaveformSelectionKind::Edit,
                0.2,
                0.6,
                changed
            ),
            None
        );
        assert_eq!(
            state.ready_for_with_edit_resize_effects(
                7,
                &context,
                WaveformSelectionKind::Edit,
                0.2,
                0.6,
                None
            ),
            None
        );
        let mut different = exact.clone();
        different.edit_resize_effects = changed;
        assert_eq!(state.take_ready_for(&different), None);
        assert_eq!(state.take_ready_for(&exact), Some(snapped));
    }

    #[test]
    fn canceled_active_result_is_ignored_until_a_new_request_can_start() {
        let mut state = ZeroCrossingState {
            enabled: true,
            ..ZeroCrossingState::default()
        };
        let first = intent(0.1, 0.4);
        let first_job = state.begin(first.clone()).expect("first job");
        assert!(state.cancel_for_interruption());
        assert!(
            state
                .finish(
                    first_job,
                    Ok(wavecrate_media::WavZeroCrossingSnapRange {
                        start_ratio: 0.2,
                        end_ratio: 0.4,
                    }),
                )
                .is_none()
        );
        let second = intent(0.2, 0.7);
        let second_job = state.begin(second.clone()).expect("second job");
        let result = state
            .finish(
                second_job,
                Ok(wavecrate_media::WavZeroCrossingSnapRange {
                    start_ratio: 0.21,
                    end_ratio: 0.69,
                }),
            )
            .expect("current result");
        assert_eq!(
            result,
            Ok(
                ZeroCrossingBounds::new(WaveformSelectionKind::Play, 0.21, 0.69)
                    .expect("valid snap")
            )
        );
    }

    #[test]
    fn disabling_clears_release_ownership_without_touching_the_raw_region() {
        let mut state = ZeroCrossingState {
            enabled: true,
            ..ZeroCrossingState::default()
        };
        let request = intent(0.25, 0.75);
        state.set_enabled(true);
        assert!(state.try_set_pending_commit(request.clone()));
        assert_eq!(state.status_label(), Some("Snapping zero crossings…"));
        state.set_enabled(false);
        assert!(!state.enabled());
        assert!(!state.pending_commit_matches(&request));
        assert_eq!(state.status_label(), None);
    }

    #[test]
    fn snap_completion_becomes_ready_for_the_same_live_intent_only() {
        let mut state = ZeroCrossingState {
            enabled: true,
            ..ZeroCrossingState::default()
        };
        let request = intent(0.3, 0.8);
        let job = state.begin(request.clone()).expect("job");
        let snapped = state
            .finish(
                job,
                Ok(wavecrate_media::WavZeroCrossingSnapRange {
                    start_ratio: 0.31,
                    end_ratio: 0.79,
                }),
            )
            .expect("completion")
            .expect("valid completion");
        state.set_ready(request.clone(), snapped);
        assert_eq!(state.take_ready_for(&request), Some(snapped));
        assert_eq!(state.take_ready_for(&request), None);
    }

    #[test]
    fn selection_kind_fences_identical_play_and_edit_bounds() {
        let mut state = ZeroCrossingState {
            enabled: true,
            ..ZeroCrossingState::default()
        };
        let edit = ZeroCrossingIntent::new(7, context(), WaveformSelectionKind::Edit, 0.2, 0.6)
            .expect("valid edit intent");
        let snapped = ZeroCrossingBounds::new(WaveformSelectionKind::Edit, 0.21, 0.59)
            .expect("valid edit snap");
        state.set_ready(edit, snapped);

        assert_eq!(
            state.ready_for(7, &context(), WaveformSelectionKind::Play, 0.2, 0.6),
            None
        );
        assert_eq!(
            state.ready_for(7, &context(), WaveformSelectionKind::Edit, 0.2, 0.6),
            Some(snapped)
        );
    }

    #[test]
    fn disabled_committed_nudge_cannot_create_snap_ownership() {
        let raw = WaveformPlayRegion::new(0.25, 0.75).expect("raw region");
        let moved_core = WaveformRegion::new(raw.start, raw.end)
            .expect("raw core region")
            .slide_left();
        let moved = WaveformPlayRegion::new(moved_core.start(), moved_core.end())
            .expect("left keyboard nudge");
        assert!(moved.start < raw.start);

        let mut state = ZeroCrossingState::default();
        let request = intent(moved.start, moved.end);
        assert!(!state.try_set_pending_commit(request.clone()));
        assert!(state.pending_commit.is_none());
        assert!(state.active.is_none());
        assert_eq!(state.status_label(), None);
        assert!(
            state
                .ready_for(
                    7,
                    &context(),
                    WaveformSelectionKind::Play,
                    moved.start,
                    moved.end
                )
                .is_none()
        );

        state.set_enabled(true);
        assert!(state.try_set_pending_commit(request));
        assert_eq!(state.status_label(), Some("Snapping zero crossings…"));
    }

    #[gpui::test]
    fn root_edit_pointer_loss_cancels_snap_and_ignores_late_completion(
        cx: &mut gpui::TestAppContext,
    ) {
        use std::sync::atomic::Ordering;

        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let play_region = WaveformPlayRegion::new(0.2, 0.8).expect("play region");
        let edit_region = wavecrate_core::WaveformEditRegion::new(0.15, 0.85).expect("edit region");
        let canceled_job = browser.update(cx, |view, _| {
            view.waveform = Some(WaveformPreview {
                peaks: vec![0.5; 8],
                bands: Vec::new(),
                duration_seconds: 1.0,
                sample_rate: 48_000,
                total_frames: 48_000,
                detail_context: None,
            });
            view.waveform_play_region = Some(play_region);
            view.waveform_edit_region = Some(edit_region);
            view.zero_crossing.enabled = true;
            view.zero_crossing
                .begin(
                    ZeroCrossingIntent::new(0, context(), WaveformSelectionKind::Edit, 0.25, 0.55)
                        .expect("edit snap intent"),
                )
                .expect("active edit snap")
        });
        cx.refresh().expect("refresh inert browser");
        cx.run_until_parked();
        let bounds = cx.debug_bounds("waveform").expect("waveform bounds");
        let start = gpui::point(
            bounds.origin.x + bounds.size.width * 0.25,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let end = gpui::point(
            bounds.origin.x + bounds.size.width * 0.55,
            bounds.origin.y + bounds.size.height / 2.0,
        );
        let modifiers = gpui::Modifiers::none();

        cx.simulate_mouse_down(start, gpui::MouseButton::Right, modifiers);
        assert!(canceled_job.cancellation.load(Ordering::Acquire));
        cx.simulate_mouse_move(end, gpui::MouseButton::Right, modifiers);
        cx.simulate_mouse_move(end, None, modifiers);

        browser.update(cx, |view, _| {
            assert!(view.waveform_region_drag.is_none());
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
            assert!(
                view.zero_crossing
                    .finish(
                        canceled_job,
                        Ok(WavZeroCrossingSnapRange {
                            start_ratio: 0.3,
                            end_ratio: 0.5,
                        }),
                    )
                    .is_none()
            );
            assert!(view.zero_crossing.active.is_none());
            assert_eq!(view.waveform_edit_region, Some(edit_region));
            assert_eq!(view.waveform_play_region, Some(play_region));
        });

        cx.simulate_mouse_down(end, gpui::MouseButton::Right, modifiers);
        cx.simulate_mouse_up(end, gpui::MouseButton::Right, modifiers);
        browser.update(cx, |view, _| {
            assert!(view.waveform_region_drag.is_none());
            assert!(view.waveform_edit_region.is_none());
            assert_eq!(view.waveform_play_region, Some(play_region));
        });
    }
}
