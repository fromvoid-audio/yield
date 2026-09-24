//! Native editable AX field fixture for OPT-1875.

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
    use yield_platform::accessibility::{AxAction, AxChild, AxEvent, AxFrame, AxRegistry, AxRole};

    const EDIT: &str = "text-probe-edit";
    const PRESS: &str = "text-probe-press";

    struct Probe {
        registry: Rc<RefCell<AxRegistry>>,
        edit_focus: FocusHandle,
        press_focus: FocusHandle,
        value: String,
        presses: usize,
        _events: Task<()>,
    }

    impl Probe {
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let (registry, receiver) =
                AxRegistry::new(window.window_handle().unwrap()).expect("attach AX registry");
            let mut probe = Self {
                registry: Rc::new(RefCell::new(registry)),
                edit_focus: cx.focus_handle(),
                press_focus: cx.focus_handle(),
                value: "Initial".into(),
                presses: 0,
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
                            cx.activate(true);
                            window.activate_window();
                            match (event.id.as_str(), event.action) {
                                (EDIT, AxAction::SetValue(value)) => {
                                    probe.value = value;
                                    window.focus(&probe.edit_focus);
                                }
                                (EDIT, AxAction::Focus) => window.focus(&probe.edit_focus),
                                (PRESS, AxAction::Press) => {
                                    probe.presses += 1;
                                    window.focus(&probe.press_focus);
                                }
                                (PRESS, AxAction::Focus) => window.focus(&probe.press_focus),
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
            self.registry
                .borrow_mut()
                .reconcile(vec![
                    AxChild {
                        id: EDIT.into(),
                        role: AxRole::TextField,
                        label: "Probe edit".into(),
                        value: Some(self.value.clone()),
                        enabled: true,
                    },
                    AxChild {
                        id: PRESS.into(),
                        role: AxRole::Button,
                        label: "Probe press".into(),
                        value: Some(self.presses.to_string()),
                        enabled: true,
                    },
                ])
                .expect("valid AX snapshot");
            let registry = self.registry.borrow();
            registry
                .set_focused(EDIT, self.edit_focus.is_focused(window))
                .unwrap();
            registry
                .set_focused(PRESS, self.press_focus.is_focused(window))
                .unwrap();
        }

        fn control(&self, id: &'static str, label: String) -> impl IntoElement + 'static {
            let registry = self.registry.clone();
            let focus = if id == EDIT {
                &self.edit_focus
            } else {
                &self.press_focus
            };
            div()
                .id(id)
                .track_focus(focus)
                .relative()
                .w(px(250.0))
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
                .child(self.control(EDIT, self.value.clone()))
                .child(self.control(PRESS, format!("Presses: {}", self.presses)))
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
            let bounds = Bounds::centered(None, size(px(650.0), px(280.0)), cx);
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
