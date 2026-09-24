//! Window-owned AppKit accessibility children for GPUI-drawn controls.
//!
//! The owner creates one registry for its GPUI window, reconciles live controls
//! in visible order, and routes received events through its GPUI window context.
//! All registry mutations run on the AppKit main thread. Native AX callbacks
//! only enqueue bounded events or read atomic availability/focus state.

use async_channel::{Receiver, Sender};
use objc2::rc::Retained;
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityButtonRole, NSAccessibilityCheckBoxRole,
    NSAccessibilityElement, NSAccessibilityGroupRole, NSAccessibilityListRole,
    NSAccessibilityMenuItemRole, NSAccessibilityPopUpButtonRole, NSAccessibilityRadioButtonRole,
    NSAccessibilityRole, NSAccessibilityRowRole, NSAccessibilitySliderRole,
    NSAccessibilityStaticTextRole, NSAccessibilityTextFieldRole, NSView,
};
use objc2_foundation::{NSArray, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use raw_window_handle::{RawWindowHandle, WindowHandle};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// Maximum number of AX children accepted by one GPUI window registry.
pub const MAX_AX_CHILDREN: usize = 2_048;
const MAX_PENDING_ACTIONS: usize = 64;

/// Native role represented by a GPUI-drawn control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AxRole {
    /// Push button.
    Button,
    /// Toggle check box.
    CheckBox,
    /// Exclusive radio button.
    RadioButton,
    /// Popup or picker control.
    PopUpButton,
    /// Adjustable slider.
    Slider,
    /// Editable text field.
    TextField,
    /// Read-only text.
    StaticText,
    /// Non-actionable grouping.
    Group,
    /// List container.
    List,
    /// One list row.
    Row,
    /// Menu item.
    MenuItem,
}

impl AxRole {
    fn native(self) -> &'static NSAccessibilityRole {
        // SAFETY: AppKit exports each immutable role string for process lifetime.
        unsafe {
            match self {
                Self::Button => NSAccessibilityButtonRole,
                Self::CheckBox => NSAccessibilityCheckBoxRole,
                Self::RadioButton => NSAccessibilityRadioButtonRole,
                Self::PopUpButton => NSAccessibilityPopUpButtonRole,
                Self::Slider => NSAccessibilitySliderRole,
                Self::TextField => NSAccessibilityTextFieldRole,
                Self::StaticText => NSAccessibilityStaticTextRole,
                Self::Group => NSAccessibilityGroupRole,
                Self::List => NSAccessibilityListRole,
                Self::Row => NSAccessibilityRowRole,
                Self::MenuItem => NSAccessibilityMenuItemRole,
            }
        }
    }

    fn pressable(self) -> bool {
        matches!(
            self,
            Self::Button
                | Self::CheckBox
                | Self::RadioButton
                | Self::PopUpButton
                | Self::Row
                | Self::MenuItem
        )
    }
}

/// Current semantic state of one visible GPUI control.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AxChild {
    /// Stable window-local control identity.
    pub id: String,
    /// Native role matching the control's actual behavior.
    pub role: AxRole,
    /// Human-readable accessible name.
    pub label: String,
    /// Current value or state, when meaningful for this role.
    pub value: Option<String>,
    /// Whether the corresponding GPUI action is available.
    pub enabled: bool,
}

/// Bounds in GPUI window coordinates, with origin at the upper left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxFrame {
    /// Left offset in GPUI logical pixels.
    pub x: f64,
    /// Top offset in GPUI logical pixels.
    pub y: f64,
    /// Width in GPUI logical pixels.
    pub width: f64,
    /// Height in GPUI logical pixels.
    pub height: f64,
}

impl AxFrame {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|component| component.is_finite())
            && self.width >= 0.0
            && self.height >= 0.0
    }
}

/// An action requested through native accessibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AxAction {
    /// Activate a button-like control.
    Press,
    /// Move keyboard focus to the owning GPUI control.
    Focus,
}

/// An AX action to dispatch on the owning GPUI window context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AxEvent {
    /// Stable window-local control identity.
    pub id: String,
    /// Requested action.
    pub action: AxAction,
}

