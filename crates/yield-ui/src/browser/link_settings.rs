//! Session-only controls and status for the optional Link provider.

use std::time::Duration;

use gpui::{Context, Div, FocusHandle, Modifiers, Task, Timer, div, prelude::*, px};
use yield_design::Theme;
use yield_link::{LinkObservation, LinkObserver};

use super::{Browser, general_settings::control};

pub(super) const CONTROL: &str = "settings-link-toggle";
pub(super) const STATUS: &str = "settings-link-status";
const STATUS_REFRESH: Duration = Duration::from_millis(250);

pub(super) struct State {
    focus: FocusHandle,
    observer: Option<LinkObserver>,
    observation: LinkObservation,
    poll: Option<Task<()>>,
}

impl State {
    pub(super) fn new(cx: &mut Context<Browser>) -> Self {
        Self {
            focus: cx.focus_handle().tab_stop(true),
            observer: None,
            observation: LinkObservation::Connecting,
            poll: None,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.observer.is_some()
    }

    pub(super) fn label(&self) -> String {
        status_label(self.observer.as_ref().map(|_| self.observation))
    }
}

fn status_label(observation: Option<LinkObservation>) -> String {
    match observation {
        None => String::from("Off"),
        Some(LinkObservation::Connecting) => String::from("Waiting for Carabiner"),
        Some(LinkObservation::Unavailable) => String::from("Carabiner unavailable"),
        Some(LinkObservation::NoPeers { tempo_bpm }) => format!("{tempo_bpm:.2} BPM · No peers"),
        Some(LinkObservation::Following { tempo_bpm, peers }) => {
            let label = if peers == 1 { "peer" } else { "peers" };
            format!("{tempo_bpm:.2} BPM · {peers} {label}")
        }
    }
}

impl Browser {
    pub(super) fn toggle_link(&mut self, cx: &mut Context<Self>) {
        if self.link_settings.observer.is_some() {
            self.link_settings.poll = None;
            self.link_settings.observer = None;
            cx.notify();
            return;
        }
        match LinkObserver::start() {
            Ok(observer) => {
                self.link_settings.observer = Some(observer);
                self.link_settings.observation = LinkObservation::Connecting;
                self.link_settings.poll = Some(cx.spawn(async move |this, cx| {
                    loop {
                        Timer::after(STATUS_REFRESH).await;
                        let enabled = this
                            .update(cx, |view, cx| {
                                let Some(observer) = &view.link_settings.observer else {
                                    return false;
                                };
                                if let Some(observation) = observer.observation()
                                    && observation != view.link_settings.observation
                                {
                                    view.link_settings.observation = observation;
                                    cx.notify();
                                }
                                true
                            })
                            .unwrap_or(false);
                        if !enabled {
                            break;
                        }
                    }
                }));
            }
            Err(error) => self.set_playback_status(format!("Could not start Link: {error}")),
        }
        cx.notify();
    }
}

pub(super) fn render(browser: &Browser, theme: &Theme, cx: &mut Context<Browser>) -> Div {
    let ax_focus = browser.ax_focus(CONTROL);
    let pointer_focus = ax_focus
        .clone()
        .unwrap_or_else(|| browser.link_settings.focus.clone());
    let label = if browser.link_settings.enabled() {
        "Disable"
    } else {
        "Enable"
    };
    let toggle = control(browser, CONTROL, label, true, theme)
        .when(ax_focus.is_none(), |button| {
            button
                .track_focus(&browser.link_settings.focus)
                .on_key_down(cx.listener(|view, event: &gpui::KeyDownEvent, _, cx| {
                    if event.keystroke.modifiers == Modifiers::none()
                        && matches!(event.keystroke.key.as_str(), "space" | "enter" | "return")
                    {
                        if !event.is_held {
                            view.toggle_link(cx);
                        }
                        cx.stop_propagation();
                    }
                }))
        })
        .on_click(cx.listener(move |view, _, window, cx| {
            window.focus(&pointer_focus);
            view.toggle_link(cx);
        }));
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(theme.spacing.control_gap))
        .child("Ableton Link")
        .child(toggle)
        .child(
            div()
                .debug_selector(|| STATUS.to_owned())
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .when_some(browser.ax_marker(STATUS), |element, marker| {
                    element.child(marker)
                })
                .child(browser.link_settings.label()),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn settings_exposes_disabled_link_without_starting_a_worker(cx: &mut gpui::TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            assert!(!view.link_settings.enabled());
            assert!(view.link_settings.poll.is_none());
            view.theme_settings_open = true;
            cx.notify();
        });
        cx.refresh().unwrap();
        assert!(cx.debug_bounds(CONTROL).is_some());
        assert!(cx.debug_bounds(STATUS).is_some());
        cx.update(|_, app| {
            let view = browser.read(app);
            assert!(!view.link_settings.enabled());
            assert!(view.link_settings.poll.is_none());
            assert_eq!(view.link_settings.label(), "Off");
        });
    }

    #[test]
    fn status_distinguishes_disabled_unavailable_and_peer_states() {
        assert_eq!(status_label(None), "Off");
        assert_eq!(
            status_label(Some(LinkObservation::Connecting)),
            "Waiting for Carabiner"
        );
        assert_eq!(
            status_label(Some(LinkObservation::Unavailable)),
            "Carabiner unavailable"
        );
        assert_eq!(
            status_label(Some(LinkObservation::NoPeers { tempo_bpm: 120.0 })),
            "120.00 BPM · No peers"
        );
        assert_eq!(
            status_label(Some(LinkObservation::Following {
                peers: 1,
                tempo_bpm: 133.25
            })),
            "133.25 BPM · 1 peer"
        );
    }
}
