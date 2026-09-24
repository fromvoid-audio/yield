//! Window-owned AppKit accessibility children for GPUI-drawn controls.
//!
//! The owner creates one registry for its GPUI window, reconciles live controls
//! in visible order, and routes received events through its GPUI window context.
//! All registry mutations run on the AppKit main thread. Native AX callbacks
//! only enqueue bounded events or read atomic availability/focus state.

use async_channel::{Receiver, Sender};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityActionName, NSAccessibilityAttributeName,
    NSAccessibilityButtonRole, NSAccessibilityCheckBoxRole, NSAccessibilityDecrementAction,
    NSAccessibilityElement, NSAccessibilityFocusedUIElementChangedNotification,
    NSAccessibilityGroupRole, NSAccessibilityIncrementAction, NSAccessibilityListRole,
    NSAccessibilityMenuItemRole, NSAccessibilityPopUpButtonRole, NSAccessibilityPostNotification,
    NSAccessibilityPressAction, NSAccessibilityRadioButtonRole, NSAccessibilityRole,
    NSAccessibilityRowRole, NSAccessibilitySliderRole, NSAccessibilityStaticTextRole,
    NSAccessibilityTextFieldRole, NSAccessibilityValueAttribute, NSApplication, NSView,
};
use objc2_foundation::{NSArray, NSNumber, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
use raw_window_handle::{RawWindowHandle, WindowHandle};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Maximum number of AX children accepted by one GPUI window registry.
pub const MAX_AX_CHILDREN: usize = 2_048;
const MAX_PENDING_ACTIONS: usize = 64;
const MAX_EDIT_VALUE_BYTES: usize = 16 * 1024;

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

/// Numeric state for a GPUI-owned native slider.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxSlider {
    /// Current value in the inclusive slider range.
    pub value: f64,
    /// Lowest allowed value.
    pub min: f64,
    /// Highest allowed value.
    pub max: f64,
}

