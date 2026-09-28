//! Bounded, profile-local runtime logs for the GPUI launcher.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, fmt};

const SEGMENT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_FILES: usize = 10;
const EVENT_QUEUE: usize = 1024;

enum Message {
    Event(Vec<u8>),
    Shutdown,
}

#[derive(Clone)]
struct EventWriterFactory {
    sender: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
}

struct EventWriter {
    sender: SyncSender<Message>,
    dropped: Arc<AtomicU64>,
    bytes: Vec<u8>,
}

impl<'a> MakeWriter<'a> for EventWriterFactory {
    type Writer = EventWriter;

    fn make_writer(&'a self) -> Self::Writer {
        EventWriter {
            sender: self.sender.clone(),
            dropped: Arc::clone(&self.dropped),
            bytes: Vec::new(),
        }
    }
}

impl Write for EventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        if self.bytes.is_empty() {
            return;
        }
        if let Err(TrySendError::Full(_)) = self
            .sender
            .try_send(Message::Event(std::mem::take(&mut self.bytes)))
        {
            // Never wait for disk I/O or a saturated logging queue on the UI or audio thread.
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(super) struct LogGuard {
    sender: SyncSender<Message>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Install the global tracing subscriber. Failure to open logs degrades to stdout.
pub(super) fn init() -> Option<LogGuard> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if std::env::args_os().any(|arg| arg == "--log" || arg == "-log")
            || std::env::var("YIELD_DEBUG_LOGGING").is_ok_and(|value| {
                matches!(
                    value.to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
        {
            EnvFilter::new("yield=debug,perf=info,info")
        } else {
            EnvFilter::new("info")
        }
    });

    let segment = yield_library::app_dirs::logs_dir()
        .map_err(|error| io::Error::other(error.to_string()))
        .and_then(|dir| SegmentWriter::new(&dir, SEGMENT_BYTES, MAX_FILES));
    let Ok(segment) = segment else {
        eprintln!("yield logging: file logs unavailable; using stdout only");
        let subscriber = Registry::default()
            .with(filter)
            .with(fmt::layer().with_writer(io::stdout));
        let _ = tracing::subscriber::set_global_default(subscriber);
        return None;
    };

    let path = segment.path.clone();
    let (sender, receiver) = mpsc::sync_channel(EVENT_QUEUE);
    let dropped = Arc::new(AtomicU64::new(0));
    let worker_dropped = Arc::clone(&dropped);
    let worker = thread::Builder::new()
        .name("yield-log-writer".into())
        .spawn(move || run_writer(segment, receiver, worker_dropped));
    let Ok(worker) = worker else {
        eprintln!("yield logging: worker unavailable; using stdout only");
        let subscriber = Registry::default()
            .with(filter)
            .with(fmt::layer().with_writer(io::stdout));
        let _ = tracing::subscriber::set_global_default(subscriber);
        return None;
    };

    let subscriber = Registry::default()
        .with(filter)
        .with(fmt::layer().with_writer(io::stdout))
        .with(
            fmt::layer()
                .with_ansi(false)
                .with_writer(EventWriterFactory {
                    sender: sender.clone(),
                    dropped,
                }),
        );
    if let Err(error) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("yield logging: subscriber unavailable: {error}");
        let _ = sender.send(Message::Shutdown);
        let _ = worker.join();
        return None;
    }
    tracing::info!(log_path = %path.display(), "Yield logging initialized");
    Some(LogGuard {
        sender,
        worker: Some(worker),
    })
}

pub(super) fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(location) = info.location() {
            tracing::error!(
                file = location.file(),
                line = location.line(),
                "Yield panic"
            );
        } else {
            tracing::error!("Yield panic");
        }
        previous(info);
    }));
}

fn run_writer(mut segment: SegmentWriter, receiver: Receiver<Message>, dropped: Arc<AtomicU64>) {
    loop {
        let message = match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => {
                report_dropped(&dropped);
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        };
        match message {
            Message::Event(bytes) => {
                if let Err(error) = segment.write_event(&bytes) {
                    eprintln!("yield logging: write failed: {error}");
                }
            }
            Message::Shutdown => break,
        }
        report_dropped(&dropped);
    }
    report_dropped(&dropped);
    if let Err(error) = segment.file.flush() {
        eprintln!("yield logging: flush failed: {error}");
    }
}

