#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded, file-fenced preparation for normalized audition playback.
//!
//! This module deliberately keeps file access on the background worker. The browser owns the
//! [`NormalizedAuditionState`], while a [`NormalizedAuditionJob`] is moved into one bounded
//! background task and returned to the browser when it completes.

use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    AnyView, App, Bounds, ClickEvent, Context, Hsla, PathBuilder, Pixels, Render, Window, canvas,
    div, point, prelude::*, px,
};
use reson::{PlaybackRequestId, PlaybackRuntimeSubmitError};
use wavecrate_media::{
    WavSpanPeakRequest, WaveformDetailRange, WaveformFileRevision, load_wav_span_peak_with_cancel,
};

use super::waveform::WaveformPlayRegion;
use super::{Browser, transport::PlaybackIntent};

const MAX_NORMALIZED_GAIN_RETRIES: u8 = 3;
const NORMALIZED_AUDITION_ICON_SIZE: f32 = 16.0;
const NORMALIZED_AUDITION_ICON_STROKE_WIDTH: f32 = 1.35;

/// Convert one raw span peak into the playback gain used by normalized audition.
///
/// Silence, non-finite peaks, and values at or below [`f32::EPSILON`] retain unity gain. The
/// caller applies this gain separately from the user's manual output volume.
pub(super) fn normalized_gain_from_peak(peak: f32) -> f32 {
    if peak.is_finite() && peak > f32::EPSILON {
        1.0 / peak
    } else {
        1.0
    }
}

/// Identity fence for one normalized audition preparation or scoped gain update.
///
/// The browser must advance the relevant generation and cancel the pending job before changing
/// any value represented here. The optional playback ID is absent while preparing a play request
/// and present when preparing a gain update for already accepted playback.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct NormalizedAuditionFence {
    pub(super) playback_epoch: u64,
    pub(super) waveform_generation: u64,
    pub(super) output_open_generation: u64,
    pub(super) playback_id: Option<PlaybackRequestId>,
    pub(super) path: PathBuf,
    pub(super) span: WaveformPlayRegion,
}

impl NormalizedAuditionFence {
    /// Construct a fence for one resolved playback intent or accepted play request.
    pub(super) fn new(
        playback_epoch: u64,
        waveform_generation: u64,
        output_open_generation: u64,
        playback_id: Option<PlaybackRequestId>,
        path: PathBuf,
        span: WaveformPlayRegion,
    ) -> Self {
        Self {
            playback_epoch,
            waveform_generation,
            output_open_generation,
            playback_id,
            path,
            span,
        }
    }
}

/// A successful, revision-fenced normalized gain for one exact frame range.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct NormalizedAuditionPreparation {
    /// Path whose header and samples produced the gain.
    pub(super) path: PathBuf,
    /// Strong file revision captured before and after the sample read.
    pub(super) file_revision: WaveformFileRevision,
    /// Exact half-open source frame range used for the peak.
    pub(super) range: WaveformDetailRange,
    /// Precomputed normalized playback gain, independent of manual volume.
    pub(super) gain: f32,
}

/// The browser action that consumes one successful normalized gain.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum NormalizedAuditionAction {
    /// Queue a new playback request only after preparation succeeds.
    Play(PlaybackIntent),
    /// Apply the gain to the currently accepted playback request.
    ApplyGain,
    /// Retarget the current playback loop mode with the prepared gain.
    RetargetLoop { desired_looped: bool },
}

/// One bounded normalized audition request retained while an older job is winding down.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingNormalizedAudition {
    pub(super) fence: NormalizedAuditionFence,
    pub(super) action: NormalizedAuditionAction,
}

/// One bounded retry for a scoped runtime gain command that met a full queue.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct PendingNormalizedGain {
    pub(super) playback_id: PlaybackRequestId,
    pub(super) gain: f32,
    pub(super) playback_epoch: u64,
    pub(super) waveform_generation: u64,
    pub(super) output_open_generation: u64,
    pub(super) path: PathBuf,
    pub(super) span: WaveformPlayRegion,
    pub(super) attempts: u8,
}

impl NormalizedAuditionPreparation {
    fn matches_request(&self, request: &WavSpanPeakRequest) -> bool {
        self.path == request.context.path
            && self.file_revision == request.context.file_revision
            && self.range == request.range
    }
}

#[derive(Clone)]
struct PendingPreparation {
    generation: u64,
    fence: NormalizedAuditionFence,
    cancellation: Arc<AtomicBool>,
}

/// A single background normalized audition preparation request.
///
/// The job contains only bounded metadata and a cancellation token. It is intended to be moved
/// to a background executor and passed to [`prepare_normalized_audition`].
pub(super) struct NormalizedAuditionJob {
    generation: u64,
    pub(super) fence: NormalizedAuditionFence,
    pub(super) cancellation: Arc<AtomicBool>,
    cached: Option<NormalizedAuditionPreparation>,
}

impl NormalizedAuditionJob {
    /// Return a bounded copy of the previous successful preparation for revision validation.
    #[cfg(test)]
    pub(super) fn cached_preparation(&self) -> Option<NormalizedAuditionPreparation> {
        self.cached.clone()
    }
}

/// Result returned when a current preparation finishes on the browser executor.
pub(super) type NormalizedAuditionCompletion = Result<NormalizedAuditionPreparation, String>;

fn deferred_play_origin(
    action: &NormalizedAuditionAction,
) -> Option<super::transport::PlaybackIntentOrigin> {
    match action {
        NormalizedAuditionAction::Play(intent) => Some(intent.origin),
        _ => None,
    }
}

