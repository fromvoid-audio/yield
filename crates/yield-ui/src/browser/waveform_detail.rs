#![deny(missing_docs)]
#![deny(warnings)]

//! Event driven, bounded waveform detail refinement for the browser canvas.

use std::{
    cell::RefCell,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use gpui::{Context, Task};
use yield_core::WaveformViewport;
use yield_media::{
    MAX_WAVEFORM_DETAIL_COLUMNS, MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES, WaveformDetailContext,
    WaveformDetailRange, WaveformDetailRequest, WaveformDetailResult, WaveformPreview,
    load_waveform_detail_with_cancel,
};

use super::Browser;

const DETAIL_DEBOUNCE: Duration = Duration::from_millis(45);

/// A retained detail projection selected for the current browser viewport.
#[derive(Clone)]
pub(super) struct WaveformDetailProjection {
    /// Detail columns retained by the state without copying on each frame.
    pub(super) bands: Arc<[[f32; 4]]>,
    /// Full-file normalized range represented by [`Self::bands`].
    pub(super) source_range: (f64, f64),
    /// Revision used to invalidate retained waveform geometry when detail arrives.
    pub(super) projection_revision: u64,
}

/// UI-owned lifecycle state for one bounded waveform detail request.
///
/// The state is cloneable because canvas paint closures outlive the render call. The mutable
/// fields remain UI-owned inside an `Rc`; filesystem work is performed only by the background
/// task created by [`Self::schedule_for_preview`].
#[derive(Clone, Default)]
pub(super) struct WaveformDetailState {
    inner: Rc<RefCell<WaveformDetailStateInner>>,
}

#[derive(Default)]
struct WaveformDetailStateInner {
    cancellation: Option<Arc<AtomicBool>>,
    task: Option<Task<()>>,
    current_request: Option<WaveformDetailRequest>,
    current_generation: Option<u64>,
    result: Option<Arc<RetainedDetailResult>>,
    result_generation: Option<u64>,
    projection_revision: u64,
    failed_key: Option<WaveformDetailKey>,
    failed_error: Option<String>,
    last_width: Option<usize>,
}

#[derive(Clone, Debug, PartialEq)]
struct WaveformDetailKey {
    generation: u64,
    request: WaveformDetailRequest,
}

struct RetainedDetailResult {
    context: WaveformDetailContext,
    range: WaveformDetailRange,
    bands: Arc<[[f32; 4]]>,
}

impl WaveformDetailState {
    /// Cancel the pending detail task while retaining a previous result as a fallback.
    pub(super) fn cancel(&self) {
        let mut state = self.inner.borrow_mut();
        cancel_pending(&mut state);
    }

    /// Cancel pending work and discard all detail associated with the old waveform generation.
    pub(super) fn reset(&self) {
        let mut state = self.inner.borrow_mut();
        cancel_pending(&mut state);
        state.result = None;
        state.result_generation = None;
        state.failed_key = None;
        state.failed_error = None;
        state.projection_revision = state.projection_revision.wrapping_add(1);
    }

    /// Remember the latest measured canvas width for the next event-driven request.
    pub(super) fn note_layout_width(&self, width: f32) -> bool {
        let Some(width) = finite_width(width) else {
            return false;
        };
        let mut state = self.inner.borrow_mut();
        let changed = state.last_width != Some(width);
        state.last_width = Some(width);
        changed
    }

    /// Return whether a refinement task is currently pending.
    pub(super) fn is_pending(&self) -> bool {
        self.inner.borrow().current_generation.is_some()
    }

    /// Return the latest accepted detail projection when it exactly covers the live request.
    pub(super) fn projection_for(
        &self,
        preview: &WaveformPreview,
        viewport: WaveformViewport,
        generation: u64,
    ) -> Option<WaveformDetailProjection> {
        let state = self.inner.borrow();
        let width = state.last_width?;
        let request = detail_request_for_preview(preview, viewport, width)?;
        let result = state.result.as_ref()?;
        if state.result_generation != Some(generation)
            || result.context != request.context
            || result.range != request.range
            || result.bands.len() != request.output_columns
        {
            return None;
        }
        let total_frames = result.context.total_frames as f64;
        if !total_frames.is_finite() || total_frames <= 0.0 {
            return None;
        }
        Some(WaveformDetailProjection {
            bands: Arc::clone(&result.bands),
            source_range: (
                result.range.start_frame as f64 / total_frames,
                result.range.end_frame as f64 / total_frames,
            ),
            projection_revision: state.projection_revision,
        })
    }

    /// Debounce and launch a detail request for the measured waveform layout, if needed.
    ///
    /// Repeated renders with the same live request are no-ops. A changed viewport, layout, or
    /// waveform generation cancels the prior token and replaces its request. The worker receives
    /// an immutable request and performs all file I/O through the media crate.
    pub(super) fn schedule_for_preview(
        &self,
        preview: &WaveformPreview,
        viewport: WaveformViewport,
        generation: u64,
        cx: &mut Context<Browser>,
    ) {
        let width = self.inner.borrow().last_width;
        let Some(width) = width else {
            return;
        };
        let Some(request) = detail_request_for_preview(preview, viewport, width) else {
            self.cancel_if_not_needed();
            return;
        };
        let key = WaveformDetailKey {
            generation,
            request: request.clone(),
        };
        {
            let mut state = self.inner.borrow_mut();
            if let Some(current_generation) = state.current_generation
                && let Some(current_request) = state.current_request.as_ref()
                && (current_generation != generation || current_request != &request)
            {
                cancel_pending(&mut state);
            }
            if state.result_generation == Some(generation)
                && state.result.as_ref().is_some_and(|result| {
                    result.context == request.context
                        && result.range == request.range
                        && result.bands.len() == request.output_columns
                })
            {
                return;
            }
            if state.current_generation == Some(generation)
                && state.current_request.as_ref() == Some(&request)
            {
                return;
            }
            if state.failed_key.as_ref() == Some(&key) {
                return;
            }
            cancel_pending(&mut state);
            state.current_request = Some(request.clone());
            state.current_generation = Some(generation);
            state.failed_key = None;
            state.failed_error = None;
        }

        let cancellation = Arc::new(AtomicBool::new(false));
        let worker_cancellation = Arc::clone(&cancellation);
        self.inner.borrow_mut().cancellation = Some(Arc::clone(&cancellation));
        let completion_cancellation = Arc::clone(&cancellation);
        let completion_request = request.clone();
        let task = cx.spawn(async move |this, cx| {
            gpui::Timer::after(DETAIL_DEBOUNCE).await;
            if cancellation.load(Ordering::Acquire) {
                return;
            }
            let result = cx
                .background_executor()
                .spawn(async move {
                    load_waveform_detail_with_cancel(&request, Some(worker_cancellation.as_ref()))
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.finish_waveform_detail(
                    generation,
                    completion_request,
                    completion_cancellation,
                    result,
                    cx,
                );
            });
        });
        self.inner.borrow_mut().task = Some(task);
    }

    fn cancel_if_not_needed(&self) {
        let mut state = self.inner.borrow_mut();
        if state.current_request.is_some() {
            cancel_pending(&mut state);
        }
    }

    fn settle(
        &self,
        generation: u64,
        request: WaveformDetailRequest,
        cancellation: Arc<AtomicBool>,
        desired: Option<WaveformDetailRequest>,
        result: Result<WaveformDetailResult, String>,
    ) -> bool {
        let key = WaveformDetailKey {
            generation,
            request: request.clone(),
        };
        let mut state = self.inner.borrow_mut();
        let token_is_current = state
            .cancellation
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &cancellation));
        if !token_is_current {
            return false;
        }
        if state.current_generation != Some(generation)
            || state.current_request.as_ref() != Some(&request)
        {
            cancel_pending(&mut state);
            return false;
        }
        if cancellation.load(Ordering::Acquire) {
            cancel_pending(&mut state);
            return false;
        }
        state.task = None;
        state.cancellation = None;
        state.current_generation = None;
        state.current_request = None;
        if desired.as_ref() != Some(&request) {
            return false;
        }
        match result {
            Ok(result) if result_matches_request(&result, &request) => {
                state.result = Some(Arc::new(RetainedDetailResult {
                    context: result.context,
                    range: result.range,
                    bands: Arc::from(result.bands.into_boxed_slice()),
                }));
                state.result_generation = Some(generation);
                state.failed_key = None;
                state.failed_error = None;
            }
            Ok(_) => {
                state.result = None;
                state.result_generation = None;
                state.failed_key = Some(key);
                state.failed_error = Some(String::from("waveform detail result was incomplete"));
            }
            Err(error) if error.contains("canceled") => return false,
            Err(error) => {
                state.result = None;
                state.result_generation = None;
                state.failed_key = Some(key);
                state.failed_error = Some(error);
            }
        }
        state.projection_revision = state.projection_revision.wrapping_add(1);
        true
    }

    #[cfg(test)]
    fn seed_pending_for_test(
        &self,
        request: WaveformDetailRequest,
        generation: u64,
        cancellation: Arc<AtomicBool>,
    ) {
        let mut state = self.inner.borrow_mut();
        state.current_request = Some(request);
        state.current_generation = Some(generation);
        state.cancellation = Some(cancellation);
    }

    #[cfg(test)]
    fn failed_key_for_test(&self) -> Option<WaveformDetailKey> {
        self.inner.borrow().failed_key.clone()
    }
}

