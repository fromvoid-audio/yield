//! Session target and one bounded background job for optional tempo audition.

use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use gpui::{AppContext, Context, Div, Entity, Task, Timer, div, prelude::*, px};
use reson::{
    MAX_TEMPO_INPUT_SAMPLES, MAX_TEMPO_SPAN_SECONDS, PlaybackRuntimeMode, PlaybackRuntimeSource,
    PlaybackRuntimeTempoSpan, TempoAuditionTarget, prepare_tempo_audition, read_tempo_wav_span,
};
use yield_design::Theme;
use yield_library::sample_sources::SourcePlaymarkGridTempo;
use yield_media::{WaveformDetailContext, WaveformFileRevision, with_fenced_wav_read};

use crate::search_input::{SearchChanged, SearchInput, SearchInputOptions};

use super::{
    Browser, playmark_tempo,
    transport::{PlaybackIntentOrigin, ResolvedAudition},
};

pub(super) const CONTROL: &str = "settings-audition-target-tempo";
const PREPARATION_BUDGET: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum TempoPlaybackLabel {
    Stretched(f64),
    Raw(&'static str),
}

pub(super) struct State {
    pub(super) input: Entity<SearchInput>,
    active: Option<Arc<AtomicBool>>,
    pending: Option<Pending>,
    timeout: Option<Task<()>>,
    pub(super) playing: Option<TempoPlaybackLabel>,
}

impl State {
    pub(super) fn new(cx: &mut Context<Browser>) -> Self {
        let input = cx.new(|cx| {
            let mut input =
                SearchInput::new_with_options(cx, SearchInputOptions::playmark_grid_tempo());
            input.enable_tab_stop();
            input
        });
        cx.subscribe(&input, |_, _, _: &SearchChanged, cx| cx.notify())
            .detach();
        Self {
            input,
            active: None,
            pending: None,
            timeout: None,
            playing: None,
        }
    }

    pub(super) fn cancel_pending(&mut self) {
        self.pending = None;
        self.timeout = None;
        if let Some(active) = &self.active {
            active.store(true, Ordering::Release);
        }
    }

    pub(super) fn is_preparing(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn set_pending_volume(&mut self, volume: f32) {
        if let Some(pending) = &mut self.pending {
            pending.audition.intent.set_volume(volume);
            pending.audition.request.volume = pending.audition.intent.volume;
        }
    }

    pub(super) fn is_stretched(&self) -> bool {
        matches!(self.playing, Some(TempoPlaybackLabel::Stretched(_)))
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel_pending();
    }
}

struct Pending {
    playback_epoch: u64,
    waveform_generation: u64,
    output_open_generation: u64,
    revision: WaveformFileRevision,
    audition: ResolvedAudition,
    target_bpm: f64,
}

struct Job {
    context: WaveformDetailContext,
    source_span: Range<u64>,
    loop_start: u64,
    source_bpm: f64,
    target_bpm: f64,
    deadline: Instant,
}

pub(super) fn render(browser: &Browser, theme: &Theme) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(theme.spacing.control_gap))
        .child("Audition target BPM")
        .child(
            div()
                .debug_selector(|| CONTROL.to_owned())
                .w(px(90.0))
                .when_some(browser.ax_marker(CONTROL), |element, marker| {
                    element.child(marker)
                })
                .child(browser.tempo_audition.input.clone()),
        )
        .child("Blank plays at source tempo")
}

pub(super) fn playing_status(label: Option<TempoPlaybackLabel>, progress: Option<f32>) -> String {
    let playing = progress.map_or_else(
        || String::from("Playing"),
        |ratio| format!("Playing {:.0}%", ratio * 100.0),
    );
    match label {
        None => playing,
        Some(TempoPlaybackLabel::Stretched(bpm)) => format!("{playing} · {bpm:.2} BPM"),
        Some(TempoPlaybackLabel::Raw(reason)) => format!("{playing} · Raw ({reason})"),
    }
}

