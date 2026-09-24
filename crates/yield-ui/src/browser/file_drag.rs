//! Bounded whole-file drag preparation and native-session lifecycle.
//!
//! Row callbacks only capture stable row identities. Filesystem validation runs on the
//! background executor, and the platform adapter is entered from a live foreground task after
//! the preparation and projection fences have been checked again. On Windows, the final GPUI
//! update extracts owned inputs before the blocking OLE call. The resulting path handoff is still
//! subject to the operating system's later path-open TOCTOU window.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;

use gpui::{
    Context, DragMoveEvent, Entity, MouseButton, MouseDownEvent, MouseUpEvent, Render, Window, div,
    prelude::*,
};
use yield_core::{AudioFile, AudioFileId, BrowserRow, BrowserSelection};
use yield_library::sample_sources::reconciliation::RootIdentity;
use yield_platform::{
    FileDragCompletion, FileDragError, FileDragHandle, MAX_FILE_DRAG_FILES,
    MAX_FILE_DRAG_PATH_BYTES,
};
use yield_session::resolve_source_root;

use super::Browser;

/// The small value attached to one visible browser row while GPUI recognizes a drag gesture.
///
/// It intentionally carries no paths or selected-file list. The current projection and selection
/// are captured only once the drag threshold is crossed.
#[derive(Clone, Debug)]
pub(super) struct FileDragRowPayload {
    pub(super) browser: Entity<Browser>,
    pub(super) visible_index: usize,
    pub(super) entry_index: usize,
    pub(super) id: AudioFileId,
    pub(super) projection_generation: u64,
    pub(super) preview_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DragTarget {
    entry_index: usize,
    id: AudioFileId,
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProjectionFence {
    handoff_generation: u64,
    scan_generation: u64,
    source_id: Option<yield_library::sample_sources::SourceId>,
    lifecycle_generation: Option<u64>,
    root: Option<PathBuf>,
    root_identity: Option<RootIdentity>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DragAnchor {
    visible_index: usize,
    entry_index: usize,
    id: AudioFileId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DragIntent {
    anchor: DragAnchor,
    anchor_selected: bool,
    targets: Vec<DragTarget>,
    fence: ProjectionFence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedDrag {
    intent: DragIntent,
    /// Paths were copied once after background validation so transient native-event retries do
    /// not rebuild a bounded path vector on the UI thread.
    paths: Vec<PathBuf>,
}

struct RunningPreparation {
    request_id: u64,
    gesture_id: u64,
    cancellation: Arc<AtomicBool>,
}

struct NativeDrag {
    handle: FileDragHandle,
    count: usize,
}

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowsNativePhase {
    Queued,
    Admitted,
}

#[cfg(any(target_os = "windows", test))]
struct WindowsNativeDrag {
    request_id: u64,
    gesture_id: u64,
    count: usize,
    phase: WindowsNativePhase,
    cancellation: Arc<AtomicBool>,
}

/// Browser-owned state for one bounded outbound native file drag.
pub(super) struct FileDragState {
    next_request_id: u64,
    next_gesture_id: u64,
    running: Option<RunningPreparation>,
    queued: Option<DragIntent>,
    ready: Option<PreparedDrag>,
    native: Option<NativeDrag>,
    #[cfg(any(target_os = "windows", test))]
    windows_native: Option<WindowsNativeDrag>,
    gesture_active: bool,
    active_gesture_id: Option<u64>,
    /// The GPUI ghost can outlive `gesture_active` when a source invalidation has
    /// no Window available to stop it immediately. Keep its originating gesture
    /// fenced until a matching deferred abort or the next Window event drains it.
    pending_ghost_gesture_id: Option<u64>,
    task: Option<gpui::Task<()>>,
}

impl Default for FileDragState {
    fn default() -> Self {
        Self {
            next_request_id: 0,
            next_gesture_id: 0,
            running: None,
            queued: None,
            ready: None,
            native: None,
            #[cfg(any(target_os = "windows", test))]
            windows_native: None,
            gesture_active: false,
            active_gesture_id: None,
            pending_ghost_gesture_id: None,
            task: None,
        }
    }
}

impl FileDragState {
    fn queue_latest(&mut self, intent: DragIntent) {
        if let Some(running) = self.running.as_ref() {
            running.cancellation.store(true, Ordering::Release);
        }
        self.queued = Some(intent);
    }

    fn cancel_unadmitted(&mut self) {
        if let Some(running) = self.running.as_ref() {
            running.cancellation.store(true, Ordering::Release);
        }
        #[cfg(any(target_os = "windows", test))]
        if let Some(native) = self.windows_native.as_ref() {
            native.cancellation.store(true, Ordering::Release);
        }
        self.queued = None;
        self.ready = None;
        self.gesture_active = false;
        self.active_gesture_id = None;
    }

    fn has_native_drag(&self) -> bool {
        if self.native.is_some() {
            return true;
        }
        #[cfg(any(target_os = "windows", test))]
        if self.windows_native.is_some() {
            return true;
        }
        false
    }

    fn mark_pending_ghost(&mut self, gesture_id: u64) {
        self.pending_ghost_gesture_id = Some(gesture_id);
    }

    fn pending_ghost_gesture_id(&self) -> Option<u64> {
        self.pending_ghost_gesture_id
    }

    fn take_pending_ghost(&mut self, gesture_id: u64) -> bool {
        if self.pending_ghost_gesture_id == Some(gesture_id) {
            self.pending_ghost_gesture_id = None;
            true
        } else {
            false
        }
    }

    #[cfg(any(target_os = "windows", test))]
    fn take_windows_native(
        &mut self,
        request_id: u64,
        gesture_id: u64,
    ) -> Option<WindowsNativeDrag> {
        let matches = self.windows_native.as_ref().is_some_and(|native| {
            native.request_id == request_id && native.gesture_id == gesture_id
        });
        matches.then(|| self.windows_native.take()).flatten()
    }

    #[cfg(any(target_os = "windows", test))]
    fn windows_release_should_cancel(&self) -> bool {
        !self
            .windows_native
            .as_ref()
            .is_some_and(|native| native.phase == WindowsNativePhase::Admitted)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CaptureError {
    NoSource,
    ProjectionPending,
    StaleRow,
    TooMany { actual: usize },
    PathBytesTooLarge { actual: usize },
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSource => formatter.write_str("choose a source before dragging files"),
            Self::ProjectionPending => formatter.write_str("the visible file list is updating"),
            Self::StaleRow => formatter.write_str("the selected file list changed"),
            Self::TooMany { actual } => write!(
                formatter,
                "file drag contains {actual} files; limit is {MAX_FILE_DRAG_FILES}"
            ),
            Self::PathBytesTooLarge { actual } => write!(
                formatter,
                "file drag paths contain {actual} bytes; limit is {MAX_FILE_DRAG_PATH_BYTES}"
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PreparationError {
    Canceled,
    RootChanged,
    OutsideRoot,
    MissingFile,
    NonRegularFile,
    RelativePath,
    TooMany { actual: usize },
    PathBytesTooLarge { actual: usize },
}

impl std::fmt::Display for PreparationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canceled => formatter.write_str("file drag canceled"),
            Self::RootChanged => formatter.write_str("source changed while preparing the drag"),
            Self::OutsideRoot => {
                formatter.write_str("a dragged file resolves outside the source root")
            }
            Self::MissingFile => formatter.write_str("one or more dragged files disappeared"),
            Self::NonRegularFile => formatter.write_str("a dragged entry is not a regular file"),
            Self::RelativePath => formatter.write_str("a dragged path is not absolute"),
            Self::TooMany { actual } => write!(
                formatter,
                "file drag contains {actual} files; limit is {MAX_FILE_DRAG_FILES}"
            ),
            Self::PathBytesTooLarge { actual } => write!(
                formatter,
                "file drag paths contain {actual} bytes; limit is {MAX_FILE_DRAG_PATH_BYTES}"
            ),
        }
    }
}

impl Browser {
    pub(super) fn file_drag_row_payload(
        &self,
        browser: Entity<Browser>,
        visible_index: usize,
        row: &BrowserRow,
    ) -> FileDragRowPayload {
        FileDragRowPayload {
            browser,
            visible_index,
            entry_index: row.entry_index,
            id: row.id.clone(),
            projection_generation: self.handoff_generation,
            preview_count: if self.selection.contains(&row.id) {
                self.selection.len().max(1)
            } else {
                1
            },
        }
    }

    pub(super) fn begin_file_drag(
        &mut self,
        payload: FileDragRowPayload,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_drag.has_native_drag() {
            self.playback_status = "A native file drag is still active".to_string();
            cx.notify();
            return;
        }
        let intent = match capture_intent(self, &payload) {
            Ok(intent) => intent,
            Err(error) => {
                self.playback_status = format!("File drag unavailable: {error}");
                cx.notify();
                return;
            }
        };

        self.file_drag.gesture_active = true;
        if self.file_drag.active_gesture_id.is_none() {
            self.file_drag.next_gesture_id = self.file_drag.next_gesture_id.wrapping_add(1);
            self.file_drag.active_gesture_id = Some(self.file_drag.next_gesture_id);
        }
        let Some(gesture_id) = self.file_drag.active_gesture_id else {
            return;
        };
        if let Some(previous_ghost) = self.file_drag.pending_ghost_gesture_id()
            && previous_ghost != gesture_id
        {
            self.file_drag.take_pending_ghost(previous_ghost);
            cx.stop_active_drag(window);
        }
        self.file_drag.mark_pending_ghost(gesture_id);
        if self.file_drag.running.is_some() {
            self.file_drag.queue_latest(intent);
            self.playback_status = "Preparing the latest file selection…".to_string();
            cx.notify();
            return;
        }
        self.start_file_drag_preparation(intent, window, cx);
    }

    fn start_file_drag_preparation(
        &mut self,
        intent: DragIntent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_drag.next_request_id = self.file_drag.next_request_id.wrapping_add(1);
        let request_id = self.file_drag.next_request_id;
        let Some(gesture_id) = self.file_drag.active_gesture_id else {
            return;
        };
        let cancellation = Arc::new(AtomicBool::new(false));
        self.file_drag.running = Some(RunningPreparation {
            request_id,
            gesture_id,
            cancellation: Arc::clone(&cancellation),
        });
        self.file_drag.ready = None;
        self.playback_status = if intent.targets.len() == 1 {
            "Preparing file drag…".to_string()
        } else {
            format!("Preparing {} file drag items…", intent.targets.len())
        };
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { prepare_drag(intent, cancellation) })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                view.finish_file_drag_preparation(request_id, result, window, cx);
            });
        });
        self.file_drag.task = Some(task);
        cx.notify();
    }

    fn finish_file_drag_preparation(
        &mut self,
        request_id: u64,
        result: Result<PreparedDrag, PreparationError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(running) = self.file_drag.running.take() else {
            return;
        };
        if running.request_id != request_id {
            self.file_drag.running = Some(running);
            return;
        }
        let gesture_id = running.gesture_id;
        self.file_drag.task.take();
        if running.cancellation.load(Ordering::Acquire) {
            self.finish_canceled_file_drag(gesture_id, window, cx);
            return;
        }
        match result {
            Ok(prepared) => {
                if let Some(queued) = self.file_drag.queued.take() {
                    if self.file_drag.gesture_active {
                        self.start_file_drag_preparation(queued, window, cx);
                    }
                    return;
                }
                if self.file_drag.active_gesture_id != Some(gesture_id) {
                    return;
                }
                self.file_drag.ready = Some(prepared);
                self.try_admit_file_drag(window, cx);
            }
            Err(PreparationError::Canceled) => {
                self.finish_canceled_file_drag(gesture_id, window, cx)
            }
            Err(error) => {
                let owns_gesture = self.file_drag.active_gesture_id == Some(gesture_id);
                let owns_ghost = self.file_drag.pending_ghost_gesture_id() == Some(gesture_id);
                if owns_gesture {
                    self.file_drag.ready = None;
                    self.file_drag.queued = None;
                    self.clear_file_drag_gesture();
                    self.playback_status = format!("File drag unavailable: {error}");
                    cx.notify();
                }
                if owns_ghost && !self.file_drag.has_native_drag() {
                    self.stop_owned_file_drag_ghost(gesture_id, window, cx);
                }
            }
        }
    }

    fn finish_canceled_file_drag(
        &mut self,
        gesture_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_drag.ready = None;
        if let Some(queued) = self.file_drag.queued.take()
            && self.file_drag.gesture_active
        {
            self.start_file_drag_preparation(queued, window, cx);
            return;
        }
        let owns_gesture = self.file_drag.active_gesture_id == Some(gesture_id);
        let owns_ghost = self.file_drag.pending_ghost_gesture_id() == Some(gesture_id);
        if owns_gesture {
            self.clear_file_drag_gesture();
        }
        if owns_ghost && !self.file_drag.has_native_drag() {
            self.stop_owned_file_drag_ghost(gesture_id, window, cx);
        }
    }

    fn try_admit_file_drag(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.file_drag.gesture_active || !cx.has_active_drag() {
            return;
        }
        let Some(prepared) = self.file_drag.ready.take() else {
            return;
        };
        if !prepared_intent_is_current(self, &prepared.intent) {
            let gesture_id = self.file_drag.active_gesture_id;
            self.clear_file_drag_gesture();
            self.playback_status = "File drag canceled: the file list changed".to_string();
            if let Some(gesture_id) = gesture_id {
                self.stop_owned_file_drag_ghost(gesture_id, window, cx);
            }
            cx.notify();
            return;
        }

        #[cfg(target_os = "windows")]
        {
            self.schedule_windows_file_drag(prepared, window, cx);
        }

        #[cfg(not(target_os = "windows"))]
        {
            let window_handle = match raw_window_handle::HasWindowHandle::window_handle(window) {
                Ok(handle) => handle,
                Err(_) => {
                    let gesture_id = self.file_drag.active_gesture_id;
                    self.clear_file_drag_gesture();
                    self.playback_status =
                        "File drag unavailable: window handle is unavailable".to_string();
                    if let Some(gesture_id) = gesture_id {
                        self.stop_owned_file_drag_ghost(gesture_id, window, cx);
                    }
                    cx.notify();
                    return;
                }
            };
            match yield_platform::start_file_drag(window_handle, &prepared.paths) {
                Ok(handle) => {
                    let count = prepared.paths.len();
                    self.file_drag.native = Some(NativeDrag { handle, count });
                    let gesture_id = self.file_drag.active_gesture_id;
                    self.clear_file_drag_gesture();
                    if let Some(gesture_id) = gesture_id {
                        self.stop_owned_file_drag_ghost(gesture_id, window, cx);
                    }
                    self.start_source_registry_poll(cx);
                    self.playback_status =
                        format!("Dragging {count} file{}", if count == 1 { "" } else { "s" });
                    cx.notify();
                }
                Err(error) if retryable_platform_error(&error) && cx.has_active_drag() => {
                    self.file_drag.ready = Some(prepared);
                }
                Err(error) => {
                    let gesture_id = self.file_drag.active_gesture_id;
                    self.clear_file_drag_gesture();
                    self.playback_status = format!("File drag unavailable: {error}");
                    if let Some(gesture_id) = gesture_id {
                        self.stop_owned_file_drag_ghost(gesture_id, window, cx);
                    }
                    cx.notify();
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn schedule_windows_file_drag(
        &mut self,
        prepared: PreparedDrag,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.file_drag.windows_native.is_some() {
            self.file_drag.ready = Some(prepared);
            return;
        }
        let Some(gesture_id) = self.file_drag.active_gesture_id else {
            return;
        };
        self.file_drag.next_request_id = self.file_drag.next_request_id.wrapping_add(1);
        let request_id = self.file_drag.next_request_id;
        let PreparedDrag { intent, paths } = prepared;
        let count = paths.len();
        let cancellation = Arc::new(AtomicBool::new(false));
        self.file_drag.windows_native = Some(WindowsNativeDrag {
            request_id,
            gesture_id,
            count,
            phase: WindowsNativePhase::Queued,
            cancellation: Arc::clone(&cancellation),
        });

        // `spawn_in` schedules this future on GPUI's foreground executor. The first
        // `update_in` is intentionally limited to a final UI fence and extraction of
        // owned native inputs. The synchronous OLE call happens only after that borrow
        // has ended, and therefore never runs inside an App/Window/Context callback.
        let task = cx.spawn_in(window, async move |this, cx| {
            let plan = match this.update_in(cx, |view, window, cx| {
                let Some(native) = view.file_drag.windows_native.as_ref() else {
                    return None;
                };
                if native.request_id != request_id || native.gesture_id != gesture_id {
                    return None;
                }
                let cancellation = Arc::clone(&native.cancellation);
                let current = !cancellation.load(Ordering::Acquire)
                    && view.file_drag.gesture_active
                    && view.file_drag.active_gesture_id == Some(gesture_id)
                    && cx.has_active_drag()
                    && prepared_intent_is_current(view, &intent);
                if !current {
                    view.abort_windows_file_drag(request_id, gesture_id, window, cx);
                    return None;
                }

                // This is the GPUI admission boundary. The native task retains
                // `cancellation` and `paths`; no Browser or Window value crosses it.
                if let Some(native) = view.file_drag.windows_native.as_mut() {
                    native.phase = WindowsNativePhase::Admitted;
                }
                view.clear_file_drag_gesture();
                view.stop_owned_file_drag_ghost(gesture_id, window, cx);
                view.playback_status =
                    format!("Dragging {count} file{}", if count == 1 { "" } else { "s" });
                cx.notify();
                Some((paths, cancellation))
            }) {
                Ok(plan) => plan,
                Err(_) => {
                    let _ = this.update(cx, |view, _cx| {
                        view.abandon_windows_file_drag(request_id, gesture_id);
                    });
                    return;
                }
            };
            let Some((paths, cancellation)) = plan else {
                return;
            };

            let result = yield_platform::start_file_drag_blocking(paths, cancellation);
            let _ = this.update_in(cx, |view, window, cx| {
                view.finish_windows_file_drag(request_id, gesture_id, result, window, cx);
            });
        });
        self.file_drag.task = Some(task);
        cx.notify();
    }

    #[cfg(target_os = "windows")]
    fn abandon_windows_file_drag(&mut self, request_id: u64, gesture_id: u64) {
        if self
            .file_drag
            .take_windows_native(request_id, gesture_id)
            .is_none()
        {
            return;
        }
        self.file_drag.task.take();
        self.file_drag.take_pending_ghost(gesture_id);
        if self.file_drag.active_gesture_id == Some(gesture_id) {
            self.clear_file_drag_gesture();
        }
    }

    #[cfg(target_os = "windows")]
    fn abort_windows_file_drag(
        &mut self,
        request_id: u64,
        gesture_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .file_drag
            .take_windows_native(request_id, gesture_id)
            .is_none()
        {
            return;
        }
        self.file_drag.task.take();
        let owns_ghost = self.file_drag.pending_ghost_gesture_id() == Some(gesture_id);
        let owns_active_drag =
            self.file_drag.gesture_active && self.file_drag.active_gesture_id == Some(gesture_id);
        if owns_ghost {
            self.stop_owned_file_drag_ghost(gesture_id, window, cx);
        }
        if owns_active_drag {
            self.clear_file_drag_gesture();
        }
        if owns_ghost || owns_active_drag {
            self.playback_status = "File drag canceled".to_string();
            cx.notify();
        }
    }

    #[cfg(target_os = "windows")]
    fn finish_windows_file_drag(
        &mut self,
        request_id: u64,
        gesture_id: u64,
        result: Result<FileDragCompletion, FileDragError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(native) = self.file_drag.take_windows_native(request_id, gesture_id) else {
            return;
        };
        let count = native.count;
        self.file_drag.task.take();
        self.stop_owned_file_drag_ghost(gesture_id, window, cx);
        if self.file_drag.active_gesture_id == Some(gesture_id) {
            self.clear_file_drag_gesture();
        }
        self.playback_status = match result {
            Ok(FileDragCompletion::Copy) => format!(
                "Copied {count} dragged file{}",
                if count == 1 { "" } else { "s" }
            ),
            Ok(FileDragCompletion::Cancelled) => "File drag canceled".to_string(),
            Ok(FileDragCompletion::Other) => "File drag finished".to_string(),
            Err(error) => format!("File drag unavailable: {error}"),
        };
        cx.notify();
    }

    pub(super) fn on_file_drag_move(
        &mut self,
        event: &DragMoveEvent<FileDragRowPayload>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(payload) = event.dragged_item().downcast_ref::<FileDragRowPayload>() else {
            return;
        };
        if payload.browser.entity_id() != cx.entity().entity_id() {
            return;
        }
        #[cfg(target_os = "windows")]
        if self.file_drag.windows_native.is_some() {
            return;
        }
        if self.file_drag.gesture_active {
            self.try_admit_file_drag(window, cx);
        } else if let Some(gesture_id) = self.file_drag.pending_ghost_gesture_id() {
            self.stop_owned_file_drag_ghost(gesture_id, window, cx);
        } else {
            cx.stop_active_drag(window);
        }
    }

    pub(super) fn cancel_file_drag_from_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left {
            return;
        }
        self.cancel_file_drag_with_window(window, cx);
    }

    pub(super) fn finish_file_drag_pointer(
        &mut self,
        _event: &MouseUpEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_os = "windows")]
        if self.file_drag.windows_native.is_some()
            && !self.file_drag.windows_release_should_cancel()
        {
            // Once the Windows native task has been admitted, the platform owns
            // button/cancellation handling. In particular, do not tear down the
            // retained token or task merely because GPUI reports pointer release;
            // the adapter rechecks the physical button before entering OLE.
            return;
        }
        self.cancel_file_drag_with_window(window, cx);
    }

    pub(super) fn cancel_file_drag(&mut self) {
        self.file_drag.cancel_unadmitted();
    }

    fn clear_file_drag_gesture(&mut self) {
        self.file_drag.gesture_active = false;
        self.file_drag.active_gesture_id = None;
    }

    fn stop_owned_file_drag_ghost(
        &mut self,
        gesture_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.file_drag.take_pending_ghost(gesture_id) {
            return false;
        }
        cx.stop_active_drag(window);
        true
    }

    pub(super) fn cancel_file_drag_with_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ghost_gesture_id = self.file_drag.pending_ghost_gesture_id();
        self.cancel_file_drag();
        if let Some(gesture_id) = ghost_gesture_id {
            self.stop_owned_file_drag_ghost(gesture_id, window, cx);
        }
    }

    pub(super) fn poll_file_drag(&mut self) -> bool {
        let Some(native) = self.file_drag.native.as_ref() else {
            return false;
        };
        let completion = match native.handle.try_recv() {
            Ok(completion) => Some(completion),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(FileDragCompletion::Cancelled),
        };
        let Some(completion) = completion else {
            return false;
        };
        let count = native.count;
        self.file_drag.native = None;
        self.playback_status = match completion {
            FileDragCompletion::Copy => format!(
                "Copied {count} dragged file{}",
                if count == 1 { "" } else { "s" }
            ),
            FileDragCompletion::Cancelled => "File drag canceled".to_string(),
            FileDragCompletion::Other => "File drag finished".to_string(),
        };
        true
    }

    pub(super) fn file_drag_poll_needed(&self) -> bool {
        self.file_drag.native.is_some()
    }

    pub(super) fn shutdown_file_drag(&mut self) {
        self.cancel_file_drag();
        self.file_drag.task.take();
        self.file_drag.running = None;
        self.file_drag.native = None;
        self.file_drag.pending_ghost_gesture_id = None;
        #[cfg(target_os = "windows")]
        {
            self.file_drag.windows_native = None;
        }
    }
}

fn capture_intent(
    browser: &Browser,
    payload: &FileDragRowPayload,
) -> Result<DragIntent, CaptureError> {
    if super::tag_filter::scope_pending(browser) {
        return Err(CaptureError::ProjectionPending);
    }
    if browser.root.is_none() {
        return Err(CaptureError::NoSource);
    }
    if payload.projection_generation != browser.handoff_generation {
        return Err(CaptureError::StaleRow);
    }
    let Some(row) = browser.view_model.rows().get(payload.visible_index) else {
        return Err(CaptureError::StaleRow);
    };
    if row.entry_index != payload.entry_index || row.id != payload.id {
        return Err(CaptureError::StaleRow);
    }
    let Some(entry) = browser.entries.get(row.entry_index) else {
        return Err(CaptureError::StaleRow);
    };
    if entry.stable_id() != row.id {
        return Err(CaptureError::StaleRow);
    }

    let anchor = DragAnchor {
        visible_index: payload.visible_index,
        entry_index: payload.entry_index,
        id: payload.id.clone(),
    };
    let (anchor_selected, targets) = capture_targets(
        &browser.entries,
        browser.view_model.rows(),
        &browser.selection,
        payload.visible_index,
    )?;
    if targets.is_empty() {
        return Err(CaptureError::StaleRow);
    }
    Ok(DragIntent {
        anchor,
        anchor_selected,
        targets,
        fence: projection_fence(browser),
    })
}

fn capture_targets(
    entries: &[AudioFile],
    rows: &[BrowserRow],
    selection: &BrowserSelection,
    anchor_visible_index: usize,
) -> Result<(bool, Vec<DragTarget>), CaptureError> {
    let Some(anchor_row) = rows.get(anchor_visible_index) else {
        return Err(CaptureError::StaleRow);
    };
    let anchor_selected = selection.contains(&anchor_row.id);
    let mut targets = Vec::new();
    let mut total_bytes = 0usize;
    if anchor_selected {
        for row in rows {
            if selection.contains(&row.id) {
                push_target(&mut targets, &mut total_bytes, entries, row)?;
            }
        }
    } else {
        push_target(&mut targets, &mut total_bytes, entries, anchor_row)?;
    }
    Ok((anchor_selected, targets))
}

fn push_target(
    targets: &mut Vec<DragTarget>,
    total_bytes: &mut usize,
    entries: &[AudioFile],
    row: &BrowserRow,
) -> Result<(), CaptureError> {
    if targets.len() >= MAX_FILE_DRAG_FILES {
        return Err(CaptureError::TooMany {
            actual: targets.len().saturating_add(1),
        });
    }
    let Some(entry) = entries.get(row.entry_index) else {
        return Err(CaptureError::StaleRow);
    };
    if entry.stable_id() != row.id {
        return Err(CaptureError::StaleRow);
    }
    *total_bytes = total_bytes.saturating_add(raw_path_bytes(&entry.path));
    if *total_bytes > MAX_FILE_DRAG_PATH_BYTES {
        return Err(CaptureError::PathBytesTooLarge {
            actual: *total_bytes,
        });
    }
    targets.push(DragTarget {
        entry_index: row.entry_index,
        id: row.id.clone(),
        path: entry.path.clone(),
    });
    Ok(())
}

fn projection_fence(browser: &Browser) -> ProjectionFence {
    let activation = browser.catalog_activation.as_ref();
    ProjectionFence {
        handoff_generation: browser.handoff_generation,
        scan_generation: browser.scan_generation,
        source_id: activation.map(|activation| activation.source_id.clone()),
        lifecycle_generation: activation.map(|activation| activation.lifecycle_generation),
        root: browser.root.clone(),
        root_identity: activation.and_then(|activation| activation.root_identity.clone()),
    }
}

fn prepared_intent_is_current(browser: &Browser, intent: &DragIntent) -> bool {
    if projection_fence(browser) != intent.fence {
        return false;
    }
    let Some(row) = browser.view_model.rows().get(intent.anchor.visible_index) else {
        return false;
    };
    if row.entry_index != intent.anchor.entry_index || row.id != intent.anchor.id {
        return false;
    }
    let Some(entry) = browser.entries.get(row.entry_index) else {
        return false;
    };
    if entry.stable_id() != row.id {
        return false;
    }
    if browser.selection.contains(&intent.anchor.id) != intent.anchor_selected {
        return false;
    }
    targets_match_current(
        &browser.entries,
        browser.view_model.rows(),
        &browser.selection,
        &intent.anchor,
        intent.anchor_selected,
        &intent.targets,
    )
}

fn targets_match_current(
    entries: &[AudioFile],
    rows: &[BrowserRow],
    selection: &BrowserSelection,
    anchor: &DragAnchor,
    anchor_selected: bool,
    targets: &[DragTarget],
) -> bool {
    let Some(row) = rows.get(anchor.visible_index) else {
        return false;
    };
    let Some(entry) = entries.get(row.entry_index) else {
        return false;
    };
    if row.entry_index != anchor.entry_index || row.id != anchor.id || entry.stable_id() != row.id {
        return false;
    }
    if selection.contains(&anchor.id) != anchor_selected {
        return false;
    }
    if !anchor_selected {
        let Some(target) = targets.first() else {
            return false;
        };
        return targets.len() == 1
            && target.entry_index == row.entry_index
            && target.id == row.id
            && target.path == entry.path;
    }
    let mut target_index = 0usize;
    for current_row in rows {
        if !selection.contains(&current_row.id) {
            continue;
        }
        let Some(target) = targets.get(target_index) else {
            return false;
        };
        let Some(current_entry) = entries.get(current_row.entry_index) else {
            return false;
        };
        if target.entry_index != current_row.entry_index
            || target.id != current_row.id
            || target.path != current_entry.path
        {
            return false;
        }
        target_index = target_index.saturating_add(1);
    }
    target_index == targets.len()
}

fn prepare_drag(
    intent: DragIntent,
    cancellation: Arc<AtomicBool>,
) -> Result<PreparedDrag, PreparationError> {
    if cancellation.load(Ordering::Acquire) {
        return Err(PreparationError::Canceled);
    }
    if intent.targets.is_empty() {
        return Err(PreparationError::Canceled);
    }
    if intent.targets.len() > MAX_FILE_DRAG_FILES {
        return Err(PreparationError::TooMany {
            actual: intent.targets.len(),
        });
    }
    let root = intent
        .fence
        .root
        .as_deref()
        .ok_or(PreparationError::RootChanged)?;
    let before = resolve_source_root(root).map_err(|_| PreparationError::RootChanged)?;
    if before.canonical_root() != root
        || intent
            .fence
            .root_identity
            .as_ref()
            .is_some_and(|identity| identity != before.root_identity())
    {
        return Err(PreparationError::RootChanged);
    }

    let mut total_bytes = 0usize;
    for target in &intent.targets {
        if cancellation.load(Ordering::Acquire) {
            return Err(PreparationError::Canceled);
        }
        if !target.path.is_absolute() {
            return Err(PreparationError::RelativePath);
        }
        total_bytes = total_bytes.saturating_add(raw_path_bytes(&target.path));
        if total_bytes > MAX_FILE_DRAG_PATH_BYTES {
            return Err(PreparationError::PathBytesTooLarge {
                actual: total_bytes,
            });
        }
        validate_drag_target(target, before.canonical_root())?;
    }
    if cancellation.load(Ordering::Acquire) {
        return Err(PreparationError::Canceled);
    }
    let after = resolve_source_root(root).map_err(|_| PreparationError::RootChanged)?;
    if after.canonical_root() != before.canonical_root()
        || after.root_identity() != before.root_identity()
        || after.canonical_root() != root
    {
        return Err(PreparationError::RootChanged);
    }
    if cancellation.load(Ordering::Acquire) {
        return Err(PreparationError::Canceled);
    }
    let mut paths = Vec::with_capacity(intent.targets.len());
    for target in &intent.targets {
        if cancellation.load(Ordering::Acquire) {
            return Err(PreparationError::Canceled);
        }
        paths.push(target.path.clone());
    }
    if cancellation.load(Ordering::Acquire) {
        return Err(PreparationError::Canceled);
    }
    for target in &intent.targets {
        if cancellation.load(Ordering::Acquire) {
            return Err(PreparationError::Canceled);
        }
        validate_drag_target(target, after.canonical_root())?;
    }
    if cancellation.load(Ordering::Acquire) {
        return Err(PreparationError::Canceled);
    }
    let final_root = resolve_source_root(root).map_err(|_| PreparationError::RootChanged)?;
    if final_root.canonical_root() != before.canonical_root()
        || final_root.root_identity() != before.root_identity()
        || final_root.canonical_root() != root
    {
        return Err(PreparationError::RootChanged);
    }
    Ok(PreparedDrag { intent, paths })
}

fn validate_drag_target(target: &DragTarget, root: &Path) -> Result<(), PreparationError> {
    let metadata = fs::symlink_metadata(&target.path).map_err(|_| PreparationError::MissingFile)?;
    if !metadata.file_type().is_file() {
        return Err(PreparationError::NonRegularFile);
    }
    let canonical = fs::canonicalize(&target.path).map_err(|_| PreparationError::MissingFile)?;
    if !canonical.starts_with(root) {
        return Err(PreparationError::OutsideRoot);
    }
    Ok(())
}

#[cfg(any(not(target_os = "windows"), test))]
fn retryable_platform_error(error: &FileDragError) -> bool {
    matches!(
        error,
        FileDragError::EventUnavailable
            | FileDragError::WrongEventType { .. }
            | FileDragError::WrongMouseButton { .. }
            | FileDragError::EventWindowMismatch
    )
}

fn raw_path_bytes(path: &Path) -> usize {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().len()
    }
}

