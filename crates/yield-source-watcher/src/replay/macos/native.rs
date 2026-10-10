//! One-shot, per-device historical FSEvents capture. The worker owns its stream and context.

use std::ffi::{CStr, CString, c_void};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fsevent_sys as fs_event;
use fsevent_sys::core_foundation as cf;
use yield_library::filesystem_identity::stable_filesystem_identity;
use yield_library::sample_sources::reconciliation::RootIdentity;

use super::{MacosReplayEvent, record_callback_event};
use crate::SourceWatchSpec;
use crate::replay::{
    MAX_REPLAY_EVENTS, ReplayCandidate, ReplayIngress, ReplayStartError, ReplayUncertainty,
};

static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Why native history could not yield a qualified replay candidate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeReplayError {
    /// Another capture still owns the sole bounded native worker slot.
    Busy,
    /// The caller supplied no time for a bounded capture.
    InvalidTimeout,
    /// A native worker could not be started.
    WorkerUnavailable,
    /// The source root was missing, replaced, or not a canonical directory.
    RootChanged,
    /// The device's history UUID was unavailable or nil.
    VolumeHistoryUnavailable,
    /// The native stream could not be constructed or started on the expected device.
    StreamUnavailable,
    /// The worker did not complete historical delivery within the caller's budget.
    TimedOut,
    /// A physical identity or event cursor failed ingress validation.
    InvalidIdentity(ReplayStartError),
    /// A device-local event cursor could not be captured before the audit.
    CursorUnavailable,
}

/// A live, bounded device stream started before an authoritative source audit.
///
/// Dropping the session stops its worker. `finish` flushes events generated through the audit
/// boundary and returns only a candidate; it does not grant replay or checkpoint authority.
pub struct BootstrapCapture {
    stop: mpsc::SyncSender<()>,
    result: mpsc::Receiver<Result<ReplayCandidate, NativeReplayError>>,
}

impl BootstrapCapture {
    /// Start a device-local FSEvents stream and wait until it is active before returning.
    pub fn start(spec: SourceWatchSpec, timeout: Duration) -> Result<Self, NativeReplayError> {
        if timeout.is_zero() {
            return Err(NativeReplayError::InvalidTimeout);
        }
        if CAPTURE_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(NativeReplayError::Busy);
        }
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        if std::thread::Builder::new()
            .name("yield-fsevents-bootstrap".into())
            .spawn(move || {
                let active = ActiveCapture;
                let result = capture_live_on_worker(spec, ready_tx, stop_rx);
                drop(active);
                let _ = result_tx.send(result);
            })
            .is_err()
        {
            CAPTURE_ACTIVE.store(false, Ordering::Release);
            return Err(NativeReplayError::WorkerUnavailable);
        }
        match ready_rx.recv_timeout(timeout) {
            Ok(Ok(())) => Ok(Self {
                stop: stop_tx,
                result: result_rx,
            }),
            Ok(Err(error)) => Err(error),
            Err(RecvTimeoutError::Timeout) => Err(NativeReplayError::TimedOut),
            Err(RecvTimeoutError::Disconnected) => match result_rx.recv() {
                Ok(Err(error)) => Err(error),
                _ => Err(NativeReplayError::WorkerUnavailable),
            },
        }
    }

    /// Stop after an audit, flush the active stream, and return its bounded event candidate.
    pub fn finish(self, timeout: Duration) -> Result<ReplayCandidate, NativeReplayError> {
        if timeout.is_zero() {
            return Err(NativeReplayError::InvalidTimeout);
        }
        let _ = self.stop.send(());
        self.result
            .recv_timeout(timeout)
            .unwrap_or(Err(NativeReplayError::TimedOut))
    }
}

impl Drop for BootstrapCapture {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
    }
}

/// Capture bounded historical changes after `after_event_id` from the source's device stream.
///
/// This operation is read-only and never grants a continuity proof. The call returns by its
/// deadline even if filesystem or CoreServices work blocks; at most one worker can remain alive.
/// A timed-out worker retains its stream and context until it finishes cleanup on its own thread.
pub fn capture_history(
    spec: SourceWatchSpec,
    after_event_id: u64,
    timeout: Duration,
) -> Result<ReplayCandidate, NativeReplayError> {
    capture_history_inner(
        spec,
        after_event_id,
        timeout,
        Arc::new(()),
        Arc::new(AtomicBool::new(false)),
        || {},
        None,
    )
}

pub(super) fn capture_history_with_custody<T: Send + Sync + 'static>(
    spec: SourceWatchSpec,
    after_event_id: u64,
    timeout: Duration,
    custody: Arc<T>,
    cancellation: Arc<AtomicBool>,
) -> Result<ReplayCandidate, NativeReplayError> {
    capture_history_inner(
        spec,
        after_event_id,
        timeout,
        custody,
        cancellation,
        || {},
        None,
    )
}

