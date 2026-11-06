#![deny(missing_docs)]
#![deny(warnings)]

//! Duration and tempo labels for waveform playmarks.

use crate::WaveformRegion;

/// Maximum number of Unicode characters accepted by the playmark length editor.
pub const MAX_PLAYMARK_TIMING_CHARACTERS: usize = 64;

const PLAYMARK_LENGTH_USAGE: &str = "Use a value such as 1000ms, 1.5s, 1min, or 142bpm";

/// Format the tempo implied by a positive beat count and exact sample-frame span.
pub fn playmark_frame_tempo_label(frames: usize, sample_rate: u32, beats: u8) -> Option<String> {
    if frames == 0 || sample_rate == 0 || beats == 0 {
        return None;
    }
    let bpm = f64::from(beats) * 60.0 * f64::from(sample_rate) / frames as f64;
    if bpm < 0.01 {
        return Some(format!("{bpm} BPM"));
    }
    format_waveform_bpm_input(bpm).map(|bpm| format!("{bpm} BPM"))
}

/// Format a playmark's duration, or its beat-guide tempo when beat_count is enabled.
///
/// The duration calculation intentionally uses f32 arithmetic to match the native waveform
/// label. A zero beat count falls back to the duration label, just as the native widget does.
/// Invalid or empty regions return None.
pub fn playmark_selection_label(
    region: WaveformRegion,
    total_frames: usize,
    sample_rate: u32,
    beat_count: Option<u8>,
) -> Option<String> {
    let duration_seconds = region.width() as f32 * total_frames as f32 / sample_rate.max(1) as f32;
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return None;
    }

    if let Some(beat_count) = beat_count.filter(|count| *count > 0) {
        let bpm = f32::from(beat_count) * 60.0 / duration_seconds;
        return format_waveform_bpm_input(f64::from(bpm)).map(|bpm| format!("{bpm} BPM"));
    }

    Some(format_selection_duration(duration_seconds))
}

/// A playmark timing edit resolved to source frames.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedPlaymarkTiming {
    /// First frame and exclusive end frame.
    pub frame_range: (usize, usize),
    /// Whether the user entered a tempo in BPM.
    pub authored_tempo: bool,
}

/// Parse a playmark duration or tempo and return frame bounds anchored at the current start edge.
///
/// Supported inputs are milliseconds (ms), seconds (s), minutes (m or min), compound values such
/// as 2m 5s, and beats-per-minute (bpm) when beat_count is positive. Whitespace and letter case
/// are ignored. The requested duration is rounded to the nearest frame, while the end edge is
/// clamped to total_frames.
pub fn parse_playmark_frame_range(
    text: &str,
    sample_rate: u32,
    total_frames: usize,
    beat_count: u8,
    current: WaveformRegion,
) -> Result<ParsedPlaymarkTiming, String> {
    if text.chars().nth(MAX_PLAYMARK_TIMING_CHARACTERS).is_some() {
        return Err(format!(
            "Playmark length must be {MAX_PLAYMARK_TIMING_CHARACTERS} characters or fewer"
        ));
    }
    if total_frames == 0 {
        return Err(String::from("The loaded sample has no audio frames"));
    }

    let (seconds, authored_tempo) = parse_length_seconds(text, beat_count)?;
    let requested_frames = (seconds * f64::from(sample_rate.max(1))).round();
    if !requested_frames.is_finite() || requested_frames < 1.0 {
        return Err(String::from(
            "Playmark length must be at least one sample frame",
        ));
    }
    let requested_frames = requested_frames as usize;
    let (start, _) = normalized_frame_bounds(current, total_frames);
    let end = start.saturating_add(requested_frames).min(total_frames);
    Ok(ParsedPlaymarkTiming {
        frame_range: (start, end),
        authored_tempo,
    })
}

