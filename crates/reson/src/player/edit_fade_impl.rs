use crate::Source;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MIN_MUTE_FADE_SECS: f32 = 0.002;

/// Parameters for one realtime playback fade curve.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FadeParams {
    /// Fade length as a fraction of the faded span.
    pub length: f32,
    /// Curve tension: 0.0 is linear and 1.0 is the strongest S-curve.
    pub curve: f32,
    /// Outer mute/fade extension as a fraction of the faded span.
    pub mute: f32,
    /// Gain at the outer edge of the extension.
    pub outer_gain: f32,
}

impl FadeParams {
    /// Create fade parameters with explicit length, curve, and extension.
    pub fn new(length: f32, curve: f32, mute: f32) -> Self {
        Self::with_outer_gain(length, curve, mute, 1.0)
    }

    /// Create fade parameters with explicit length, curve, extension, and outer gain.
    pub fn with_outer_gain(length: f32, curve: f32, mute: f32, outer_gain: f32) -> Self {
        Self {
            length: length.clamp(0.0, 1.0),
            curve: curve.clamp(0.0, 1.0),
            mute: mute.max(0.0),
            outer_gain: outer_gain.clamp(0.0, 1.0),
        }
    }
}

/// Normalized edit-fade range for realtime playback gain automation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditFadeRange {
    /// Normalized range start.
    pub start: f32,
    /// Normalized range end.
    pub end: f32,
    /// Gain applied inside the range.
    pub gain: f32,
    /// Fade-in shape.
    pub fade_in: Option<FadeParams>,
    /// Fade-out shape.
    pub fade_out: Option<FadeParams>,
}

impl EditFadeRange {
    /// Create a normalized range with optional fade and gain parameters.
    pub fn new(
        start: f32,
        end: f32,
        gain: f32,
        fade_in: Option<FadeParams>,
        fade_out: Option<FadeParams>,
    ) -> Self {
        let start = start.clamp(0.0, 1.0);
        let end = end.clamp(0.0, 1.0);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };
        Self {
            start,
            end,
            gain: if gain.is_finite() { gain.max(0.0) } else { 1.0 },
            fade_in,
            fade_out,
        }
    }
}

/// Snapshot of the current edit fade configuration.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct EditFadeSnapshot {
    pub active: bool,
    /// Absolute start time of the selection in seconds
    pub start_seconds: f32,
    /// Absolute end time of the selection in seconds
    pub end_seconds: f32,
    /// Gain applied across the selection.
    pub gain: f32,
    pub fade_in: Option<FadeParams>,
    pub fade_out: Option<FadeParams>,
}

#[derive(Debug)]
struct EditFadeShared {
    writer: Mutex<()>,
    published: AtomicU64,
    slots: [EditFadeSlot; 2],
}

#[derive(Debug)]
struct EditFadeSlot {
    active: AtomicU32,
    start_seconds: AtomicU32,
    end_seconds: AtomicU32,
    gain: AtomicU32,
    fade_in: AtomicFadeParams,
    fade_out: AtomicFadeParams,
}

#[derive(Debug)]
struct AtomicFadeParams {
    present: AtomicU32,
    length: AtomicU32,
    curve: AtomicU32,
    mute: AtomicU32,
    outer_gain: AtomicU32,
}

impl Default for AtomicFadeParams {
    fn default() -> Self {
        Self {
            present: AtomicU32::new(0),
            length: AtomicU32::new(0.0_f32.to_bits()),
            curve: AtomicU32::new(0.0_f32.to_bits()),
            mute: AtomicU32::new(0.0_f32.to_bits()),
            outer_gain: AtomicU32::new(0.0_f32.to_bits()),
        }
    }
}

impl AtomicFadeParams {
    fn store(&self, params: Option<FadeParams>) {
        if let Some(params) = params {
            self.length.store(params.length.to_bits(), Ordering::SeqCst);
            self.curve.store(params.curve.to_bits(), Ordering::SeqCst);
            self.mute.store(params.mute.to_bits(), Ordering::SeqCst);
            self.outer_gain
                .store(params.outer_gain.to_bits(), Ordering::SeqCst);
            self.present.store(1, Ordering::SeqCst);
        } else {
            self.present.store(0, Ordering::SeqCst);
        }
    }

