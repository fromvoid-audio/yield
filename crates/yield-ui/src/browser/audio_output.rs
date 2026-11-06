#![deny(missing_docs)]
#![deny(warnings)]

//! Audio-output discovery, selection, and asynchronous reconfiguration.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use gpui::{
    App, ClickEvent, Context, KeyDownEvent, MouseButton, MouseDownEvent, Window, div, prelude::*,
    px,
};
use reson::{
    AudioDeviceSummary, AudioHostSummary, AudioOutputConfig, AudioPlayer, PlaybackRuntime,
    PlaybackRuntimeConfig, PlaybackRuntimeEventRecvTimeoutError, ResolvedOutput, available_devices,
    available_hosts, supported_sample_rates,
};
use yield_session::{AudioOutputPreferences, PreferencePatch};

use super::Browser;
use crate::theme::color;

const AUDIO_OUTPUT_DROPDOWN_VISIBLE_ROWS: usize = 6;
const AUDIO_OUTPUT_DROPDOWN_MAX_HEIGHT: f32 = 160.0;

/// The audio-output dropdown currently expanded in the settings strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AudioOutputDropdown {
    /// Select an output backend.
    Host,
    /// Select an output device.
    Device,
    /// Select a device sample rate.
    SampleRate,
}

#[derive(Debug)]
struct AudioOptionsResult {
    generation: u64,
    config: AudioOutputConfig,
    hosts: Vec<AudioHostSummary>,
    devices: Vec<AudioDeviceSummary>,
    sample_rates: Vec<u32>,
    error: Option<String>,
}

struct AudioOpenResult {
    runtime: PlaybackRuntime,
    resolved: ResolvedOutput,
}

enum AudioOutputOpenError {
    RetirementTimedOut(PlaybackRuntime),
    Open(String),
}

impl AudioOutputOpenError {
    fn message(&self) -> String {
        match self {
            Self::RetirementTimedOut(_) => {
                "Timed out waiting for the previous audio output to shut down".to_string()
            }
            Self::Open(error) => error.clone(),
        }
    }

    fn is_retirement_failure(&self) -> bool {
        matches!(self, Self::RetirementTimedOut(_))
    }
}

const AUDIO_OUTPUT_RETIREMENT_TIMEOUT: Duration = Duration::from_secs(5);