fn pending_loop_retarget_for_disable(
    active_fence: Option<NormalizedAuditionFence>,
    queued_request: Option<&PendingNormalizedAudition>,
    desired_looped: Option<bool>,
) -> Option<PendingNormalizedAudition> {
    if let Some(request) = queued_request.filter(|request| {
        matches!(
            &request.action,
            NormalizedAuditionAction::RetargetLoop { .. }
        )
    }) {
        return Some(request.clone());
    }
    let (Some(fence), Some(desired_looped)) = (active_fence, desired_looped) else {
        return None;
    };
    Some(PendingNormalizedAudition {
        fence,
        action: NormalizedAuditionAction::RetargetLoop { desired_looped },
    })
}

fn delayed_play_intent_with_volume(mut intent: PlaybackIntent, volume: f32) -> PlaybackIntent {
    intent.set_volume(volume);
    intent
}

fn normalized_gain_fence_matches(
    expected_playback_id: u64,
    latest_playback_id: Option<u64>,
    playback_epoch: u64,
    expected_playback_epoch: u64,
    waveform_generation: u64,
    expected_waveform_generation: u64,
    output_open_generation: u64,
    expected_output_open_generation: u64,
    playing_path: Option<&std::path::Path>,
    expected_path: &std::path::Path,
    accepted_path: Option<&std::path::Path>,
    accepted_span: Option<WaveformPlayRegion>,
    expected_span: WaveformPlayRegion,
) -> bool {
    latest_playback_id == Some(expected_playback_id)
        && playback_epoch == expected_playback_epoch
        && waveform_generation == expected_waveform_generation
        && output_open_generation == expected_output_open_generation
        && playing_path == Some(expected_path)
        && accepted_path == Some(expected_path)
        && accepted_span == Some(expected_span)
}

fn normalized_gain_retry_is_exhausted(attempts: u8) -> bool {
    attempts >= MAX_NORMALIZED_GAIN_RETRIES
}

fn normalized_audition_icon_point(bounds: Bounds<Pixels>, x: f32, y: f32) -> gpui::Point<Pixels> {
    let width = f32::from(bounds.size.width);
    let height = f32::from(bounds.size.height);
    let scale = (width / NORMALIZED_AUDITION_ICON_SIZE)
        .min(height / NORMALIZED_AUDITION_ICON_SIZE)
        .max(0.0);
    let left = f32::from(bounds.origin.x) + (width - NORMALIZED_AUDITION_ICON_SIZE * scale) / 2.0;
    let top = f32::from(bounds.origin.y) + (height - NORMALIZED_AUDITION_ICON_SIZE * scale) / 2.0;
    point(px(left + x * scale), px(top + y * scale))
}

fn normalized_audition_cap_path(
    bounds: Bounds<Pixels>,
    x: f32,
    y: f32,
    radius: f32,
) -> PathBuilder {
    const KAPPA: f32 = 0.552_284_8;
    let mut path = PathBuilder::fill();
    path.move_to(normalized_audition_icon_point(bounds, x + radius, y));
    path.cubic_bezier_to(
        normalized_audition_icon_point(bounds, x, y + radius),
        normalized_audition_icon_point(bounds, x + radius, y + KAPPA * radius),
        normalized_audition_icon_point(bounds, x + KAPPA * radius, y + radius),
    );
    path.cubic_bezier_to(
        normalized_audition_icon_point(bounds, x - radius, y),
        normalized_audition_icon_point(bounds, x - KAPPA * radius, y + radius),
        normalized_audition_icon_point(bounds, x - radius, y + KAPPA * radius),
    );
    path.cubic_bezier_to(
        normalized_audition_icon_point(bounds, x, y - radius),
        normalized_audition_icon_point(bounds, x - radius, y - KAPPA * radius),
        normalized_audition_icon_point(bounds, x - KAPPA * radius, y - radius),
    );
    path.cubic_bezier_to(
        normalized_audition_icon_point(bounds, x + radius, y),
        normalized_audition_icon_point(bounds, x + KAPPA * radius, y - radius),
        normalized_audition_icon_point(bounds, x + radius, y - KAPPA * radius),
    );
    path.close();
    path
}

