//! Isolated GPUI/AppKit accessibility bridge probe for OPT-1870.
//! Run with `cargo run -p wavecrate-platform --example ax_bridge_probe` on macOS.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This probe requires macOS");
}

#[cfg(target_os = "macos")]
mod macos {
    use async_channel::{Receiver, Sender};
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, Focusable, IntoElement, Render, Task,
        Window, WindowBounds, WindowOptions, canvas, div, prelude::*, px, rgb, size,
    };
    use objc2::rc::Retained;
    use objc2::{AnyThread, DefinedClass, MainThreadMarker, define_class, msg_send};
    use objc2_app_kit::{
        NSAccessibility, NSAccessibilityButtonRole, NSAccessibilityElement, NSView,
    };
    use objc2_foundation::{NSArray, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone, Copy)]
    enum Input {
        Press,
        Focus,
    }

    struct ButtonIvars {
        input: Sender<Input>,
        focused: AtomicBool,
    }

    define_class!(
        // SAFETY: NSAccessibilityElement permits subclassing. AX callbacks
        // only enqueue an event or read atomic state; GPUI and AppKit state
        // is updated later on the main thread.
        #[unsafe(super(NSAccessibilityElement))]
        #[thread_kind = AnyThread]
        #[ivars = ButtonIvars]
        #[name = "WavecrateAxBridgeProbeButton"]
        struct ButtonElement;

        impl ButtonElement {
            #[unsafe(method(accessibilityPerformPress))]
            fn perform_press(&self) -> bool {
                self.ivars().input.try_send(Input::Press).is_ok()
            }

            #[unsafe(method(setAccessibilityFocused:))]
            fn set_focused(&self, focused: bool) {
                if focused {
                    let _ = self.ivars().input.try_send(Input::Focus);
                }
            }

            #[unsafe(method(isAccessibilityFocused))]
            fn is_focused(&self) -> bool {
                self.ivars().focused.load(Ordering::Acquire)
            }
        }

        unsafe impl NSObjectProtocol for ButtonElement {}
    );

    impl ButtonElement {
        fn new(marker: MainThreadMarker, input: Sender<Input>) -> Retained<Self> {
            let this = marker.alloc().set_ivars(ButtonIvars {
                input,
                focused: AtomicBool::new(false),
            });
            // SAFETY: The allocated subclass uses its superclass initializer.
            unsafe { msg_send![super(this), init] }
        }
    }

    struct Probe {
        pressed: bool,
        focus: FocusHandle,
        native_view: Retained<NSView>,
        native_button: Retained<ButtonElement>,
        _events: Task<()>,
    }

    impl Probe {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let marker = MainThreadMarker::new().expect("AppKit main thread");
            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
                panic!("expected AppKit window handle");
            };
            // SAFETY: Borrowed GPUI window handle has a valid NSView; retain it
            // while this view and its AX child are alive.
            let native_view: Retained<NSView> =
                unsafe { Retained::retain(handle.ns_view.as_ptr().cast()).expect("GPUI NSView") };
            let (sender, receiver) = async_channel::bounded(8);
            let native_button = ButtonElement::new(marker, sender);
            // SAFETY: AppKit exports the process-lifetime button role string.
            native_button.setAccessibilityRole(Some(unsafe { NSAccessibilityButtonRole }));
            native_button.setAccessibilityLabel(Some(&NSString::from_str("Probe toggle")));
            let initial_value = NSString::from_str("Off");
            // SAFETY: NSString is an Objective-C object suitable for AXValue.
            unsafe { native_button.setAccessibilityValue(Some(&initial_value)) };
            // SAFETY: NSView is the actual parent of this GPUI-drawn button.
            unsafe { native_button.setAccessibilityParent(Some(&native_view)) };
            // SAFETY: ButtonElement is an Objective-C subclass of NSObject.
            let erased = unsafe { Retained::cast_unchecked(native_button.clone()) };
            let children: Retained<NSArray> = NSArray::from_retained_slice(&[erased]);
            // SAFETY: NSArray contains NSAccessibilityElement subclasses.
            unsafe { native_view.setAccessibilityChildren(Some(&children)) };
            let task = Self::receive(receiver, window, cx);
            Self {
                pressed: false,
                focus: cx.focus_handle(),
                native_view,
                native_button,
                _events: task,
            }
        }

        fn receive(receiver: Receiver<Input>, window: &Window, cx: &mut Context<Self>) -> Task<()> {
            cx.spawn_in(window, async move |this, cx| {
                while let Ok(input) = receiver.recv().await {
                    if this
                        .update_in(cx, |view, window, cx| match input {
                            Input::Press => view.toggle(cx),
                            Input::Focus => {
                                window.activate_window();
                                window.focus(&view.focus);
                                view.native_button.ivars().focused.store(
                                    view.focus.is_focused(window) && window.is_window_active(),
                                    Ordering::Release,
                                );
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        }

        fn toggle(&mut self, cx: &mut Context<Self>) {
            self.pressed = !self.pressed;
            let value = NSString::from_str(if self.pressed { "On" } else { "Off" });
            // SAFETY: NSString is an Objective-C object suitable for AXValue.
            unsafe { self.native_button.setAccessibilityValue(Some(&value)) };
            cx.notify();
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            // SAFETY: Remove only the proof child owned by this view before GPUI
            // releases the root entity. AppKit owns no callback after this point.
            unsafe {
                self.native_view.setAccessibilityChildren(None);
                self.native_button.setAccessibilityParent(None);
            }
            eprintln!("OPT-1870 AX child detached");
        }
    }

    impl Focusable for Probe {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus.clone()
        }
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.native_button.ivars().focused.store(
                self.focus.is_focused(window) && window.is_window_active(),
                Ordering::Release,
            );
            let button = self.native_button.clone();
            let view = self.native_view.clone();
            let label = if self.pressed { "On" } else { "Off" };
            div()
                .size_full()
                .bg(rgb(0x202020))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .id("probe-toggle")
                        .relative()
                        .w(px(220.0))
                        .h(px(64.0))
                        .bg(rgb(0x305070))
                        .text_color(rgb(0xffffff))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .on_click(cx.listener(|probe, _, _, cx| probe.toggle(cx)))
                        .child(format!("Probe toggle: {label}"))
                        .child(
                            canvas(
                                move |bounds, _, _| {
                                    let height = view.bounds().size.height;
                                    button.setAccessibilityFrameInParentSpace(NSRect::new(
                                        NSPoint::new(
                                            f64::from(f32::from(bounds.origin.x)),
                                            height
                                                - f64::from(f32::from(bounds.origin.y))
                                                - f64::from(f32::from(bounds.size.height)),
                                        ),
                                        NSSize::new(
                                            f64::from(f32::from(bounds.size.width)),
                                            f64::from(f32::from(bounds.size.height)),
                                        ),
                                    ));
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .inset_0(),
                        ),
                )
        }
    }

    pub fn run() {
        Application::new().run(|cx: &mut App| {
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(500.0), px(300.0)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| Probe::new(window, cx)),
            )
            .expect("open probe window");
            cx.activate(true);
        });
    }
}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}