pub(super) fn route(
    browser: &mut Browser,
    mut audition: ResolvedAudition,
    cx: &mut Context<Browser>,
) -> Option<ResolvedAudition> {
    let draft = browser.tempo_audition.input.read(cx).content().trim();
    if draft.is_empty() {
        return Some(audition);
    }
    let Some(target) = SourcePlaymarkGridTempo::parse(draft) else {
        audition.tempo_label = Some(TempoPlaybackLabel::Raw("invalid target BPM"));
        return Some(audition);
    };
    let job = make_job(browser, &audition, target.bpm());
    let job = match job {
        Ok(job) if browser.tempo_audition.active.is_none() => job,
        Ok(_) => {
            audition.tempo_label = Some(TempoPlaybackLabel::Raw("tempo worker busy"));
            return Some(audition);
        }
        Err(reason) => {
            audition.tempo_label = Some(TempoPlaybackLabel::Raw(reason));
            return Some(audition);
        }
    };

    let deadline = job.deadline;
    let active = Arc::new(AtomicBool::new(false));
    browser.tempo_audition.active = Some(Arc::clone(&active));
    browser.tempo_audition.pending = Some(Pending {
        playback_epoch: browser.playback_epoch,
        waveform_generation: browser.waveform_generation,
        output_open_generation: browser.audio_output_open_generation,
        revision: job.context.file_revision.clone(),
        audition,
        target_bpm: job.target_bpm,
    });
    let token = Arc::clone(&active);
    let worker = cx.background_spawn(async move { prepare(job, &token) });
    let completion_token = Arc::clone(&active);
    cx.spawn(async move |browser, cx| {
        let result = worker.await;
        let _ = browser.update(cx, |browser, cx| {
            complete(browser, &completion_token, Some(result), cx);
        });
    })
    .detach();
    browser.tempo_audition.timeout = Some(cx.spawn(async move |browser, cx| {
        Timer::after(deadline.saturating_duration_since(Instant::now())).await;
        let _ = browser.update(cx, |browser, cx| complete(browser, &active, None, cx));
    }));
    browser.set_playback_status("Preparing tempo audition…");
    cx.notify();
    None
}

fn make_job(
    browser: &Browser,
    audition: &ResolvedAudition,
    target_bpm: f64,
) -> Result<Job, &'static str> {
    if audition.request.metronome.is_some() || audition.request.edit_fade.is_some() {
        return Err("metronome or edit preview");
    }
    if !matches!(
        audition.intent.origin,
        PlaybackIntentOrigin::Explicit | PlaybackIntentOrigin::SelectionAutoplay
    ) {
        return Err("audition mode");
    }
    let context = browser
        .waveform
        .as_ref()
        .and_then(|preview| preview.detail_context.as_ref())
        .filter(|context| context.path == audition.intent.path)
        .ok_or("no verified WAV preview")?;
    let (source_span, loop_start) =
        source_frames(audition.request.mode, context.total_frames as u64).ok_or("empty span")?;
    let frames = source_span.end - source_span.start;
    if frames > u64::from(context.spec.sample_rate) * u64::from(MAX_TEMPO_SPAN_SECONDS)
        || frames.saturating_mul(u64::from(context.spec.channels)) > MAX_TEMPO_INPUT_SAMPLES as u64
    {
        return Err("WAV span exceeds the preparation budget");
    }
    let source_bpm = browser
        .bpm_snap_entry
        .tempo
        .map(SourcePlaymarkGridTempo::bpm)
        .or_else(|| {
            playmark_tempo::authored_value(browser)
                .filter(|tempo| tempo.frame_range() == (source_span.start, source_span.end))
                .map(|tempo| tempo.bpm())
        })
        .ok_or("source BPM unavailable")?;
    let ratio = target_bpm / source_bpm;
    if !(0.5..=2.0).contains(&ratio) {
        return Err("tempo ratio outside 0.5–2");
    }
    if (ratio - 1.0).abs() < 1e-3 {
        return Err("already at target tempo");
    }
    Ok(Job {
        context: context.clone(),
        source_span,
        loop_start,
        source_bpm,
        target_bpm,
        deadline: Instant::now() + PREPARATION_BUDGET,
    })
}

