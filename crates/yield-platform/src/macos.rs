//! AppKit implementation of the bounded outbound file drag.

use crate::{FileDragCompletion, FileDragError, FileDragHandle, ValidatedPaths};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingItem, NSDraggingSession, NSDraggingSource, NSEvent,
    NSEventType, NSImage, NSImageNameIconViewTemplate, NSView,
};
use objc2_foundation::{
    NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSURL,
};
use raw_window_handle::{RawWindowHandle, WindowHandle};
use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};

static NEXT_IDENTITY: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static ACTIVE_DRAG: RefCell<Option<ActiveDrag>> = const { RefCell::new(None) };
}

struct ActiveDrag {
    identity: u64,
    _view: Retained<NSView>,
    _source: Retained<DragSource>,
    _urls: Vec<Retained<NSURL>>,
    items: Vec<Retained<NSDraggingItem>>,
    _session: Option<Retained<NSDraggingSession>>,
    sender: SyncSender<FileDragCompletion>,
}

struct PreparedDragItems {
    urls: Vec<Retained<NSURL>>,
    items: Vec<Retained<NSDraggingItem>>,
}

struct DragSourceIvars {
    identity: u64,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - DragSource has no Rust `Drop` implementation; objc2 owns its ivars
    //   and releases the retained object after AppKit no longer calls it.
    // - The class is explicitly main-thread-only because AppKit invokes its
    //   protocol callbacks on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = DragSourceIvars]
    #[name = "YieldNativeFileDragSource"]
    struct DragSource;

    impl DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: objc2_app_kit::NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn session_ended(
            &self,
            _session: &NSDraggingSession,
            _point: NSPoint,
            operation: NSDragOperation,
        ) {
            // Retain the callback receiver before taking the app-owned state.
            // AppKit may not retain its source for the whole callback, and
            // taking ActiveDrag can release the last app-owned source retain.
            let callback_guard = self.retain();
            complete_drag(self.ivars().identity, operation);
            drop(callback_guard);
        }
    }

    unsafe impl NSObjectProtocol for DragSource {}
    unsafe impl NSDraggingSource for DragSource {}
);

impl DragSource {
    fn new(marker: MainThreadMarker, identity: u64) -> Retained<Self> {
        let this = marker.alloc().set_ivars(DragSourceIvars { identity });
        // SAFETY: `this` is freshly allocated for this class and is initialized
        // with NSObject's designated initializer on the AppKit main thread.
        unsafe { msg_send![super(this), init] }
    }
}

pub(super) fn start_file_drag(
    window: WindowHandle<'_>,
    paths: ValidatedPaths<'_>,
) -> Result<FileDragHandle, FileDragError> {
    let marker = MainThreadMarker::new().ok_or(FileDragError::NotMainThread)?;
    let view = appkit_view(window)?;
    let event = current_left_mouse_event(&view, marker)?;
    let identity = next_identity();
    let (sender, receiver) = mpsc::sync_channel(1);
    let prepared = make_drag_items(paths.paths, &view, &event, marker)?;
    let source = DragSource::new(marker, identity);

    let active = ActiveDrag {
        identity,
        _view: view.clone(),
        _source: source.clone(),
        _urls: prepared.urls,
        items: prepared.items,
        _session: None,
        sender,
    };
    install_active(active)?;

    let item_array = {
        let active = active_items(identity).ok_or(FileDragError::WindowUnavailable)?;
        NSArray::from_retained_slice(&active)
    };
    let source_protocol: &ProtocolObject<dyn NSDraggingSource> = ProtocolObject::from_ref(&*source);
    // AppKit's generated API returns a retained, nonnull NSDraggingSession.
    // Keep all native objects in ActiveDrag before this call because AppKit may
    // synchronously invoke the source's ended callback during begin.
    let session =
        view.beginDraggingSessionWithItems_event_source(&item_array, &event, source_protocol);
    attach_session(identity, session);

    Ok(FileDragHandle::new(identity, receiver))
}

fn next_identity() -> u64 {
    loop {
        let identity = NEXT_IDENTITY.fetch_add(1, Ordering::Relaxed);
        if identity != 0 {
            return identity;
        }
    }
}