fn report_dropped(dropped: &AtomicU64) {
    let count = dropped.swap(0, Ordering::Relaxed);
    if count > 0 {
        eprintln!("yield logging: {count} event(s) dropped because the queue was full");
    }
}

struct SegmentWriter {
    dir: PathBuf,
    path: PathBuf,
    file: File,
    bytes: u64,
    stamp: u64,
    run: u32,
    sequence: u32,
    cap: u64,
    keep: usize,
    rotation_warning: bool,
}

impl SegmentWriter {
    fn new(dir: &Path, cap: u64, keep: usize) -> io::Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut opened = None;
        for run in 0..u32::MAX {
            let path = segment_path(dir, stamp, run, 0);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    opened = Some((path, file, run));
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let (path, file, run) = opened.ok_or_else(|| io::Error::other("log run exhausted"))?;
        let result = Self {
            dir: dir.to_path_buf(),
            path,
            file,
            bytes: 0,
            stamp,
            run,
            sequence: 0,
            cap,
            keep,
            rotation_warning: false,
        };
        if let Err(error) = result.prune() {
            eprintln!("yield logging: startup retention degraded: {error}");
        }
        Ok(result)
    }

    fn write_event(&mut self, event: &[u8]) -> io::Result<()> {
        if self.bytes > 0 && self.bytes.saturating_add(event.len() as u64) > self.cap {
            if let Err(error) = self.rotate() {
                if !self.rotation_warning {
                    eprintln!("yield logging: rotation degraded; retaining current file: {error}");
                    self.rotation_warning = true;
                }
            } else {
                self.rotation_warning = false;
            }
        }
        if let Err(error) = self.file.write_all(event) {
            // write_all may have written a prefix before failing. Keep the size
            // accounting aligned with the open file for a later retry.
            self.bytes = self
                .file
                .metadata()
                .map(|meta| meta.len())
                .unwrap_or(self.bytes);
            return Err(error);
        }
        self.bytes = self.bytes.saturating_add(event.len() as u64);
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let mut sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("log sequence exhausted"))?;
        let (path, file) = loop {
            let path = segment_path(&self.dir, self.stamp, self.run, sequence);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => break (path, file),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    sequence = sequence
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("log sequence exhausted"))?;
                }
                Err(error) => return Err(error),
            }
        };
        self.file = file;
        self.path = path;
        self.bytes = 0;
        self.sequence = sequence;
        if let Err(error) = self.prune() {
            eprintln!("yield logging: retention cleanup degraded: {error}");
        }
        Ok(())
    }

    fn prune(&self) -> io::Result<()> {
        let mut files = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !is_segment_name(name) || !entry.file_type()?.is_file() {
                continue;
            }
            let modified = entry
                .metadata()?
                .modified()
                .unwrap_or(SystemTime::UNIX_EPOCH);
            files.push((modified, entry.path()));
        }
        files.sort_by(|a, b| a.cmp(b));
        while files.len() > self.keep {
            let Some(index) = files.iter().position(|(_, path)| path != &self.path) else {
                break;
            };
            fs::remove_file(&files[index].1)?;
            files.remove(index);
        }
        Ok(())
    }
}