/// Render the fixed 16×16 normalized-audition speaker glyph.
pub(super) fn render_normalized_audition_icon(icon_color: Hsla) -> impl IntoElement {
    canvas(
        move |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }

            let speaker = {
                let mut path = PathBuilder::fill();
                path.move_to(normalized_audition_icon_point(bounds, 2.0, 6.0));
                path.line_to(normalized_audition_icon_point(bounds, 4.5, 6.0));
                path.line_to(normalized_audition_icon_point(bounds, 8.0, 3.2));
                path.line_to(normalized_audition_icon_point(bounds, 8.0, 12.8));
                path.line_to(normalized_audition_icon_point(bounds, 4.5, 10.0));
                path.line_to(normalized_audition_icon_point(bounds, 2.0, 10.0));
                path.close();
                path
            };
            if let Ok(path) = speaker.build() {
                window.paint_path(path, icon_color);
            }

            let scale = (width / NORMALIZED_AUDITION_ICON_SIZE)
                .min(height / NORMALIZED_AUDITION_ICON_SIZE)
                .max(0.0);
            let mut waves = PathBuilder::stroke(px(NORMALIZED_AUDITION_ICON_STROKE_WIDTH * scale));
            waves.move_to(normalized_audition_icon_point(bounds, 10.0, 5.3));
            waves.cubic_bezier_to(
                normalized_audition_icon_point(bounds, 11.4, 8.0),
                normalized_audition_icon_point(bounds, 10.9, 6.0),
                normalized_audition_icon_point(bounds, 11.4, 6.9),
            );
            waves.cubic_bezier_to(
                normalized_audition_icon_point(bounds, 10.0, 10.7),
                normalized_audition_icon_point(bounds, 11.4, 9.1),
                normalized_audition_icon_point(bounds, 10.9, 10.7),
            );
            waves.move_to(normalized_audition_icon_point(bounds, 11.8, 3.6));
            waves.cubic_bezier_to(
                normalized_audition_icon_point(bounds, 14.0, 8.0),
                normalized_audition_icon_point(bounds, 13.2, 4.7),
                normalized_audition_icon_point(bounds, 14.0, 6.2),
            );
            waves.cubic_bezier_to(
                normalized_audition_icon_point(bounds, 11.8, 12.4),
                normalized_audition_icon_point(bounds, 14.0, 9.8),
                normalized_audition_icon_point(bounds, 13.2, 11.3),
            );
            if let Ok(path) = waves.build() {
                window.paint_path(path, icon_color);
            }

            let radius = NORMALIZED_AUDITION_ICON_STROKE_WIDTH / 2.0;
            for (x, y) in [(10.0, 5.3), (10.0, 10.7), (11.8, 3.6), (11.8, 12.4)] {
                if let Ok(path) = normalized_audition_cap_path(bounds, x, y, radius).build() {
                    window.paint_path(path, icon_color);
                }
            }
        },
    )
    .w(px(NORMALIZED_AUDITION_ICON_SIZE))
    .h(px(NORMALIZED_AUDITION_ICON_SIZE))
}

struct NormalizedAuditionTooltip;

impl Render for NormalizedAuditionTooltip {
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
            .child("Toggle normalized audition")
    }
}

pub(super) fn normalized_audition_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| NormalizedAuditionTooltip).into()
}

/// Browser-owned state for one normalized audition task and one cached result.
///
/// The cache never grows beyond one entry. Every preparation still captures a fresh header
/// context before consulting the cache, so a replacement or modification cannot reuse a stale
/// gain merely because its path and span are unchanged.
#[derive(Default)]
pub(super) struct NormalizedAuditionState {
    enabled: bool,
    next_generation: u64,
    pending: Option<PendingPreparation>,
    cache: Option<NormalizedAuditionPreparation>,
}

impl NormalizedAuditionState {
    /// Return whether normalized audition is currently enabled in the effective preference view.
    pub(super) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Set the effective normalized audition preference and cancel work when disabling it.
    pub(super) fn set_enabled(&mut self, enabled: bool) -> bool {
        let changed = self.enabled != enabled;
        self.enabled = enabled;
        if !enabled {
            self.cancel();
        }
        changed
    }

    /// Start one cancellable preparation for `fence`, replacing any older preparation.
    pub(super) fn begin(
        &mut self,
        fence: NormalizedAuditionFence,
    ) -> Option<NormalizedAuditionJob> {
        if !self.enabled {
            return None;
        }
        self.cancel();
        self.next_generation = self.next_generation.wrapping_add(1);
        let cancellation = Arc::new(AtomicBool::new(false));
        self.pending = Some(PendingPreparation {
            generation: self.next_generation,
            fence: fence.clone(),
            cancellation: Arc::clone(&cancellation),
        });
        Some(NormalizedAuditionJob {
            generation: self.next_generation,
            fence,
            cancellation,
            cached: self.cache.clone(),
        })
    }

    /// Cancel and forget the current preparation, if any.
    pub(super) fn cancel(&mut self) {
        if let Some(pending) = self.pending.take() {
            pending.cancellation.store(true, Ordering::Release);
        }
    }

    /// Return whether a preparation is currently in flight.
    pub(super) fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    fn pending_fence(&self) -> Option<NormalizedAuditionFence> {
        self.pending.as_ref().map(|pending| pending.fence.clone())
    }

    /// Accept a background result only if its job is still the current request.
    ///
    /// A stale or cancelled completion returns `None`. Current failures return `Some(Err(...))`
    /// so the browser can show a bounded status and decide whether a later explicit request may
    /// retry. Successful results replace the one-entry cache.
    pub(super) fn finish(
        &mut self,
        job: &NormalizedAuditionJob,
        result: NormalizedAuditionCompletion,
    ) -> Option<NormalizedAuditionCompletion> {
        let current = self.pending.as_ref().filter(|pending| {
            pending.generation == job.generation
                && pending.fence == job.fence
                && Arc::ptr_eq(&pending.cancellation, &job.cancellation)
        })?;
        if current.cancellation.load(Ordering::Acquire) {
            self.pending = None;
            return None;
        }
        self.pending = None;
        match result {
            Ok(preparation) => {
                if preparation.path != job.fence.path {
                    return Some(Err(String::from(
                        "normalized audition result path no longer matches request",
                    )));
                }
                self.cache = Some(preparation.clone());
                Some(Ok(preparation))
            }
            Err(error) if is_cancellation_error(&error) => None,
            Err(error) => Some(Err(error)),
        }
    }
}

/// Handle one background normalized audition request and apply its result to the current browser
/// transport.
impl Browser {
    /// Return whether normalized audition currently has a queued or running preparation.
    pub(super) fn normalized_audition_is_preparing(&self) -> bool {
        self.normalized_audition_task.is_some()
            || self.normalized_audition_queued.is_some()
            || self.normalized_audition.is_pending()
    }

