use std::path::Path;
use std::time::Duration;

use yield_aiff::AiffPcmReader;

use crate::Source;
use crate::player::PlaybackSpanPlan;
use crate::player::state::open_aiff_file_for_playback;
use crate::timebase::duration_for_frames;

pub(in crate::player::playback) struct AiffSpanSource {
    reader: AiffPcmReader,
    start_frame: u64,
    span_samples: u64,
    remaining_samples: u64,
    repeating: bool,
    last_error: Option<String>,
}

impl AiffSpanSource {
    pub(in crate::player::playback) fn try_new(
        path: &Path,
        plan: &PlaybackSpanPlan,
        repeating: bool,
    ) -> Result<Self, String> {
        let mut reader = open_aiff_file_for_playback(path)?;
        if reader.sample_rate() != plan.layout().sample_rate()
            || reader.channels() != plan.layout().channels()
            || reader.frames() != plan.track_frames()
        {
            return Err(format!(
                "AIFF playback layout changed for {}",
                path.display()
            ));
        }
        let offset_frames = plan.seek_offset_frames();
        reader
            .seek_frame(plan.start_frame() + offset_frames)
            .map_err(|error| error.to_string())?;
        let span_samples = plan.sample_count();
        Ok(Self {
            reader,
            start_frame: plan.start_frame(),
            span_samples,
            remaining_samples: span_samples - offset_frames * u64::from(plan.layout().channels()),
            repeating,
            last_error: None,
        })
    }
}

impl Iterator for AiffSpanSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.last_error.is_some() {
            return None;
        }
        if self.remaining_samples == 0 {
            if !self.repeating {
                return None;
            }
            if let Err(error) = self.reader.seek_frame(self.start_frame) {
                self.last_error = Some(error.to_string());
                return None;
            }
            self.remaining_samples = self.span_samples;
        }
        match self.reader.read_sample() {
            Ok(Some(sample)) => {
                self.remaining_samples -= 1;
                Some(sample)
            }
            Ok(None) => {
                self.last_error = Some(String::from(
                    "AIFF playback ended before the requested frame",
                ));
                None
            }
            Err(error) => {
                self.last_error = Some(error.to_string());
                None
            }
        }
    }
}

impl Source for AiffSpanSource {
    fn current_frame_len(&self) -> Option<usize> {
        if self.repeating {
            None
        } else if self.last_error.is_some() {
            Some(0)
        } else {
            usize::try_from(self.remaining_samples).ok()
        }
    }

    fn channels(&self) -> u16 {
        self.reader.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.reader.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        (!self.repeating)
            .then(|| duration_for_frames(self.reader.frames(), self.reader.sample_rate()))
    }

    fn last_error(&self) -> Option<String> {
        self.last_error.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::playback::repeating_source_for_audio_source;
    use crate::player::{
        AudioPlaybackSource, PlaybackChannelLayout, PlaybackRuntimeSource, PlaybackSeekBehavior,
        PlaybackSourceIdentity, PlaybackSourceKind, PlaybackSpanRequest,
    };

    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../test-fixtures/stereo-pcm16.aiff"
    ));

    #[test]
    fn aiff_file_dispatch_streams_exact_offset_and_wraps_stereo_frames() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clip.AIF");
        std::fs::write(&path, FIXTURE).unwrap();
        assert!(matches!(
            PlaybackRuntimeSource::probe_file(path.clone()),
            PlaybackRuntimeSource::AiffFile { path: source_path } if source_path == path
        ));
        let duration = 2.0 / 48_000.0;
        let plan = PlaybackSpanPlan::new(
            PlaybackSourceIdentity::new(PlaybackSourceKind::File, None),
            PlaybackChannelLayout::new(2, 48_000).unwrap(),
            PlaybackSpanRequest::new(
                0.0,
                duration,
                duration,
                true,
                PlaybackSeekBehavior::FrameOffset(1),
            )
            .with_exact_track_frames(2),
        )
        .unwrap();
        let source =
            repeating_source_for_audio_source(AudioPlaybackSource::File(path), &plan).unwrap();
        assert_eq!(source.sample_rate(), 48_000);
        assert_eq!(source.channels(), 2);
        assert_eq!(
            source.take(6).collect::<Vec<_>>(),
            [0.0, -0.5, -1.0, 0.5, 0.0, -0.5]
        );
    }

    #[test]
    fn aiff_file_probe_rejects_invalid_and_excessive_sample_rates() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("clip.aiff");
        let mut fixture = FIXTURE.to_vec();
        fixture[28..38].fill(0);
        std::fs::write(&path, &fixture).unwrap();
        assert!(
            open_aiff_file_for_playback(&path)
                .err()
                .unwrap()
                .contains("invalid AIFF sample rate")
        );

        fixture[28..38].copy_from_slice(&[0x40, 0x12, 0xc0, 0, 0, 0, 0, 0, 0, 0]);
        std::fs::write(&path, &fixture).unwrap();
        assert!(
            open_aiff_file_for_playback(&path)
                .err()
                .unwrap()
                .contains("exceeds playback limits")
        );
    }
}