    fn load(&self) -> Option<FadeParams> {
        (self.present.load(Ordering::SeqCst) != 0).then(|| FadeParams {
            length: f32::from_bits(self.length.load(Ordering::SeqCst)),
            curve: f32::from_bits(self.curve.load(Ordering::SeqCst)),
            mute: f32::from_bits(self.mute.load(Ordering::SeqCst)),
            outer_gain: f32::from_bits(self.outer_gain.load(Ordering::SeqCst)),
        })
    }
}

impl Default for EditFadeSlot {
    fn default() -> Self {
        Self {
            active: AtomicU32::new(0),
            start_seconds: AtomicU32::new(0.0_f32.to_bits()),
            end_seconds: AtomicU32::new(0.0_f32.to_bits()),
            gain: AtomicU32::new(1.0_f32.to_bits()),
            fade_in: AtomicFadeParams::default(),
            fade_out: AtomicFadeParams::default(),
        }
    }
}

impl EditFadeSlot {
    fn store(&self, snapshot: EditFadeSnapshot) {
        self.active
            .store(u32::from(snapshot.active), Ordering::SeqCst);
        self.start_seconds
            .store(snapshot.start_seconds.to_bits(), Ordering::SeqCst);
        self.end_seconds
            .store(snapshot.end_seconds.to_bits(), Ordering::SeqCst);
        self.gain.store(snapshot.gain.to_bits(), Ordering::SeqCst);
        self.fade_in.store(snapshot.fade_in);
        self.fade_out.store(snapshot.fade_out);
    }

    fn load(&self) -> EditFadeSnapshot {
        EditFadeSnapshot {
            active: self.active.load(Ordering::SeqCst) != 0,
            start_seconds: f32::from_bits(self.start_seconds.load(Ordering::SeqCst)),
            end_seconds: f32::from_bits(self.end_seconds.load(Ordering::SeqCst)),
            gain: f32::from_bits(self.gain.load(Ordering::SeqCst)),
            fade_in: self.fade_in.load(),
            fade_out: self.fade_out.load(),
        }
    }
}

impl Default for EditFadeShared {
    fn default() -> Self {
        Self {
            writer: Mutex::new(()),
            published: AtomicU64::new(2),
            slots: [EditFadeSlot::default(), EditFadeSlot::default()],
        }
    }
}

impl EditFadeShared {
    fn try_snapshot(&self) -> Option<(u64, EditFadeSnapshot)> {
        let before = self.published.load(Ordering::SeqCst);
        let slot_index = (before & 1) as usize;
        let snapshot = self.slots[slot_index].load();
        let after = self.published.load(Ordering::SeqCst);
        (before == after).then_some((before >> 1, snapshot))
    }

    fn publish(&self, snapshot: EditFadeSnapshot) {
        let current = self.published.load(Ordering::SeqCst);
        let generation = (current >> 1).wrapping_add(1);
        let slot_index = ((current & 1) ^ 1) as usize;
        self.slots[slot_index].store(snapshot);
        self.published.store(
            generation.wrapping_mul(2) | slot_index as u64,
            Ordering::SeqCst,
        );
    }
}

#[derive(Clone, Debug)]
pub(crate) struct EditFadeHandle {
    shared: Arc<EditFadeShared>,
}

impl EditFadeHandle {
    pub(crate) fn new() -> Self {
        Self {
            shared: Arc::new(EditFadeShared::default()),
        }
    }

    pub(crate) fn update(&self, range: Option<EditFadeRange>, total_duration_secs: f32) {
        let _writer = self
            .shared
            .writer
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut snapshot = self
            .shared
            .try_snapshot()
            .map_or_else(EditFadeSnapshot::default, |(_, snapshot)| snapshot);
        if let Some(range) = range {
            snapshot.active = true;
            snapshot.start_seconds = range.start * total_duration_secs;
            snapshot.end_seconds = range.end * total_duration_secs;
            snapshot.gain = range.gain;
            snapshot.fade_in = range.fade_in;
            snapshot.fade_out = range.fade_out;
        } else {
            snapshot.active = false;
            snapshot.gain = 1.0;
            snapshot.fade_in = None;
            snapshot.fade_out = None;
        }
        self.shared.publish(snapshot);
    }

    /// Return the current edit-fade snapshot version.
    pub(crate) fn version(&self) -> u64 {
        self.shared.published.load(Ordering::SeqCst) >> 1
    }

    /// Return a copy of the latest edit-fade snapshot.
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> EditFadeSnapshot {
        loop {
            if let Some((_, snapshot)) = self.shared.try_snapshot() {
                return snapshot;
            }
            std::hint::spin_loop();
        }
    }