impl Drop for WaveformDetailState {
    fn drop(&mut self) {
        if Rc::strong_count(&self.inner) == 1 {
            cancel_pending(&mut self.inner.borrow_mut());
        }
    }
}

impl Browser {
    pub(super) fn finish_waveform_detail(
        &mut self,
        generation: u64,
        request: WaveformDetailRequest,
        cancellation: Arc<AtomicBool>,
        result: Result<WaveformDetailResult, String>,
        cx: &mut Context<Self>,
    ) {
        let desired = self.waveform.as_ref().and_then(|preview| {
            let width = self.waveform_detail.inner.borrow().last_width?;
            detail_request_for_preview(preview, self.waveform_viewport, width)
        });
        if self
            .waveform_detail
            .settle(generation, request, cancellation, desired, result)
        {
            cx.notify();
        }
    }
}

fn cancel_pending(state: &mut WaveformDetailStateInner) {
    if let Some(cancellation) = state.cancellation.take() {
        cancellation.store(true, Ordering::Release);
    }
    state.task.take();
    state.current_request = None;
    state.current_generation = None;
}

fn result_matches_request(result: &WaveformDetailResult, request: &WaveformDetailRequest) -> bool {
    result.context == request.context
        && result.range == request.range
        && result.bands.len() == request.output_columns
}

