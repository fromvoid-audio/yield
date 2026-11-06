//! Child-side fixed-width protocol for one native watcher.

use std::io::{self, Read, Write};

use super::{MacosLiveWatcher, SourceWatchSpec, WatchHintEvidence, WatcherSignal};

/// Request one coalesced watcher hint. EOF ends the helper.
pub const HELPER_POLL: u8 = 1;

/// Version 1 readiness byte emitted once after checked native registration.
pub const HELPER_READY_V1: u8 = 0x80;

/// Decode an exact evidence byte from a version 1 helper pull.
///
/// Empty evidence is valid. Reserved bits and backend errors without an audit obligation
/// indicate a protocol error and must take the parent's fallback path.
pub fn decode_helper_hint(byte: u8) -> Option<WatchHintEvidence> {
    const KNOWN: u8 = WatchHintEvidence::DESCENDANT_CHANGED.0
        | WatchHintEvidence::ROOT_METADATA_AMBIGUOUS.0
        | WatchHintEvidence::FULL_RESCAN.0
        | WatchHintEvidence::BACKEND_ERROR.0
        | WatchHintEvidence::FINDER_METADATA_CHANGED.0;
    let evidence = WatchHintEvidence(byte);
    if byte & !KNOWN != 0 || (evidence.has_backend_error() && !evidence.requires_full_rescan()) {
        return None;
    }
    Some(evidence)
}

/// Serve one supervised live watcher through private request and response streams.
///
/// After readiness, every pull receives one evidence byte. Zero means the live stream has no
/// pending evidence. The flags come from [the bounded hint type](crate::WatchHintEvidence).
/// The parent retains source identity and treats any helper error as a need for audit and
/// retirement.
///
/// Native start, stream I/O and worker retirement may block. Run this in the child process,
/// where the parent can own its lifetime independently of those calls.
pub fn serve_live_watcher(
    spec: SourceWatchSpec,
    input: impl Read,
    output: impl Write,
) -> io::Result<()> {
    let watcher = MacosLiveWatcher::start(spec)?;
    serve(watcher.signal(), input, output)
}

fn serve(signal: &WatcherSignal, mut input: impl Read, mut output: impl Write) -> io::Result<()> {
    output.write_all(&[HELPER_READY_V1])?;
    output.flush()?;
    let mut request = [0];
    loop {
        match input.read_exact(&mut request) {
            Ok(()) if request == [HELPER_POLL] => {}
            Ok(()) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unknown macOS live watcher request",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(error) => return Err(error),
        }
        let hint = signal.take_hint();
        if hint.is_none() && signal.is_closed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "macOS live watcher stream closed",
            ));
        }
        let evidence = hint.map_or(0, |hint| hint.evidence().0);
        output.write_all(&[evidence])?;
        output.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::Shutdown;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    use yield_library::sample_sources::SourceId;

    use super::*;
    fn spec(root: PathBuf) -> SourceWatchSpec {
        SourceWatchSpec::new(SourceId::from_string("watcher-helper"), 2, root, None)
    }

    #[test]
    fn one_byte_pulls_drain_once_and_unknown_requests_fail() {
        let signal = WatcherSignal::new(spec(PathBuf::from("/private/tmp/watcher-helper")));
        signal.record_evidence(WatchHintEvidence::DESCENDANT_CHANGED, Instant::now());
        signal.record_evidence(WatchHintEvidence::FULL_RESCAN, Instant::now());
        let mut bytes = Vec::new();
        let error = serve(&signal, &[HELPER_POLL, HELPER_POLL, 0xff][..], &mut bytes).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            bytes,
            [
                HELPER_READY_V1,
                WatchHintEvidence::DESCENDANT_CHANGED
                    .union(WatchHintEvidence::FULL_RESCAN)
                    .0,
                0,
            ]
        );
        assert_eq!(decode_helper_hint(0), Some(WatchHintEvidence::EMPTY));
        assert_eq!(
            decode_helper_hint(bytes[1]),
            Some(WatchHintEvidence(bytes[1]))
        );
        assert_eq!(decode_helper_hint(HELPER_READY_V1), None);
        assert_eq!(decode_helper_hint(WatchHintEvidence::BACKEND_ERROR.0), None);
    }

    #[test]
    fn closed_stream_has_no_empty_success_reply() {
        let signal = WatcherSignal::new(spec(PathBuf::from("/private/tmp/watcher-helper")));
        signal.close();
        let mut bytes = Vec::new();
        let error = serve(&signal, &[HELPER_POLL][..], &mut bytes).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(bytes, [HELPER_READY_V1]);
    }

    #[test]
    fn missing_native_root_emits_no_readiness() {
        let temp = tempfile::tempdir().expect("watcher fixture");
        let mut bytes = Vec::new();
        let error = serve_live_watcher(spec(temp.path().join("missing")), io::empty(), &mut bytes)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(bytes.is_empty());
    }

    #[test]
    fn live_pipe_reports_native_hint_and_retires_at_eof() {
        let temp = tempfile::tempdir().expect("watcher fixture");
        let parent = temp.path().canonicalize().expect("canonical parent");
        let root = parent.join("source");
        fs::create_dir(&root).expect("disposable source");
        thread::scope(|scope| {
            let (mut client, input) = UnixStream::pair().expect("private socket pair");
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .expect("bound test socket reads");
            let output = input.try_clone().expect("response socket handle");
            let worker = scope.spawn(|| serve_live_watcher(spec(root.clone()), input, output));
            let mut byte = [0];
            client.read_exact(&mut byte).expect("native readiness");
            assert_eq!(byte, [HELPER_READY_V1]);
            fs::write(root.join("kick.wav"), b"disposable").expect("write watched fixture");
            let deadline = Instant::now() + Duration::from_millis(650);
            loop {
                client.write_all(&[HELPER_POLL]).expect("one pull");
                client.read_exact(&mut byte).expect("one response");
                if byte[0] & WatchHintEvidence::DESCENDANT_CHANGED.0 != 0 {
                    break;
                }
                assert!(Instant::now() < deadline, "missing native descendant hint");
                thread::sleep(Duration::from_millis(5));
            }
            client.shutdown(Shutdown::Write).expect("request EOF");
            worker.join().expect("helper joins").expect("graceful EOF");
            assert_eq!(client.read(&mut byte).expect("response EOF"), 0);
        });
    }
}
