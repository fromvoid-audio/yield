//! Checked live FSEvents registration. Call startup and close in a supervised process.

use std::ffi::{CString, c_void};
use std::fs;
use std::io;
use std::marker::PhantomData;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};
use std::ptr;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use fsevent_sys as fs_event;
use fsevent_sys::core_foundation as cf;
use yield_library::filesystem_identity::stable_filesystem_identity;

use crate::replay::macos::KNOWN_FLAGS;
use crate::{SourceWatchSpec, WatchHintEvidence, WatcherSignal, is_ignored_file_name};

mod helper;

pub use helper::{HELPER_POLL, HELPER_READY_V1, decode_helper_hint, serve_live_watcher};

const MAX_CALLBACK_EVENTS: usize = 256;
const MAX_PATH_BYTES: usize = libc::PATH_MAX as usize;
const RUN_LOOP_TIMED_OUT: i32 = 3; // kCFRunLoopRunTimedOut in CFRunLoop.h
const DELIVERY_LOST: u32 = fs_event::kFSEventStreamEventFlagMustScanSubDirs
    | fs_event::kFSEventStreamEventFlagUserDropped
    | fs_event::kFSEventStreamEventFlagKernelDropped
    | fs_event::kFSEventStreamEventFlagEventIdsWrapped;
const ROOT_LIFECYCLE: u32 = fs_event::kFSEventStreamEventFlagRootChanged
    | fs_event::kFSEventStreamEventFlagMount
    | fs_event::kFSEventStreamEventFlagUnmount;
const ITEM_LIFECYCLE: u32 = fs_event::kFSEventStreamEventFlagItemCreated
    | fs_event::kFSEventStreamEventFlagItemRemoved
    | fs_event::kFSEventStreamEventFlagItemRenamed;
const ITEM_WRITE: u32 = fs_event::kFSEventStreamEventFlagItemModified
    | fs_event::kFSEventStreamEventFlagItemInodeMetaMod
    | fs_event::kFSEventStreamEventFlagItemFinderInfoMod
    | fs_event::kFSEventStreamEventFlagItemChangeOwner
    | fs_event::kFSEventStreamEventFlagItemXattrMod;

/// One checked macOS stream and its joined run-loop worker.
pub struct MacosLiveWatcher {
    signal: WatcherSignal,
    stop: SyncSender<()>,
    worker: Option<JoinHandle<()>>,
}

impl MacosLiveWatcher {
    /// Start one stream for the source and its parent, checking native readiness and root identity.
    ///
    /// CoreServices can block during startup. A caller requiring a deadline must supervise the
    /// owning process. The returned signal carries audit hints, with no replay authority.
    pub fn start(spec: SourceWatchSpec) -> io::Result<Self> {
        let signal = WatcherSignal::new(spec.clone());
        let context = CallbackContext {
            spec,
            signal: signal.clone(),
        };
        let (ready, readiness) = mpsc::sync_channel(1);
        let (stop, stopping) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("yield-fsevents-live".into())
            .spawn(move || run_stream(context, ready, stopping))?;
        let mut watcher = Self {
            signal,
            stop,
            worker: Some(worker),
        };
        match readiness.recv() {
            Ok(Ok(())) => Ok(watcher),
            Ok(Err(error)) => {
                watcher.close();
                Err(error)
            }
            Err(_) => {
                watcher.close();
                Err(io::Error::other(
                    "macOS live watcher exited before readiness",
                ))
            }
        }
    }

    /// Borrow the single coalesced evidence slot for this generation.
    pub fn signal(&self) -> &WatcherSignal {
        &self.signal
    }

    /// Close callback admission, stop the stream, and join the original worker.
    pub fn close(&mut self) {
        self.signal.close();
        if let Some(worker) = self.worker.take() {
            let _ = self.stop.try_send(());
            let _ = worker.join();
        }
    }
}

impl Drop for MacosLiveWatcher {
    fn drop(&mut self) {
        self.close();
    }
}

struct CallbackContext {
    spec: SourceWatchSpec,
    signal: WatcherSignal,
}