    /// Toggle normalized audition from the compact browser toolbar.
    pub(super) fn toggle_normalized_audition_from_toolbar(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let enabled = !self.normalized_audition.enabled();
        if self.persist_normalized_audition_preference(enabled, cx) {
            self.reconcile_normalized_audition_preference(enabled, cx);
            cx.notify();
        }
    }

    /// Reconcile the effective preference and keep live playback on the selected gain.
    pub(super) fn reconcile_normalized_audition_preference(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) {
        let pending_loop_retarget = (!enabled).then(|| {
            pending_loop_retarget_for_disable(
                self.normalized_audition.pending_fence(),
                self.normalized_audition_queued.as_ref(),
                self.normalized_audition_pending_loop_target,
            )
        });
        let pending_loop_retarget = pending_loop_retarget
            .flatten()
            .filter(|request| self.normalized_live_request_is_current(request));
        if !self.normalized_audition.set_enabled(enabled) {
            return;
        }
        if !enabled {
            self.normalized_audition_queued = None;
            self.normalized_audition_pending_loop_target = None;
            if self.playing_path.is_some()
                && let Some(playback_id) = self.latest_playback_request_id
            {
                if let Err(error) = self.apply_normalized_gain(playback_id, 1.0, cx) {
                    self.playback_status =
                        format!("Could not disable normalized audition: {error}");
                }
            }
            if let Some(request) = pending_loop_retarget {
                let NormalizedAuditionAction::RetargetLoop { desired_looped } = request.action
                else {
                    unreachable!("disable retarget helper returned another action");
                };
                if let Some(Err(error)) =
                    self.submit_live_loop_retarget_with_gain(desired_looped, 1.0, cx)
                {
                    self.playback_status = format!("Could not apply loop change: {error}");
                }
            }
            self.resume_deferred_play_after_preparation(cx);
        } else if self.playing_path.is_some() {
            self.schedule_normalized_live_gain(cx);
        }
    }

    /// Cancel preparation and any newer request retained while its worker winds down.
    pub(super) fn cancel_normalized_audition(&mut self) {
        self.normalized_audition.cancel();
        self.normalized_audition_queued = None;
        self.normalized_audition_deferred_play = None;
        self.normalized_audition_pending_loop_target = None;
        self.normalized_gain_runtime_pending = None;
    }

    /// Schedule a normalized gain before submitting a new playback request.
    pub(super) fn schedule_normalized_playback(
        &mut self,
        intent: PlaybackIntent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(span) = span_for_playback_intent(&intent) else {
            return false;
        };
        if !self.normalized_audition.enabled() {
            return false;
        }
        let fence = self.normalized_fence(&intent.path, span, None);
        let request = PendingNormalizedAudition {
            fence,
            action: NormalizedAuditionAction::Play(intent),
        };
        self.normalized_audition_deferred_play = Some(request.clone());
        self.schedule_normalized_request(request, cx);
        true
    }

    /// Schedule a normalized gain update for the currently accepted playback request.
    pub(super) fn schedule_normalized_live_gain(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(playback_id) = self.latest_playback_request_id else {
            return false;
        };
        let Some(path) = self.playing_path.clone() else {
            return false;
        };
        let Some(accepted) = self.accepted_playback_intent.as_ref() else {
            return false;
        };
        let Some(span) = span_for_playback_intent(accepted) else {
            return false;
        };
        if accepted.path != path || !self.normalized_audition.enabled() {
            return false;
        }
        let fence = self.normalized_fence(&path, span, Some(playback_id));
        self.schedule_normalized_request(
            PendingNormalizedAudition {
                fence,
                action: NormalizedAuditionAction::ApplyGain,
            },
            cx,
        );
        true
    }

    /// Schedule a normalized gain update for a live loop-mode retarget.
    pub(super) fn schedule_normalized_loop_retarget(
        &mut self,
        desired_looped: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(playback_id) = self.latest_playback_request_id else {
            return false;
        };
        let Some(path) = self.playing_path.clone() else {
            return false;
        };
        let Some(accepted) = self.accepted_playback_intent.as_ref() else {
            return false;
        };
        let Some(span) = span_for_playback_intent(accepted) else {
            return false;
        };
        if accepted.path != path || !self.normalized_audition.enabled() {
            return false;
        }
        self.normalized_audition_pending_loop_target = Some(desired_looped);
        let fence = self.normalized_fence(&path, span, Some(playback_id));
        self.schedule_normalized_request(
            PendingNormalizedAudition {
                fence,
                action: NormalizedAuditionAction::RetargetLoop { desired_looped },
            },
            cx,
        );
        true
    }

    fn normalized_fence(
        &self,
        path: &std::path::Path,
        span: WaveformPlayRegion,
        playback_id: Option<PlaybackRequestId>,
    ) -> NormalizedAuditionFence {
        NormalizedAuditionFence::new(
            self.playback_epoch,
            self.waveform_generation,
            self.audio_output_open_generation,
            playback_id,
            path.to_path_buf(),
            span,
        )
    }

    fn schedule_normalized_request(
        &mut self,
        request: PendingNormalizedAudition,
        cx: &mut Context<Self>,
    ) {
        self.normalized_gain_runtime_pending = None;
        if !matches!(
            &request.action,
            NormalizedAuditionAction::RetargetLoop { .. }
        ) {
            self.normalized_audition_pending_loop_target = None;
        }
        if self.normalized_audition_task.is_some() {
            self.normalized_audition.cancel();
            self.normalized_audition_queued = Some(request);
        } else {
            self.launch_normalized_request(request, cx);
        }
    }

