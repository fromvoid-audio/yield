//! One-shot, per-device historical FSEvents capture. The worker owns its stream and context.

use std::ffi::{CStr, CString, c_void};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

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
    let spawned = std::thread::Builder::new()
        .name("yield-fsevents-history".into())
        .spawn(move || {
            let active = ActiveCapture;
            let result = capture_on_worker(spec, after_event_id, run_loop_tx);
            drop(active);
            let _ = result_tx.send(result);
        });
    if spawned.is_err() {
        CAPTURE_ACTIVE.store(false, Ordering::Release);
        return Err(NativeReplayError::WorkerUnavailable);
    }
    let run_loop =
        match run_loop_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(run_loop) => run_loop,
            Err(RecvTimeoutError::Timeout) => return Err(NativeReplayError::TimedOut),
            Err(RecvTimeoutError::Disconnected) => {
                return result_rx
                    .recv()
                    .unwrap_or(Err(NativeReplayError::WorkerUnavailable));
            }
        };
    let result = result_rx.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    if result.is_err() {
        // SAFETY: CoreFoundation permits stopping another thread's retained run loop.
        unsafe { cf::CFRunLoopStop(run_loop.0) };
    }
    // SAFETY: The worker retained this run loop specifically for the caller's one release.
    unsafe { cf::CFRelease(run_loop.0) };
    match result {
        Ok(result) => result,
        Err(_) => Err(NativeReplayError::TimedOut),
    }
}

struct ActiveCapture;

impl Drop for ActiveCapture {
    fn drop(&mut self) {
        CAPTURE_ACTIVE.store(false, Ordering::Release);
    }
}

struct RunLoopHandle(cf::CFRunLoopRef);

// SAFETY: The caller only calls CFRunLoopStop and CFRelease on a retained CoreFoundation object.
unsafe impl Send for RunLoopHandle {}

struct CallbackContext {
    ingress: ReplayIngress,
    volume_root: PathBuf,
    run_loop: cf::CFRunLoopRef,
    seen: usize,
}

fn capture_on_worker(
    spec: SourceWatchSpec,
    after_event_id: u64,
    run_loop_tx: mpsc::SyncSender<RunLoopHandle>,
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
            cf::CFRelease(retained);
            fs_event::FSEventStreamStop(stream);
            fs_event::FSEventStreamInvalidate(stream);
            fs_event::FSEventStreamRelease(stream);
            drop(Box::from_raw(context));
        }
        return Err(NativeReplayError::TimedOut);
    }
    unsafe { cf::CFRunLoopRun() };
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
        unsafe { cf::CFRunLoopStop(context.run_loop) };
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
                unsafe { cf::CFRunLoopStop(context.run_loop) };
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
            unsafe { cf::CFRunLoopStop(context.run_loop) };
        }
    }
    context.seen += bounded;
}

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventsCopyUUIDForDevice(device: libc::dev_t) -> cf::CFRef;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDCreateString(allocator: cf::CFAllocatorRef, uuid: cf::CFRef) -> cf::CFStringRef;
    fn CFRetain(value: cf::CFRef) -> cf::CFRef;
}

#[cfg(test)]
mod tests {
    use super::*;
    use yield_library::sample_sources::SourceId;

    #[test]
    fn captures_native_history_for_a_disposable_source() {
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
        let since = unsafe { fs_event::FSEventsGetCurrentEventId() };
        fs::write(temp.path().join("kick.wav"), b"sample").expect("changed sample");
        let candidate =
            capture_history(spec, since, Duration::from_secs(5)).expect("native history capture");
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
}