fn parse_length_seconds(text: &str, beat_count: u8) -> Result<(f64, bool), String> {
    let compact = text
        .chars()
        .filter(|character| !character.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if let Some(number) = compact.strip_suffix("bpm") {
        let bpm = positive_number(number)?;
        if beat_count == 0 {
            return Err(String::from(
                "Set a positive beat count before entering BPM",
            ));
        }
        return Ok((f64::from(beat_count) * 60.0 / bpm, true));
    }

    let mut remaining = compact.as_str();
    let mut seconds = 0.0;
    let mut components = 0;
    while !remaining.is_empty() {
        let number_end = remaining
            .char_indices()
            .take_while(|(_, character)| character.is_ascii_digit() || *character == '.')
            .map(|(index, character)| index + character.len_utf8())
            .last()
            .unwrap_or(0);
        if number_end == 0 {
            return Err(String::from(PLAYMARK_LENGTH_USAGE));
        }
        let number = non_negative_number(&remaining[..number_end])?;
        remaining = &remaining[number_end..];
        let (factor, unit_len) = if remaining.starts_with("min") {
            (60.0, 3)
        } else if remaining.starts_with("ms") {
            (0.001, 2)
        } else if remaining.starts_with('m') {
            (60.0, 1)
        } else if remaining.starts_with('s') {
            (1.0, 1)
        } else {
            return Err(String::from(PLAYMARK_LENGTH_USAGE));
        };
        seconds += number * factor;
        components += 1;
        remaining = &remaining[unit_len..];
    }
    if components == 0 || !seconds.is_finite() || seconds <= 0.0 {
        return Err(String::from("Playmark length must be positive and finite"));
    }
    Ok((seconds, false))
}

fn positive_number(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| String::from("Playmark length must be positive and finite"))
}

fn non_negative_number(text: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| String::from("Playmark length must be positive and finite"))
}

fn normalized_frame_bounds(region: WaveformRegion, total_frames: usize) -> (usize, usize) {
    let total = total_frames as f64;
    let start = normalized_frame(
        region.start() * total,
        total_frames.saturating_sub(1),
        false,
    );
    let end = normalized_frame(region.end() * total, total_frames, true);
    (start, end.max(start.saturating_add(1)).min(total_frames))
}

fn normalized_frame(value: f64, max_frame: usize, ceil: bool) -> usize {
    if !value.is_finite() {
        return 0;
    }
    let value = if ceil { value.ceil() } else { value.floor() };
    if value <= 0.0 {
        0
    } else if value >= max_frame as f64 {
        max_frame
    } else {
        value as usize
    }
}

fn format_selection_duration(seconds: f32) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return String::from("0 ms");
    }
    if seconds < 1.0 {
        return format!("{:.0} ms", seconds * 1_000.0);
    }
    if seconds < 60.0 {
        return format!("{seconds:.2} s");
    }
    let minutes = (seconds / 60.0).floor() as u32;
    let remaining = seconds - minutes as f32 * 60.0;
    format!("{minutes}m {remaining:05.2}s")
}