fn source_frames(mode: PlaybackRuntimeMode, total: u64) -> Option<(Range<u64>, u64)> {
    let (start, end, offset) = match mode {
        PlaybackRuntimeMode::OneShot { start, end } => (start, end, start),
        PlaybackRuntimeMode::Looped { start, end, offset } => (start, end, offset),
    };
    if !start.is_finite() || !end.is_finite() || !offset.is_finite() || start >= end {
        return None;
    }
    let frame = |value: f64| (value.clamp(0.0, 1.0) * total as f64).round() as u64;
    let start = frame(start);
    let end = frame(end);
    (start < end).then(|| (start..end, frame(offset).clamp(start, end)))
}

fn prepare(job: Job, cancel: &AtomicBool) -> Result<PlaybackRuntimeTempoSpan, &'static str> {
    with_fenced_wav_read(&job.context, cancel, job.deadline, |file| {
        let span = read_tempo_wav_span(file, job.source_span, cancel, job.deadline)
            .map_err(|error| format!("{error:?}"))?;
        if span.source_frames != job.context.total_frames as u64
            || span.sample_rate != job.context.spec.sample_rate
            || span.channels != usize::from(job.context.spec.channels)
        {
            return Err(String::from("WAV dimensions changed"));
        }
        let prepared = prepare_tempo_audition(
            TempoAuditionTarget::Enabled {
                source_bpm: Some(job.source_bpm),
                target_bpm: Some(job.target_bpm),
            },
            span.source_span,
            &span.samples,
            span.sample_rate,
            span.channels,
            cancel,
            job.deadline,
        )
        .map_err(|reason| format!("{reason:?}"))?;
        PlaybackRuntimeTempoSpan::new(
            job.context.path.clone(),
            prepared,
            span.source_frames,
            job.loop_start,
        )
    })
    .map_err(|_| {
        if Instant::now() >= job.deadline {
            "preparation timed out"
        } else {
            "WAV span could not be prepared"
        }
    })
}

fn complete(
    browser: &mut Browser,
    token: &Arc<AtomicBool>,
    result: Option<Result<PlaybackRuntimeTempoSpan, &'static str>>,
    cx: &mut Context<Browser>,
) {
    let state = &mut browser.tempo_audition;
    if !state
        .active
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(active, token))
    {
        return;
    }
    if result.is_some() {
        state.active = None;
    } else {
        token.store(true, Ordering::Release);
    }
    state.timeout = None;
    let Some(mut pending) = state.pending.take() else {
        return;
    };
    if pending.playback_epoch != browser.playback_epoch
        || pending.waveform_generation != browser.waveform_generation
        || pending.output_open_generation != browser.audio_output_open_generation
        || (pending.audition.intent.origin == PlaybackIntentOrigin::SelectionAutoplay
            && !browser.autoplay_selection)
        || browser
            .waveform
            .as_ref()
            .and_then(|preview| preview.detail_context.as_ref())
            .is_none_or(|context| {
                context.path != pending.audition.intent.path
                    || context.file_revision != pending.revision
            })
    {
        return;
    }
    pending.audition.tempo_label = Some(match result {
        Some(Ok(tempo)) => {
            pending.audition.request.source = PlaybackRuntimeSource::PreparedTempo(tempo);
            TempoPlaybackLabel::Stretched(pending.target_bpm)
        }
        Some(Err(reason)) => TempoPlaybackLabel::Raw(reason),
        None => TempoPlaybackLabel::Raw("preparation timed out"),
    });
    browser.submit_resolved_audition(pending.audition, cx);
}

#[cfg(test)]
mod tests;
