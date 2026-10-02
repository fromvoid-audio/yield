//! Private decision fixtures only: model gate observations never grant filesystem authority.
use std::io::{self, Cursor, Seek, SeekFrom, Write};

#[derive(Debug, Eq, PartialEq)]
enum BoundsError {
    Format,
    Frames,
    Size,
}

fn bounded_plan(rate: u32, channels: u16, frames: u64) -> Result<(usize, u64), BoundsError> {
    if !(1..=192_000).contains(&rate) || !(1..=8).contains(&channels) {
        return Err(BoundsError::Format);
    }
    if frames == 0 {
        return Err(BoundsError::Frames);
    }
    let bytes = frames
        .checked_mul(u64::from(channels))
        .and_then(|v| v.checked_mul(4))
        .and_then(|v| v.checked_add(128))
        .filter(|v| *v <= u64::from(u32::MAX))
        .ok_or(BoundsError::Size)?;
    let ring = usize::try_from(rate)
        .ok()
        .and_then(|v| v.checked_mul(usize::from(channels)))
        .and_then(|v| v.checked_mul(2))
        .filter(|v| *v <= 3_072_000)
        .ok_or(BoundsError::Size)?;
    Ok((ring, bytes))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GateResult {
    Stale,
    Cancelled,
    Unsupported,
    Collision,
    MayRequestOwnedStaging,
}

// This boolean matrix specifies ordering only; it cannot register a root or mint a live grant.
fn proposed_gate(current: bool, cancelled: bool, backend: bool, absent: bool) -> GateResult {
    if !current {
        GateResult::Stale
    } else if cancelled {
        GateResult::Cancelled
    } else if !backend {
        GateResult::Unsupported
    } else if !absent {
        GateResult::Collision
    } else {
        GateResult::MayRequestOwnedStaging
    }
}

#[test]
fn recording_sink_contract_rejects_unbounded_format_ring_and_riff_plan() {
    assert_eq!(bounded_plan(192_000, 8, 1), Ok((3_072_000, 160)));
    for (rate, channels) in [(0, 2), (192_001, 1), (48_000, 0), (48_000, 9)] {
        assert_eq!(bounded_plan(rate, channels, 1), Err(BoundsError::Format));
    }
    assert_eq!(bounded_plan(48_000, 2, 0), Err(BoundsError::Frames));
    assert_eq!(bounded_plan(48_000, 2, u64::MAX), Err(BoundsError::Size));
    let max = (u64::from(u32::MAX) - 128) / 8;
    assert!(bounded_plan(48_000, 2, max).is_ok());
    assert_eq!(bounded_plan(48_000, 2, max + 1), Err(BoundsError::Size));
}

#[test]
fn recording_sink_contract_pre_effect_rejections_do_not_invoke_a_sink_factory() {
    for (current, cancelled, backend, absent, expected) in [
        (false, false, true, true, GateResult::Stale),
        (true, true, true, true, GateResult::Cancelled),
        (true, false, false, true, GateResult::Unsupported),
        (true, false, true, false, GateResult::Collision),
    ] {
        let result = proposed_gate(current, cancelled, backend, absent);
        assert_eq!(result, expected);
        let mut factory_calls = 0;
        if result == GateResult::MayRequestOwnedStaging {
            factory_calls += 1;
        }
        assert_eq!(factory_calls, 0);
    }
    assert_eq!(
        proposed_gate(true, false, true, true),
        GateResult::MayRequestOwnedStaging
    );
}

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

struct FailingSink {
    bytes: Cursor<Vec<u8>>,
    // Shared switches permit faults after successful header creation while hound owns the sink.
    fault: Arc<AtomicU8>,
}
impl Write for FailingSink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fault.load(Ordering::Relaxed) == 1 {
            Err(io::Error::other("sink write unavailable"))
        } else {
            self.bytes.write(bytes)
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.fault.load(Ordering::Relaxed) == 3 {
            Err(io::Error::other("sink flush unconfirmed"))
        } else {
            Ok(())
        }
    }
}
impl Seek for FailingSink {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.fault.load(Ordering::Relaxed) == 2 {
            Err(io::Error::other("sink seek unavailable"))
        } else {
            self.bytes.seek(position)
        }
    }
}
fn spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: 2,
        sample_rate: 48_000,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    }
}
fn sink() -> FailingSink {
    FailingSink {
        bytes: Cursor::new(vec![]),
        fault: Arc::new(AtomicU8::new(0)),
    }
}

#[test]
fn recording_sink_contract_header_write_failure_returns_no_success() {
    let mut output = sink();
    output.fault.store(1, Ordering::Relaxed);
    assert!(hound::WavWriter::new(&mut output, spec()).is_err());
    assert!(output.bytes.get_ref().is_empty());
}

#[test]
fn recording_sink_contract_write_and_finalize_failures_are_not_published_results() {
    let mut output = sink();
    let fault = Arc::clone(&output.fault);
    let mut writer = hound::WavWriter::new(output, spec()).unwrap();
    fault.store(1, Ordering::Relaxed);
    assert!(writer.write_sample(0.5f32).is_err());
    assert!(writer.finalize().is_err());
    for fault_code in [2, 3] {
        output = sink();
        let fault = Arc::clone(&output.fault);
        let mut writer = hound::WavWriter::new(output, spec()).unwrap();
        writer.write_sample(0.5f32).unwrap();
        writer.write_sample(-0.5f32).unwrap();
        fault.store(fault_code, Ordering::Relaxed);
        assert!(writer.finalize().is_err());
    }
    // An incomplete interleaved frame cannot become a rounded-down successful outcome.
    let mut writer = hound::WavWriter::new(sink(), spec()).unwrap();
    writer.write_sample(0.5f32).unwrap();
    assert!(writer.finalize().is_err());
}