#[cfg(test)]
pub(super) fn capture_history_with_test_hooks<T: Send + Sync + 'static>(
    spec: SourceWatchSpec,
    after_event_id: u64,
    timeout: Duration,
    custody: Arc<T>,
    cancellation: Arc<AtomicBool>,
    entered: mpsc::SyncSender<()>,
    before_first_slice: mpsc::Receiver<()>,
    cleaned: mpsc::SyncSender<()>,
) -> Result<ReplayCandidate, NativeReplayError> {
    capture_history_inner(
        spec,
        after_event_id,
        timeout,
        custody,
        cancellation,
        move || {
            let _ = entered.send(());
            let _ = before_first_slice.recv();
        },
        Some(cleaned),
    )
}

fn capture_history_inner<T, F>(
    spec: SourceWatchSpec,
    after_event_id: u64,
    timeout: Duration,
    custody: Arc<T>,
    cancellation: Arc<AtomicBool>,
    before_first_slice: F,
    cleaned: Option<mpsc::SyncSender<()>>,
) -> Result<ReplayCandidate, NativeReplayError>
where
    T: Send + Sync + 'static,
    F: FnOnce() + Send + 'static,
{
    if timeout.is_zero() {
        return Err(NativeReplayError::InvalidTimeout);
    }
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or(NativeReplayError::InvalidTimeout)?;
    if CAPTURE_ACTIVE
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(NativeReplayError::Busy);
    }
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let (run_loop_tx, run_loop_rx) = mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = mpsc::sync_channel(1);
    let worker_cancellation = cancellation.clone();
    let spawned = std::thread::Builder::new()
        .name("yield-fsevents-history".into())
        .spawn(move || {
            let active = ActiveCapture;
            let result = capture_on_worker(
                spec,
                after_event_id,
                deadline,
                run_loop_tx,
                stop_rx,
                worker_cancellation,
                before_first_slice,
            );
            drop(active);
            drop(custody);
            if let Some(cleaned) = cleaned {
                let _ = cleaned.send(());
            }
            let _ = result_tx.send(result);
        });
    if spawned.is_err() {
        CAPTURE_ACTIVE.store(false, Ordering::Release);
        return Err(NativeReplayError::WorkerUnavailable);
    }
    let poll_interval = Duration::from_millis(25);
    let run_loop = loop {
        if cancellation.load(Ordering::Acquire) {
            let _ = stop_tx.try_send(());
            return Err(NativeReplayError::TimedOut);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = stop_tx.try_send(());
            return Err(NativeReplayError::TimedOut);
        }
        match run_loop_rx.recv_timeout(remaining.min(poll_interval)) {
            Ok(run_loop) => break run_loop,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return match result_rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(result) => result,
                    Err(RecvTimeoutError::Timeout) => Err(NativeReplayError::TimedOut),
                    Err(RecvTimeoutError::Disconnected) => {
                        Err(NativeReplayError::WorkerUnavailable)
                    }
                };
            }
        }
    };
    let result = loop {
        if cancellation.load(Ordering::Acquire) {
            let _ = stop_tx.try_send(());
            // SAFETY: CoreFoundation permits stopping another thread's retained run loop.
            unsafe { cf::CFRunLoopStop(run_loop.0) };
            break None;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = stop_tx.try_send(());
            // SAFETY: CoreFoundation permits stopping another thread's retained run loop.
            unsafe { cf::CFRunLoopStop(run_loop.0) };
            break None;
        }
        match result_rx.recv_timeout(remaining.min(poll_interval)) {
            Ok(result) => break Some(result),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                break Some(Err(NativeReplayError::WorkerUnavailable));
            }
        }
    };
    drop(run_loop);
    match result {
        Some(result) => result,
        None => Err(NativeReplayError::TimedOut),
    }
}

struct ActiveCapture;

impl Drop for ActiveCapture {
    fn drop(&mut self) {
        CAPTURE_ACTIVE.store(false, Ordering::Release);
    }
}

struct RunLoopHandle(cf::CFRunLoopRef);

impl Drop for RunLoopHandle {
    fn drop(&mut self) {
        // SAFETY: Each handle owns exactly the retain made before it was sent to the caller.
        unsafe { cf::CFRelease(self.0) };
    }
}

// SAFETY: The caller only calls CFRunLoopStop and CFRelease on a retained CoreFoundation object.
unsafe impl Send for RunLoopHandle {}

struct CallbackContext {
    ingress: ReplayIngress,
    volume_root: PathBuf,
    run_loop: cf::CFRunLoopRef,
    seen: usize,
    stop_after_history: bool,
    history_done: bool,
    stop_requested: bool,
}