    fn launch_normalized_request(
        &mut self,
        request: PendingNormalizedAudition,
        cx: &mut Context<Self>,
    ) {
        let Some(job) = self.normalized_audition.begin(request.fence.clone()) else {
            return;
        };
        if matches!(&request.action, NormalizedAuditionAction::Play(_)) {
            self.normalized_audition_deferred_play = Some(request.clone());
        }
        self.playback_status = "Preparing normalized audition".to_string();
        self.normalized_audition_task = Some(cx.spawn(async move |this, cx| {
            let (job, result) = cx
                .background_executor()
                .spawn(async move {
                    let result = prepare_normalized_audition(&job);
                    (job, result)
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.normalized_audition_task = None;
                view.finish_normalized_request(request, job, result, cx);
            });
        }));
    }

    fn finish_normalized_request(
        &mut self,
        request: PendingNormalizedAudition,
        job: NormalizedAuditionJob,
        result: NormalizedAuditionCompletion,
        cx: &mut Context<Self>,
    ) {
        let completion = self.normalized_audition.finish(&job, result);
        let request_is_current = self.normalized_request_is_current(&request);
        if matches!(
            &request.action,
            NormalizedAuditionAction::RetargetLoop { .. }
        ) && self.normalized_audition_queued.is_none()
        {
            self.normalized_audition_pending_loop_target = None;
        }
        match completion {
            Some(Ok(preparation)) if request_is_current => match request.action {
                NormalizedAuditionAction::Play(intent) => {
                    self.normalized_audition_deferred_play = None;
                    let intent = delayed_play_intent_with_volume(intent, self.volume);
                    self.queue_playback_intent_with_gain_and_span(
                        intent,
                        Some(preparation.gain),
                        Some(request.fence.span),
                        cx,
                    );
                }
                NormalizedAuditionAction::ApplyGain => {
                    if let Some(playback_id) = request.fence.playback_id {
                        if let Err(error) =
                            self.apply_normalized_gain(playback_id, preparation.gain, cx)
                        {
                            self.playback_status =
                                format!("Could not apply normalized audition gain: {error}");
                        }
                    }
                }
                NormalizedAuditionAction::RetargetLoop { desired_looped } => {
                    match self.submit_live_loop_retarget_with_gain(
                        desired_looped,
                        preparation.gain,
                        cx,
                    ) {
                        Some(Err(error)) => {
                            self.playback_status = format!("Loop live update unavailable: {error}");
                        }
                        Some(Ok(())) | None => {}
                    }
                }
            },
            Some(Err(error)) if request_is_current => {
                if matches!(request.action, NormalizedAuditionAction::Play(_)) {
                    self.normalized_audition_deferred_play = None;
                }
                self.playback_status = match request.action {
                    NormalizedAuditionAction::RetargetLoop { .. } => {
                        format!("Loop live update normalization failed: {error}")
                    }
                    _ => format!("Normalized audition unavailable: {error}"),
                };
            }
            _ => {}
        }

        if let Some(next) = self.normalized_audition_queued.take()
            && self.normalized_audition.enabled()
        {
            self.launch_normalized_request(next, cx);
        }
        self.resume_deferred_play_after_preparation(cx);
        cx.notify();
    }

    fn normalized_request_is_current(&self, request: &PendingNormalizedAudition) -> bool {
        if !self.normalized_audition.enabled() || !self.normalized_fence_is_current(request) {
            return false;
        }
        match &request.action {
            NormalizedAuditionAction::Play(intent) => {
                request.fence.playback_id.is_none()
                    && intent.path == request.fence.path
                    && self.selected_path() == Some(request.fence.path.as_path())
            }
            NormalizedAuditionAction::ApplyGain | NormalizedAuditionAction::RetargetLoop { .. } => {
                self.normalized_live_request_is_current(request)
            }
        }
    }

    fn normalized_live_request_is_current(&self, request: &PendingNormalizedAudition) -> bool {
        let Some(playback_id) = request.fence.playback_id else {
            return false;
        };
        self.normalized_fence_is_current(request)
            && self.latest_playback_request_id == Some(playback_id)
            && self.playing_path.as_deref() == Some(request.fence.path.as_path())
            && self
                .accepted_playback_intent
                .as_ref()
                .is_some_and(|intent| {
                    intent.path == request.fence.path
                        && span_for_playback_intent(intent) == Some(request.fence.span)
                })
    }

    fn normalized_fence_is_current(&self, request: &PendingNormalizedAudition) -> bool {
        self.playback_epoch == request.fence.playback_epoch
            && self.waveform_generation == request.fence.waveform_generation
            && self.audio_output_open_generation == request.fence.output_open_generation
    }