fn root_identity(spec: &SourceWatchSpec) -> io::Result<(u64, u64)> {
    let root = spec.root();
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || fs::canonicalize(root)? != root {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "macOS live watcher requires a canonical directory",
        ));
    }
    if let Some(expected) = spec.root_identity()
        && !stable_filesystem_identity(root, &metadata)
            .is_some_and(|actual| actual.into_bytes() == expected.as_bytes())
    {
        return Err(io::Error::other("macOS live watcher root identity changed"));
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn run_stream(context: CallbackContext, ready: SyncSender<io::Result<()>>, stop: Receiver<()>) {
    let stream = (|| {
        let original = root_identity(&context.spec)?;
        let stream = LiveStream::start(&context)?;
        if root_identity(&context.spec)? != original {
            return Err(io::Error::other(
                "macOS live watcher root changed during startup",
            ));
        }
        Ok(stream)
    })();
    let _stream = match stream {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    while matches!(stop.try_recv(), Err(TryRecvError::Empty)) {
        // SAFETY: The current thread owns the scheduled stream and its callback context.
        let result = unsafe { CFRunLoopRunInMode(cf::kCFRunLoopDefaultMode, 0.25, 0) };
        if result != RUN_LOOP_TIMED_OUT {
            context.signal.record_evidence(
                WatchHintEvidence::FULL_RESCAN.union(WatchHintEvidence::BACKEND_ERROR),
                Instant::now(),
            );
            context.signal.close();
            break;
        }
    }
}

struct LiveStream<'context> {
    raw: fs_event::FSEventStreamRef,
    started: bool,
    _context: PhantomData<&'context CallbackContext>,
}

impl<'context> LiveStream<'context> {
    fn start(context: &'context CallbackContext) -> io::Result<Self> {
        let root = context.spec.root();
        let paths = watch_paths(root)?;
        let stream_context = fs_event::FSEventStreamContext {
            version: 0,
            info: ptr::from_ref(context).cast_mut().cast(),
            retain: None,
            release: None,
            copy_description: None,
        };
        // SAFETY: FSEvents copies the array and context descriptor. The borrowed callback data
        // remains on this run-loop thread until the stream has been invalidated and released.
        let raw = unsafe {
            let raw = fs_event::FSEventStreamCreate(
                ptr::null_mut(),
                callback,
                &stream_context,
                paths,
                fs_event::kFSEventStreamEventIdSinceNow,
                0.01,
                fs_event::kFSEventStreamCreateFlagFileEvents
                    | fs_event::kFSEventStreamCreateFlagNoDefer
                    | fs_event::kFSEventStreamCreateFlagWatchRoot,
            );
            cf::CFRelease(paths);
            raw
        };
        if raw.is_null() {
            return Err(io::Error::other(
                "could not create macOS live watcher stream",
            ));
        }
        let mut stream = Self {
            raw,
            started: false,
            _context: PhantomData,
        };
        // SAFETY: `raw` is a live stream, scheduled on the current owning run loop.
        stream.started = unsafe {
            fs_event::FSEventStreamScheduleWithRunLoop(
                raw,
                cf::CFRunLoopGetCurrent(),
                cf::kCFRunLoopDefaultMode,
            );
            fs_event::FSEventStreamStart(raw) != 0
        };
        if !stream.started {
            return Err(io::Error::other("macOS live watcher registration refused"));
        }
        Ok(stream)
    }
}

impl Drop for LiveStream<'_> {
    fn drop(&mut self) {
        // SAFETY: The worker owns the stream and still holds its callback context.
        unsafe {
            if self.started {
                fs_event::FSEventStreamStop(self.raw);
            }
            fs_event::FSEventStreamInvalidate(self.raw);
            fs_event::FSEventStreamRelease(self.raw);
        }
    }
}

fn watch_paths(root: &Path) -> io::Result<cf::CFArrayRef> {
    let parent = root.parent().filter(|parent| *parent != root);
    let root = CString::new(root.as_os_str().as_bytes())?;
    let parent = parent
        .map(|path| CString::new(path.as_os_str().as_bytes()))
        .transpose()?;
    // SAFETY: The array retains appended CF strings. Each local CF reference is released once.
    unsafe {
        let paths = cf::CFArrayCreateMutable(ptr::null_mut(), 2, &cf::kCFTypeArrayCallBacks);
        if paths.is_null() {
            return Err(io::Error::other(
                "could not allocate macOS live watcher paths",
            ));
        }
        for path in std::iter::once(&root).chain(parent.as_ref()) {
            let value = cf::CFStringCreateWithCString(
                ptr::null_mut(),
                path.as_ptr(),
                cf::kCFStringEncodingUTF8,
            );
            if value.is_null() {
                cf::CFRelease(paths);
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unrepresentable watcher path",
                ));
            }
            cf::CFArrayAppendValue(paths, value);
            cf::CFRelease(value);
        }
        Ok(paths)
    }
}