fn appkit_view(window: WindowHandle<'_>) -> Result<Retained<NSView>, FileDragError> {
    let RawWindowHandle::AppKit(handle) = window.as_raw() else {
        return Err(FileDragError::UnsupportedWindowHandle);
    };

    // SAFETY: raw-window-handle guarantees that an AppKit handle contains a
    // valid, nonnull NSView pointer for the lifetime of the borrowed handle.
    // We are on the AppKit main thread and retain it before that borrow ends.
    unsafe { Retained::retain(handle.ns_view.as_ptr().cast()) }
        .ok_or(FileDragError::WindowUnavailable)
}

fn current_left_mouse_event(
    view: &NSView,
    marker: MainThreadMarker,
) -> Result<Retained<NSEvent>, FileDragError> {
    let app = NSApplication::sharedApplication(marker);
    let event = app.currentEvent().ok_or(FileDragError::EventUnavailable)?;
    let event_type = event.r#type();
    if event_type != NSEventType::LeftMouseDown && event_type != NSEventType::LeftMouseDragged {
        return Err(FileDragError::WrongEventType {
            actual: event_type.0 as u64,
        });
    }
    let button = event.buttonNumber();
    if button != 0 {
        return Err(FileDragError::WrongMouseButton {
            actual: button as i64,
        });
    }
    let pressed_buttons = NSEvent::pressedMouseButtons() as u64;
    if !primary_button_pressed(pressed_buttons) {
        return Err(FileDragError::PrimaryButtonNotPressed {
            mask: pressed_buttons,
        });
    }

    let view_window = view.window().ok_or(FileDragError::WindowUnavailable)?;
    let event_window = event
        .window(marker)
        .ok_or(FileDragError::EventUnavailable)?;
    if view_window != event_window {
        return Err(FileDragError::EventWindowMismatch);
    }
    Ok(event)
}

fn primary_button_pressed(mask: u64) -> bool {
    mask & 1 != 0
}

fn make_drag_items(
    paths: &[std::path::PathBuf],
    view: &NSView,
    event: &NSEvent,
    _marker: MainThreadMarker,
) -> Result<PreparedDragItems, FileDragError> {
    let mut urls = Vec::with_capacity(paths.len());
    let mut items = Vec::with_capacity(paths.len());
    // SAFETY: AppKit owns this exported image-name object for the process
    // lifetime, as required by the generated extern static binding.
    let image = unsafe { NSImage::imageNamed(NSImageNameIconViewTemplate) };
    let pointer = view.convertPoint_fromView(event.locationInWindow(), None);
    let frame_size = NSSize::new(32.0, 32.0);

    for (index, path) in paths.iter().enumerate() {
        // `ValidatedPaths` has already rejected non-UTF-8 and embedded NUL
        // values. Keep this conversion explicit so no lossy path conversion
        // can be introduced by a future caller or platform binding change.
        let path_text = path
            .to_str()
            .expect("validated native drag path must be UTF-8");
        let path_string = NSString::from_str(path_text);
        let url = NSURL::fileURLWithPath_isDirectory(&path_string, false);
        let writer = ProtocolObject::from_retained(url.clone());
        let item = NSDraggingItem::initWithPasteboardWriter(NSDraggingItem::alloc(), &writer);
        let stagger = (index.min(4) as f64) * 4.0;
        let frame = NSRect::new(
            NSPoint::new(pointer.x - 16.0 + stagger, pointer.y - 16.0 + stagger),
            frame_size,
        );
        // `contents` is an NSImage returned by AppKit's generic system image
        // lookup, which is exactly the type required by this setter. The
        // image is optional on older systems; the frame remains bounded.
        unsafe { item.setDraggingFrame_contents(frame, image.as_deref().map(|image| image as _)) };
        urls.push(url);
        items.push(item);
    }

    Ok(PreparedDragItems { urls, items })
}

fn install_active(active: ActiveDrag) -> Result<(), FileDragError> {
    let result = ACTIVE_DRAG.with(|slot| {
        let mut slot = slot.borrow_mut();
        install_unique(&mut slot, active, |active| active.identity)
    });
    match result {
        Ok(()) => Ok(()),
        Err((identity, active)) => {
            // Release incoming native objects after the RefCell borrow ends;
            // their Objective-C deallocators must never run while the slot is
            // mutably borrowed.
            drop(active);
            Err(FileDragError::AlreadyActive { identity })
        }
    }
}

fn install_unique<T>(
    slot: &mut Option<T>,
    value: T,
    identity_of: impl FnOnce(&T) -> u64,
) -> Result<(), (u64, T)> {
    if let Some(existing) = slot.as_ref() {
        return Err((identity_of(existing), value));
    }
    *slot = Some(value);
    Ok(())
}