impl Browser {
    /// Toggle the compact audio settings panel.
    pub(super) fn toggle_audio_output_settings(
        &mut self,
        _: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.audio_settings_open = !self.audio_settings_open;
        self.audio_output_dropdown = None;
        if self.audio_settings_open {
            self.theme_settings_open = false;
            self.general_settings.updates.close();
            self.general_settings.invalidate_picker();
            self.general_settings.previous_focus.take();
            window.focus(&self.audio_output_focus_handle);
            self.start_audio_output_options_refresh(cx);
        } else {
            self.cancel_audio_output_options();
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    /// Focus the audio settings panel when it receives a pointer press.
    pub(super) fn focus_audio_output_panel(
        &mut self,
        _: &MouseDownEvent,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.focus(&self.audio_output_focus_handle);
    }

    /// Close an expanded audio dropdown when the pointer presses outside the panel.
    pub(super) fn close_audio_output_dropdown(
        &mut self,
        _: &MouseDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.audio_output_dropdown.take().is_some() {
            cx.notify();
        }
    }

    /// Handle keyboard navigation while the audio settings panel owns focus.
    pub(super) fn handle_audio_output_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.audio_output_focus_handle.is_focused(window) || !self.audio_settings_open {
            return false;
        }

        let key = event.keystroke.key.as_str();
        if key.eq_ignore_ascii_case("escape") {
            if self.audio_output_dropdown.take().is_some() {
                cx.notify();
            } else {
                self.audio_settings_open = false;
                window.focus(&self.focus_handle);
                cx.notify();
            }
            return true;
        }

        let Some(dropdown) = self.audio_output_dropdown else {
            if !event.is_held && matches!(key, "enter" | "arrowdown" | "down") {
                self.open_audio_output_dropdown(AudioOutputDropdown::Host, window, cx);
            }
            return matches!(key, "enter" | "arrowdown" | "down");
        };

        let option_count = self.audio_output_option_count(dropdown);
        if option_count == 0 {
            return matches!(key, "enter" | "arrowup" | "arrowdown" | "up" | "down");
        }
        match key {
            "arrowdown" | "down" => {
                self.audio_output_option_index =
                    (self.audio_output_option_index + 1) % option_count;
                self.audio_output_options_scroll_handle
                    .scroll_to_item(self.audio_output_option_index);
                cx.notify();
                true
            }
            "arrowup" | "up" => {
                self.audio_output_option_index = if self.audio_output_option_index == 0 {
                    option_count - 1
                } else {
                    self.audio_output_option_index - 1
                };
                self.audio_output_options_scroll_handle
                    .scroll_to_item(self.audio_output_option_index);
                cx.notify();
                true
            }
            "enter" if !event.is_held => {
                self.activate_audio_output_option(dropdown, cx);
                true
            }
            _ => false,
        }
    }

    /// Return the short status label used by the toolbar action.
    pub(super) fn audio_output_toolbar_label(&self) -> String {
        if self.audio_output_open_task.is_some() {
            return "AUDIO…".to_string();
        }
        if self.audio_output_error.is_some() && self.audio_output_resolved.is_none() {
            return "AUDIO OFF".to_string();
        }
        if self.audio_output_error.is_some() {
            return "AUDIO UNSAVED".to_string();
        }
        if audio_output_can_retry_persistence(
            self.audio_output_dirty,
            self.audio_output_applied_config.as_ref(),
            &self.audio_output_config,
        ) {
            return "AUDIO UNSAVED".to_string();
        }
        if self
            .audio_output_resolved
            .as_ref()
            .is_some_and(|resolved| resolved.used_fallback)
        {
            return "AUDIO FALLBACK".to_string();
        }
        if self.audio_output_resolved.is_some() {
            return "AUDIO".to_string();
        }
        "AUDIO IDLE".to_string()
    }

    /// Render the compact audio settings panel and resolved-output status.
    pub(super) fn render_audio_output_settings(
        &self,
        theme: yield_design::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let editable = self.preferences_editable();
        let retry_hover = color(theme.interaction.hover_fill);
        let desired = if self.audio_output_open_task.is_some() {
            "Opening audio output…".to_string()
        } else {
            desired_output_detail(
                &self.audio_output_config,
                &self.audio_output_resolved,
                &self.audio_output_applied_config,
            )
        };
        let status_color = if self.audio_output_error.is_some() {
            theme.colors.danger
        } else if audio_output_can_retry_persistence(
            self.audio_output_dirty,
            self.audio_output_applied_config.as_ref(),
            &self.audio_output_config,
        ) {
            theme.colors.warning
        } else if self
            .audio_output_resolved
            .as_ref()
            .is_some_and(|output| output.used_fallback)
        {
            theme.colors.warning
        } else {
            theme.colors.text_muted
        };
        let focus_color = color(theme.interaction.focus_outline);
        let focus_width = theme.interaction.focus_outline_width;
        div()
            .id("audio-output-settings")
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(theme.spacing.control_gap))
            .px(px(theme.spacing.panel_padding))
            .py(px(theme.spacing.control_gap))
            .bg(color(theme.colors.surface_overlay))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .text_size(px(theme.typography.small_size))
            .focus(move |style| style.border(px(focus_width)).border_color(focus_color))
            .track_focus(&self.audio_output_focus_handle)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(Self::focus_audio_output_panel),
            )
            .on_mouse_down_out(cx.listener(Self::close_audio_output_dropdown))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .text_color(color(theme.colors.text_muted))
                    .child("AUDIO OUTPUT")
                    .child(div().flex_1())
                    .child(div().text_color(color(status_color)).child(desired)),
            )
            .child(self.render_audio_output_choice(
                "Backend",
                AudioOutputDropdown::Host,
                self.audio_host_label(),
                editable,
                theme.clone(),
                cx,
            ))
            .when(
                self.audio_output_dropdown == Some(AudioOutputDropdown::Host),
                |element| {
                    element.child(self.render_audio_output_host_options(
                        editable,
                        theme.clone(),
                        cx,
                    ))
                },
            )
            .child(self.render_audio_output_choice(
                "Output",
                AudioOutputDropdown::Device,
                self.audio_device_label(),
                editable,
                theme.clone(),
                cx,
            ))
            .when(
                self.audio_output_dropdown == Some(AudioOutputDropdown::Device),
                |element| {
                    element.child(self.render_audio_output_device_options(
                        editable,
                        theme.clone(),
                        cx,
                    ))
                },
            )
            .child(self.render_audio_output_choice(
                "Sample rate",
                AudioOutputDropdown::SampleRate,
                self.audio_sample_rate_label(),
                editable,
                theme.clone(),
                cx,
            ))
            .when(
                self.audio_output_dropdown == Some(AudioOutputDropdown::SampleRate),
                |element| {
                    element.child(self.render_audio_output_sample_rate_options(
                        editable,
                        theme.clone(),
                        cx,
                    ))
                },
            )
            .child(
                div()
                    .text_color(color(theme.colors.text_muted))
                    .child(self.audio_output_detail_label()),
            )
            .when(self.audio_output_error.is_some(), |element| {
                element.child(
                    div()
                        .text_color(color(theme.colors.danger))
                        .child(self.audio_output_error.clone().unwrap_or_default()),
                )
            })
            .when(
                self.audio_output_dirty
                    && audio_output_can_retry_persistence(
                        self.audio_output_dirty,
                        self.audio_output_applied_config.as_ref(),
                        &self.audio_output_config,
                    ),
                |element| {
                    element.child(
                        div()
                            .id("retry-audio-output-save")
                            .flex()
                            .items_center()
                            .h(px(theme.spacing.control_height))
                            .px(px(theme.spacing.control_gap))
                            .cursor_pointer()
                            .border(px(theme.spacing.border_width))
                            .border_color(color(theme.colors.border_emphasis))
                            .text_color(color(theme.colors.text_primary))
                            .hover(move |style| style.bg(retry_hover))
                            .on_click(cx.listener(Self::retry_audio_output_persistence))
                            .child("Retry save"),
                    )
                },
            )
    }

    fn render_audio_output_choice(
        &self,
        label: &'static str,
        dropdown: AudioOutputDropdown,
        value: String,
        editable: bool,
        theme: yield_design::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self.audio_output_dropdown == Some(dropdown);
        let fill = if selected {
            theme.interaction.pressed_fill
        } else {
            theme.colors.surface
        };
        let hover_fill = color(theme.interaction.hover_fill);
        let text = if editable {
            theme.colors.text_primary
        } else {
            theme.colors.text_disabled
        };
        div()
            .id(audio_dropdown_id(dropdown))
            .flex()
            .items_center()
            .gap(px(theme.spacing.control_gap))
            .h(px(theme.spacing.control_height))
            .px(px(theme.spacing.control_gap))
            .bg(color(fill))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border_emphasis))
            .text_color(color(text))
            .when(editable, |element| {
                element
                    .cursor_pointer()
                    .when(!selected, |element| {
                        element.hover(move |style| style.bg(hover_fill))
                    })
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.toggle_audio_output_dropdown(dropdown, window, cx);
                    }))
            })
            .child(label)
            .child(div().flex_1())
            .child(value)
            .child(if selected { "▴" } else { "▾" })
    }

    fn render_audio_output_host_options(
        &self,
        editable: bool,
        theme: yield_design::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut options = Vec::with_capacity(self.audio_output_hosts.len() + 1);
        options.push(self.render_audio_output_option(
            "System default".to_string(),
            self.audio_output_config.host.is_none(),
            self.audio_output_option_index == 0,
            editable,
            theme.clone(),
            Box::new(cx.listener(|view, _, _, cx| view.set_audio_output_host(None, cx))),
        ));
        for (index, host) in self.audio_output_hosts.iter().enumerate() {
            let id = host.id.clone();
            options.push(self.render_audio_output_option(
                format_default_label(&host.label, host.is_default),
                self.audio_output_config.host.as_deref() == Some(host.id.as_str()),
                self.audio_output_option_index == index + 1,
                editable,
                theme.clone(),
                Box::new(cx.listener(move |view: &mut Browser, _, _, cx| {
                    view.set_audio_output_host(Some(id.clone()), cx)
                })),
            ));
        }
        div()
            .id("audio-output-host-options")
            .flex()
            .flex_col()
            .gap(px(1.0))
            .track_scroll(&self.audio_output_options_scroll_handle)
            .when(
                options.len() > AUDIO_OUTPUT_DROPDOWN_VISIBLE_ROWS,
                |element| {
                    element
                        .h(px(AUDIO_OUTPUT_DROPDOWN_MAX_HEIGHT))
                        .overflow_y_scroll()
                },
            )
            .children(options)
    }

    fn render_audio_output_device_options(
        &self,
        editable: bool,
        theme: yield_design::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut options = Vec::with_capacity(self.audio_output_devices.len() + 1);
        options.push(self.render_audio_output_option(
            "Host default".to_string(),
            self.audio_output_config.device.is_none(),
            self.audio_output_option_index == 0,
            editable,
            theme.clone(),
            Box::new(cx.listener(|view, _, _, cx| view.set_audio_output_device(None, cx))),
        ));
        for (index, device) in self.audio_output_devices.iter().enumerate() {
            let name = device.name.clone();
            options.push(self.render_audio_output_option(
                format_default_label(&device.name, device.is_default),
                self.audio_output_config.device.as_deref() == Some(device.name.as_str()),
                self.audio_output_option_index == index + 1,
                editable,
                theme.clone(),
                Box::new(cx.listener(move |view: &mut Browser, _, _, cx| {
                    view.set_audio_output_device(Some(name.clone()), cx)
                })),
            ));
        }
        div()
            .id("audio-output-device-options")
            .flex()
            .flex_col()
            .gap(px(1.0))
            .track_scroll(&self.audio_output_options_scroll_handle)
            .when(
                options.len() > AUDIO_OUTPUT_DROPDOWN_VISIBLE_ROWS,
                |element| {
                    element
                        .h(px(AUDIO_OUTPUT_DROPDOWN_MAX_HEIGHT))
                        .overflow_y_scroll()
                },
            )
            .children(options)
    }

    fn render_audio_output_sample_rate_options(
        &self,
        editable: bool,
        theme: yield_design::Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut options = Vec::with_capacity(self.audio_output_sample_rates.len() + 1);
        options.push(self.render_audio_output_option(
            "Device default".to_string(),
            self.audio_output_config.sample_rate.is_none(),
            self.audio_output_option_index == 0,
            editable,
            theme.clone(),
            Box::new(cx.listener(|view, _, _, cx| view.set_audio_output_sample_rate(None, cx))),
        ));
        for (index, rate) in self.audio_output_sample_rates.iter().enumerate() {
            let rate = *rate;
            options.push(self.render_audio_output_option(
                format_sample_rate(rate),
                self.audio_output_config.sample_rate == Some(rate),
                self.audio_output_option_index == index + 1,
                editable,
                theme.clone(),
                Box::new(cx.listener(move |view: &mut Browser, _, _, cx| {
                    view.set_audio_output_sample_rate(Some(rate), cx)
                })),
            ));
        }
        div()
            .id("audio-output-sample-rate-options")
            .flex()
            .flex_col()
            .gap(px(1.0))
            .track_scroll(&self.audio_output_options_scroll_handle)
            .when(
                options.len() > AUDIO_OUTPUT_DROPDOWN_VISIBLE_ROWS,
                |element| {
                    element
                        .h(px(AUDIO_OUTPUT_DROPDOWN_MAX_HEIGHT))
                        .overflow_y_scroll()
                },
            )
            .children(options)
    }

    fn render_audio_output_option(
        &self,
        label: String,
        selected: bool,
        highlighted: bool,
        editable: bool,
        theme: yield_design::Theme,
        handler: Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App) + 'static>,
    ) -> impl IntoElement {
        let fill = if highlighted {
            theme.interaction.hover_fill
        } else if selected {
            theme.colors.selection_fill
        } else {
            theme.colors.surface
        };
        let text = if editable {
            theme.colors.text_primary
        } else {
            theme.colors.text_disabled
        };
        let hover_fill = color(theme.interaction.hover_fill);
        div()
            .flex()
            .items_center()
            .h(px(theme.spacing.control_height))
            .px(px(theme.spacing.control_gap * 2.0))
            .bg(color(fill))
            .text_color(color(text))
            .when(editable, |element| {
                element
                    .cursor_pointer()
                    .when(!selected && !highlighted, |element| {
                        element.hover(move |style| style.bg(hover_fill))
                    })
                    .on_mouse_down(MouseButton::Left, handler)
            })
            .child(label)
    }

    fn audio_host_label(&self) -> String {
        self.audio_output_config
            .host
            .as_deref()
            .and_then(|id| {
                self.audio_output_hosts
                    .iter()
                    .find(|host| host.id == id)
                    .map(|host| host.label.clone())
            })
            .or_else(|| self.audio_output_config.host.clone())
            .unwrap_or_else(|| "System default".to_string())
    }

    fn audio_device_label(&self) -> String {
        self.audio_output_config
            .device
            .clone()
            .unwrap_or_else(|| "Host default".to_string())
    }

    fn audio_sample_rate_label(&self) -> String {
        self.audio_output_config
            .sample_rate
            .map(format_sample_rate)
            .unwrap_or_else(|| "Device default".to_string())
    }

    fn audio_output_detail_label(&self) -> String {
        let Some(output) = self.audio_output_resolved.as_ref() else {
            return self.audio_output_status.clone();
        };
        let buffer = output.buffer_size_frames.map_or_else(
            || "driver buffer".to_string(),
            |frames| format!("{frames} frames"),
        );
        let fallback = if output.used_fallback {
            " · fallback"
        } else {
            ""
        };
        let unsaved = if audio_output_can_retry_persistence(
            self.audio_output_dirty,
            self.audio_output_applied_config.as_ref(),
            &self.audio_output_config,
        ) {
            " · settings not saved"
        } else {
            ""
        };
        format!(
            "Using {} · {} · {} · {}{}{}",
            output.host_id,
            output.device_name,
            format_sample_rate(output.sample_rate),
            buffer,
            fallback,
            unsaved,
        )
    }

    fn toggle_audio_output_dropdown(
        &mut self,
        dropdown: AudioOutputDropdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.audio_output_focus_handle);
        if self.audio_output_dropdown == Some(dropdown) {
            self.audio_output_dropdown = None;
        } else {
            self.audio_output_dropdown = Some(dropdown);
            self.audio_output_option_index = self.audio_output_current_option_index(dropdown);
            self.audio_output_options_scroll_handle
                .scroll_to_item(self.audio_output_option_index);
        }
        cx.notify();
    }

    fn open_audio_output_dropdown(
        &mut self,
        dropdown: AudioOutputDropdown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.audio_output_dropdown = Some(dropdown);
        self.audio_output_option_index = self.audio_output_current_option_index(dropdown);
        self.audio_output_options_scroll_handle
            .scroll_to_item(self.audio_output_option_index);
        window.focus(&self.audio_output_focus_handle);
        cx.notify();
    }

    fn audio_output_option_count(&self, dropdown: AudioOutputDropdown) -> usize {
        match dropdown {
            AudioOutputDropdown::Host => self.audio_output_hosts.len() + 1,
            AudioOutputDropdown::Device => self.audio_output_devices.len() + 1,
            AudioOutputDropdown::SampleRate => self.audio_output_sample_rates.len() + 1,
        }
    }

    fn audio_output_current_option_index(&self, dropdown: AudioOutputDropdown) -> usize {
        match dropdown {
            AudioOutputDropdown::Host => self
                .audio_output_config
                .host
                .as_deref()
                .and_then(|host| {
                    self.audio_output_hosts
                        .iter()
                        .position(|candidate| candidate.id == host)
                })
                .map_or(0, |index| index + 1),
            AudioOutputDropdown::Device => self
                .audio_output_config
                .device
                .as_deref()
                .and_then(|device| {
                    self.audio_output_devices
                        .iter()
                        .position(|candidate| candidate.name == device)
                })
                .map_or(0, |index| index + 1),
            AudioOutputDropdown::SampleRate => self
                .audio_output_config
                .sample_rate
                .and_then(|rate| {
                    self.audio_output_sample_rates
                        .iter()
                        .position(|candidate| *candidate == rate)
                })
                .map_or(0, |index| index + 1),
        }
    }

    fn activate_audio_output_option(
        &mut self,
        dropdown: AudioOutputDropdown,
        cx: &mut Context<Self>,
    ) {
        let index = self.audio_output_option_index;
        self.audio_output_dropdown = None;
        match dropdown {
            AudioOutputDropdown::Host => {
                let host = index
                    .checked_sub(1)
                    .and_then(|index| self.audio_output_hosts.get(index))
                    .map(|host| host.id.clone());
                self.set_audio_output_host(host, cx);
            }
            AudioOutputDropdown::Device => {
                let device = index
                    .checked_sub(1)
                    .and_then(|index| self.audio_output_devices.get(index))
                    .map(|device| device.name.clone());
                self.set_audio_output_device(device, cx);
            }
            AudioOutputDropdown::SampleRate => {
                let rate = index
                    .checked_sub(1)
                    .and_then(|index| self.audio_output_sample_rates.get(index))
                    .copied();
                self.set_audio_output_sample_rate(rate, cx);
            }
        }
        cx.notify();
    }

    fn start_audio_output_options_refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.audio_output_options_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.audio_output_options_generation = self.audio_output_options_generation.wrapping_add(1);
        let generation = self.audio_output_options_generation;
        let config = self.audio_output_config.clone();
        let cancellation = Arc::new(AtomicBool::new(false));
        self.audio_output_options_cancel = Some(Arc::clone(&cancellation));
        self.audio_output_options_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    discover_audio_options(&config, generation, || {
                        cancellation.load(Ordering::Acquire)
                    })
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                if view.audio_output_options_generation != result.generation {
                    return;
                }
                if result.config != view.audio_output_config {
                    view.audio_output_options_cancel = None;
                    view.audio_output_options_task = None;
                    return;
                }
                view.audio_output_options_cancel = None;
                view.audio_output_options_task = None;
                view.audio_output_hosts = result.hosts;
                view.audio_output_devices = result.devices;
                view.audio_output_sample_rates = result.sample_rates;
                if let Some(dropdown) = view.audio_output_dropdown {
                    let option_count = view.audio_output_option_count(dropdown);
                    if option_count > 0 {
                        view.audio_output_option_index %= option_count;
                        view.audio_output_options_scroll_handle
                            .scroll_to_item(view.audio_output_option_index);
                    }
                }
                if let Some(error) = result.error {
                    view.audio_output_error = Some(error.clone());
                    view.set_audio_output_status("Audio options unavailable".to_string());
                }
                cx.notify();
            });
        }));
    }

    pub(super) fn set_audio_output_host(&mut self, host: Option<String>, cx: &mut Context<Self>) {
        let mut config = self.audio_output_config.clone();
        if config.host == host {
            return;
        }
        config.host = host;
        config.device = None;
        config.sample_rate = None;
        self.apply_audio_output_config(config, cx);
    }

    pub(super) fn set_audio_output_device(
        &mut self,
        device: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let mut config = self.audio_output_config.clone();
        if config.device == device {
            return;
        }
        config.device = device;
        config.sample_rate = None;
        self.apply_audio_output_config(config, cx);
    }

    pub(super) fn set_audio_output_sample_rate(
        &mut self,
        sample_rate: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        let mut config = self.audio_output_config.clone();
        if config.sample_rate == sample_rate {
            return;
        }
        config.sample_rate = sample_rate;
        self.apply_audio_output_config(config, cx);
    }

    fn apply_audio_output_config(&mut self, config: AudioOutputConfig, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        self.audio_output_config = config;
        self.audio_output_dirty = true;
        self.audio_output_error = None;
        self.set_audio_output_status("Audio output change pending".to_string());
        self.audio_output_dropdown = None;
        self.start_audio_output_options_refresh(cx);
        self.reconfigure_audio_output(cx);
    }

    fn retry_audio_output_persistence(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !audio_output_can_retry_persistence(
            self.audio_output_dirty,
            self.audio_output_applied_config.as_ref(),
            &self.audio_output_config,
        ) {
            return;
        }
        if self.submit_preference_patch(
            PreferencePatch::SetAudioOutput(audio_preferences_from_config(
                &self.audio_output_config,
            )),
            cx,
        ) {
            self.audio_output_dirty = false;
            self.audio_output_error = None;
            self.set_audio_output_status("Audio output ready · saving settings…".to_string());
            cx.notify();
        }
    }

    pub(super) fn apply_preference_audio_output(
        &mut self,
        preferences: &AudioOutputPreferences,
        reset_pending: bool,
        cx: &mut Context<Self>,
    ) {
        if self.audio_output_dirty && !reset_pending {
            return;
        }
        let config = audio_config_from_preferences(preferences);
        if config == self.audio_output_config {
            if reset_pending {
                self.audio_output_dirty = false;
                self.audio_output_error = None;
                self.set_audio_output_status("Audio output reset".to_string());
            }
            return;
        }
        self.audio_output_config = config;
        if reset_pending {
            self.audio_output_dirty = false;
            self.audio_output_error = None;
        }
        self.set_audio_output_status(
            if self
                .audio_output_applied_config
                .as_ref()
                .is_some_and(|applied| applied != &self.audio_output_config)
            {
                "Audio output preference loaded · applies on next audition".to_string()
            } else {
                "Audio output preference loaded".to_string()
            },
        );
        if self.audio_settings_open {
            self.start_audio_output_options_refresh(cx);
        }
    }

    pub(super) fn audio_output_persistence_failed(&mut self, error: String) {
        self.audio_output_dirty = true;
        self.audio_output_error = Some(error);
        self.set_audio_output_status("Audio output ready · settings not saved".to_string());
    }

    pub(super) fn cancel_audio_output_options(&mut self) {
        if let Some(cancel) = self.audio_output_options_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        self.audio_output_options_task.take();
    }

    fn audio_output_is_current(&self, config: &AudioOutputConfig, generation: u64) -> bool {
        audio_output_open_is_current(
            self.audio_output_open_generation,
            &self.audio_output_config,
            generation,
            config,
        )
    }

    pub(super) fn retire_audio_output_runtime(&mut self, runtime: PlaybackRuntime) {
        debug_assert!(self.audio_output_retiring.is_none());
        let _ = runtime.handle.try_shutdown();
        self.audio_output_retiring = Some(runtime);
    }

    fn finish_audio_output_open(
        &mut self,
        generation: u64,
        config: AudioOutputConfig,
        result: Result<AudioOpenResult, AudioOutputOpenError>,
        cx: &mut Context<Self>,
    ) {
        if !self.audio_output_is_current(&config, generation) {
            let retirement_failed = matches!(&result, Err(error) if error.is_retirement_failure());
            match result {
                Ok(opened) => self.retire_audio_output_runtime(opened.runtime),
                Err(AudioOutputOpenError::RetirementTimedOut(runtime)) => {
                    self.audio_output_retiring = Some(runtime);
                }
                Err(AudioOutputOpenError::Open(_)) => {}
            }
            self.audio_output_open_task = None;
            if retirement_failed {
                let error = "Timed out waiting for the previous audio output to shut down";
                self.audio_output_error = Some(error.to_string());
                self.set_audio_output_status("Audio output unavailable".to_string());
                self.playback_init_intent = None;
                self.playback_waiting_for_preferences = false;
                self.audio_output_resume = None;
                self.clear_playback_cursor();
                self.set_playback_status(format!("Audio output unavailable: {error}"));
            } else {
                self.begin_audio_output_open(cx);
            }
            cx.notify();
            return;
        }
        self.audio_output_open_task = None;
        match result {
            Ok(opened) => {
                self.audio_output_resolved = Some(opened.resolved.clone());
                self.audio_output_applied_config = Some(config.clone());
                self.audio_output_error = None;
                self.set_audio_output_status(if opened.resolved.used_fallback {
                    "Audio output ready with fallback".to_string()
                } else {
                    "Audio output ready".to_string()
                });
                self.playback = Some(opened.runtime);
                self.sync_runtime_metronome();
                self.retry_runtime_volume();
                if self.audio_output_dirty {
                    let preferences = audio_preferences_from_config(&config);
                    if self
                        .submit_preference_patch(PreferencePatch::SetAudioOutput(preferences), cx)
                    {
                        self.audio_output_dirty = false;
                    } else {
                        self.set_audio_output_status(
                            "Audio output ready · settings not saved".to_string(),
                        );
                    }
                }
                self.resume_audio_output_playback_if_current(cx);
            }
            Err(error) => {
                let message = error.message();
                let retiring = match error {
                    AudioOutputOpenError::RetirementTimedOut(runtime) => Some(runtime),
                    AudioOutputOpenError::Open(_) => None,
                };
                if let Some(runtime) = retiring {
                    self.audio_output_retiring = Some(runtime);
                }
                self.audio_output_resolved = None;
                self.audio_output_applied_config = None;
                self.audio_output_error = Some(message.clone());
                self.set_audio_output_status(if self.audio_output_dirty {
                    "Audio output unavailable · settings not saved".to_string()
                } else {
                    "Audio output unavailable".to_string()
                });
                self.audio_output_resume = None;
                self.playback_init_intent = None;
                self.playback_waiting_for_preferences = false;
                self.clear_playback_cursor();
                self.set_playback_status(format!("Audio output unavailable: {message}"));
            }
        }
        cx.notify();
    }

    pub(super) fn begin_audio_output_open(&mut self, cx: &mut Context<Self>) {
        if self.audio_output_open_task.is_some() {
            return;
        }
        self.audio_output_open_generation = self.audio_output_open_generation.wrapping_add(1);
        let generation = self.audio_output_open_generation;
        let config = self.audio_output_config.clone();
        let config_for_worker = config.clone();
        let anti_click_settings = self.preferences.anti_click_settings();
        let retiring = self.audio_output_retiring.take();
        self.audio_output_open_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if let Some(retiring) = retiring {
                        wait_for_audio_output_runtime_shutdown(retiring)?;
                    }
                    let mut player = AudioPlayer::from_config(&config_for_worker)
                        .map_err(|error| AudioOutputOpenError::Open(error.to_string()))?;
                    player.set_anti_clip_settings(anti_click_settings.0, anti_click_settings.1);
                    let resolved = player.output_details().clone();
                    let runtime = PlaybackRuntime::spawn(player, PlaybackRuntimeConfig::default())
                        .map_err(|error| {
                            AudioOutputOpenError::Open(format!(
                                "Could not start audio runtime: {error}"
                            ))
                        })?;
                    Ok(AudioOpenResult { runtime, resolved })
                })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.finish_audio_output_open(generation, config, result, cx);
            });
        }));
    }
}