fn is_segment_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix("yield_")
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    let parts: Vec<_> = body.split('_').collect();
    parts.len() == 4
        && parts.iter().zip([16, 8, 8, 8]).all(|(part, width)| {
            part.len() == width && part.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn segment_path(dir: &Path, stamp: u64, run: u32, sequence: u32) -> PathBuf {
    dir.join(format!(
        "yield_{stamp:016x}_{:08x}_{run:08x}_{sequence:08x}.log",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logs(dir: &Path) -> Vec<PathBuf> {
        let mut paths: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_segment_name)
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn whole_events_rotate_at_boundary_and_retain_ten_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("yield_notes.log"), b"user file").unwrap();
        let mut writer = SegmentWriter::new(dir.path(), 10, MAX_FILES).unwrap();
        writer.write_event(b"123456").unwrap();
        writer.write_event(b"7890").unwrap();
        assert_eq!(logs(dir.path()).len(), 1);
        writer.write_event(b"abcd").unwrap();
        assert_eq!(logs(dir.path()).len(), 2);
        for _ in 0..20 {
            writer.write_event(b"abcdefghijk").unwrap();
        }
        let paths = logs(dir.path());
        assert_eq!(paths.len(), MAX_FILES);
        assert_eq!(
            fs::read(dir.path().join("yield_notes.log")).unwrap(),
            b"user file"
        );
        assert!(paths.contains(&writer.path));
        assert!(
            paths
                .iter()
                .filter(|path| is_segment_name(path.file_name().unwrap().to_str().unwrap()))
                .all(|path| fs::metadata(path).unwrap().len() <= 11)
        );
        assert_eq!(fs::read(&writer.path).unwrap(), b"abcdefghijk");
    }

    #[test]
    #[ignore = "writes more than 120 MiB to exercise the production segment cap"]
    fn production_cap_stress_rotates_more_than_ten_times() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SegmentWriter::new(dir.path(), SEGMENT_BYTES, MAX_FILES).unwrap();
        let event = vec![b'x'; SEGMENT_BYTES as usize];
        for _ in 0..12 {
            writer.write_event(&event).unwrap();
        }
        writer.write_event(b"next event stays whole").unwrap();
        let paths = logs(dir.path());
        assert_eq!(paths.len(), MAX_FILES);
        assert!(paths.contains(&writer.path));
        assert_eq!(fs::read(&writer.path).unwrap(), b"next event stays whole");
        assert!(
            paths
                .iter()
                .all(|path| fs::metadata(path).unwrap().len() <= SEGMENT_BYTES)
        );
    }

    #[test]
    fn relaunch_names_are_unique_and_non_logs_survive() {
        let dir = tempfile::tempdir().unwrap();
        let first = SegmentWriter::new(dir.path(), 10, MAX_FILES).unwrap();
        fs::write(dir.path().join("notes.log"), b"keep").unwrap();
        fs::write(dir.path().join("yield_note.txt"), b"keep").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&first.path, dir.path().join("yield_link.log")).unwrap();
        let second = SegmentWriter::new(dir.path(), 10, MAX_FILES).unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(fs::read(dir.path().join("notes.log")).unwrap(), b"keep");
        assert_eq!(
            fs::read(dir.path().join("yield_note.txt")).unwrap(),
            b"keep"
        );
        #[cfg(unix)]
        assert!(dir.path().join("yield_link.log").is_symlink());
    }

    #[test]
    fn failed_rotation_keeps_writable_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = SegmentWriter::new(dir.path(), 4, MAX_FILES).unwrap();
        writer.write_event(b"abcd").unwrap();
        let held = writer.path.clone();
        let moved = dir.path().with_extension("moved");
        fs::rename(dir.path(), &moved).unwrap();
        writer.write_event(b"efgh").unwrap();
        assert_eq!(
            fs::read(moved.join(held.file_name().unwrap())).unwrap(),
            b"abcdefgh"
        );
    }

    #[test]
    fn formatted_tracing_event_arrives_as_one_worker_message() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let subscriber = Registry::default().with(fmt::layer().with_ansi(false).with_writer(
            EventWriterFactory {
                sender,
                dropped: Arc::new(AtomicU64::new(0)),
            },
        ));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(sample = "kick.wav", "loaded sample");
        });
        let Message::Event(event) = receiver.recv_timeout(Duration::from_secs(1)).unwrap() else {
            panic!("expected a formatted log event");
        };
        let text = String::from_utf8(event).unwrap();
        assert!(text.contains("loaded sample"));
        assert!(text.contains("kick.wav"));
        assert!(receiver.try_recv().is_err());
    }
}
