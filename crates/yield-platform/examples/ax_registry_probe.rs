//! Two-control native AX registry fixture for OPT-1872.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This probe requires macOS");
}

#[cfg(target_os = "macos")]
mod macos {
    use async_channel::Receiver;
    use gpui::{
        App, Application, Bounds, Context, FocusHandle, Focusable, IntoElement, Render,
        Subscription, Task, Window, WindowBounds, WindowOptions, canvas, div, prelude::*, px, rgb,
        size,
    };
    use raw_window_handle::HasWindowHandle;
    use std::cell::RefCell;
    use std::rc::Rc;
    use yield_platform::accessibility::{AxAction, AxChild, AxEvent, AxFrame, AxRegistry, AxRole};

    const FIRST: &str = "first-toggle";
    const SECOND: &str = "second-control";

    struct Probe {
        registry: Rc<RefCell<AxRegistry>>,
        first_focus: FocusHandle,
        second_focus: FocusHandle,
        first_on: bool,
        second_stage: u8,
        reversed: bool,
        _activation: Subscription,
        _events: Task<()>,
    }

    impl Probe {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let (registry, receiver) = AxRegistry::new(window.window_handle().unwrap())
                .expect("attach AX registry to GPUI view");
            let activation = cx.observe_window_activation(window, |view, window, cx| {
                view.sync_focus(window);
                window.refresh();
                cx.notify();
            });
            let events = Self::receive(receiver, window, cx);
            let mut probe = Self {
                registry: Rc::new(RefCell::new(registry)),
                first_focus: cx.focus_handle(),
                second_focus: cx.focus_handle(),
                first_on: false,
                second_stage: 0,
                reversed: false,
                _activation: activation,
                _events: events,
            };
            probe.sync_ax();
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
                        .update_in(cx, |view, window, cx| {
                            // A queued event can outlive a removed child. Recheck the
                            // currently registered GPUI identity before dispatch.
                            if !view.registry.borrow().contains(&event.id) {
                                return;
                            }
                            cx.activate(true);
                            window.activate_window();
                            match event.action {
                                AxAction::Press => view.activate(&event.id, window, cx),
                                AxAction::Focus => {
                                    match event.id.as_str() {
                                        FIRST => window.focus(&view.first_focus),
                                        SECOND => window.focus(&view.second_focus),
                                        _ => return,
                                    }
                                    view.sync_focus(window);
                                    window.refresh();
                                    cx.notify();
                                }
                                AxAction::SetValue(_)
                                | AxAction::Increment
                                | AxAction::Decrement => {}
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
        }

        fn activate(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
            match id {
                FIRST => {
                    window.focus(&self.first_focus);
                    self.first_on = !self.first_on;
                }
                SECOND if self.second_stage == 0 => {
                    if !self.first_on {
                        return;
                    }
                    window.focus(&self.second_focus);
                    self.reversed = true;
                    self.second_stage = 1;
                }
                SECOND if self.second_stage == 1 => {
                    if !self.first_on {
                        return;
                    }
                    self.second_stage = 2;
                    window.focus(&self.first_focus);
                }
                _ => return,
            }
            self.sync_ax();
            self.sync_focus(window);
            window.refresh();
            cx.notify();
        }

        fn sync_ax(&mut self) {
            let first = AxChild {
                id: FIRST.into(),
                role: AxRole::Button,
                label: "First toggle".into(),
                value: Some(if self.first_on { "On" } else { "Off" }.into()),
                enabled: true,
            };
            let second = (self.second_stage < 2).then(|| AxChild {
                id: SECOND.into(),
                role: AxRole::Button,
                label: "Second control".into(),
                value: Some(
                    if self.second_stage == 0 {
                        "Swap next"
                    } else {
                        "Remove next"
                    }
                    .into(),
                ),
                enabled: self.first_on,
            });
            let controls = match (self.reversed, second) {
                (true, Some(second)) => vec![second, first],
                (false, Some(second)) => vec![first, second],
                (_, None) => vec![first],
            };
            self.registry
                .borrow_mut()
                .reconcile(controls)
                .expect("valid AX snapshot");
        }

        fn sync_focus(&self, window: &Window) {
            let registry = self.registry.borrow();
            registry
                .set_focused(
                    FIRST,
                    self.first_focus.is_focused(window) && window.is_window_active(),
                )
                .expect("first registered");
            if registry.contains(SECOND) {
                registry
                    .set_focused(
                        SECOND,
                        self.second_focus.is_focused(window) && window.is_window_active(),
                    )
                    .expect("second registered");
            }
        }

        fn control(
            &self,
            id: &'static str,
            label: String,
            enabled: bool,
            cx: &mut Context<Self>,
        ) -> impl IntoElement + 'static {
            let registry = self.registry.clone();
            let focus = if id == FIRST {
                &self.first_focus
            } else {
                &self.second_focus
            };
            div()
                .id(id)
                .track_focus(focus)
                .relative()
                .w(px(260.0))
                .h(px(64.0))
                .bg(rgb(0x305070))
                .text_color(if enabled {
                    rgb(0xffffff)
                } else {
                    rgb(0x999999)
                })
                .flex()
                .items_center()
                .justify_center()
                .when(enabled, |element| {
                    element
                        .cursor_pointer()
                        .on_click(cx.listener(move |probe, _, window, cx| {
                            probe.activate(id, window, cx);
                        }))
                })
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
                                .expect("visible child registered");
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            self.registry
                .borrow_mut()
                .reconcile(Vec::new())
                .expect("detach children");
            eprintln!("OPT-1872 AX children detached");
        }
    }

    impl Focusable for Probe {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.first_focus.clone()
        }
    }

    impl Render for Probe {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.sync_focus(window);
            let first = self.control(
                FIRST,
                format!("First toggle: {}", if self.first_on { "On" } else { "Off" }),
                true,
                cx,
            );
            let second = (self.second_stage < 2).then(|| {
                self.control(
                    SECOND,
                    if self.second_stage == 0 {
                        "Swap order"
                    } else {
                        "Remove this control"
                    }
                    .into(),
                    self.first_on,
                    cx,
                )
            });
            let mut row = div().flex().gap(px(16.0));
            if self.reversed {
                if let Some(second) = second {
                    row = row.child(second);
                }
                row = row.child(first);
            } else {
                row = row.child(first);
                if let Some(second) = second {
                    row = row.child(second);
                }
            }
            div()
                .size_full()
                .bg(rgb(0x202020))
                .flex()
                .items_center()
                .justify_center()
                .child(row)
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
            let bounds = Bounds::centered(None, size(px(700.0), px(300.0)), cx);
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