impl AxSlider {
    fn valid(self) -> bool {
        [self.value, self.min, self.max]
            .iter()
            .all(|value| value.is_finite())
            && self.min <= self.value
            && self.value <= self.max
            && self.min < self.max
    }
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AxAction {
    /// Activate a button-like control.
    Press,
    /// Move keyboard focus to the owning GPUI control.
    Focus,
    /// Replace an editable field's text or an adjustable slider's numeric value.
    SetValue(String),
    /// Increase an adjustable slider by the owning control's step.
    Increment,
    /// Decrease an adjustable slider by the owning control's step.
    Decrement,
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
    /// Slider value or bounds are invalid.
    InvalidSlider,
    /// Slider configuration targeted a non-slider control.
    NotSlider,
    /// The control identity is no longer registered.
    UnknownIdentity,
}

struct ChildIvars {
    id: String,
    events: Sender<AxEvent>,
    alive: AtomicBool,
    enabled: AtomicBool,
    pressable: AtomicBool,
    editable: AtomicBool,
    slider_role: AtomicBool,
    slider_configured: AtomicBool,
    slider_min: AtomicU64,
    slider_max: AtomicU64,
    focused: AtomicBool,
}

define_class!(
    // SAFETY: NSAccessibilityElement permits subclassing. The callbacks use
    // only thread-safe atomics and an async-channel sender. AppKit and GPUI
    // mutations stay with the registry owner on the main thread.
    #[unsafe(super(NSAccessibilityElement))]
    #[thread_kind = AnyThread]
    #[ivars = ChildIvars]
    #[name = "YieldGpuiAccessibilityChild"]
    struct NativeChild;

    impl NativeChild {
        #[unsafe(method(isAccessibilitySelectorAllowed:))]
        fn selector_allowed(&self, selector: Sel) -> bool {
            if selector == sel!(setAccessibilityValue:) {
                let ivars = self.ivars();
                return (ivars.alive.load(Ordering::Acquire)
                    && ivars.enabled.load(Ordering::Acquire)
                    && (ivars.editable.load(Ordering::Acquire)
                        || ivars.slider_configured.load(Ordering::Acquire)))
                    .into();
            }
            unsafe { msg_send![super(self), isAccessibilitySelectorAllowed: selector] }
        }

        #[unsafe(method(setAccessibilityValue:))]
        fn set_ax_value(&self, value: Option<&AnyObject>) {
            if !self.ivars().alive.load(Ordering::Acquire)
                || !self.ivars().enabled.load(Ordering::Acquire)
            {
                return;
            }
            self.queue_ax_value(value);
        }

        #[unsafe(method(accessibilityActionNames))]
        fn action_names(&self) -> *mut NSArray<NSAccessibilityActionName> {
            let ivars = self.ivars();
            let actions = if ivars.alive.load(Ordering::Acquire)
                && ivars.enabled.load(Ordering::Acquire)
            {
                if ivars.pressable.load(Ordering::Acquire) {
                    // SAFETY: AppKit exports this immutable action name for process lifetime.
                    NSArray::from_slice(&[unsafe { NSAccessibilityPressAction }])
                } else if ivars.slider_configured.load(Ordering::Acquire) {
                    // SAFETY: AppKit exports these immutable action names for process lifetime.
                    NSArray::from_slice(&unsafe { [NSAccessibilityIncrementAction, NSAccessibilityDecrementAction] })
                } else {
                    NSArray::from_slice(&[])
                }
            } else {
                NSArray::from_slice(&[])
            };
            Retained::autorelease_return(actions)
        }

        #[unsafe(method(accessibilityIsAttributeSettable:))]
        fn value_is_settable(&self, attribute: &NSAccessibilityAttributeName) -> bool {
            self.can_edit_value(attribute)
        }

        #[unsafe(method(accessibilitySetValue:forAttribute:))]
        fn set_value_for_attribute(
            &self,
            value: Option<&AnyObject>,
            attribute: &NSAccessibilityAttributeName,
        ) {
            if !self.can_edit_value(attribute) {
                return;
            }
            self.queue_ax_value(value);
        }

        #[unsafe(method(accessibilityPerformAction:))]
        fn perform_action(&self, action: &NSAccessibilityActionName) {
            let ivars = self.ivars();
            if !ivars.alive.load(Ordering::Acquire)
                || !ivars.enabled.load(Ordering::Acquire)
            {
                return;
            }
            // SAFETY: AppKit exports these immutable action names for process lifetime.
            let requested = if ivars.pressable.load(Ordering::Acquire)
                && unsafe { action.isEqualToString(NSAccessibilityPressAction) }
            {
                Some(AxAction::Press)
            } else if ivars.slider_configured.load(Ordering::Acquire)
                && unsafe { action.isEqualToString(NSAccessibilityIncrementAction) }
            {
                Some(AxAction::Increment)
            } else if ivars.slider_configured.load(Ordering::Acquire)
                && unsafe { action.isEqualToString(NSAccessibilityDecrementAction) }
            {
                Some(AxAction::Decrement)
            } else {
                None
            };
            let Some(action) = requested else { return };
            let _ = ivars.events.try_send(AxEvent {
                id: ivars.id.clone(),
                action,
            });
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
    fn queue_ax_value(&self, value: Option<&AnyObject>) {
        let ivars = self.ivars();
        if ivars.editable.load(Ordering::Acquire) {
            if let Some(value) = value.and_then(|value| value.downcast_ref::<NSString>()) {
                self.queue_value(value);
            }
        } else if ivars.slider_configured.load(Ordering::Acquire)
            && let Some(value) = value.and_then(|value| value.downcast_ref::<NSNumber>())
        {
            let number = value.as_f64();
            let min = f64::from_bits(ivars.slider_min.load(Ordering::Acquire));
            let max = f64::from_bits(ivars.slider_max.load(Ordering::Acquire));
            if number.is_finite() && (min..=max).contains(&number) {
                let _ = ivars.events.try_send(AxEvent {
                    id: ivars.id.clone(),
                    action: AxAction::SetValue(number.to_string()),
                });
            }
        }
    }

    fn queue_value(&self, value: &NSString) {
        let value = value.to_string();
        if value.len() <= MAX_EDIT_VALUE_BYTES {
            let _ = self.ivars().events.try_send(AxEvent {
                id: self.ivars().id.clone(),
                action: AxAction::SetValue(value),
            });
        }
    }

    fn can_edit_value(&self, attribute: &NSAccessibilityAttributeName) -> bool {
        // SAFETY: AppKit exports this immutable attribute name for process lifetime.
        let is_value = unsafe { attribute.isEqualToString(NSAccessibilityValueAttribute) };
        let ivars = self.ivars();
        is_value
            && ivars.alive.load(Ordering::Acquire)
            && ivars.enabled.load(Ordering::Acquire)
            && (ivars.editable.load(Ordering::Acquire)
                || ivars.slider_configured.load(Ordering::Acquire))
    }

    fn new(marker: MainThreadMarker, id: String, events: Sender<AxEvent>) -> Retained<Self> {
        let this = marker.alloc().set_ivars(ChildIvars {
            id,
            events,
            alive: AtomicBool::new(true),
            enabled: AtomicBool::new(false),
            pressable: AtomicBool::new(false),
            editable: AtomicBool::new(false),
            slider_role: AtomicBool::new(false),
            slider_configured: AtomicBool::new(false),
            slider_min: AtomicU64::new(0),
            slider_max: AtomicU64::new(0),
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
            if control.role == AxRole::Slider && control.value.is_some() {
                return Err(AxRegistryError::InvalidSlider);
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
            child
                .ivars()
                .slider_configured
                .store(false, Ordering::Release);
            let value = control.value.as_deref().map(NSString::from_str);
            // SAFETY: NSString is an Objective-C object suitable for AXValue.
            let value = value.as_deref().map(|value| &***value);
            // Reconciliation writes the value owned by GPUI without feeding it
            // back through the native edit callback.
            let _: () = unsafe { msg_send![super(&*child), setAccessibilityValue: value] };
            child.setAccessibilityEnabled(control.enabled);
            if !control.enabled && child.ivars().focused.swap(false, Ordering::AcqRel) {
                Self::clear_app_focus_if_child(&child);
            }
            child
                .ivars()
                .enabled
                .store(control.enabled, Ordering::Release);
            child
                .ivars()
                .pressable
                .store(control.role.pressable(), Ordering::Release);
            child
                .ivars()
                .editable
                .store(control.role == AxRole::TextField, Ordering::Release);
            child
                .ivars()
                .slider_role
                .store(control.role == AxRole::Slider, Ordering::Release);
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

    /// Set a slider's numeric value and bounds after reconciling its `AxChild`.
    /// Call this after each reconciliation that includes the slider.
    pub fn set_slider(&self, id: &str, slider: AxSlider) -> Result<(), AxRegistryError> {
        MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?;
        if !slider.valid() {
            return Err(AxRegistryError::InvalidSlider);
        }
        let child = self
            .children
            .get(id)
            .ok_or(AxRegistryError::UnknownIdentity)?;
        let ivars = child.ivars();
        if !ivars.slider_role.load(Ordering::Acquire) {
            return Err(AxRegistryError::NotSlider);
        }
        ivars.slider_configured.store(false, Ordering::Release);
        let value = NSNumber::new_f64(slider.value);
        let min = NSNumber::new_f64(slider.min);
        let max = NSNumber::new_f64(slider.max);
        // SAFETY: Numeric objects are valid AppKit slider accessibility values.
        unsafe {
            let _: () = msg_send![super(&**child), setAccessibilityValue: &*value];
            child.setAccessibilityMinValue(Some(&min));
            child.setAccessibilityMaxValue(Some(&max));
        }
        ivars
            .slider_min
            .store(slider.min.to_bits(), Ordering::Release);
        ivars
            .slider_max
            .store(slider.max.to_bits(), Ordering::Release);
        ivars.slider_configured.store(true, Ordering::Release);
        Ok(())
    }

    /// Update AX focus from the owning GPUI focus handle and window activity.
    pub fn set_focused(&self, id: &str, focused: bool) -> Result<(), AxRegistryError> {
        MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?;
        let child = self
            .children
            .get(id)
            .ok_or(AxRegistryError::UnknownIdentity)?;
        let focused = focused
            && child.ivars().alive.load(Ordering::Acquire)
            && child.ivars().enabled.load(Ordering::Acquire);
        let previous = child.ivars().focused.swap(focused, Ordering::AcqRel);
        if focused && !previous {
            let app = NSApplication::sharedApplication(
                MainThreadMarker::new().ok_or(AxRegistryError::NotMainThread)?,
            );
            // SAFETY: The live child belongs to this application's focused window.
            unsafe { app.setAccessibilityApplicationFocusedUIElement(Some(&**child)) };
            // SAFETY: This is a live AppKit accessibility element on the main thread.
            unsafe {
                NSAccessibilityPostNotification(
                    &**child,
                    NSAccessibilityFocusedUIElementChangedNotification,
                )
            };
        } else if !focused && previous {
            Self::clear_app_focus_if_child(child);
        }
        Ok(())
    }

    /// Return whether a currently registered control identity exists.
    pub fn contains(&self, id: &str) -> bool {
        self.children.contains_key(id)
    }

    fn clear_app_focus_if_child(child: &NativeChild) {
        let Some(marker) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(marker);
        if app
            .accessibilityApplicationFocusedUIElement()
            .as_ref()
            .is_some_and(|focused| {
                std::ptr::eq(
                    &**focused,
                    (child as *const NativeChild).cast::<AnyObject>(),
                )
            })
        {
            // SAFETY: Clearing the focus target prevents an inactive or retired
            // child from remaining the application's reported AX focus.
            unsafe { app.setAccessibilityApplicationFocusedUIElement(None) };
            // SAFETY: The AppKit application is live on the main thread.
            unsafe {
                NSAccessibilityPostNotification(
                    &*app,
                    NSAccessibilityFocusedUIElementChangedNotification,
                )
            };
        }
    }

    fn retire(child: &NativeChild) {
        Self::clear_app_focus_if_child(child);
        child.ivars().alive.store(false, Ordering::Release);
        child.ivars().enabled.store(false, Ordering::Release);
        child.ivars().editable.store(false, Ordering::Release);
        child
            .ivars()
            .slider_configured
            .store(false, Ordering::Release);
        child.ivars().slider_role.store(false, Ordering::Release);
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
