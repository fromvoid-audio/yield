//! Native adjustable-slider AX fixture for OPT-1878.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This probe requires macOS");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::RefCell;
    use std::rc::Rc;

    use async_channel::Receiver;
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, IntoElement, Render, Task, Window,
        WindowBounds, WindowOptions, canvas, div, prelude::*, px, rgb, size,
    };
    use raw_window_handle::HasWindowHandle;
    use yield_platform::accessibility::{
        AxAction, AxChild, AxEvent, AxFrame, AxRegistry, AxRole, AxSlider,
    };

    const SLIDER: &str = "slider-probe-volume";
    const TOGGLE: &str = "slider-probe-toggle";
    const REMOVE: &str = "slider-probe-remove";
    const STEP: f64 = 0.1;

    struct Probe {
        registry: Rc<RefCell<AxRegistry>>,
        slider_focus: FocusHandle,
        toggle_focus: FocusHandle,
        remove_focus: FocusHandle,
        value: f64,
        enabled: bool,
        visible: bool,
        _events: Task<()>,
    }

    impl Probe {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let (registry, receiver) =
                AxRegistry::new(window.window_handle().unwrap()).expect("attach AX registry");
            let mut probe = Self {
                registry: Rc::new(RefCell::new(registry)),
                slider_focus: cx.focus_handle(),
                toggle_focus: cx.focus_handle(),
                remove_focus: cx.focus_handle(),
                value: 0.5,
                enabled: true,
                visible: true,
                _events: Self::receive(receiver, window, cx),
            };
            probe.sync_ax(window);
            probe
        }

        fn receive(
            receiver: Receiver<AxEvent>,
            window: &Window,
            cx: &mut Context<Self>,
        ) -> Task<()> {
            cx.spawn_in(window, async move |this, cx| {
                while let Ok(event) = receiver.recv().await {
                    if this
                        .update_in(cx, |probe, window, cx| {
                            if !probe.registry.borrow().contains(&event.id) {
                                return;
                            }
                            match (event.id.as_str(), event.action) {
                                (SLIDER, AxAction::SetValue(value))
                                    if probe.enabled && probe.visible =>
                                {
                                    let Ok(value) = value.parse::<f64>() else {
                                        return;
                                    };
                                    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
                                        return;
                                    }
                                    probe.value = value;
                                    window.focus(&probe.slider_focus);
                                }
                                (SLIDER, AxAction::Increment) if probe.enabled && probe.visible => {
                                    probe.value = (probe.value + STEP).min(1.0);
                                    window.focus(&probe.slider_focus);
                                }
                                (SLIDER, AxAction::Decrement) if probe.enabled && probe.visible => {
                                    probe.value = (probe.value - STEP).max(0.0);
                                    window.focus(&probe.slider_focus);
                                }
                                (SLIDER, AxAction::Focus) if probe.enabled && probe.visible => {
                                    window.focus(&probe.slider_focus)
                                }
                                (TOGGLE, AxAction::Press) => {
                                    probe.enabled = !probe.enabled;
                                    window.focus(&probe.toggle_focus);
                                }
                                (TOGGLE, AxAction::Focus) => window.focus(&probe.toggle_focus),
                                (REMOVE, AxAction::Press) => {
                                    probe.visible = false;
                                    window.focus(&probe.remove_focus);
                                }
                                (REMOVE, AxAction::Focus) => window.focus(&probe.remove_focus),
                                _ => return,
                            }
                            probe.sync_ax(window);
                            window.refresh();
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        }

        fn sync_ax(&mut self, window: &Window) {
            let mut controls = Vec::new();
            if self.visible {
                controls.push(AxChild {
                    id: SLIDER.into(),
                    role: AxRole::Slider,
                    label: "Probe volume".into(),
                    value: None,
                    enabled: self.enabled,
                });
            }
            controls.extend([
                AxChild {
                    id: TOGGLE.into(),
                    role: AxRole::Button,
                    label: "Toggle slider enabled".into(),
                    value: None,
                    enabled: true,
                },
                AxChild {
                    id: REMOVE.into(),
                    role: AxRole::Button,
                    label: "Remove slider".into(),
                    value: None,
                    enabled: true,
                },
            ]);
            let mut registry = self.registry.borrow_mut();
            registry.reconcile(controls).expect("valid AX snapshot");
            if self.visible {
                registry
                    .set_slider(
                        SLIDER,
                        AxSlider {
                            value: self.value,
                            min: 0.0,
                            max: 1.0,
                        },
                    )
                    .expect("valid slider");
                registry
                    .set_focused(SLIDER, self.slider_focus.is_focused(window))
                    .unwrap();
            }
            registry
                .set_focused(TOGGLE, self.toggle_focus.is_focused(window))
                .unwrap();
            registry
                .set_focused(REMOVE, self.remove_focus.is_focused(window))
                .unwrap();
        }

        fn control(&self, id: &'static str, label: String) -> impl IntoElement + 'static {
            let registry = self.registry.clone();
            let focus = match id {
                SLIDER => &self.slider_focus,
                TOGGLE => &self.toggle_focus,
                _ => &self.remove_focus,
            };
            div()
                .id(id)
                .track_focus(focus)
                .relative()
                .w(px(180.0))
                .h(px(60.0))
                .flex()
                .items_center()
                .justify_center()
                .bg(rgb(0x305070))
                .text_color(rgb(0xffffff))
                .child(label)
                .child(
                    canvas(
                        move |bounds, _, _| {
                            registry
                                .borrow()
                                .set_frame(
                                    id,
                                    AxFrame {
                                        x: f64::from(f32::from(bounds.origin.x)),
                                        y: f64::from(f32::from(bounds.origin.y)),
                                        width: f64::from(f32::from(bounds.size.width)),
                                        height: f64::from(f32::from(bounds.size.height)),
                                    },
                                )
                                .expect("visible child");
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
        }
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.sync_ax(window);
            div()
                .size_full()
                .bg(rgb(0x202020))
                .flex()
                .gap(px(20.0))
                .items_center()
                .justify_center()
                .when(self.visible, |element| {
                    element.child(self.control(SLIDER, format!("Volume {:.1}", self.value)))
                })
                .child(self.control(TOGGLE, format!("Enabled: {}", self.enabled)))
                .child(self.control(REMOVE, "Remove".into()))
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
            let bounds = Bounds::centered(None, size(px(760.0), px(280.0)), cx);
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