    fn selected_path(&self) -> Option<&std::path::Path> {
        self.selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.as_path())
    }

    fn resume_deferred_play_after_preparation(&mut self, cx: &mut Context<Self>) {
        if self.normalized_audition_task.is_some() {
            return;
        }
        let Some(request) = self.normalized_audition_deferred_play.take() else {
            return;
        };
        let current = self.normalized_fence_is_current(&request)
            && matches!(
                &request.action,
                NormalizedAuditionAction::Play(intent)
                    if intent.path == request.fence.path && self.selected_path() == Some(request.fence.path.as_path())
            );
        if !current {
            return;
        }
        let NormalizedAuditionAction::Play(intent) = request.action else {
            return;
        };
        let mut intent = delayed_play_intent_with_volume(intent, self.volume);
        if self.normalized_audition.enabled() {
            if intent.origin != super::transport::PlaybackIntentOrigin::AudioOutputResume {
                intent.looped = self.playback_policy.enabled();
            }
            let span = span_for_playback_intent(&intent);
            let fence = span.map_or(request.fence.clone(), |span| {
                if span == request.fence.span {
                    request.fence.clone()
                } else {
                    self.normalized_fence(&request.fence.path, span, None)
                }
            });
            self.launch_normalized_request(
                PendingNormalizedAudition {
                    fence,
                    action: NormalizedAuditionAction::Play(intent),
                },
                cx,
            );
        } else {
            self.queue_playback_intent_with_gain(intent, Some(1.0), cx);
        }
    }

    /// Apply one precomputed gain to one still-current runtime playback ID.
    pub(super) fn apply_normalized_gain(
        &mut self,
        playback_id: PlaybackRequestId,
        gain: f32,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(handle) = self.playback.as_ref().map(|runtime| runtime.handle.clone()) else {
            return Err(String::from("audio runtime is unavailable"));
        };
        let gain = if gain.is_finite() && gain > 0.0 {
            gain
        } else {
            1.0
        };
        let Some(path) = self.playing_path.clone() else {
            return Err(String::from("playback is no longer active"));
        };
        let Some(accepted) = self.accepted_playback_intent.as_ref() else {
            return Err(String::from("playback intent is no longer active"));
        };
        let Some(span) = span_for_playback_intent(accepted) else {
            return Err(String::from("accepted playback span is unavailable"));
        };
        if self.latest_playback_request_id != Some(playback_id) || accepted.path != path {
            return Err(String::from("playback request is no longer current"));
        }
        let pending = PendingNormalizedGain {
            playback_id,
            gain,
            playback_epoch: self.playback_epoch,
            waveform_generation: self.waveform_generation,
            output_open_generation: self.audio_output_open_generation,
            path,
            span,
            attempts: 1,
        };
        match handle.try_set_playback_gain_for(playback_id, gain) {
            Ok(()) => {
                self.normalized_gain_runtime_pending = None;
                Ok(())
            }
            Err(PlaybackRuntimeSubmitError::QueueFull) => {
                self.playback_status = "Applying normalized audition…".to_string();
                self.normalized_gain_runtime_pending = Some(pending);
                self.ensure_playback_monitor(cx);
                Ok(())
            }
            Err(PlaybackRuntimeSubmitError::Closed) => {
                self.normalized_gain_runtime_pending = None;
                Err(String::from("audio runtime is closed"))
            }
        }
    }

    /// Retry one queued scoped gain command from the existing playback monitor.
    pub(super) fn retry_pending_normalized_gain(&mut self) {
        let Some(mut pending) = self.normalized_gain_runtime_pending.take() else {
            return;
        };
        if !self.normalized_gain_is_current(&pending) {
            return;
        }
        let Some(handle) = self.playback.as_ref().map(|runtime| runtime.handle.clone()) else {
            return;
        };
        if normalized_gain_retry_is_exhausted(pending.attempts) {
            self.playback_status = normalized_gain_retry_failure_status(pending.gain);
            return;
        }
        match handle.try_set_playback_gain_for(pending.playback_id, pending.gain) {
            Ok(()) => {
                if self.playback_status == "Applying normalized audition…" {
                    self.playback_status = "Playing".to_string();
                }
            }
            Err(PlaybackRuntimeSubmitError::QueueFull) => {
                pending.attempts = pending.attempts.saturating_add(1);
                if normalized_gain_retry_is_exhausted(pending.attempts) {
                    self.playback_status = normalized_gain_retry_failure_status(pending.gain);
                } else {
                    self.normalized_gain_runtime_pending = Some(pending);
                }
            }
            Err(PlaybackRuntimeSubmitError::Closed) => {
                self.playback_status = String::from("Audio runtime closed before gain update");
            }
        }
    }

    /// Return the newest loop target still waiting for normalized preparation.
    pub(super) fn normalized_audition_pending_loop_target(&self) -> Option<bool> {
        self.normalized_audition_pending_loop_target
    }

    /// Return the origin of a deferred play, if normalized preparation owns one.
    pub(super) fn normalized_audition_pending_play_origin(
        &self,
    ) -> Option<super::transport::PlaybackIntentOrigin> {
        self.normalized_audition_deferred_play
            .as_ref()
            .and_then(|request| deferred_play_origin(&request.action))
    }

    fn normalized_gain_is_current(&self, pending: &PendingNormalizedGain) -> bool {
        let accepted = self.accepted_playback_intent.as_ref();
        normalized_gain_fence_matches(
            pending.playback_id.get(),
            self.latest_playback_request_id.map(PlaybackRequestId::get),
            self.playback_epoch,
            pending.playback_epoch,
            self.waveform_generation,
            pending.waveform_generation,
            self.audio_output_open_generation,
            pending.output_open_generation,
            self.playing_path.as_deref(),
            pending.path.as_path(),
            accepted.map(|intent| intent.path.as_path()),
            accepted.and_then(span_for_playback_intent),
            pending.span,
        )
    }
}

fn normalized_gain_retry_failure_status(gain: f32) -> String {
    if gain.to_bits() == 1.0f32.to_bits() {
        String::from("Could not disable normalized audition")
    } else {
        String::from("Could not apply normalized audition gain")
    }
}