fn take_matching<T>(
    slot: &mut Option<T>,
    identity: u64,
    identity_of: impl FnOnce(&T) -> u64,
) -> Option<T> {
    if slot
        .as_ref()
        .is_some_and(|active| identity_of(active) == identity)
    {
        slot.take()
    } else {
        None
    }
}

fn active_items(identity: u64) -> Option<Vec<Retained<NSDraggingItem>>> {
    ACTIVE_DRAG.with(|slot| {
        let slot = slot.borrow();
        let active = slot.as_ref().filter(|active| active.identity == identity)?;
        Some(active.items.iter().map(Retained::clone).collect())
    })
}

fn attach_session(identity: u64, session: Retained<NSDraggingSession>) {
    let mut session = Some(session);
    let old_session = ACTIVE_DRAG.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(active) = slot.as_mut().filter(|active| active.identity == identity) {
            session
                .take()
                .and_then(|session| active._session.replace(session))
        } else {
            None
        }
        // If a synchronous ended callback removed this identity, dropping the
        // returned session here is intentional and cannot resurrect the state.
    });
    drop(old_session);
    drop(session);
}

fn complete_drag(identity: u64, operation: NSDragOperation) {
    let completion = completion_for_operation(operation);

    let active = ACTIVE_DRAG.with(|slot| {
        let mut slot = slot.borrow_mut();
        take_matching(&mut slot, identity, |active| active.identity)
    });
    if let Some(active) = active {
        let _ = active.sender.try_send(completion);
        // `active` drops after notification, while DragSource's callback guard
        // is still alive. This releases the explicit source/items/URL/session
        // retention only after the callback has stopped borrowing the source.
    }
}

fn completion_for_operation(operation: NSDragOperation) -> FileDragCompletion {
    if operation == NSDragOperation::Copy {
        FileDragCompletion::Copy
    } else if operation.is_empty() {
        FileDragCompletion::Cancelled
    } else {
        FileDragCompletion::Other
    }
}

#[cfg(test)]
mod tests {
    use super::{completion_for_operation, install_unique, take_matching};
    use objc2_app_kit::NSDragOperation;
    use std::cell::Cell;
    use std::rc::Rc;

    struct DropProbe(Rc<Cell<bool>>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    struct TestSession {
        identity: u64,
        _probe: DropProbe,
    }

    #[test]
    fn replacement_is_rejected_without_releasing_existing_session() {
        let existing_dropped = Rc::new(Cell::new(false));
        let replacement_dropped = Rc::new(Cell::new(false));
        let mut slot = Some(TestSession {
            identity: 7,
            _probe: DropProbe(existing_dropped.clone()),
        });
        let replacement = TestSession {
            identity: 8,
            _probe: DropProbe(replacement_dropped.clone()),
        };

        let rejected = match install_unique(&mut slot, replacement, |session| session.identity) {
            Err(rejected) => rejected,
            Ok(()) => panic!("the occupied slot must reject replacement"),
        };
        assert_eq!(rejected.0, 7);
        drop(rejected.1);
        assert!(!existing_dropped.get());
        assert!(replacement_dropped.get());
        drop(slot.take());
        assert!(existing_dropped.get());
    }

    #[test]
    fn stale_and_reentrant_completion_are_ignored_after_matching_take() {
        let dropped = Rc::new(Cell::new(false));
        let mut slot = Some(TestSession {
            identity: 11,
            _probe: DropProbe(dropped.clone()),
        });

        assert!(take_matching(&mut slot, 12, |session| session.identity).is_none());
        assert!(!dropped.get());
        drop(take_matching(&mut slot, 11, |session| session.identity));
        assert!(dropped.get());
        assert!(take_matching(&mut slot, 11, |session| session.identity).is_none());
    }

    #[test]
    fn completion_mapping_preserves_copy_cancel_and_other() {
        assert_eq!(
            completion_for_operation(NSDragOperation::Copy),
            super::FileDragCompletion::Copy
        );
        assert_eq!(
            completion_for_operation(NSDragOperation::None),
            super::FileDragCompletion::Cancelled
        );
        assert_eq!(
            completion_for_operation(NSDragOperation::Link),
            super::FileDragCompletion::Other
        );
    }

    #[test]
    fn primary_button_mask_requires_left_button() {
        assert!(!super::primary_button_pressed(0));
        assert!(!super::primary_button_pressed(1 << 1));
        assert!(super::primary_button_pressed(1));
        assert!(super::primary_button_pressed(1 | (1 << 4)));
    }
}