    fn snapshot_with_version(&self) -> Option<(u64, EditFadeSnapshot)> {
        self.shared.try_snapshot()
    }
}

/// A Source that applies fades based on the live EditFadeState.
#[derive(Clone)]
pub(crate) struct EditFadeSource<S> {
    inner: S,
    handle: EditFadeHandle,
    /// The global timestamp (relative to track start) where this source segment begins.
    global_start_secs: f32,
    /// Optional number of frames in a loop, used to repeat fades each cycle.
    loop_frames: Option<u64>,
    /// Initial frame offset applied before looping.
    frame_offset: u64,
    sample_rate: u32,
    channels: u16,
    samples_emitted: u64,
    cached_state: EditFadeSnapshot,
    cached_version: u64,
}

impl<S> EditFadeSource<S>
where
    S: Source,
{
    pub(crate) fn new_with_frame_offset(
        inner: S,
        handle: EditFadeHandle,
        global_start_secs: f32,
        frame_offset: u64,
    ) -> Self {
        Self::with_loop(inner, handle, global_start_secs, None, frame_offset)
    }

    /// Create a source that repeats edit fades for each loop iteration.
    pub(crate) fn new_looped(
        inner: S,
        handle: EditFadeHandle,
        global_start_secs: f32,
        loop_frames: u64,
        frame_offset: u64,
    ) -> Self {
        let loop_frames = if loop_frames > 0 {
            Some(loop_frames)
        } else {
            None
        };
        Self::with_loop(inner, handle, global_start_secs, loop_frames, frame_offset)
    }

    fn with_loop(
        inner: S,
        handle: EditFadeHandle,
        global_start_secs: f32,
        loop_frames: Option<u64>,
        frame_offset: u64,
    ) -> Self {
        let sample_rate = inner.sample_rate();
        let channels = inner.channels();
        let (cached_version, cached_state) = loop {
            if let Some(snapshot) = handle.snapshot_with_version() {
                break snapshot;
            }
            // Construction happens off the audio callback. If a control writer
            // is publishing the alternate slot, retry rather than recording a
            // newer version alongside an older default snapshot.
            std::hint::spin_loop();
        };
        Self {
            inner,
            handle,
            global_start_secs,
            loop_frames,
            frame_offset,
            sample_rate,
            channels,
            samples_emitted: 0,
            cached_state,
            cached_version,
        }
    }

    fn refresh_state_if_needed(&mut self) {
        let version = self.handle.version();
        if version == self.cached_version {
            return;
        }
        if let Some((snapshot_version, snapshot)) = self.handle.snapshot_with_version() {
            self.cached_state = snapshot;
            self.cached_version = snapshot_version;
        }
    }
}

impl<S> Iterator for EditFadeSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        let sample = self.inner.next()?;

        self.refresh_state_if_needed();
        let state = self.cached_state;

        if !state.active {
            self.samples_emitted += 1;
            return Some(sample);
        }

        let frame_index = self.samples_emitted / self.channels.max(1) as u64;
        let frame_index = frame_index.saturating_add(self.frame_offset);
        let frame_index = if let Some(loop_frames) = self.loop_frames {
            frame_index % loop_frames.max(1)
        } else {
            frame_index
        };
        let local_time = if self.sample_rate > 0 {
            frame_index as f32 / self.sample_rate as f32
        } else {
            0.0
        };
        let current_time = self.global_start_secs + local_time;

        self.samples_emitted += 1;

        let gain = fade_gain_at_position(
            current_time,
            state.start_seconds,
            state.end_seconds,
            state.gain,
            state.fade_in,
            state.fade_out,
            MIN_MUTE_FADE_SECS,
        );
        Some(sample * gain)
    }
}

fn fade_gain_at_position(
    position: f32,
    selection_start: f32,
    selection_end: f32,
    selection_gain: f32,
    fade_in: Option<FadeParams>,
    fade_out: Option<FadeParams>,
    min_fade_len: f32,
) -> f32 {
    let start = selection_start.min(selection_end);
    let end = selection_start.max(selection_end);
    let width = end - start;
    if width <= 0.0 {
        return 1.0;
    }
    if let Some(gain) = extension_gain(position, start, end, width, fade_in, true) {
        return gain;
    }
    if let Some(gain) = extension_gain(position, start, end, width, fade_out, false) {
        return gain;
    }
    if position < start || position > end {
        return 1.0;
    }

    let mut gain = 1.0;
    gain *= inner_fade_gain(position - start, width, fade_in, min_fade_len);
    gain *= inner_fade_gain(end - position, width, fade_out, min_fade_len);
    gain * selection_gain.clamp(0.0, 4.0)
}