fn wait_for_audio_output_runtime_shutdown(
    runtime: PlaybackRuntime,
) -> Result<(), AudioOutputOpenError> {
    let deadline = Instant::now() + AUDIO_OUTPUT_RETIREMENT_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(AudioOutputOpenError::RetirementTimedOut(runtime));
        }
        match runtime.events.recv_timeout(remaining) {
            Ok(_) => {}
            Err(PlaybackRuntimeEventRecvTimeoutError::Disconnected) => {
                drop(runtime);
                return Ok(());
            }
            Err(PlaybackRuntimeEventRecvTimeoutError::Timeout) => {
                return Err(AudioOutputOpenError::RetirementTimedOut(runtime));
            }
        }
    }
}

fn discover_audio_options(
    config: &AudioOutputConfig,
    generation: u64,
    is_cancelled: impl Fn() -> bool,
) -> AudioOptionsResult {
    let hosts = available_hosts();
    if is_cancelled() {
        return AudioOptionsResult {
            generation,
            config: config.clone(),
            hosts,
            devices: Vec::new(),
            sample_rates: Vec::new(),
            error: None,
        };
    }
    let host_id = config.host.clone().or_else(|| {
        hosts
            .iter()
            .find(|host| host.is_default)
            .or_else(|| hosts.first())
            .map(|host| host.id.clone())
    });
    let mut error = None;
    let devices = host_id
        .as_deref()
        .and_then(|host| match available_devices(host) {
            Ok(devices) => Some(devices),
            Err(error_value) => {
                error = Some(error_value.to_string());
                None
            }
        })
        .unwrap_or_default();
    if is_cancelled() {
        return AudioOptionsResult {
            generation,
            config: config.clone(),
            hosts,
            devices,
            sample_rates: Vec::new(),
            error,
        };
    }
    let device_name = config.device.clone().or_else(|| {
        devices
            .iter()
            .find(|device| device.is_default)
            .or_else(|| devices.first())
            .map(|device| device.name.clone())
    });
    let sample_rates = match (host_id.as_deref(), device_name.as_deref()) {
        (Some(host), Some(device)) => match supported_sample_rates(host, device) {
            Ok(rates) => rates,
            Err(error_value) => {
                error = Some(error_value.to_string());
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    AudioOptionsResult {
        generation,
        config: config.clone(),
        hosts,
        devices,
        sample_rates,
        error,
    }
}

fn audio_config_from_preferences(preferences: &AudioOutputPreferences) -> AudioOutputConfig {
    AudioOutputConfig {
        host: preferences.host.clone(),
        device: preferences.device.clone(),
        sample_rate: preferences.sample_rate,
        buffer_size: preferences.buffer_size,
    }
}

fn audio_output_can_retry_persistence(
    dirty: bool,
    applied: Option<&AudioOutputConfig>,
    desired: &AudioOutputConfig,
) -> bool {
    dirty && applied.is_some_and(|applied| applied == desired)
}

fn audio_output_open_is_current(
    current_generation: u64,
    current_config: &AudioOutputConfig,
    result_generation: u64,
    result_config: &AudioOutputConfig,
) -> bool {
    current_generation == result_generation && current_config == result_config
}

fn audio_preferences_from_config(config: &AudioOutputConfig) -> AudioOutputPreferences {
    AudioOutputPreferences {
        host: config.host.clone(),
        device: config.device.clone(),
        sample_rate: config.sample_rate,
        buffer_size: config.buffer_size,
    }
}

fn desired_output_detail(
    config: &AudioOutputConfig,
    resolved: &Option<ResolvedOutput>,
    applied: &Option<AudioOutputConfig>,
) -> String {
    let has_requested_values =
        config.host.is_some() || config.device.is_some() || config.sample_rate.is_some();
    if applied.as_ref().is_some_and(|active| active != config)
        || (applied.is_none() && has_requested_values)
    {
        return "pending · applies on next audition".to_string();
    }
    if let Some(resolved) = resolved {
        if resolved.used_fallback {
            return "fallback active".to_string();
        }
        return format_sample_rate(resolved.sample_rate);
    }
    if config.host.is_none() && config.device.is_none() && config.sample_rate.is_none() {
        "system defaults".to_string()
    } else {
        "pending".to_string()
    }
}

fn format_default_label(label: &str, is_default: bool) -> String {
    if is_default {
        format!("{label} (default)")
    } else {
        label.to_string()
    }
}

fn format_sample_rate(rate: u32) -> String {
    if rate % 1_000 == 0 {
        format!("{} kHz", rate / 1_000)
    } else {
        format!("{:.1} kHz", rate as f32 / 1_000.0)
    }
}

fn audio_dropdown_id(dropdown: AudioOutputDropdown) -> &'static str {
    match dropdown {
        AudioOutputDropdown::Host => "host",
        AudioOutputDropdown::Device => "device",
        AudioOutputDropdown::SampleRate => "sample-rate",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolved_output_detail_marks_fallback_without_losing_rate() {
        let resolved = Some(ResolvedOutput {
            host_id: "coreaudio".to_string(),
            device_name: "Built-in Output".to_string(),
            sample_rate: 48_000,
            buffer_size_frames: Some(256),
            channel_count: 2,
            used_fallback: true,
        });
        assert_eq!(
            desired_output_detail(
                &AudioOutputConfig::default(),
                &resolved,
                &Some(AudioOutputConfig::default()),
            ),
            "fallback active"
        );
        assert_eq!(
            desired_output_detail(
                &AudioOutputConfig::default(),
                &resolved,
                &Some(AudioOutputConfig {
                    host: Some("other".to_string()),
                    ..AudioOutputConfig::default()
                }),
            ),
            "pending · applies on next audition"
        );
        assert_eq!(format_sample_rate(44_100), "44.1 kHz");
        assert_eq!(format_sample_rate(48_000), "48 kHz");
    }

    #[test]
    fn preference_config_round_trip_preserves_cascade_fields() {
        let preferences = AudioOutputPreferences {
            host: Some("coreaudio".to_string()),
            device: Some("Built-in Output".to_string()),
            sample_rate: Some(44_100),
            buffer_size: Some(512),
        };
        let config = audio_config_from_preferences(&preferences);
        assert_eq!(audio_preferences_from_config(&config), preferences);
    }

    #[test]
    fn stale_open_completion_cannot_publish_after_newer_config() {
        let old_config = AudioOutputConfig {
            host: Some("old".to_string()),
            ..AudioOutputConfig::default()
        };
        let new_config = AudioOutputConfig {
            host: Some("new".to_string()),
            ..AudioOutputConfig::default()
        };
        assert!(!audio_output_open_is_current(
            2,
            &new_config,
            1,
            &old_config
        ));
        assert!(audio_output_open_is_current(2, &new_config, 2, &new_config));
    }

    #[test]
    fn retry_save_requires_a_successfully_applied_matching_output() {
        let config = AudioOutputConfig::default();
        assert!(!audio_output_can_retry_persistence(
            false,
            Some(&config),
            &config
        ));
        assert!(!audio_output_can_retry_persistence(true, None, &config));
        assert!(audio_output_can_retry_persistence(
            true,
            Some(&config),
            &config
        ));
        let other = AudioOutputConfig {
            sample_rate: Some(48_000),
            ..config.clone()
        };
        assert!(!audio_output_can_retry_persistence(
            true,
            Some(&config),
            &other
        ));
    }
}