/// Errors before or during registry updates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AxRegistryError {
    /// Registry operations require the AppKit main thread.
    NotMainThread,
    /// The borrowed window handle did not contain an AppKit view.
    UnsupportedWindowHandle,
    /// GPUI's AppKit view was unavailable.
    ViewUnavailable,
    /// Too many children were supplied in one reconciliation.
    TooManyChildren,
    /// A control identity or name was empty.
    EmptyIdentityOrLabel,
    /// The same control identity occurred more than once.
    DuplicateIdentity,
    /// A frame contained an invalid coordinate or negative size.
    InvalidFrame,
    /// The control identity is no longer registered.
    UnknownIdentity,
}

struct ChildIvars {
    id: String,
    events: Sender<AxEvent>,
    alive: AtomicBool,
    enabled: AtomicBool,
    pressable: AtomicBool,
    focused: AtomicBool,
}

define_class!(
    // SAFETY: NSAccessibilityElement permits subclassing. The callbacks use
    // only thread-safe atomics and an async-channel sender. AppKit and GPUI
    // mutations stay with the registry owner on the main thread.
    #[unsafe(super(NSAccessibilityElement))]
    #[thread_kind = AnyThread]
    #[ivars = ChildIvars]
    #[name = "WavecrateGpuiAccessibilityChild"]
    struct NativeChild;

    impl NativeChild {
        #[unsafe(method(accessibilityPerformPress))]
        fn perform_press(&self) -> bool {
            let ivars = self.ivars();
            if !ivars.alive.load(Ordering::Acquire)
                || !ivars.enabled.load(Ordering::Acquire)
                || !ivars.pressable.load(Ordering::Acquire)
            {
                return false.into();
            }
            ivars.events.try_send(AxEvent {
                id: ivars.id.clone(),
                action: AxAction::Press,
            }).is_ok()
        }

        #[unsafe(method(setAccessibilityFocused:))]
        fn set_focused(&self, focused: bool) {
            let ivars = self.ivars();
            if focused
                && ivars.alive.load(Ordering::Acquire)
                && ivars.enabled.load(Ordering::Acquire)
            {
                let _ = ivars.events.try_send(AxEvent {
                    id: ivars.id.clone(),
                    action: AxAction::Focus,
                });
            }
        }

        #[unsafe(method(isAccessibilityFocused))]
        fn is_focused(&self) -> bool {
            self.ivars().focused.load(Ordering::Acquire)
        }
    }

    unsafe impl NSObjectProtocol for NativeChild {}
);

impl NativeChild {
    fn new(marker: MainThreadMarker, id: String, events: Sender<AxEvent>) -> Retained<Self> {
        let this = marker.alloc().set_ivars(ChildIvars {
            id,
            events,
            alive: AtomicBool::new(true),
            enabled: AtomicBool::new(false),
            pressable: AtomicBool::new(false),
            focused: AtomicBool::new(false),
        });
        // SAFETY: Initialize the freshly allocated subclass through its
        // NSObject superclass initializer.
        unsafe { msg_send![super(this), init] }
    }
}

/// The single native accessibility child owner for one GPUI window.
///
/// Keep this value on the AppKit main thread and drop it with the GPUI root
/// entity. A later AX event may still be queued when a child is removed; the
/// caller must recheck its current GPUI control identity before dispatch.
pub struct AxRegistry {
    view: Retained<NSView>,
    children: BTreeMap<String, Retained<NativeChild>>,
    events: Sender<AxEvent>,
}

impl AxRegistry {
    /// Attach a registry to the actual NSView in a borrowed GPUI window handle.
    /// The returned receiver belongs to the owning GPUI window context.
    pub fn new(window: WindowHandle<'_>) -> Result<(Self, Receiver<AxEvent>), AxRegistryError> {
        MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?;
        let RawWindowHandle::AppKit(handle) = window.as_raw() else {
            return Err(AxRegistryError::UnsupportedWindowHandle);
        };
        // SAFETY: The borrowed AppKit handle contains a live NSView. Retain it
        // before the handle borrow ends; Drop detaches the children.
        let view = unsafe { Retained::retain(handle.ns_view.as_ptr().cast()) }
            .ok_or(AxRegistryError::ViewUnavailable)?;
        let (events, receiver) = async_channel::bounded(MAX_PENDING_ACTIONS);
        Ok((
            Self {
                view,
                children: BTreeMap::new(),
                events,
            },
            receiver,
        ))
    }