fn extension_gain(
    position: f32,
    start: f32,
    end: f32,
    width: f32,
    fade: Option<FadeParams>,
    fade_in: bool,
) -> Option<f32> {
    let fade = fade?;
    let extension_len = (width * fade.mute).max(0.0);
    if extension_len <= 0.0 {
        return None;
    }
    let t = if fade_in {
        normalized_t(position, start - extension_len, start)?
    } else {
        normalized_t(position, end, end + extension_len)?
    };
    let extension_gain = if fade_in {
        1.0 - fade_curve_value(t, fade.curve)
    } else {
        fade_curve_value(t, fade.curve)
    };
    Some((fade.outer_gain * extension_gain).clamp(0.0, 1.0))
}

fn inner_fade_gain(
    distance_from_edge: f32,
    width: f32,
    fade: Option<FadeParams>,
    min_len: f32,
) -> f32 {
    let Some(fade) = fade else {
        return 1.0;
    };
    let fade_len = (width * fade.length).max(min_len).min(width);
    if fade_len <= 0.0 || distance_from_edge >= fade_len {
        return 1.0;
    }
    fade_curve_value(distance_from_edge / fade_len, fade.curve)
}

fn normalized_t(position: f32, start: f32, end: f32) -> Option<f32> {
    if position < start || position > end || end <= start {
        return None;
    }
    Some(((position - start) / (end - start)).clamp(0.0, 1.0))
}

fn fade_curve_value(t: f32, curve: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let curve = curve.clamp(0.0, 1.0);
    let smooth = t * t * (3.0 - 2.0 * t);
    (t * (1.0 - curve) + smooth * curve).clamp(0.0, 1.0)
}

impl<S> Source for EditFadeSource<S>
where
    S: Source<Item = f32>,
{
    #[inline]
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }

    #[inline]
    fn channels(&self) -> u16 {
        self.inner.channels()
    }

    #[inline]
    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn last_error(&self) -> Option<String> {
        self.inner.last_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn outer_gain_scales_realtime_extension_fades() {
        let fade_in = FadeParams::with_outer_gain(0.25, 0.0, 0.25, 0.5);
        let fade_out = FadeParams::with_outer_gain(0.25, 0.0, 0.25, 0.25);

        let left_outer_start =
            fade_gain_at_position(0.1, 0.2, 0.6, 1.0, Some(fade_in), Some(fade_out), 0.0);
        let left_selection_edge =
            fade_gain_at_position(0.2, 0.2, 0.6, 1.0, Some(fade_in), Some(fade_out), 0.0);
        let right_selection_edge =
            fade_gain_at_position(0.6, 0.2, 0.6, 1.0, Some(fade_in), Some(fade_out), 0.0);
        let right_outer_end =
            fade_gain_at_position(0.7, 0.2, 0.6, 1.0, Some(fade_in), Some(fade_out), 0.0);

        assert!((left_outer_start - 0.5).abs() < 1e-6);
        assert!(left_selection_edge.abs() < 1e-6);
        assert!(right_selection_edge.abs() < 1e-6);
        assert!((right_outer_end - 0.25).abs() < 1e-6);
    }

    #[test]
    fn atomic_edit_fade_mailbox_publishes_coherent_snapshots() {
        let handle = EditFadeHandle::new();
        let writer = handle.clone();
        let update_thread = thread::spawn(move || {
            for index in 0..20_000 {
                let start = index as f32 / 20_000.0;
                writer.update(
                    Some(EditFadeRange::new(
                        start,
                        start + 0.25,
                        start + 1.0,
                        Some(FadeParams::new(0.1, 0.4, 0.2)),
                        None,
                    )),
                    4.0,
                );
            }
        });

        for _ in 0..20_000 {
            let snapshot = handle.snapshot();
            if snapshot.active {
                assert!((snapshot.end_seconds - snapshot.start_seconds - 1.0).abs() < 1e-5);
                assert!((snapshot.gain - (snapshot.start_seconds / 4.0 + 1.0)).abs() < 1e-5);
                assert_eq!(snapshot.fade_in, Some(FadeParams::new(0.1, 0.4, 0.2)));
                assert_eq!(snapshot.fade_out, None);
            }
        }
        update_thread.join().unwrap();
    }
}