fn finite_width(width: f32) -> Option<usize> {
    width.is_finite().then(|| {
        width
            .ceil()
            .max(1.0)
            .min(MAX_WAVEFORM_DETAIL_COLUMNS as f32) as usize
    })
}

/// Compute the exact source frames covered by a normalized viewport.
pub(super) fn detail_frame_range(
    viewport: WaveformViewport,
    total_frames: usize,
) -> Option<WaveformDetailRange> {
    if total_frames == 0 || !viewport.start.is_finite() || !viewport.end.is_finite() {
        return None;
    }
    let raw_start = viewport.start.min(viewport.end);
    let raw_end = viewport.start.max(viewport.end);
    if raw_end <= 0.0 || raw_start >= 1.0 {
        return None;
    }
    let start = raw_start.clamp(0.0, 1.0);
    let end = raw_end.clamp(0.0, 1.0);
    let mut start_frame = (start * total_frames as f64).floor() as usize;
    let mut end_frame = (end * total_frames as f64).ceil() as usize;
    start_frame = start_frame.min(total_frames.saturating_sub(1));
    end_frame = end_frame.min(total_frames);
    if end_frame <= start_frame {
        end_frame = start_frame.saturating_add(1).min(total_frames);
    }
    (start_frame < end_frame).then_some(WaveformDetailRange {
        start_frame,
        end_frame,
    })
}

/// Return the detail request that is useful for one preview layout, if its overview is too coarse.
pub(super) fn detail_request_for_preview(
    preview: &WaveformPreview,
    viewport: WaveformViewport,
    width: usize,
) -> Option<WaveformDetailRequest> {
    let context = preview.detail_context.as_ref()?;
    detail_request_for_context(context, preview.bands.len(), viewport, width)
}

fn detail_request_for_context(
    context: &WaveformDetailContext,
    overview_columns: usize,
    viewport: WaveformViewport,
    width: usize,
) -> Option<WaveformDetailRequest> {
    let range = detail_frame_range(viewport, context.total_frames)?;
    if range.len() > MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES {
        return None;
    }
    let output_columns = width
        .max(1)
        .min(MAX_WAVEFORM_DETAIL_COLUMNS)
        .min(range.len());
    if !detail_resolution_is_insufficient(
        context.total_frames,
        overview_columns,
        range.len(),
        output_columns,
    ) {
        return None;
    }
    WaveformDetailRequest::new(
        context.clone(),
        range.start_frame,
        range.end_frame,
        output_columns,
    )
    .ok()
}

