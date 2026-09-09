#![deny(missing_docs)]
#![deny(warnings)]

//! Playback-heard range ownership and paint-only waveform overlay.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{Bounds, Hsla, IntoElement, canvas, fill, point, prelude::*, px, size};
use reson::{PlaybackRuntimeProgress, PlaybackRuntimeStarted};
use wavecrate_core::{PlayedRangeHistory, WaveformRegion};

use super::waveform::WaveformPlayRegion;
use super::{Browser, transport};

const PLAYED_RANGE_RAIL_HEIGHT: f32 = 4.0;
const PLAYED_RANGE_MIN_WIDTH: f32 = 1.0;

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackIdentity {
    playback_id: u64,
    playback_epoch: u64,
    waveform_generation: u64,
    root: PathBuf,
    path: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RuntimeSpan {
    start_frame: u64,
    end_frame: u64,
    total_frames: u64,
    configured_span_request_id: Option<u64>,
}

/// UI-owned heard-range history for the currently loaded waveform.
#[derive(Default)]
pub(super) struct PlayedRanges {
    history: PlayedRangeHistory,
    admission: Option<PlaybackIdentity>,
    identity: Option<PlaybackIdentity>,
    runtime_span: Option<RuntimeSpan>,
    started_position: Option<f64>,
    started: bool,
}

impl PlayedRanges {
    fn admit(&mut self, identity: PlaybackIdentity) {
        self.admission = Some(identity);
    }

    fn start(&mut self, identity: PlaybackIdentity, span: WaveformRegion, position: f64) {
        self.identity = Some(identity);
        self.runtime_span = None;
        self.started_position = position.is_finite().then_some(position);
        self.started = self.history.start(span, position);
        if !self.started {
            self.started_position = None;
        }
    }

    fn confirm_started(
        &mut self,
        current: PlaybackIdentity,
        span: WaveformRegion,
        position: f64,
    ) -> bool {
        let Some(admission) = self.admission.take() else {
            self.clear_anchor();
            return false;
        };
        if !admission_matches_current(&admission, &current) {
            self.clear_anchor();
            return false;
        }
        self.start(current, span, position);
        self.started
    }

    fn clear_anchor(&mut self) {
        self.history.clear_anchor();
        self.admission = None;
        self.runtime_span = None;
        self.started_position = None;
        self.started = false;
    }

    fn reset_observation_gap(&mut self) {
        self.history.clear_anchor();
        self.runtime_span = None;
        self.started_position = None;
    }

    fn clear_history(&mut self) {
        self.history.clear();
        self.admission = None;
        self.identity = None;
        self.runtime_span = None;
        self.started_position = None;
        self.started = false;
    }

    fn waveform_identity_is_current(&self, browser: &Browser) -> bool {
        let Some(identity) = self.identity.as_ref() else {
            return false;
        };
        browser.waveform.is_some()
            && browser.waveform_generation == identity.waveform_generation
            && browser.root.as_deref() == Some(identity.root.as_path())
            && browser.selected_audio_path() == Some(identity.path.as_path())
            && browser
                .waveform
                .as_ref()
                .and_then(|preview| preview.detail_context.as_ref())
                .is_none_or(|context| context.path == identity.path)
    }

    fn playback_identity_is_current(&self, browser: &Browser, identity: &PlaybackIdentity) -> bool {
        self.identity.as_ref() == Some(identity)
            && browser.playback_epoch == identity.playback_epoch
            && browser.latest_playback_request_id.map(|id| id.get()) == Some(identity.playback_id)
            && browser.playing_path.as_deref() == Some(identity.path.as_path())
            && browser
                .accepted_playback_intent
                .as_ref()
                .is_some_and(|intent| intent.path == identity.path)
            && self.waveform_identity_is_current(browser)
    }

    fn record_runtime_progress(
        &mut self,
        identity: &PlaybackIdentity,
        span: WaveformRegion,
        runtime_span: RuntimeSpan,
        position: f64,
        looping: bool,
    ) -> bool {
        if self.identity.as_ref() != Some(identity) || !self.started {
            return false;
        }
        if self.runtime_span != Some(runtime_span) {
            let previous_runtime_span = self.runtime_span;
            let start_position = self.started_position.take();
            self.runtime_span = Some(runtime_span);
            if !self.history.start(
                span,
                if previous_runtime_span.is_none()
                    && runtime_span.configured_span_request_id.is_none()
                {
                    start_position.unwrap_or(position)
                } else {
                    position
                },
            ) {
                self.started = false;
                return false;
            }
            if previous_runtime_span.is_none() && runtime_span.configured_span_request_id.is_none()
            {
                return self.history.record_progress(span, position, looping);
            }
            return false;
        }
        self.history.record_progress(span, position, looping)
    }
}

fn current_playback_identity(browser: &Browser, playback_id: u64) -> Option<PlaybackIdentity> {
    let path = browser.playing_path.as_ref()?.clone();
    let accepted = browser.accepted_playback_intent.as_ref()?;
    if accepted.path != path
        || browser.selected_audio_path() != Some(path.as_path())
        || browser.waveform.is_none()
    {
        return None;
    }
    if browser
        .waveform
        .as_ref()
        .and_then(|preview| preview.detail_context.as_ref())
        .is_some_and(|context| context.path != path)
    {
        return None;
    }
    Some(PlaybackIdentity {
        playback_id,
        playback_epoch: browser.playback_epoch,
        waveform_generation: browser.waveform_generation,
        root: browser.root.as_ref()?.clone(),
        path,
    })
}

fn admission_matches_current(admission: &PlaybackIdentity, current: &PlaybackIdentity) -> bool {
    admission == current
}

fn runtime_span(progress: &PlaybackRuntimeProgress) -> Option<(WaveformRegion, RuntimeSpan)> {
    if !progress.active || progress.error.is_some() {
        return None;
    }
    let ((start_frame, end_frame), total_frames) =
        (progress.playback_span_frames?, progress.track_total_frames?);
    if total_frames == 0 || start_frame >= end_frame || end_frame > total_frames {
        return None;
    }
    let start = start_frame as f64 / total_frames as f64;
    let end = end_frame as f64 / total_frames as f64;
    Some((
        WaveformRegion::new(start, end)?,
        RuntimeSpan {
            start_frame,
            end_frame,
            total_frames,
            configured_span_request_id: progress.configured_span_request_id.map(|id| id.get()),
        },
    ))
}

fn normalized_progress(progress: &PlaybackRuntimeProgress) -> Option<f64> {
    let position = f64::from(progress.progress?);
    position.is_finite().then_some(position.clamp(0.0, 1.0))
}

/// Record the position confirmed by a current playback start event.
pub(super) fn record_started(browser: &mut Browser, started: &PlaybackRuntimeStarted) {
    let Some(current) = current_playback_identity(browser, started.id.get()) else {
        browser.played_ranges.clear_anchor();
        return;
    };
    let Some(admission) = browser.played_ranges.admission.take() else {
        browser.played_ranges.clear_anchor();
        return;
    };
    if !admission_matches_current(&admission, &current) {
        browser.played_ranges.clear_anchor();
        return;
    }
    let Some(position) = normalized_started_position(started.playback_start) else {
        browser.played_ranges.clear_anchor();
        return;
    };
    let Some(span) = browser
        .accepted_playback_intent
        .as_ref()
        .and_then(transport::playback_span_for_intent)
        .and_then(WaveformPlayRegion::core)
    else {
        browser.played_ranges.clear_anchor();
        return;
    };
    browser
        .played_ranges
        .confirm_started(current, span, position);
}

fn normalized_started_position(position: f32) -> Option<f64> {
    let position = f64::from(position);
    position.is_finite().then_some(position.clamp(0.0, 1.0))
}

/// Capture the source and waveform generation at the moment a playback request is admitted.
pub(super) fn record_admission(browser: &mut Browser, playback_id: u64, path: &std::path::Path) {
    let Some(root) = browser.root.as_ref() else {
        browser.played_ranges.admission = None;
        return;
    };
    if browser.selected_audio_path() != Some(path) {
        browser.played_ranges.admission = None;
        return;
    }
    browser.played_ranges.admit(PlaybackIdentity {
        playback_id,
        playback_epoch: browser.playback_epoch,
        waveform_generation: browser.waveform_generation,
        root: root.clone(),
        path: path.to_path_buf(),
    });
}

/// Record an identity-gated active playback progress event.
pub(super) fn record_progress(browser: &mut Browser, progress: &PlaybackRuntimeProgress) {
    let Some(playback_id) = browser.latest_playback_request_id.map(|id| id.get()) else {
        return;
    };
    if progress.playback_request_id.map(|id| id.get()) != Some(playback_id) {
        return;
    }
    let Some(identity) = current_playback_identity(browser, playback_id) else {
        return;
    };
    let Some(position) = normalized_progress(progress) else {
        if browser
            .played_ranges
            .playback_identity_is_current(browser, &identity)
        {
            browser.played_ranges.reset_observation_gap();
        }
        return;
    };
    let Some((span, runtime_span)) = runtime_span(progress) else {
        if browser
            .played_ranges
            .playback_identity_is_current(browser, &identity)
        {
            browser.played_ranges.reset_observation_gap();
        }
        return;
    };

    browser.played_ranges.record_runtime_progress(
        &identity,
        span,
        runtime_span,
        position,
        progress.looping,
    );
}

/// Reset only the transient progress anchor while preserving the visible history.
pub(super) fn clear_anchor(browser: &mut Browser) {
    browser.played_ranges.clear_anchor();
}

/// Reset progress observation state while retaining the confirmed playback lifetime.
pub(super) fn reset_observation_gap(browser: &mut Browser) {
    browser.played_ranges.reset_observation_gap();
}

/// Clear history owned by an invalidated waveform generation.
pub(super) fn clear_history(browser: &mut Browser) {
    browser.played_ranges.clear_history();
}

fn ranges_for_waveform(browser: &Browser) -> Option<Arc<Vec<WaveformRegion>>> {
    if !browser.played_ranges.waveform_identity_is_current(browser)
        || browser.played_ranges.history.is_empty()
    {
        return None;
    }
    Some(browser.played_ranges.history.snapshot())
}

/// Paint the heard ranges as a clipped four-pixel rail at the waveform bottom.
pub(super) fn render_overlay(browser: &Browser, rail_color: Hsla) -> impl IntoElement {
    let ranges = ranges_for_waveform(browser);
    let viewport = browser.waveform_viewport;
    let ranges_for_prepaint = ranges.clone();
    canvas(
        move |_, _, _| {
            ranges_for_prepaint.as_ref().map_or(0, |ranges| {
                ranges.partition_point(|range| range.end() < viewport.start)
            })
        },
        move |bounds, first_visible, window, _| {
            let width = f32::from(bounds.size.width);
            let height = f32::from(bounds.size.height);
            if !width.is_finite() || width <= 0.0 || !height.is_finite() || height <= 0.0 {
                return;
            }
            let rail_height = PLAYED_RANGE_RAIL_HEIGHT.min(height);
            let top = bounds.bottom() - px(rail_height);
            let Some(ranges) = ranges.as_ref() else {
                return;
            };
            for range in ranges.iter().skip(first_visible) {
                if range.start() > viewport.end {
                    break;
                }
                let Some((start, end)) =
                    viewport.visible_range_from_absolute(range.start(), range.end())
                else {
                    continue;
                };
                let left = (start as f32 * width).clamp(0.0, width);
                let right = (end as f32 * width).clamp(left, width);
                let span = right - left;
                if !span.is_finite() || span <= 0.0 {
                    continue;
                }
                let rail_width = span.max(PLAYED_RANGE_MIN_WIDTH).min(width);
                let left = if span < PLAYED_RANGE_MIN_WIDTH {
                    (right - rail_width).max(0.0)
                } else {
                    left
                };
                window.paint_quad(fill(
                    Bounds::new(
                        point(bounds.origin.x + px(left), top),
                        size(px(rail_width), px(rail_height)),
                    ),
                    rail_color,
                ));
            }
        },
    )
    .absolute()
    .top_0()
    .bottom_0()
    .left_0()
    .right_0()
}

#[cfg(test)]
mod tests {
    use super::{PlaybackIdentity, PlayedRanges, RuntimeSpan, runtime_span as parse_runtime_span};
    use reson::PlaybackRuntimeProgress;
    use std::path::PathBuf;
    use wavecrate_core::WaveformRegion;

    fn identity(playback_id: u64, waveform_generation: u64) -> PlaybackIdentity {
        PlaybackIdentity {
            playback_id,
            playback_epoch: 4,
            waveform_generation,
            root: PathBuf::from("/library"),
            path: PathBuf::from("/library/kick.wav"),
        }
    }

    fn span(start: f64, end: f64) -> WaveformRegion {
        WaveformRegion::new(start, end).expect("valid span")
    }

    fn runtime_span(configured_span_request_id: Option<u64>) -> RuntimeSpan {
        RuntimeSpan {
            start_frame: 20,
            end_frame: 80,
            total_frames: 100,
            configured_span_request_id,
        }
    }

    fn ranges(state: &PlayedRanges) -> Vec<(f64, f64)> {
        state
            .history
            .snapshot()
            .iter()
            .map(|range| (range.start(), range.end()))
            .collect()
    }

    #[test]
    fn runtime_span_requires_valid_frame_metadata() {
        let valid = PlaybackRuntimeProgress {
            active: true,
            progress: Some(0.25),
            playback_span_frames: Some((20, 80)),
            track_total_frames: Some(100),
            ..PlaybackRuntimeProgress::default()
        };
        let (region, span) = parse_runtime_span(&valid).expect("valid runtime span");
        assert_eq!((region.start(), region.end()), (0.2, 0.8));
        assert_eq!(span.start_frame, 20);
        assert_eq!(span.end_frame, 80);
        assert_eq!(span.total_frames, 100);

        for invalid in [
            PlaybackRuntimeProgress {
                active: true,
                playback_span_frames: Some((80, 20)),
                track_total_frames: Some(100),
                ..PlaybackRuntimeProgress::default()
            },
            PlaybackRuntimeProgress {
                active: true,
                playback_span_frames: Some((20, 101)),
                track_total_frames: Some(100),
                ..PlaybackRuntimeProgress::default()
            },
            PlaybackRuntimeProgress {
                active: true,
                playback_span_frames: Some((20, 80)),
                track_total_frames: Some(0),
                ..PlaybackRuntimeProgress::default()
            },
        ] {
            assert!(parse_runtime_span(&invalid).is_none());
        }
    }

    #[test]
    fn admission_fence_rejects_same_path_after_waveform_reload() {
        let mut state = PlayedRanges::default();
        state.admit(identity(9, 7));
        assert!(!state.confirm_started(identity(9, 8), span(0.2, 0.8), 0.2));
        assert!(state.history.is_empty());
    }

    #[test]
    fn first_valid_progress_records_from_confirmed_start() {
        let current = identity(9, 7);
        let mut state = PlayedRanges::default();
        state.admit(current.clone());
        assert!(state.confirm_started(current.clone(), span(0.2, 0.8), 0.2));
        assert!(state.record_runtime_progress(
            &current,
            span(0.2, 0.8),
            runtime_span(None),
            0.35,
            false,
        ));
        assert_eq!(ranges(&state), vec![(0.2, 0.35)]);
    }

    #[test]
    fn old_progress_identity_cannot_re_admit_after_reload() {
        let current = identity(9, 8);
        let old = identity(9, 7);
        let mut state = PlayedRanges::default();
        state.admit(current.clone());
        assert!(state.confirm_started(current.clone(), span(0.2, 0.8), 0.2));
        assert!(!state.record_runtime_progress(
            &old,
            span(0.2, 0.8),
            runtime_span(None),
            0.35,
            false,
        ));
        assert!(state.history.is_empty());
    }

    #[test]
    fn changed_configured_request_id_resets_anchor_for_same_bounds_seek() {
        let current = identity(9, 7);
        let span = span(0.2, 0.8);
        let mut state = PlayedRanges::default();
        state.admit(current.clone());
        assert!(state.confirm_started(current.clone(), span, 0.2));
        assert!(state.record_runtime_progress(&current, span, runtime_span(None), 0.4, false,));
        assert!(
            !state.record_runtime_progress(&current, span, runtime_span(Some(33)), 0.6, false,)
        );
        assert_eq!(ranges(&state), vec![(0.2, 0.4)]);
        assert!(
            state.record_runtime_progress(&current, span, runtime_span(Some(33)), 0.65, false,)
        );
        assert_eq!(ranges(&state), vec![(0.2, 0.4), (0.6, 0.65)]);
    }

    #[test]
    fn stop_anchor_reset_preserves_history_but_waveform_clear_erases_it() {
        let current = identity(9, 7);
        let span = span(0.0, 1.0);
        let mut state = PlayedRanges::default();
        state.admit(current.clone());
        assert!(state.confirm_started(current.clone(), span, 0.1));
        state.record_runtime_progress(&current, span, runtime_span(None), 0.4, false);
        state.clear_anchor();
        assert_eq!(ranges(&state), vec![(0.1, 0.4)]);
        state.clear_history();
        assert!(state.history.is_empty());
    }

    #[test]
    fn invalid_observation_gap_recovers_without_bridging() {
        let current = identity(9, 7);
        let span = span(0.0, 1.0);
        let mut state = PlayedRanges::default();
        state.admit(current.clone());
        assert!(state.confirm_started(current.clone(), span, 0.1));
        assert!(state.record_runtime_progress(&current, span, runtime_span(None), 0.4, false,));

        state.reset_observation_gap();
        assert!(!state.record_runtime_progress(&current, span, runtime_span(None), 0.7, false,));
        assert_eq!(ranges(&state), vec![(0.1, 0.4)]);
        assert!(state.record_runtime_progress(&current, span, runtime_span(None), 0.8, false,));
        assert_eq!(ranges(&state), vec![(0.1, 0.4), (0.7, 0.8)]);
    }
}