fn format_waveform_bpm_input(value: f64) -> Option<String> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let rounded = value.round();
    if (value - rounded).abs() < 0.01 {
        Some(format!("{rounded:.0}"))
    } else {
        Some(format!("{value:.2}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(start: f64, end: f64) -> WaveformRegion {
        WaveformRegion::new(start, end).expect("non-empty region")
    }

    #[test]
    fn frame_tempo_formats_precisely_and_rejects_empty_inputs() {
        assert_eq!(
            playmark_frame_tempo_label(96_000, 48_000, 4).as_deref(),
            Some("120 BPM")
        );
        assert_eq!(
            playmark_frame_tempo_label(192_000, 48_000, 8).as_deref(),
            Some("120 BPM")
        );
        assert_eq!(
            playmark_frame_tempo_label(80_640, 48_000, 4).as_deref(),
            Some("142.86 BPM")
        );
        assert_eq!(playmark_frame_tempo_label(0, 48_000, 4), None);
        assert_eq!(playmark_frame_tempo_label(1, 0, 4), None);
        assert_eq!(playmark_frame_tempo_label(1, 48_000, 0), None);
    }

    #[test]
    fn labels_use_native_duration_units() {
        assert_eq!(
            playmark_selection_label(region(0.0, 0.75), 1_000, 1_000, None),
            Some(String::from("750 ms"))
        );
        assert_eq!(
            playmark_selection_label(region(0.0, 0.75), 2_000, 1_000, None),
            Some(String::from("1.50 s"))
        );
        assert_eq!(
            playmark_selection_label(region(0.0, 0.5), 250_000, 1_000, None),
            Some(String::from("2m 05.00s"))
        );
    }

    #[test]
    fn labels_use_bpm_only_for_a_positive_beat_count() {
        assert_eq!(
            playmark_selection_label(region(0.0, 0.5), 2_000, 1_000, Some(4)),
            Some(String::from("240 BPM"))
        );
        assert_eq!(
            playmark_selection_label(region(0.0, 0.5), 2_000, 1_000, Some(0)),
            Some(String::from("1.00 s"))
        );
    }

    #[test]
    fn labels_reject_empty_audio() {
        assert_eq!(
            playmark_selection_label(region(0.0, 0.5), 0, 1_000, None),
            None
        );
    }

    #[test]
    fn parses_supported_time_and_bpm_forms_case_insensitively() {
        assert_eq!(parse_length_seconds(" 1000 MS ", 4), Ok((1.0, false)));
        assert_eq!(parse_length_seconds("1.5s", 4), Ok((1.5, false)));
        assert_eq!(parse_length_seconds("1 MIN", 4), Ok((60.0, false)));
        assert_eq!(parse_length_seconds("2m 05.00s", 4), Ok((125.0, false)));
        assert_eq!(parse_length_seconds("120 bpm", 4), Ok((2.0, true)));
    }

    #[test]
    fn parses_duration_from_the_floored_current_start_and_clamps_eof() {
        assert_eq!(
            parse_playmark_frame_range("0.4s", 1_000, 1_000, 4, region(0.2505, 0.95)),
            Ok(ParsedPlaymarkTiming {
                frame_range: (250, 650),
                authored_tempo: false,
            })
        );
        assert_eq!(
            parse_playmark_frame_range("2s", 1_000, 1_000, 4, region(0.85, 0.95)),
            Ok(ParsedPlaymarkTiming {
                frame_range: (850, 1_000),
                authored_tempo: false,
            })
        );
    }

    #[test]
    fn bpm_duration_rounds_to_nearest_frame() {
        assert_eq!(
            parse_playmark_frame_range(" 140 BpM ", 1_000, 2_000, 4, region(0.1, 0.2)),
            Ok(ParsedPlaymarkTiming {
                frame_range: (200, 1_914),
                authored_tempo: true,
            })
        );
    }

    #[test]
    fn rejects_invalid_values_with_native_errors() {
        let current = region(0.2, 0.4);
        assert_eq!(
            parse_playmark_frame_range("0s", 1_000, 1_000, 4, current),
            Err(String::from("Playmark length must be positive and finite"))
        );
        assert_eq!(
            parse_playmark_frame_range("1s", 1_000, 0, 4, current),
            Err(String::from("The loaded sample has no audio frames"))
        );
        assert_eq!(
            parse_playmark_frame_range("120bpm", 1_000, 1_000, 0, current),
            Err(String::from(
                "Set a positive beat count before entering BPM"
            ))
        );
        assert_eq!(
            parse_playmark_frame_range("fast", 1_000, 1_000, 4, current),
            Err(String::from(PLAYMARK_LENGTH_USAGE))
        );
    }

    #[test]
    fn rejects_input_longer_than_editor_limit_before_parsing() {
        let text = "1".repeat(MAX_PLAYMARK_TIMING_CHARACTERS + 1);
        assert_eq!(
            parse_playmark_frame_range(&text, 1_000, 1_000, 4, region(0.2, 0.4)),
            Err(format!(
                "Playmark length must be {MAX_PLAYMARK_TIMING_CHARACTERS} characters or fewer"
            ))
        );
    }

    #[test]
    fn empty_input_is_rejected_and_huge_duration_clamps_to_eof() {
        let current = region(0.2, 0.4);
        assert_eq!(
            parse_playmark_frame_range("", 1_000, 1_000, 4, current),
            Err(String::from("Playmark length must be positive and finite"))
        );
        assert_eq!(
            parse_playmark_frame_range(
                "999999999999999999999999999999999999999999s",
                1_000,
                1_000,
                4,
                current
            ),
            Ok(ParsedPlaymarkTiming {
                frame_range: (200, 1_000),
                authored_tempo: false,
            })
        );
    }
}