fn capture_live_on_worker(
    spec: SourceWatchSpec,
    ready: mpsc::SyncSender<Result<(), NativeReplayError>>,
    stop: mpsc::Receiver<()>,
) -> Result<ReplayCandidate, NativeReplayError> {
    let root = spec.root().to_path_buf();
    let metadata = fs::metadata(&root).map_err(|_| NativeReplayError::RootChanged)?;
    if !metadata.is_dir() || fs::canonicalize(&root).ok().as_deref() != Some(root.as_path()) {
        return Err(NativeReplayError::RootChanged);
    }
    let expected_root = spec
        .root_identity()
        .cloned()
        .ok_or(NativeReplayError::RootChanged)?;
    if physical_root_identity(&root, &metadata).as_ref() != Some(&expected_root) {
        return Err(NativeReplayError::RootChanged);
    }
    let device =
        libc::dev_t::try_from(metadata.dev()).map_err(|_| NativeReplayError::StreamUnavailable)?;
    let volume_root = find_volume_root(&root, metadata.dev())?;
    let uuid = volume_uuid(device)?;
    // A device-local cursor is necessary: the host-wide cursor cannot identify a removable
    // volume's history. The live stream starts before the caller can begin its audit.
    // Despite the CFAbsoluteTime parameter type, FSEvents.h specifies a POSIX epoch value.
    // CFAbsoluteTimeGetCurrent uses the 2001 epoch and would usually return cursor zero here.
    let posix_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NativeReplayError::CursorUnavailable)?
        .as_secs_f64();
    let cursor = unsafe { FSEventsGetLastEventIdForDeviceBeforeTime(device, posix_now) };
    // Zero is a valid beginning-of-history cursor on a fresh test volume. The bounded ingress
    // still rejects an oversized history; `u64::MAX` is the unrepresentable since-now sentinel.
    if cursor == u64::MAX {
        return Err(NativeReplayError::CursorUnavailable);
    }
    let ingress = ReplayIngress::new(spec.clone(), metadata.dev(), uuid, cursor)
        .map_err(NativeReplayError::InvalidIdentity)?;
    let context = Box::into_raw(Box::new(CallbackContext {
        ingress,
        volume_root,
        run_loop: ptr::null_mut(),
        seen: 0,
        stop_after_history: false,
        history_done: false,
        stop_requested: false,
    }));
    let stream = unsafe { create_stream(&root, &(*context).volume_root, device, cursor, context) };
    let Some(stream) = stream else {
        unsafe { drop(Box::from_raw(context)) };
        return Err(NativeReplayError::StreamUnavailable);
    };
    if unsafe { fs_event::FSEventStreamGetDeviceBeingWatched(stream) } != device {
        unsafe {
            fs_event::FSEventStreamInvalidate(stream);
            fs_event::FSEventStreamRelease(stream);
            drop(Box::from_raw(context));
        }
        return Err(NativeReplayError::StreamUnavailable);
    }
    let run_loop = unsafe { cf::CFRunLoopGetCurrent() };
    unsafe {
        (*context).run_loop = run_loop;
        fs_event::FSEventStreamScheduleWithRunLoop(stream, run_loop, cf::kCFRunLoopDefaultMode);
    }
    if unsafe { fs_event::FSEventStreamStart(stream) } == 0 {
        unsafe {
            fs_event::FSEventStreamInvalidate(stream);
            fs_event::FSEventStreamRelease(stream);
            drop(Box::from_raw(context));
        }
        return Err(NativeReplayError::StreamUnavailable);
    }
    // The device time lookup may conservatively include events from before this capture.
    // Drain that history while the audit has not begun, then use the stream's own device-local
    // cursor as the exact start of the live audit interval.
    while !unsafe { (*context).history_done } && unsafe { (*context).seen } < MAX_REPLAY_EVENTS {
        if !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            unsafe {
                fs_event::FSEventStreamStop(stream);
                fs_event::FSEventStreamInvalidate(stream);
                fs_event::FSEventStreamRelease(stream);
                drop(Box::from_raw(context));
            }
            return Err(NativeReplayError::TimedOut);
        }
        unsafe { CFRunLoopRunInMode(cf::kCFRunLoopDefaultMode, 0.025, 1) };
    }
    let baseline = unsafe { fs_event::FSEventStreamGetLatestEventId(stream) };
    let ready_for_audit = unsafe { (*context).history_done }
        && baseline >= cursor
        && baseline != u64::MAX
        && fs::metadata(&root)
            .ok()
            .and_then(|current| physical_root_identity(&root, &current))
            .as_ref()
            == Some(&expected_root)
        && volume_uuid(device).ok() == Some(uuid);
    if !ready_for_audit {
        unsafe {
            fs_event::FSEventStreamStop(stream);
            fs_event::FSEventStreamInvalidate(stream);
            fs_event::FSEventStreamRelease(stream);
            drop(Box::from_raw(context));
        }
        return Err(NativeReplayError::StreamUnavailable);
    }
    let live_ingress = match ReplayIngress::new(spec, metadata.dev(), uuid, baseline) {
        Ok(ingress) => ingress,
        Err(error) => {
            unsafe {
                fs_event::FSEventStreamStop(stream);
                fs_event::FSEventStreamInvalidate(stream);
                fs_event::FSEventStreamRelease(stream);
                drop(Box::from_raw(context));
            }
            return Err(NativeReplayError::InvalidIdentity(error));
        }
    };
    live_ingress.mark_history_complete();
    unsafe {
        (*context).ingress = live_ingress;
        (*context).seen = 0;
    }
    if ready.send(Ok(())).is_ok() {
        // Short run-loop slices make a fast audit/finish handshake safe without racing a
        // cross-thread CFRunLoopStop before the run loop has entered.
        while matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty)) {
            unsafe { CFRunLoopRunInMode(cf::kCFRunLoopDefaultMode, 0.025, 1) };
        }
        // Flush while the stream is still active. A blocked flush leaves the worker owning all
        // CoreFoundation objects; the caller's finish deadline still returns promptly.
        unsafe { FSEventStreamFlushSync(stream) };
    }
    unsafe {
        fs_event::FSEventStreamStop(stream);
        fs_event::FSEventStreamInvalidate(stream);
        fs_event::FSEventStreamRelease(stream);
    }
    let context = unsafe { Box::from_raw(context) };
    let final_metadata = fs::metadata(&root).map_err(|_| NativeReplayError::RootChanged)?;
    if physical_root_identity(&root, &final_metadata).as_ref() != Some(&expected_root)
        || fs::canonicalize(&root).ok().as_deref() != Some(root.as_path())
        || final_metadata.dev() != metadata.dev()
        || volume_uuid(device).ok() != Some(uuid)
    {
        context.ingress.mark_reason(ReplayUncertainty::ROOT_CHANGED);
    }
    Ok(context.ingress.into_candidate())
}