/// The compact GPUI ghost shown while the background preparation is pending.
pub(super) struct FileDragGhost {
    count: usize,
}

impl FileDragGhost {
    pub(super) fn new(count: usize) -> Self {
        Self { count }
    }
}

impl Render for FileDragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(gpui::px(8.0))
            .py(gpui::px(4.0))
            .bg(crate::theme::color(theme.colors.surface_raised))
            .text_color(crate::theme::color(theme.colors.text_primary))
            .child(if self.count == 1 {
                "Dragging 1 file".to_string()
            } else {
                format!("Dragging {} files", self.count)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn audio_file(path: impl Into<PathBuf>) -> AudioFile {
        AudioFile {
            path: path.into(),
            relative_path: PathBuf::from("sample.wav"),
            byte_len: 1,
            duration_seconds: Some(1.0),
            sample_rate: Some(44_100),
            playback_type: None,
            curation: None,
        }
    }

    fn row(entry_index: usize, entry: &AudioFile) -> BrowserRow {
        BrowserRow {
            entry_index,
            id: entry.stable_id(),
        }
    }

    fn intent_for(entry: &AudioFile, generation: u64) -> DragIntent {
        let id = entry.stable_id();
        DragIntent {
            anchor: DragAnchor {
                visible_index: 0,
                entry_index: 0,
                id: id.clone(),
            },
            anchor_selected: false,
            targets: vec![DragTarget {
                entry_index: 0,
                id,
                path: entry.path.clone(),
            }],
            fence: ProjectionFence {
                handoff_generation: generation,
                scan_generation: 1,
                source_id: None,
                lifecycle_generation: None,
                root: None,
                root_identity: None,
            },
        }
    }

    #[test]
    fn platform_retry_errors_are_only_current_event_failures() {
        assert!(retryable_platform_error(&FileDragError::EventUnavailable));
        assert!(retryable_platform_error(&FileDragError::WrongEventType {
            actual: 1
        }));
        assert!(!retryable_platform_error(
            &FileDragError::PrimaryButtonNotPressed { mask: 0 }
        ));
        assert!(!retryable_platform_error(
            &FileDragError::UnsupportedPlatform
        ));
    }

    #[test]
    fn path_bytes_use_raw_unix_bytes() {
        assert_eq!(raw_path_bytes(Path::new("/tmp/a.wav")), 10);
    }

    #[test]
    fn selected_anchor_captures_visible_selected_rows_in_order() {
        let entries = vec![
            audio_file("/tmp/one.wav"),
            audio_file("/tmp/two.wav"),
            audio_file("/tmp/three.wav"),
        ];
        let rows = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| row(index, entry))
            .collect::<Vec<_>>();
        let mut selection = BrowserSelection::new();
        selection.select_all(&[rows[0].id.clone(), rows[2].id.clone()]);

        let (selected, targets) = capture_targets(&entries, &rows, &selection, 0).unwrap();

        assert!(selected);
        assert_eq!(
            targets
                .iter()
                .map(|target| target.path.clone())
                .collect::<Vec<_>>(),
            vec![
                PathBuf::from("/tmp/one.wav"),
                PathBuf::from("/tmp/three.wav")
            ]
        );
    }

    #[test]
    fn unselected_anchor_uses_one_row_even_when_other_rows_are_selected() {
        let entries = vec![audio_file("/tmp/one.wav"), audio_file("/tmp/two.wav")];
        let rows = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| row(index, entry))
            .collect::<Vec<_>>();
        let mut selection = BrowserSelection::new();
        selection.select_all(std::slice::from_ref(&rows[0].id));

        let (selected, targets) = capture_targets(&entries, &rows, &selection, 1).unwrap();

        assert!(!selected);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].path, PathBuf::from("/tmp/two.wav"));
    }

    #[test]
    fn bounds_reject_whole_target_set_without_truncation() {
        let entries = (0..=MAX_FILE_DRAG_FILES)
            .map(|index| audio_file(format!("/tmp/{index}.wav")))
            .collect::<Vec<_>>();
        let rows = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| row(index, entry))
            .collect::<Vec<_>>();
        let mut selection = BrowserSelection::new();
        selection.select_all(&rows.iter().map(|row| row.id.clone()).collect::<Vec<_>>());

        assert_eq!(
            capture_targets(&entries, &rows, &selection, 0),
            Err(CaptureError::TooMany {
                actual: MAX_FILE_DRAG_FILES + 1,
            })
        );
    }

    #[test]
    fn path_byte_bound_rejects_the_complete_target_set() {
        let huge_path = PathBuf::from(format!("/{}", "a".repeat(MAX_FILE_DRAG_PATH_BYTES)));
        let entries = vec![audio_file(huge_path)];
        let rows = vec![row(0, &entries[0])];
        let mut selection = BrowserSelection::new();
        selection.select_all(std::slice::from_ref(&rows[0].id));

        assert!(matches!(
            capture_targets(&entries, &rows, &selection, 0),
            Err(CaptureError::PathBytesTooLarge { .. })
        ));
    }

    #[test]
    fn preparation_accepts_complete_regular_files_and_retains_bounded_paths() {
        let directory = tempdir().unwrap();
        let first = directory.path().join("first.wav");
        let second = directory.path().join("second.wav");
        std::fs::write(&first, b"RIFF").unwrap();
        std::fs::write(&second, b"RIFF").unwrap();
        let entries = vec![audio_file(first.clone()), audio_file(second.clone())];
        let intent = intent_for_entries(&entries, 1, directory.path());

        let prepared = prepare_drag(intent, Arc::new(AtomicBool::new(false))).unwrap();

        assert_eq!(prepared.intent.targets.len(), 2);
        assert_eq!(prepared.paths, vec![first, second]);
    }

    #[test]
    fn preparation_rejects_the_whole_intent_when_one_path_is_missing() {
        let directory = tempdir().unwrap();
        let present = directory.path().join("present.wav");
        let path = directory.path().join("gone.wav");
        std::fs::write(&present, b"RIFF").unwrap();
        let entries = vec![audio_file(present), audio_file(path)];
        let intent = intent_for_entries(&entries, 1, directory.path());

        assert_eq!(
            prepare_drag(intent, Arc::new(AtomicBool::new(false))),
            Err(PreparationError::MissingFile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_a_captured_file_replaced_by_an_external_symlink() {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let path = directory.path().join("sample.wav");
        let outside_path = outside.path().join("outside.wav");
        std::fs::write(&path, b"RIFF").unwrap();
        std::fs::write(&outside_path, b"RIFF").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&outside_path, &path).unwrap();
        let entries = vec![audio_file(path)];
        let intent = intent_for_entries(&entries, 1, directory.path());

        assert_eq!(
            prepare_drag(intent, Arc::new(AtomicBool::new(false))),
            Err(PreparationError::NonRegularFile)
        );
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_a_parent_symlink_that_escapes_the_source_root() {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_path = outside.path().join("sample.wav");
        std::fs::write(&outside_path, b"RIFF").unwrap();
        let linked = directory.path().join("linked");
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        let path = linked.join("sample.wav");
        let entries = vec![audio_file(path)];
        let intent = intent_for_entries(&entries, 1, directory.path());

        assert_eq!(
            prepare_drag(intent, Arc::new(AtomicBool::new(false))),
            Err(PreparationError::OutsideRoot)
        );
    }

    #[test]
    fn running_preparation_keeps_only_the_newest_queued_intent() {
        let first = audio_file("/tmp/first.wav");
        let second = audio_file("/tmp/second.wav");
        let mut state = FileDragState::default();
        let cancellation = Arc::new(AtomicBool::new(false));
        state.running = Some(RunningPreparation {
            request_id: 7,
            gesture_id: 4,
            cancellation: Arc::clone(&cancellation),
        });

        state.queue_latest(intent_for(&first, 1));
        state.queue_latest(intent_for(&second, 2));

        assert!(cancellation.load(Ordering::Acquire));
        assert_eq!(state.queued, Some(intent_for(&second, 2)));
    }

    #[test]
    fn cancel_unadmitted_clears_ready_and_queued_work() {
        let entry = audio_file("/tmp/ready.wav");
        let cancellation = Arc::new(AtomicBool::new(false));
        let intent = intent_for(&entry, 1);
        let mut state = FileDragState::default();
        state.running = Some(RunningPreparation {
            request_id: 7,
            gesture_id: 4,
            cancellation: Arc::clone(&cancellation),
        });
        state.queued = Some(intent.clone());
        state.ready = Some(PreparedDrag {
            paths: intent
                .targets
                .iter()
                .map(|target| target.path.clone())
                .collect(),
            intent,
        });
        state.gesture_active = true;
        state.active_gesture_id = Some(4);

        state.cancel_unadmitted();

        assert!(cancellation.load(Ordering::Acquire));
        assert!(state.queued.is_none());
        assert!(state.ready.is_none());
        assert!(!state.gesture_active);
        assert!(state.active_gesture_id.is_none());
        assert_eq!(
            state.running.as_ref().map(|running| running.request_id),
            Some(7)
        );
    }

    #[test]
    fn cancellation_keeps_the_running_slot_for_late_completion() {
        let entry = audio_file("/tmp/running.wav");
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut state = FileDragState::default();
        state.running = Some(RunningPreparation {
            request_id: 11,
            gesture_id: 8,
            cancellation: Arc::clone(&cancellation),
        });
        state.gesture_active = true;
        state.active_gesture_id = Some(8);
        state.queue_latest(intent_for(&entry, 1));

        // Release, a new press, or a source/projection invalidation all use this
        // transition. The in-flight task remains owned until its completion callback
        // drains it, preventing a second preparation from running concurrently.
        state.cancel_unadmitted();

        assert!(cancellation.load(Ordering::Acquire));
        assert!(state.running.is_some());
        assert!(state.queued.is_none());
        assert!(!state.gesture_active);
        assert!(state.active_gesture_id.is_none());
    }

    #[test]
    fn source_cancel_retains_ghost_until_matching_deferred_abort() {
        let mut state = FileDragState::default();
        state.gesture_active = true;
        state.active_gesture_id = Some(21);
        state.mark_pending_ghost(21);

        state.cancel_unadmitted();

        assert!(!state.gesture_active);
        assert!(state.active_gesture_id.is_none());
        assert_eq!(state.pending_ghost_gesture_id(), Some(21));
        assert!(state.take_pending_ghost(21));
        assert!(state.pending_ghost_gesture_id().is_none());
    }

    #[test]
    fn old_deferred_abort_cannot_clear_a_newer_ghost_owner() {
        let mut state = FileDragState::default();
        state.mark_pending_ghost(21);
        state.cancel_unadmitted();
        state.mark_pending_ghost(22);

        assert!(!state.take_pending_ghost(21));
        assert_eq!(state.pending_ghost_gesture_id(), Some(22));
        assert!(state.take_pending_ghost(22));
        assert!(state.pending_ghost_gesture_id().is_none());
    }

    #[test]
    fn windows_release_cancels_queued_launch_but_drops_only_a_terminal_slot() {
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut state = FileDragState::default();
        state.windows_native = Some(WindowsNativeDrag {
            request_id: 12,
            gesture_id: 9,
            count: 1,
            phase: WindowsNativePhase::Queued,
            cancellation: Arc::clone(&cancellation),
        });

        assert!(state.windows_release_should_cancel());
        state.cancel_unadmitted();

        assert!(cancellation.load(Ordering::Acquire));
        assert!(state.windows_native.is_some());

        let admitted_cancellation = Arc::new(AtomicBool::new(false));
        state.windows_native = Some(WindowsNativeDrag {
            request_id: 13,
            gesture_id: 10,
            count: 1,
            phase: WindowsNativePhase::Admitted,
            cancellation: Arc::clone(&admitted_cancellation),
        });
        assert_eq!(
            state.windows_native.as_ref().map(|native| native.count),
            Some(1)
        );
        assert!(!state.windows_release_should_cancel());
        assert!(!admitted_cancellation.load(Ordering::Acquire));

        // A later left press, Escape, or source invalidation still cancels an
        // admitted native operation through its token.
        state.cancel_unadmitted();
        assert!(admitted_cancellation.load(Ordering::Acquire));

        assert!(state.take_windows_native(12, 9).is_none());
        assert!(state.windows_native.is_some());
        assert!(state.take_windows_native(13, 10).is_some());
        assert!(state.windows_native.is_none());
    }

    #[test]
    fn prepared_targets_reject_a_changed_selection() {
        let entries = vec![audio_file("/tmp/one.wav"), audio_file("/tmp/two.wav")];
        let rows = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| row(index, entry))
            .collect::<Vec<_>>();
        let mut selection = BrowserSelection::new();
        selection.select_all(std::slice::from_ref(&rows[0].id));
        let (anchor_selected, targets) = capture_targets(&entries, &rows, &selection, 0).unwrap();
        let anchor = DragAnchor {
            visible_index: 0,
            entry_index: 0,
            id: rows[0].id.clone(),
        };
        assert!(anchor_selected);

        selection.select_all(std::slice::from_ref(&rows[1].id));

        assert!(!targets_match_current(
            &entries, &rows, &selection, &anchor, true, &targets,
        ));
    }

    fn intent_for_entries(entries: &[AudioFile], generation: u64, root: &Path) -> DragIntent {
        let first = &entries[0];
        DragIntent {
            anchor: DragAnchor {
                visible_index: 0,
                entry_index: 0,
                id: first.stable_id(),
            },
            anchor_selected: false,
            targets: entries
                .iter()
                .enumerate()
                .map(|(entry_index, entry)| DragTarget {
                    entry_index,
                    id: entry.stable_id(),
                    path: entry.path.clone(),
                })
                .collect(),
            fence: ProjectionFence {
                handoff_generation: generation,
                scan_generation: 1,
                source_id: None,
                lifecycle_generation: None,
                root: Some(std::fs::canonicalize(root).unwrap()),
                root_identity: None,
            },
        }
    }
}