/// Capture a current header revision and read one exact span peak on a background worker.
///
/// The request constructor performs the header-only revision capture before the bounded sample
/// scan. A cached result is used only after that fresh context has the same strong revision and
/// exact frame range. No waveform overview, retained PCM buffer, or UI state is accessed here.
pub(super) fn prepare_normalized_audition(
    job: &NormalizedAuditionJob,
) -> NormalizedAuditionCompletion {
    let request = WavSpanPeakRequest::from_path_span_with_cancel(
        &job.fence.path,
        job.fence.span.start,
        job.fence.span.end,
        Some(job.cancellation.as_ref()),
    )?;
    if let Some(cached) = job.cached.as_ref()
        && cached.matches_request(&request)
    {
        return Ok(cached.clone());
    }
    let peak = load_wav_span_peak_with_cancel(&request, Some(job.cancellation.as_ref()))?;
    Ok(NormalizedAuditionPreparation {
        path: request.context.path,
        file_revision: request.context.file_revision,
        range: request.range,
        gain: normalized_gain_from_peak(peak),
    })
}

fn is_cancellation_error(error: &str) -> bool {
    error.contains("canceled") || error.contains("cancelled") || error.contains("cancellation")
}

/// Return the playback span that should be normalized for one resolved intent.
pub(super) fn span_for_playback_intent(
    intent: &super::transport::PlaybackIntent,
) -> Option<WaveformPlayRegion> {
    super::transport::playback_span_for_intent(intent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::SystemTime;

    fn make_fence(path: &str, start: f64, end: f64) -> NormalizedAuditionFence {
        NormalizedAuditionFence::new(
            1,
            2,
            3,
            None,
            PathBuf::from(path),
            WaveformPlayRegion::new(start, end).expect("valid span"),
        )
    }

    fn preparation(
        path: &str,
        range: WaveformDetailRange,
        gain: f32,
    ) -> NormalizedAuditionPreparation {
        NormalizedAuditionPreparation {
            path: PathBuf::from(path),
            file_revision: WaveformFileRevision {
                filesystem_identity: String::from("file-id"),
                change_marker: String::from("marker"),
                byte_len: 12,
                modified_time: Some(SystemTime::UNIX_EPOCH),
            },
            range,
            gain,
        }
    }

    #[test]
    fn normalized_gain_preserves_unity_for_silence_and_non_finite_peaks() {
        assert_eq!(normalized_gain_from_peak(0.0), 1.0);
        assert_eq!(normalized_gain_from_peak(f32::EPSILON), 1.0);
        assert_eq!(normalized_gain_from_peak(f32::NAN), 1.0);
        assert_eq!(normalized_gain_from_peak(f32::INFINITY), 1.0);
        assert_eq!(normalized_gain_from_peak(0.25), 4.0);
    }

    #[test]
    fn disabled_state_does_not_retain_or_start_preparation() {
        let mut state = NormalizedAuditionState::default();
        let first_fence = make_fence("one.wav", 0.0, 1.0);
        assert!(!state.enabled());
        assert!(state.begin(first_fence).is_none());
        assert!(!state.is_pending());
        assert!(state.set_enabled(true));
        assert!(state.begin(make_fence("one.wav", 0.0, 1.0)).is_some());
        assert!(state.is_pending());
        assert!(state.set_enabled(false));
        assert!(!state.is_pending());
    }

    #[test]
    fn stale_completion_after_newer_fence_is_discarded() {
        let mut state = NormalizedAuditionState::default();
        state.set_enabled(true);
        let old = state
            .begin(make_fence("one.wav", 0.0, 0.5))
            .expect("old job");
        let newer = state
            .begin(make_fence("one.wav", 0.5, 1.0))
            .expect("new job");
        assert!(old.cancellation.load(Ordering::Acquire));
        let old_result = state.finish(
            &old,
            Ok(preparation(
                "one.wav",
                WaveformDetailRange {
                    start_frame: 0,
                    end_frame: 5,
                },
                2.0,
            )),
        );
        assert!(old_result.is_none());
        assert!(state.is_pending());
        let new_result = state.finish(
            &newer,
            Ok(preparation(
                "one.wav",
                WaveformDetailRange {
                    start_frame: 5,
                    end_frame: 10,
                },
                0.5,
            )),
        );
        assert_eq!(
            new_result
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .map(|result| result.gain),
            Some(0.5)
        );
        assert!(!state.is_pending());
    }

    #[test]
    fn cancellation_marks_token_and_discards_completion() {
        let mut state = NormalizedAuditionState::default();
        state.set_enabled(true);
        let job = state.begin(make_fence("one.wav", 0.0, 1.0)).expect("job");
        state.cancel();
        assert!(job.cancellation.load(Ordering::Acquire));
        assert!(state.finish(&job, Err(String::from("cancelled"))).is_none());
    }

    #[test]
    fn current_completion_with_a_different_path_fails_closed() {
        let mut state = NormalizedAuditionState::default();
        state.set_enabled(true);
        let job = state.begin(make_fence("one.wav", 0.0, 1.0)).expect("job");
        let result = state
            .finish(
                &job,
                Ok(preparation(
                    "other.wav",
                    WaveformDetailRange {
                        start_frame: 0,
                        end_frame: 10,
                    },
                    0.5,
                )),
            )
            .expect("current completion");
        assert!(result.is_err());
        assert!(!state.is_pending());
    }

    #[test]
    fn successful_completion_replaces_only_one_cached_result() {
        let mut state = NormalizedAuditionState::default();
        state.set_enabled(true);
        let first = state
            .begin(make_fence("one.wav", 0.0, 0.5))
            .expect("first job");
        assert!(
            state
                .finish(
                    &first,
                    Ok(preparation(
                        "one.wav",
                        WaveformDetailRange {
                            start_frame: 0,
                            end_frame: 5,
                        },
                        2.0,
                    )),
                )
                .is_some_and(|result| result.is_ok())
        );
        let second = state
            .begin(make_fence("two.wav", 0.0, 1.0))
            .expect("second job");
        assert!(
            state
                .finish(
                    &second,
                    Ok(preparation(
                        "two.wav",
                        WaveformDetailRange {
                            start_frame: 0,
                            end_frame: 10,
                        },
                        0.75,
                    )),
                )
                .is_some_and(|result| result.is_ok())
        );
        let third = state
            .begin(make_fence("three.wav", 0.0, 1.0))
            .expect("third job");
        assert_eq!(
            third.cached_preparation().map(|entry| entry.path),
            Some(PathBuf::from("two.wav"))
        );
    }

    #[test]
    fn playback_intent_span_preserves_region_and_full_track_loop_semantics() {
        let mut intent = super::super::transport::PlaybackIntent {
            start: 0.25,
            region: None,
            looped: true,
            volume: 1.0,
            path: PathBuf::from("one.wav"),
            origin: super::super::transport::PlaybackIntentOrigin::Explicit,
        };
        assert_eq!(
            span_for_playback_intent(&intent),
            WaveformPlayRegion::new(0.0, 1.0)
        );
        intent.region = WaveformPlayRegion::new(0.2, 0.4);
        intent.looped = false;
        assert_eq!(span_for_playback_intent(&intent), intent.region);
    }

    #[test]
    fn delayed_play_refreshes_manual_volume_before_submission() {
        let intent = super::super::transport::PlaybackIntent {
            start: 0.0,
            region: None,
            looped: false,
            volume: 0.2,
            path: PathBuf::from("one.wav"),
            origin: super::super::transport::PlaybackIntentOrigin::Explicit,
        };
        let delayed = delayed_play_intent_with_volume(intent, 0.7346);
        assert_eq!(delayed.volume, 0.735);
    }

    #[test]
    fn deferred_play_origin_preserves_autoplay_cancellation_semantics() {
        let intent = super::super::transport::PlaybackIntent {
            start: 0.0,
            region: None,
            looped: false,
            volume: 1.0,
            path: PathBuf::from("one.wav"),
            origin: super::super::transport::PlaybackIntentOrigin::SelectionAutoplay,
        };
        assert_eq!(
            deferred_play_origin(&NormalizedAuditionAction::Play(intent)),
            Some(super::super::transport::PlaybackIntentOrigin::SelectionAutoplay)
        );
        assert_eq!(
            deferred_play_origin(&NormalizedAuditionAction::ApplyGain),
            None
        );
    }

    #[test]
    fn disabling_keeps_the_latest_pending_loop_retarget() {
        let active = PendingNormalizedAudition {
            fence: make_fence("one.wav", 0.0, 1.0),
            action: NormalizedAuditionAction::RetargetLoop {
                desired_looped: true,
            },
        };
        let latest = PendingNormalizedAudition {
            fence: make_fence("one.wav", 0.2, 0.8),
            action: NormalizedAuditionAction::RetargetLoop {
                desired_looped: false,
            },
        };
        let retained = pending_loop_retarget_for_disable(
            Some(active.fence.clone()),
            Some(&latest),
            Some(false),
        )
        .expect("latest queued retarget");
        assert_eq!(retained, latest);

        let retained_active =
            pending_loop_retarget_for_disable(Some(active.fence.clone()), None, Some(true))
                .expect("active retarget");
        assert_eq!(retained_active, active);
    }

    #[test]
    fn normalized_gain_fence_rejects_stale_identity_epoch_or_span() {
        let path = Path::new("one.wav");
        let other_path = Path::new("other.wav");
        let span = WaveformPlayRegion::new(0.0, 1.0).expect("valid span");
        let other_span = WaveformPlayRegion::new(0.25, 1.0).expect("valid span");
        let matches = |latest_id,
                       epoch,
                       waveform_generation,
                       output_open_generation,
                       playing_path,
                       accepted_path,
                       accepted_span| {
            normalized_gain_fence_matches(
                17,
                latest_id,
                epoch,
                4,
                waveform_generation,
                5,
                output_open_generation,
                6,
                playing_path,
                path,
                accepted_path,
                accepted_span,
                span,
            )
        };
        assert!(matches(
            Some(17),
            4,
            5,
            6,
            Some(path),
            Some(path),
            Some(span)
        ));
        assert!(!matches(
            Some(16),
            4,
            5,
            6,
            Some(path),
            Some(path),
            Some(span)
        ));
        assert!(!matches(
            Some(17),
            3,
            5,
            6,
            Some(path),
            Some(path),
            Some(span)
        ));
        assert!(!matches(
            Some(17),
            4,
            5,
            6,
            Some(other_path),
            Some(path),
            Some(span)
        ));
        assert!(!matches(
            Some(17),
            4,
            5,
            6,
            Some(path),
            Some(path),
            Some(other_span)
        ));
    }

    #[test]
    fn normalized_gain_retry_budget_is_bounded() {
        assert!(!normalized_gain_retry_is_exhausted(0));
        assert!(!normalized_gain_retry_is_exhausted(
            MAX_NORMALIZED_GAIN_RETRIES - 1
        ));
        assert!(normalized_gain_retry_is_exhausted(
            MAX_NORMALIZED_GAIN_RETRIES
        ));
    }
}