    /// Reconcile a full visible control snapshot in native traversal order.
    /// Existing identities retain their native AX object across updates.
    pub fn reconcile(&mut self, controls: Vec<AxChild>) -> Result<(), AxRegistryError> {
        MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?;
        if controls.len() > MAX_AX_CHILDREN {
            return Err(AxRegistryError::TooManyChildren);
        }
        let mut seen = HashSet::with_capacity(controls.len());
        for control in &controls {
            if control.id.trim().is_empty() || control.label.trim().is_empty() {
                return Err(AxRegistryError::EmptyIdentityOrLabel);
            }
            if !seen.insert(control.id.as_str()) {
                return Err(AxRegistryError::DuplicateIdentity);
            }
        }

        let marker = MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?;
        let mut previous = std::mem::take(&mut self.children);
        let mut next = BTreeMap::new();
        let mut order = Vec::with_capacity(controls.len());
        for control in controls {
            let child = previous.remove(&control.id).unwrap_or_else(|| {
                let child = NativeChild::new(marker, control.id.clone(), self.events.clone());
                // SAFETY: The actual GPUI NSView owns this AX child.
                unsafe { child.setAccessibilityParent(Some(&self.view)) };
                child.setAccessibilityIdentifier(Some(&NSString::from_str(&control.id)));
                child
            });
            child.setAccessibilityRole(Some(control.role.native()));
            child.setAccessibilityLabel(Some(&NSString::from_str(&control.label)));
            let value = control.value.as_deref().map(NSString::from_str);
            // SAFETY: NSString is an Objective-C object suitable for AXValue.
            unsafe { child.setAccessibilityValue(value.as_deref().map(|value| &***value)) };
            child.setAccessibilityEnabled(control.enabled);
            child
                .ivars()
                .enabled
                .store(control.enabled, Ordering::Release);
            child
                .ivars()
                .pressable
                .store(control.role.pressable(), Ordering::Release);
            order.push(control.id.clone());
            next.insert(control.id, child);
        }
        for child in previous.into_values() {
            Self::retire(&child);
        }
        self.children = next;
        let native_children = order
            .iter()
            .map(|id| {
                // SAFETY: NativeChild is an Objective-C NSObject subclass.
                unsafe { Retained::cast_unchecked(self.children[id].clone()) }
            })
            .collect::<Vec<_>>();
        let array: Retained<NSArray> = NSArray::from_retained_slice(&native_children);
        // SAFETY: Every array member is an NSAccessibilityElement subclass.
        unsafe { self.view.setAccessibilityChildren(Some(&array)) };
        Ok(())
    }

    /// Update a registered child's frame from its real GPUI layout bounds.
    pub fn set_frame(&self, id: &str, frame: AxFrame) -> Result<(), AxRegistryError> {
        MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?;
        if !frame.valid() {
            return Err(AxRegistryError::InvalidFrame);
        }
        let child = self
            .children
            .get(id)
            .ok_or(AxRegistryError::UnknownIdentity)?;
        let parent_height = self.view.bounds().size.height;
        child.setAccessibilityFrameInParentSpace(NSRect::new(
            NSPoint::new(frame.x, parent_height - frame.y - frame.height),
            NSSize::new(frame.width, frame.height),
        ));
        Ok(())
    }

    /// Update AX focus from the owning GPUI focus handle and window activity.
    pub fn set_focused(&self, id: &str, focused: bool) -> Result<(), AxRegistryError> {
        MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?;
        let child = self
            .children
            .get(id)
            .ok_or(AxRegistryError::UnknownIdentity)?;
        child.ivars().focused.store(focused, Ordering::Release);
        Ok(())
    }

    /// Return whether a currently registered control identity exists.
    pub fn contains(&self, id: &str) -> bool {
        self.children.contains_key(id)
    }

    fn retire(child: &NativeChild) {
        child.ivars().alive.store(false, Ordering::Release);
        child.ivars().enabled.store(false, Ordering::Release);
        child.ivars().focused.store(false, Ordering::Release);
        // SAFETY: A retired AX child no longer belongs to the GPUI view.
        unsafe { child.setAccessibilityParent(None) };
    }
}

impl Drop for AxRegistry {
    fn drop(&mut self) {
        self.events.close();
        for child in self.children.values() {
            Self::retire(child);
        }
        // SAFETY: This registry owns the complete AX child list installed on
        // its GPUI NSView, which is still retained here during Drop.
        unsafe { self.view.setAccessibilityChildren(None) };
    }
}