extern "C" fn callback(
    _stream: fs_event::FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    if info.is_null() || count == 0 {
        return;
    }
    // SAFETY: Only the owning worker invokes the callback and it still holds the context.
    let context = unsafe { &*info.cast::<CallbackContext>() };
    let mut evidence = WatchHintEvidence::EMPTY;
    if count > MAX_CALLBACK_EVENTS || paths.is_null() || flags.is_null() {
        evidence = WatchHintEvidence::FULL_RESCAN;
    } else {
        let paths = paths.cast::<*const libc::c_char>();
        for index in 0..count {
            // SAFETY: With UseCFTypes unset, FSEvents supplies `count` pointers and flags.
            let (path, flag) = unsafe { (*paths.add(index), *flags.add(index)) };
            if path.is_null() {
                evidence = evidence.union(WatchHintEvidence::FULL_RESCAN);
                continue;
            }
            // SAFETY: FSEvents supplies a NUL-terminated path valid for the callback.
            let length = unsafe { libc::strnlen(path, MAX_PATH_BYTES + 1) };
            if length > MAX_PATH_BYTES {
                evidence = evidence.union(WatchHintEvidence::FULL_RESCAN);
                continue;
            }
            // SAFETY: The bounded length excludes the NUL and stays within the callback data.
            let bytes = unsafe { std::slice::from_raw_parts(path.cast::<u8>(), length) };
            let path = Path::new(std::ffi::OsStr::from_bytes(bytes));
            evidence = evidence.union(classify(&context.spec, path, flag));
        }
    }
    context.signal.record_evidence(evidence, Instant::now());
}