/// Determine whether viewport detail has finer frame buckets than the bounded overview.
pub(super) fn detail_resolution_is_insufficient(
    total_frames: usize,
    overview_columns: usize,
    visible_frames: usize,
    output_columns: usize,
) -> bool {
    if total_frames == 0
        || visible_frames == 0
        || output_columns == 0
        || overview_columns == 0
        || visible_frames > MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES
    {
        return false;
    }
    let overview_bucket = ceil_div(total_frames, overview_columns);
    let detail_bucket = ceil_div(visible_frames, output_columns);
    detail_bucket < overview_bucket
}

fn ceil_div(value: usize, divisor: usize) -> usize {
    value / divisor + usize::from(value % divisor != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_range_uses_floor_start_and_ceil_end_and_clips_silence_margin() {
        let range = detail_frame_range(
            WaveformViewport {
                start: -0.1,
                end: 0.251,
            },
            1_000,
        )
        .expect("clipped viewport has frames");
        assert_eq!(range.start_frame, 0);
        assert_eq!(range.end_frame, 251);

        let range = detail_frame_range(
            WaveformViewport {
                start: 0.251,
                end: 0.2511,
            },
            1_000,
        )
        .expect("narrow viewport has at least one frame");
        assert_eq!(range.start_frame, 251);
        assert_eq!(range.end_frame, 252);
    }

    #[test]
    fn detail_only_starts_when_overview_buckets_are_coarser() {
        assert!(!detail_resolution_is_insufficient(
            2_048, 2_048, 2_048, 1_024
        ));
        assert!(detail_resolution_is_insufficient(
            1_000_000, 2_048, 250_000, 1_000
        ));
        assert!(!detail_resolution_is_insufficient(
            1_000_000,
            2_048,
            MAX_WAVEFORM_DETAIL_VISIBLE_FRAMES + 1,
            1_000,
        ));
    }

    #[test]
    fn state_cancel_clears_pending_without_marking_a_failure() {
        let (_file, preview) = test_preview();
        let request = detail_request_for_preview(
            &preview,
            WaveformViewport {
                start: 0.25,
                end: 0.5,
            },
            1_000,
        )
        .expect("fixture viewport needs detail");
        let state = WaveformDetailState::default();
        let cancellation = Arc::new(AtomicBool::new(false));
        state.seed_pending_for_test(request, 1, Arc::clone(&cancellation));
        assert!(state.is_pending());
        state.cancel();
        assert!(!state.is_pending());
        assert!(cancellation.load(Ordering::Acquire));
        assert!(state.failed_key_for_test().is_none());
    }

    #[test]
    fn stale_generation_range_and_token_cannot_settle_a_new_request() {
        let (_file, preview) = test_preview();
        let request = detail_request_for_preview(
            &preview,
            WaveformViewport {
                start: 0.25,
                end: 0.5,
            },
            1_000,
        )
        .expect("fixture viewport needs detail");
        let state = WaveformDetailState::default();
        let token = Arc::new(AtomicBool::new(false));
        state.seed_pending_for_test(request.clone(), 7, Arc::clone(&token));

        let wrong_token = Arc::new(AtomicBool::new(false));
        assert!(!state.settle(
            7,
            request.clone(),
            wrong_token,
            Some(request.clone()),
            Ok(WaveformDetailResult {
                context: request.context.clone(),
                range: request.range,
                bands: vec![[0.0; 4]; request.output_columns],
            }),
        ));
        assert!(state.is_pending());
        assert!(!state.settle(
            8,
            request.clone(),
            Arc::clone(&token),
            Some(request.clone()),
            Ok(WaveformDetailResult {
                context: request.context.clone(),
                range: request.range,
                bands: vec![[0.0; 4]; request.output_columns],
            }),
        ));
        assert!(!state.is_pending());

        let other_request = detail_request_for_preview(
            &preview,
            WaveformViewport {
                start: 0.5,
                end: 0.75,
            },
            1_000,
        )
        .expect("second fixture viewport needs detail");
        let range_state = WaveformDetailState::default();
        let range_token = Arc::new(AtomicBool::new(false));
        range_state.seed_pending_for_test(other_request.clone(), 9, Arc::clone(&range_token));
        assert!(!range_state.settle(
            9,
            request,
            range_token,
            Some(other_request),
            Err(String::from("stale range")),
        ));
        assert!(!range_state.is_pending());
    }

    #[test]
    fn mismatched_result_context_is_rejected_without_projection() {
        let (_file, preview) = test_preview();
        let request = detail_request_for_preview(
            &preview,
            WaveformViewport {
                start: 0.25,
                end: 0.5,
            },
            1_000,
        )
        .expect("fixture viewport needs detail");
        let mut changed_context = request.context.clone();
        changed_context.path.push("replaced.wav");
        assert!(!result_matches_request(
            &WaveformDetailResult {
                context: changed_context,
                range: request.range,
                bands: vec![[0.0; 4]; request.output_columns],
            },
            &request,
        ));
    }

    #[test]
    fn successful_detail_exposes_arc_projection_then_stale_retry_falls_back() {
        let (_file, preview) = test_preview();
        let viewport = WaveformViewport {
            start: 0.25,
            end: 0.5,
        };
        let request = detail_request_for_preview(&preview, viewport, 1_000)
            .expect("fixture viewport needs detail");
        let state = WaveformDetailState::default();
        state.note_layout_width(1_000.0);
        let cancellation = Arc::new(AtomicBool::new(false));
        state.seed_pending_for_test(request.clone(), 3, Arc::clone(&cancellation));
        assert!(state.settle(
            3,
            request.clone(),
            cancellation,
            Some(request.clone()),
            Ok(WaveformDetailResult {
                context: request.context.clone(),
                range: request.range,
                bands: vec![[0.25; 4]; request.output_columns],
            }),
        ));
        assert!(!state.is_pending());
        let projection = state
            .projection_for(&preview, viewport, 3)
            .expect("settled detail projection");
        assert_eq!(projection.bands.len(), request.output_columns);
        assert_eq!(projection.source_range.0, 0.25);
        assert_eq!(projection.source_range.1, 0.5);

        let retry_cancellation = Arc::new(AtomicBool::new(false));
        state.seed_pending_for_test(request.clone(), 3, Arc::clone(&retry_cancellation));
        assert!(state.settle(
            3,
            request.clone(),
            retry_cancellation,
            Some(request.clone()),
            Err(String::from("stale detail")),
        ));
        assert!(state.projection_for(&preview, viewport, 3).is_none());

        assert!(!state.settle(
            3,
            request.clone(),
            Arc::new(AtomicBool::new(false)),
            Some(request),
            Err(String::from("late duplicate")),
        ));
    }

    #[test]
    fn failed_detail_is_keyed_until_the_viewport_request_changes() {
        let (_file, preview) = test_preview();
        let request = detail_request_for_preview(
            &preview,
            WaveformViewport {
                start: 0.25,
                end: 0.5,
            },
            1_000,
        )
        .expect("fixture viewport needs detail");
        let state = WaveformDetailState::default();
        let cancellation = Arc::new(AtomicBool::new(false));
        state.seed_pending_for_test(request.clone(), 4, Arc::clone(&cancellation));
        assert!(state.settle(
            4,
            request.clone(),
            cancellation,
            Some(request.clone()),
            Err(String::from("stale detail")),
        ));
        assert!(!state.is_pending());
        assert_eq!(
            state.failed_key_for_test(),
            Some(WaveformDetailKey {
                generation: 4,
                request,
            })
        );
    }

    fn test_preview() -> (tempfile::TempPath, WaveformPreview) {
        use std::io::Write;

        const SAMPLE_COUNT: usize = 100_000;
        let mut bytes = Vec::with_capacity(44 + SAMPLE_COUNT * 2);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&((36 + SAMPLE_COUNT * 2) as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&8_000_u32.to_le_bytes());
        bytes.extend_from_slice(&(16_000_u32).to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&((SAMPLE_COUNT * 2) as u32).to_le_bytes());
        for index in 0..SAMPLE_COUNT {
            let sample = if index % 1_000 == 0 { i16::MAX } else { 0 };
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let mut file = tempfile::Builder::new()
            .suffix(".wav")
            .tempfile()
            .expect("temporary WAV");
        file.write_all(&bytes).expect("write temporary WAV");
        // Retain cleanup ownership while closing the writer before the Windows read lease.
        let path = file.into_temp_path();
        let preview = yield_media::load_waveform_preview(&path).expect("preview");
        (path, preview)
    }

    #[test]
    fn detail_range_never_exceeds_the_file_at_edges() {
        assert_eq!(
            detail_frame_range(
                WaveformViewport {
                    start: 0.9999,
                    end: 1.5,
                },
                10,
            ),
            Some(WaveformDetailRange {
                start_frame: 9,
                end_frame: 10,
            })
        );
        assert!(
            detail_frame_range(
                WaveformViewport {
                    start: 1.1,
                    end: 2.0,
                },
                10,
            )
            .is_none()
        );
        assert!(
            detail_frame_range(
                WaveformViewport {
                    start: -2.0,
                    end: -1.1,
                },
                10,
            )
            .is_none()
        );
        assert!(detail_frame_range(WaveformViewport::full(), 0).is_none());
    }
}