fn capture_on_worker(
    spec: SourceWatchSpec,
    after_event_id: u64,
    deadline: Instant,
    run_loop_tx: mpsc::SyncSender<RunLoopHandle>,
    stop: mpsc::Receiver<()>,
    cancellation: Arc<AtomicBool>,
    before_first_slice: impl FnOnce(),
) -> Result<ReplayCandidate, NativeReplayError> {
    let root = spec.root().to_path_buf();
    let metadata = fs::metadata(&root).map_err(|_| NativeReplayError::RootChanged)?;
    if !metadata.is_dir() || fs::canonicalize(&root).ok().as_deref() != Some(root.as_path()) {
        return Err(NativeReplayError::RootChanged);
    }
    let expected_root = spec
        .root_identity()
        .cloned()
        .ok_or(NativeReplayError::RootChanged)?;
    if physical_root_identity(&root, &metadata).as_ref() != Some(&expected_root) {
        return Err(NativeReplayError::RootChanged);
    }
    let device =
        libc::dev_t::try_from(metadata.dev()).map_err(|_| NativeReplayError::StreamUnavailable)?;
    let volume_root = find_volume_root(&root, metadata.dev())?;
    let uuid = volume_uuid(device)?;
    let ingress = ReplayIngress::new(spec, metadata.dev(), uuid, after_event_id)
        .map_err(NativeReplayError::InvalidIdentity)?;
    let context = Box::into_raw(Box::new(CallbackContext {
        ingress,
        volume_root,
        run_loop: ptr::null_mut(),
        seen: 0,
        stop_after_history: true,
        history_done: false,
        stop_requested: false,
    }));
    let stream = unsafe {
        create_stream(
            &root,
            &(*context).volume_root,
            device,
            after_event_id,
            context,
        )
    };
    let Some(stream) = stream else {
        // SAFETY: No stream retained the context when construction failed.
        unsafe { drop(Box::from_raw(context)) };
        return Err(NativeReplayError::StreamUnavailable);
    };
    let watched_device = unsafe { fs_event::FSEventStreamGetDeviceBeingWatched(stream) };
    if watched_device != device {
        unsafe {
            fs_event::FSEventStreamInvalidate(stream);
            fs_event::FSEventStreamRelease(stream);
            drop(Box::from_raw(context));
        }
        return Err(NativeReplayError::StreamUnavailable);
    }
    let run_loop = unsafe { cf::CFRunLoopGetCurrent() };
    unsafe {
        (*context).run_loop = run_loop;
        fs_event::FSEventStreamScheduleWithRunLoop(stream, run_loop, cf::kCFRunLoopDefaultMode);
    }
    if unsafe { fs_event::FSEventStreamStart(stream) } == 0 {
        unsafe {
            fs_event::FSEventStreamInvalidate(stream);
            fs_event::FSEventStreamRelease(stream);
            drop(Box::from_raw(context));
        }
        return Err(NativeReplayError::StreamUnavailable);
    }
    let retained = unsafe { CFRetain(run_loop) };
    if run_loop_tx.send(RunLoopHandle(retained)).is_err() {
        unsafe {
            fs_event::FSEventStreamStop(stream);
            fs_event::FSEventStreamInvalidate(stream);
            fs_event::FSEventStreamRelease(stream);
            drop(Box::from_raw(context));
        }
        return Err(NativeReplayError::TimedOut);
    }
    before_first_slice();
    let interrupted = || {
        Instant::now() >= deadline
            || cancellation.load(Ordering::Acquire)
            || !matches!(stop.try_recv(), Err(mpsc::TryRecvError::Empty))
    };
    let run_loop_result = loop {
        if interrupted() {
            break Err(NativeReplayError::TimedOut);
        }
        if unsafe { (*context).stop_requested } {
            break Ok(());
        }
        // A bounded mode run makes cancellation durable: even if a stop request raced before
        // the worker entered CoreFoundation, the worker checks it before the next slice.
        let status = unsafe { CFRunLoopRunInMode(cf::kCFRunLoopDefaultMode, 0.025, 1) };
        if interrupted() {
            break Err(NativeReplayError::TimedOut);
        }
        if unsafe { (*context).stop_requested } {
            break Ok(());
        }
        match status {
            1 | 2 => break Ok(()), // finished or explicitly stopped by a callback
            3 | 4 => {}            // slice elapsed or one source was handled
            _ => break Err(NativeReplayError::StreamUnavailable),
        }
    };
    unsafe {
        fs_event::FSEventStreamStop(stream);
        fs_event::FSEventStreamInvalidate(stream);
        fs_event::FSEventStreamRelease(stream);
    }
    let context = unsafe { Box::from_raw(context) };
    run_loop_result?;
    let final_metadata = fs::metadata(&root).map_err(|_| NativeReplayError::RootChanged)?;
    if physical_root_identity(&root, &final_metadata).as_ref() != Some(&expected_root)
        || fs::canonicalize(&root).ok().as_deref() != Some(root.as_path())
        || final_metadata.dev() != metadata.dev()
        || volume_uuid(device).ok() != Some(uuid)
    {
        context.ingress.mark_reason(ReplayUncertainty::ROOT_CHANGED);
    }
    Ok(context.ingress.into_candidate())
}