fn classify(spec: &SourceWatchSpec, path: &Path, flags: u32) -> WatchHintEvidence {
    if flags & DELIVERY_LOST != 0
        || !path.is_absolute()
        || path.components().any(|part| part == Component::ParentDir)
    {
        return WatchHintEvidence::FULL_RESCAN;
    }
    if flags == fs_event::kFSEventStreamEventFlagHistoryDone {
        return WatchHintEvidence::EMPTY;
    }
    let root = spec.root();
    if path == root || (root.parent() == Some(path) && flags & ROOT_LIFECYCLE != 0) {
        return if flags & (ROOT_LIFECYCLE | ITEM_LIFECYCLE | !KNOWN_FLAGS) != 0 {
            WatchHintEvidence::FULL_RESCAN
        } else {
            WatchHintEvidence::ROOT_METADATA_AMBIGUOUS
        };
    }
    if !path.starts_with(root) {
        return WatchHintEvidence::EMPTY;
    }
    if flags & (ROOT_LIFECYCLE | !KNOWN_FLAGS) != 0 {
        return WatchHintEvidence::FULL_RESCAN;
    }
    if path.file_name().is_some_and(|name| name == ".DS_Store")
        && flags & fs_event::kFSEventStreamEventFlagItemIsFile != 0
        && flags & ITEM_WRITE != 0
        && flags & ITEM_LIFECYCLE == 0
    {
        return WatchHintEvidence::FINDER_METADATA_CHANGED;
    }
    if is_ignored_file_name(path, spec.ignored_file_name_prefixes()) {
        return WatchHintEvidence::EMPTY;
    }
    if flags & (ITEM_LIFECYCLE | ITEM_WRITE) != 0 {
        WatchHintEvidence::DESCENDANT_CHANGED
    } else {
        WatchHintEvidence::FULL_RESCAN
    }
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRunLoopRunInMode(
        mode: cf::CFStringRef,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;
    use yield_library::sample_sources::SourceId;
    use yield_library::sample_sources::reconciliation::RootIdentity;

    fn spec(root: PathBuf) -> SourceWatchSpec {
        SourceWatchSpec::new(SourceId::from_string("native-live"), 7, root, None)
    }

    fn wait_for(watcher: &MacosLiveWatcher, expected: WatchHintEvidence) {
        let deadline = Instant::now() + Duration::from_millis(800);
        loop {
            if let Some(hint) = watcher.signal().take_hint()
                && hint.evidence().contains(expected)
            {
                assert_eq!(hint.lifecycle_generation(), 7);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing native evidence {expected:?}"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn checks_native_registration_delivers_two_scopes_and_joins() {
        let fixture = tempfile::tempdir().expect("watch fixture");
        let parent = fixture.path().canonicalize().expect("canonical parent");
        let root = parent.join("source");
        let nested = root.join("nested");
        fs::create_dir_all(&nested).expect("disposable source");
        let metadata = fs::metadata(&root).expect("read source metadata");
        let identity = stable_filesystem_identity(&root, &metadata).expect("real root identity");
        let expected = SourceWatchSpec::new(
            SourceId::from_string("native-live"),
            7,
            root.clone(),
            Some(RootIdentity::from_bytes(identity.into_bytes())),
        );
        let mut watcher = MacosLiveWatcher::start(expected).expect("checked native start");
        fs::write(parent.join("sibling.wav"), b"unrelated").expect("write sibling");
        fs::write(nested.join("kick.wav"), b"fixture").expect("write descendant");
        wait_for(&watcher, WatchHintEvidence::DESCENDANT_CHANGED);
        fs::rename(&root, parent.join("moved")).expect("rename original root");
        fs::create_dir(&root).expect("replace original root name");
        wait_for(&watcher, WatchHintEvidence::FULL_RESCAN);
        let signal = watcher.signal().clone();
        watcher.close();
        assert!(watcher.worker.is_none());
        assert!(signal.is_closed());
    }

    #[test]
    fn rejects_missing_aliased_or_mismatched_roots_before_ready() {
        let fixture = tempfile::tempdir().expect("watch fixture");
        let root = fixture.path().canonicalize().expect("canonical root");
        let missing = root.join("missing");
        assert!(matches!(
            MacosLiveWatcher::start(spec(missing)),
            Err(error) if error.kind() == io::ErrorKind::NotFound
        ));
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&root, &alias).expect("disposable symlink");
        assert!(matches!(
            MacosLiveWatcher::start(spec(alias)),
            Err(error) if error.kind() == io::ErrorKind::InvalidInput
        ));
        let foreign = SourceWatchSpec::new(
            SourceId::from_string("foreign"),
            7,
            root,
            Some(RootIdentity::from_bytes(b"foreign".to_vec())),
        );
        assert!(matches!(
            MacosLiveWatcher::start(foreign),
            Err(error) if error.to_string().contains("identity changed")
        ));
    }

    #[test]
    fn filters_siblings_and_promotes_loss_before_path_filtering() {
        let spec = spec(PathBuf::from("/private/tmp/source"));
        let outside = Path::new("/private/tmp/source-other/kick.wav");
        assert_eq!(
            classify(
                &spec,
                outside,
                fs_event::kFSEventStreamEventFlagItemModified
            ),
            WatchHintEvidence::EMPTY
        );
        assert_eq!(
            classify(&spec, outside, fs_event::kFSEventStreamEventFlagUserDropped),
            WatchHintEvidence::FULL_RESCAN
        );
        assert_eq!(
            classify(&spec, Path::new("relative"), 0),
            WatchHintEvidence::FULL_RESCAN
        );
        assert_eq!(
            classify(
                &spec,
                Path::new("/private/tmp"),
                fs_event::kFSEventStreamEventFlagRootChanged
            ),
            WatchHintEvidence::FULL_RESCAN
        );
    }

    #[test]
    fn finder_bookkeeping_needs_exact_file_write_flags() {
        let spec = spec(PathBuf::from("/private/tmp/source"));
        let bookkeeping = Path::new("/private/tmp/source/.DS_Store");
        let flags = fs_event::kFSEventStreamEventFlagItemIsFile
            | fs_event::kFSEventStreamEventFlagItemModified;
        assert_eq!(
            classify(&spec, bookkeeping, flags),
            WatchHintEvidence::FINDER_METADATA_CHANGED
        );
        assert_eq!(
            classify(
                &spec,
                bookkeeping,
                flags | fs_event::kFSEventStreamEventFlagItemRenamed
            ),
            WatchHintEvidence::DESCENDANT_CHANGED
        );
    }

    #[test]
    fn invalid_callback_envelopes_coalesce_as_one_rescan() {
        let spec = spec(PathBuf::from("/private/tmp/source"));
        let signal = WatcherSignal::new(spec.clone());
        let mut context = CallbackContext {
            spec,
            signal: signal.clone(),
        };
        for count in [1, MAX_CALLBACK_EVENTS + 1] {
            callback(
                ptr::null_mut(),
                ptr::from_mut(&mut context).cast(),
                count,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
            );
        }
        let hint = signal.take_hint().expect("conservative callback hint");
        assert_eq!(hint.evidence(), WatchHintEvidence::FULL_RESCAN);
        assert!(signal.take_hint().is_none());
    }
}