fn physical_root_identity(root: &Path, metadata: &fs::Metadata) -> Option<RootIdentity> {
    stable_filesystem_identity(root, metadata)
        .map(|identity| RootIdentity::from_bytes(identity.into_bytes()))
}

fn find_volume_root(root: &Path, device: u64) -> Result<PathBuf, NativeReplayError> {
    let mut candidate = root;
    while let Some(parent) = candidate.parent() {
        if parent == candidate
            || fs::metadata(parent)
                .map_err(|_| NativeReplayError::RootChanged)?
                .dev()
                != device
        {
            break;
        }
        candidate = parent;
    }
    Ok(candidate.to_path_buf())
}

fn volume_uuid(device: libc::dev_t) -> Result<[u8; 16], NativeReplayError> {
    let cf_uuid = unsafe { FSEventsCopyUUIDForDevice(device) };
    if cf_uuid.is_null() {
        return Err(NativeReplayError::VolumeHistoryUnavailable);
    }
    let cf_string = unsafe { CFUUIDCreateString(ptr::null_mut(), cf_uuid) };
    unsafe { cf::CFRelease(cf_uuid) };
    if cf_string.is_null() {
        return Err(NativeReplayError::VolumeHistoryUnavailable);
    }
    let mut bytes = [0i8; 64];
    let converted = unsafe {
        cf::CFStringGetCString(cf_string, bytes.as_mut_ptr(), 64, cf::kCFStringEncodingUTF8)
    };
    unsafe { cf::CFRelease(cf_string) };
    if !converted {
        return Err(NativeReplayError::VolumeHistoryUnavailable);
    }
    let spelling = unsafe { CStr::from_ptr(bytes.as_ptr()) }
        .to_str()
        .map_err(|_| NativeReplayError::VolumeHistoryUnavailable)?;
    let parsed =
        uuid::Uuid::parse_str(spelling).map_err(|_| NativeReplayError::VolumeHistoryUnavailable)?;
    let result = *parsed.as_bytes();
    if result == [0; 16] {
        return Err(NativeReplayError::VolumeHistoryUnavailable);
    }
    Ok(result)
}

unsafe fn create_stream(
    root: &Path,
    volume_root: &Path,
    device: libc::dev_t,
    after_event_id: u64,
    context: *mut CallbackContext,
) -> Option<fs_event::FSEventStreamRef> {
    let relative_root = root.strip_prefix(volume_root).ok()?;
    let relative_root = Path::new("/").join(relative_root);
    let bytes = relative_root.as_os_str().as_bytes();
    let relative = CString::new(bytes).ok()?;
    let cf_path = unsafe {
        cf::CFStringCreateWithCString(
            ptr::null_mut(),
            relative.as_ptr(),
            cf::kCFStringEncodingUTF8,
        )
    };
    if cf_path.is_null() {
        return None;
    }
    let paths = unsafe { cf::CFArrayCreateMutable(ptr::null_mut(), 1, &cf::kCFTypeArrayCallBacks) };
    if paths.is_null() {
        unsafe { cf::CFRelease(cf_path) };
        return None;
    }
    unsafe {
        cf::CFArrayAppendValue(paths, cf_path);
        cf::CFRelease(cf_path);
    }
    let stream_context = fs_event::FSEventStreamContext {
        version: 0,
        info: context.cast(),
        retain: None,
        release: None,
        copy_description: None,
    };
    let stream = unsafe {
        fs_event::FSEventStreamCreateRelativeToDevice(
            ptr::null_mut(),
            callback,
            &stream_context,
            device,
            paths,
            after_event_id,
            0.0,
            fs_event::kFSEventStreamCreateFlagFileEvents
                | fs_event::kFSEventStreamCreateFlagNoDefer
                | fs_event::kFSEventStreamCreateFlagWatchRoot,
        )
    };
    unsafe { cf::CFRelease(paths) };
    (!stream.is_null()).then_some(stream)
}

extern "C" fn callback(
    _stream: fs_event::FSEventStreamRef,
    info: *mut c_void,
    count: usize,
    raw_paths: *mut c_void,
    flags: *const fs_event::FSEventStreamEventFlags,
    ids: *const fs_event::FSEventStreamEventId,
) {
    if info.is_null() {
        return;
    }
    let context = unsafe { &mut *info.cast::<CallbackContext>() };
    if count > MAX_REPLAY_EVENTS.saturating_sub(context.seen) {
        context.ingress.mark_reason(ReplayUncertainty::EVENT_LIMIT);
        request_capture_stop(context);
    }
    if count != 0 && (raw_paths.is_null() || flags.is_null() || ids.is_null()) {
        context
            .ingress
            .mark_reason(ReplayUncertainty::UNKNOWN_EVENT);
        return;
    }
    let bounded = count.min(MAX_REPLAY_EVENTS.saturating_sub(context.seen));
    for index in 0..bounded {
        let event_flags = unsafe { *flags.add(index) };
        let event_id = unsafe { *ids.add(index) };
        let is_done = event_flags & fs_event::kFSEventStreamEventFlagHistoryDone != 0;
        let path = if is_done {
            Path::new("/")
        } else {
            let pointer = unsafe { *raw_paths.cast::<*const libc::c_char>().add(index) };
            if pointer.is_null() {
                context.ingress.mark_reason(ReplayUncertainty::INVALID_PATH);
                continue;
            }
            let length =
                unsafe { libc::strnlen(pointer, crate::replay::MAX_REPLAY_PATH_BYTES + 1) };
            if length > crate::replay::MAX_REPLAY_PATH_BYTES {
                context.ingress.mark_reason(ReplayUncertainty::PATH_LIMIT);
                request_capture_stop(context);
                continue;
            }
            let native = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
            Path::new(std::ffi::OsStr::from_bytes(native))
        };
        record_callback_event(
            &context.ingress,
            &context.volume_root,
            MacosReplayEvent {
                event_id,
                volume_relative_path: path,
                flags: event_flags,
            },
        );
        if is_done {
            context.history_done = true;
            if context.stop_after_history {
                request_capture_stop(context);
            }
        }
    }
    context.seen += bounded;
}

fn request_capture_stop(context: &mut CallbackContext) {
    context.stop_requested = true;
    // SAFETY: This callback runs on the stream's owning run-loop thread.
    unsafe { cf::CFRunLoopStop(context.run_loop) };
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> cf::CFRef;
    fn FSEventsGetLastEventIdForDeviceBeforeTime(device: libc::dev_t, time: f64) -> u64;
    fn FSEventStreamFlushSync(stream: fs_event::FSEventStreamRef);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDCreateString(allocator: cf::CFAllocatorRef, uuid: cf::CFRef) -> cf::CFStringRef;
    fn CFRetain(value: cf::CFRef) -> cf::CFRef;
    fn CFRunLoopRunInMode(
        mode: cf::CFStringRef,
        seconds: f64,
        return_after_source_handled: u8,
    ) -> i32;
}

#[cfg(test)]
pub(in crate::replay) fn capture_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static CAPTURE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    CAPTURE_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use yield_library::sample_sources::SourceId;
    use yield_library::sample_sources::reconciliation::WatcherGeneration;

    #[test]
    fn captures_native_history_for_a_disposable_source() {
        let _capture_guard = capture_test_guard();
        let temp = tempfile::tempdir().expect("temporary source");
        let root = fs::canonicalize(temp.path()).expect("canonical source");
        let metadata = fs::metadata(&root).expect("source metadata");
        let identity = physical_root_identity(&root, &metadata).expect("physical root identity");
        let spec = SourceWatchSpec::new(
            SourceId::from_string("native-history"),
            1,
            root,
            Some(identity),
        );
        let seed = BootstrapCapture::start(spec.clone(), Duration::from_secs(5))
            .expect("device-local seed stream");
        fs::write(temp.path().join("seed.wav"), b"seed").expect("seed event");
        let seed = seed.finish(Duration::from_secs(5)).expect("seed capture");
        let since = seed.last_seen().expect("device-local event cursor");
        fs::write(temp.path().join("kick.wav"), b"sample").expect("changed sample");
        // FSEvents can publish a just-written file after an initial HistoryDone callback.
        // A later capture must still replay it from the exact device-local cursor.
        let deadline = Instant::now() + Duration::from_secs(5);
        let candidate = loop {
            let candidate = capture_history(spec.clone(), since, Duration::from_secs(5))
                .expect("native history capture");
            if candidate
                .events()
                .iter()
                .any(|event| event.relative_path() == Path::new("kick.wav"))
            {
                break candidate;
            }
            assert!(Instant::now() < deadline, "{candidate:?}");
            std::thread::sleep(Duration::from_millis(100));
        };
        assert_eq!(candidate.started_after(), since);
        assert!(candidate.history_complete());
        assert!(
            candidate
                .events()
                .iter()
                .any(|event| event.relative_path() == Path::new("kick.wav")),
            "{candidate:?}"
        );
        assert!(
            candidate.uncertainty().is_empty()
                || candidate
                    .uncertainty()
                    .contains(ReplayUncertainty::MUST_SCAN_SUBDIRS),
            "{candidate:?}"
        );
    }

    #[test]
    fn native_history_overflow_is_uncertain_and_releases_worker_slot() {
        let _capture_guard = capture_test_guard();
        let temp = tempfile::tempdir().expect("temporary source");
        let root = fs::canonicalize(temp.path()).expect("canonical source");
        let metadata = fs::metadata(&root).expect("source metadata");
        let identity = physical_root_identity(&root, &metadata).expect("physical root identity");
        let spec = SourceWatchSpec::new(
            SourceId::from_string("native-history-overflow"),
            1,
            root,
            Some(identity),
        );

        let seed = BootstrapCapture::start(spec.clone(), Duration::from_secs(5))
            .expect("device-local seed stream");
        fs::write(temp.path().join("seed.wav"), b"seed").expect("seed event");
        let seed = seed.finish(Duration::from_secs(5)).expect("seed capture");
        let since = seed.last_seen().expect("device-local event cursor");

        for index in 0..=(MAX_REPLAY_EVENTS * 2) {
            let path = temp.path().join(format!("overflow-{index:04}.wav"));
            fs::write(path, b"native filesystem change").expect("overflow source event");
        }

        let candidate = capture_history(spec, since, Duration::from_secs(10))
            .expect("native overflow history capture");
        assert_eq!(candidate.started_after(), since);
        assert!(
            candidate.events().len() <= MAX_REPLAY_EVENTS,
            "{candidate:?}"
        );
        assert!(
            candidate.events().len() > 0,
            "native capture retained no source events: {candidate:?}"
        );
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::EVENT_LIMIT),
            "{candidate:?}"
        );
        assert!(
            candidate
                .into_bootstrap_evidence(WatcherGeneration::new(1))
                .is_none(),
            "overflow history must not produce bootstrap continuity evidence"
        );
        assert!(
            !CAPTURE_ACTIVE.load(Ordering::Acquire),
            "native worker slot remained occupied after capture returned"
        );
    }

    #[test]
    fn live_bootstrap_capture_starts_before_audit_and_flushes_changes() {
        let _capture_guard = capture_test_guard();
        let temp = tempfile::tempdir().expect("temporary source");
        let root = fs::canonicalize(temp.path()).expect("canonical source");
        let metadata = fs::metadata(&root).expect("source metadata");
        let identity = physical_root_identity(&root, &metadata).expect("physical root identity");
        let spec = SourceWatchSpec::new(
            SourceId::from_string("native-bootstrap"),
            1,
            root,
            Some(identity),
        );
        let capture = BootstrapCapture::start(spec, Duration::from_secs(5))
            .expect("live stream ready before the audit boundary");
        fs::write(temp.path().join("kick.wav"), b"sample").expect("audit-overlap mutation");
        let candidate = capture
            .finish(Duration::from_secs(5))
            .expect("bounded live capture");
        assert!(candidate.history_complete(), "{candidate:?}");
        assert!(
            candidate
                .events()
                .iter()
                .any(|event| event.relative_path() == Path::new("kick.wav")),
            "{candidate:?}"
        );
    }

    #[test]
    fn live_bootstrap_capture_rejects_root_replacement() {
        let _capture_guard = capture_test_guard();
        let temp = tempfile::tempdir().expect("temporary parent");
        let root = temp.path().join("source");
        fs::create_dir(&root).expect("source root");
        let root = fs::canonicalize(root).expect("canonical source");
        let metadata = fs::metadata(&root).expect("source metadata");
        let identity = physical_root_identity(&root, &metadata).expect("physical root identity");
        let spec = SourceWatchSpec::new(
            SourceId::from_string("replaced-bootstrap"),
            1,
            root.clone(),
            Some(identity),
        );
        let capture =
            BootstrapCapture::start(spec, Duration::from_secs(5)).expect("live stream ready");
        fs::rename(&root, temp.path().join("old-source")).expect("move old root");
        fs::create_dir(&root).expect("replacement root");
        let outcome = capture.finish(Duration::from_secs(5));
        assert!(
            matches!(outcome, Err(NativeReplayError::RootChanged))
                || matches!(outcome, Ok(ref candidate) if candidate.uncertainty().contains(ReplayUncertainty::ROOT_CHANGED)),
            "{outcome:?}"
        );
    }

    /// Deliberately run on a macOS host when refreshing the checked-in, redacted shapes.
    /// Native event IDs and callback grouping vary, so this prints only stable action labels,
    /// ordinals, and source-relative paths for review instead of rewriting the fixture.
    #[test]
    #[ignore = "manual disposable-source FSEvents capture for Finder fixture review"]
    fn capture_disposable_action_shapes_for_fixture_review() {
        let _capture_guard = capture_test_guard();
        let temp = tempfile::tempdir().expect("disposable capture root");
        let root = fs::canonicalize(temp.path()).expect("canonical capture root");
        let metadata = fs::metadata(&root).expect("capture root metadata");
        let identity = physical_root_identity(&root, &metadata).expect("physical root identity");
        let spec = SourceWatchSpec::new(
            SourceId::from_string("finder-fixture-capture"),
            1,
            root.clone(),
            Some(identity),
        );
        let capture_action = |action: &str, expected: &Path, mutate: &mut dyn FnMut()| {
            let capture = BootstrapCapture::start(spec.clone(), Duration::from_secs(5))
                .expect("isolated native stream");
            mutate();
            let mut candidate = capture
                .finish(Duration::from_secs(5))
                .expect("bounded native capture");
            // FSEvents may publish a just-completed action after HistoryDone.
            let after = candidate.started_after();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !candidate
                .events()
                .iter()
                .any(|event| event.relative_path() == expected)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(100));
                candidate = capture_history(spec.clone(), after, Duration::from_secs(5))
                    .expect("bounded native history retry");
            }
            assert!(candidate.history_complete(), "{action} history incomplete");
            for (ordinal, event) in candidate.events().iter().enumerate() {
                let relative = event.relative_path();
                assert!(
                    !relative.is_absolute(),
                    "{action} retained an absolute path"
                );
                assert!(
                    relative
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_)))
                );
                eprintln!(
                    "{action}\t{}\t{}\t{:?}",
                    ordinal + 1,
                    relative.display(),
                    event.kind()
                );
            }
            assert!(
                candidate
                    .events()
                    .iter()
                    .any(|event| event.relative_path() == expected),
                "{action} did not capture its disposable target"
            );
        };
        capture_action("create", Path::new("kick.wav"), &mut || {
            fs::write(root.join("kick.wav"), b"sample").expect("create disposable sample");
        });
        capture_action("duplicate", Path::new("kick copy.wav"), &mut || {
            fs::copy(root.join("kick.wav"), root.join("kick copy.wav"))
                .expect("duplicate disposable sample");
        });
        capture_action("rename", Path::new("snare.wav"), &mut || {
            fs::rename(root.join("kick copy.wav"), root.join("snare.wav"))
                .expect("rename disposable sample");
        });
        capture_action("reparent", Path::new("drums/snare.wav"), &mut || {
            fs::create_dir(root.join("drums")).expect("create disposable parent");
            fs::rename(root.join("snare.wav"), root.join("drums/snare.wav"))
                .expect("move disposable sample");
        });
        capture_action("delete", Path::new("drums/snare.wav"), &mut || {
            fs::remove_file(root.join("drums/snare.wav")).expect("remove disposable sample");
        });
        capture_action("empty-folder", Path::new("empty"), &mut || {
            fs::create_dir(root.join("empty")).expect("create empty disposable folder");
        });
        capture_action(
            "unsupported-only",
            Path::new("unsupported-only"),
            &mut || {
                fs::create_dir(root.join("unsupported-only"))
                    .expect("create unsupported-only disposable folder");
                fs::write(root.join("unsupported-only/notes.txt"), b"notes")
                    .expect("create unsupported disposable file");
            },
        );
    }
}
