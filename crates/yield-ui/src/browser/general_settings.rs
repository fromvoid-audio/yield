//! General settings controls backed by the ordered session preferences.

use std::{path::PathBuf, sync::mpsc::TryRecvError, time::Duration};

use gpui::{ClickEvent, Context, Div, FocusHandle, PathPromptOptions, Window, div, prelude::*, px};
use yield_design::Theme;
use yield_session::{
    DEFAULT_RATING_DECAY_WEEKS, DEFAULT_TEXT_SCALE_PERCENT, ENLARGED_TEXT_SCALE_PERCENT,
    MAX_RATING_DECAY_WEEKS, MIN_RATING_DECAY_WEEKS, PreferencePatch,
};

use super::{Browser, source_registry::SourceRegistryGlobal};
use crate::theme::color;
use yield_session::{CacheClearOutcome, GlobalStorageUsage, MaintenanceRequest};

pub(super) struct GeneralSettings {
    pub(super) text_scale_percent: u16,
    pub(super) rating_decay_weeks: u16,
    pub(super) trash_folder: Option<PathBuf>,
    pub(super) previous_focus: Option<FocusHandle>,
    picker_generation: u64,
    picking: bool,
    status: String,
    storage: Option<GlobalStorageUsage>,
    maintenance_status: String,
    maintenance: Option<PendingMaintenance>,
    maintenance_task: Option<gpui::Task<()>>,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        Self {
            text_scale_percent: DEFAULT_TEXT_SCALE_PERCENT,
            rating_decay_weeks: DEFAULT_RATING_DECAY_WEEKS,
            trash_folder: None,
            previous_focus: None,
            picker_generation: 0,
            picking: false,
            status: String::new(),
            storage: None,
            maintenance_status: String::new(),
            maintenance: None,
            maintenance_task: None,
        }
    }
}

enum PendingMaintenance {
    Usage(MaintenanceRequest<GlobalStorageUsage>),
    Clear(MaintenanceRequest<CacheClearOutcome>),
}

impl GeneralSettings {
    pub(super) fn picking(&self) -> bool {
        self.picking
    }

    pub(super) fn maintenance_busy(&self) -> bool {
        self.maintenance.is_some()
    }

    pub(super) fn storage_label(&self) -> String {
        self.storage
            .map(|usage| {
                format!(
                    "{} total · Database {} · Cache {}",
                    format_bytes(usage.total_bytes()),
                    format_bytes(usage.database_bytes),
                    format_bytes(usage.cache_bytes)
                )
            })
            .unwrap_or_else(|| "Not measured".into())
    }

    pub(super) fn status(&self) -> &str {
        &self.status
    }

    pub(super) fn maintenance_status(&self) -> &str {
        &self.maintenance_status
    }

    pub(super) fn invalidate_picker(&mut self) {
        self.picker_generation = self.picker_generation.wrapping_add(1);
        self.picking = false;
    }
}

impl Browser {
    pub(super) fn toggle_text_scale(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_text_scale_from_accessibility(cx);
    }

    pub(super) fn toggle_text_scale_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let percent = if self.general_settings.text_scale_percent == DEFAULT_TEXT_SCALE_PERCENT {
            ENLARGED_TEXT_SCALE_PERCENT
        } else {
            DEFAULT_TEXT_SCALE_PERCENT
        };
        if self.submit_preference_patch(PreferencePatch::SetTextScalePercent(percent), cx) {
            self.apply_preference_projection(cx);
            cx.notify();
        }
    }

    pub(super) fn close_general_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.theme_settings_open = false;
        self.general_settings.invalidate_picker();
        let restore = self
            .general_settings
            .previous_focus
            .take()
            .unwrap_or_else(|| {
                self.ax_focus("settings-open")
                    .unwrap_or_else(|| self.focus_handle.clone())
            });
        window.focus(&restore);
        cx.notify();
    }

    pub(super) fn change_rating_decay(&mut self, delta: i16, cx: &mut Context<Self>) {
        if !self.preferences_editable() {
            self.mark_preferences_edit_blocked(cx);
            return;
        }
        let weeks = (i32::from(self.general_settings.rating_decay_weeks) + i32::from(delta)).clamp(
            i32::from(MIN_RATING_DECAY_WEEKS),
            i32::from(MAX_RATING_DECAY_WEEKS),
        ) as u16;
        if self.submit_preference_patch(PreferencePatch::SetRatingDecayWeeks(weeks), cx) {
            self.general_settings.rating_decay_weeks = weeks;
            cx.notify();
        }
    }

    fn choose_trash_folder(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.choose_trash_folder_from_accessibility(cx);
    }

    pub(super) fn choose_trash_folder_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable()
            || self.general_settings.picking
            || self.general_settings.maintenance.is_some()
        {
            return;
        }
        self.general_settings.picker_generation =
            self.general_settings.picker_generation.wrapping_add(1);
        let generation = self.general_settings.picker_generation;
        self.general_settings.picking = true;
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose a trash folder".into()),
        });
        cx.spawn(async move |this, cx| {
            let result = picker.await;
            let _ = this.update(cx, |view, cx| {
                if generation != view.general_settings.picker_generation {
                    return;
                }
                view.general_settings.picking = false;
                match result {
                    Ok(Ok(Some(paths))) => {
                        if let Some(path) = paths.into_iter().next()
                            && view.preferences_editable()
                            && view.general_settings.maintenance.is_none()
                            && view.submit_preference_patch(
                                PreferencePatch::SetTrashFolder(Some(path.clone())),
                                cx,
                            )
                        {
                            view.general_settings.trash_folder = Some(path);
                            view.general_settings.status.clear();
                        }
                    }
                    Ok(Err(error)) => {
                        view.general_settings.status = format!("Folder selection failed: {error}")
                    }
                    Err(error) => {
                        view.general_settings.status =
                            format!("Folder selection unavailable: {error}")
                    }
                    Ok(Ok(None)) => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn clear_trash_folder(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.clear_trash_folder_from_accessibility(cx);
    }

    pub(super) fn clear_trash_folder_from_accessibility(&mut self, cx: &mut Context<Self>) {
        if !self.preferences_editable() || self.general_settings.maintenance.is_some() {
            return;
        }
        if self.submit_preference_patch(PreferencePatch::SetTrashFolder(None), cx) {
            self.general_settings.picker_generation =
                self.general_settings.picker_generation.wrapping_add(1);
            self.general_settings.picking = false;
            self.general_settings.trash_folder = None;
            self.general_settings.status.clear();
            cx.notify();
        }
    }

    pub(super) fn refresh_global_storage(&mut self, cx: &mut Context<Self>) {
        if self.general_settings.maintenance.is_some() {
            return;
        }
        let request = cx
            .try_global::<SourceRegistryGlobal>()
            .ok_or(yield_session::SubmitError::Closed)
            .and_then(SourceRegistryGlobal::request_storage_usage);
        match request {
            Ok(request) => {
                self.general_settings.storage = None;
                self.general_settings.maintenance = Some(PendingMaintenance::Usage(request));
                self.general_settings.maintenance_status = "Measuring storage…".into();
                self.poll_general_maintenance(cx);
            }
            Err(error) => {
                self.general_settings.maintenance_status =
                    format!("Storage query unavailable: {error}")
            }
        }
        cx.notify();
    }

    pub(super) fn clear_rebuildable_cache(&mut self, cx: &mut Context<Self>) {
        if self.general_settings.maintenance.is_some()
            || !self.preferences.loaded()
            || !self.preferences_editable()
            || self.preferences.has_pending()
        {
            return;
        }
        let request = cx
            .try_global::<SourceRegistryGlobal>()
            .ok_or(yield_session::SubmitError::Closed)
            .and_then(SourceRegistryGlobal::clear_rebuildable_cache);
        match request {
            Ok(request) => {
                self.general_settings.invalidate_picker();
                // Admission can change cache contents before a completion is received.
                // Invalidate the old measurement even if the service later disconnects.
                self.general_settings.storage = None;
                self.general_settings.maintenance = Some(PendingMaintenance::Clear(request));
                self.general_settings.maintenance_status = "Clearing rebuildable caches…".into();
                self.poll_general_maintenance(cx);
            }
            Err(error) => {
                self.general_settings.maintenance_status =
                    format!("Cache maintenance unavailable: {error}")
            }
        }
        cx.notify();
    }

    fn poll_general_maintenance(&mut self, cx: &mut Context<Self>) {
        self.general_settings.maintenance_task = Some(cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(100)).await;
                let pending = this.update(cx, |view, cx| view.drain_general_maintenance(cx));
                if !matches!(pending, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn drain_general_maintenance(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(request) = self.general_settings.maintenance.as_mut() else {
            return false;
        };
        let result = match request {
            PendingMaintenance::Usage(request) => match request.try_result() {
                Ok(Ok(usage)) => {
                    self.general_settings.storage = Some(usage);
                    Some(Ok("Storage usage updated".into()))
                }
                Ok(Err(error)) => Some(Err(error.to_string())),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err("Storage service disconnected".into())),
            },
            PendingMaintenance::Clear(request) => match request.try_result() {
                Ok(Ok(outcome)) => {
                    // Old cache bytes are no longer authoritative after any cleanup.
                    self.general_settings.storage = None;
                    Some(if outcome.incomplete || outcome.namespace_replaced {
                        Err(format!(
                            "Cache cleanup incomplete: {} files removed; {}",
                            outcome.removed_files,
                            if outcome.remaining_entries_known {
                                format!("{} entries remain", outcome.remaining_entries)
                            } else {
                                "remaining entries were not measured".into()
                            }
                        ))
                    } else {
                        Ok(format!(
                            "Caches cleared: {} files, {} removed. Refresh to measure storage.",
                            outcome.removed_files,
                            format_bytes(outcome.removed_bytes)
                        ))
                    })
                }
                Ok(Err(error)) => {
                    self.general_settings.storage = None;
                    Some(Err(error.to_string()))
                }
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(
                    "Cache service disconnected; cleanup outcome unknown".into(),
                )),
            },
        };
        let Some(result) = result else {
            return true;
        };
        self.general_settings.maintenance.take();
        self.general_settings.maintenance_status = match result {
            Ok(message) => message,
            Err(message) => format!("Maintenance failed: {message}"),
        };
        cx.notify();
        false
    }

    pub(super) fn render_general_settings(&self, theme: Theme, cx: &mut Context<Self>) -> Div {
        let editable = self.preferences_editable();
        let maintenance_busy = self.general_settings.maintenance.is_some();
        let trash_editable = editable && !maintenance_busy;
        let choose_enabled = trash_editable && !self.general_settings.picking;
        let clear_enabled = !maintenance_busy
            && self.preferences.loaded()
            && editable
            && !self.preferences.has_pending();
        let decrease_enabled =
            editable && self.general_settings.rating_decay_weeks > MIN_RATING_DECAY_WEEKS;
        let increase_enabled =
            editable && self.general_settings.rating_decay_weeks < MAX_RATING_DECAY_WEEKS;
        div()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .min_w(px(0.0))
            .gap(px(theme.spacing.control_gap))
            .p(px(theme.spacing.panel_padding))
            .bg(color(theme.colors.surface_overlay))
            .border_b(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border))
            .text_size(px(theme.typography.small_size))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child("GENERAL")
                    .child(div().flex_1())
                    .child(
                        control(self, "general-settings-close", "Close", true, &theme).on_click(
                            cx.listener(|view, _, window, cx| {
                                view.close_general_settings(window, cx)
                            }),
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Rating Decay"))
                    .child(
                        control(self, "rating-decay-decrease", "−", decrease_enabled, &theme).when(
                            decrease_enabled,
                            |button| {
                                button.on_click(
                                    cx.listener(|view, _, _, cx| view.change_rating_decay(-1, cx)),
                                )
                            },
                        ),
                    )
                    .child(format!(
                        "{} weeks",
                        self.general_settings.rating_decay_weeks
                    ))
                    .child(
                        control(self, "rating-decay-increase", "+", increase_enabled, &theme).when(
                            increase_enabled,
                            |button| {
                                button.on_click(
                                    cx.listener(|view, _, _, cx| view.change_rating_decay(1, cx)),
                                )
                            },
                        ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Trash Folder"))
                    .child(
                        div().flex_1().min_w(px(0.0)).overflow_hidden().child(
                            self.general_settings
                                .trash_folder
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "No trash folder configured".into()),
                        ),
                    )
                    .child(
                        control(
                            self,
                            "choose-trash-folder",
                            if self.general_settings.picking {
                                "Choosing…"
                            } else {
                                "Choose Folder"
                            },
                            choose_enabled,
                            &theme,
                        )
                        .when(choose_enabled, |button| {
                            button.on_click(cx.listener(Self::choose_trash_folder))
                        }),
                    )
                    .child(
                        control(self, "clear-trash-folder", "Clear", trash_editable, &theme)
                            .when(trash_editable, |button| {
                                button.on_click(cx.listener(Self::clear_trash_folder))
                            }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Global Storage"))
                    .child(div().flex_1().child(self.general_settings.storage_label()))
                    .child(
                        control(
                            self,
                            "refresh-global-storage",
                            "Refresh",
                            !maintenance_busy,
                            &theme,
                        )
                        .when(!maintenance_busy, |button| {
                            button.on_click(
                                cx.listener(|view, _, _, cx| view.refresh_global_storage(cx)),
                            )
                        }),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(theme.spacing.control_gap))
                    .child(div().w(px(120.0)).child("Maintenance"))
                    .child(
                        control(
                            self,
                            "clear-rebuildable-cache",
                            "Clear Rebuildable Caches",
                            clear_enabled,
                            &theme,
                        )
                        .when(clear_enabled, |button| {
                            button.on_click(
                                cx.listener(|view, _, _, cx| view.clear_rebuildable_cache(cx)),
                            )
                        }),
                    ),
            )
            .when(
                !self.general_settings.maintenance_status.is_empty(),
                |panel| {
                    panel.child(
                        div()
                            .relative()
                            .when_some(
                                self.ax_marker("settings-maintenance-status"),
                                |element, marker| element.child(marker),
                            )
                            .w_full()
                            .overflow_hidden()
                            .child(self.general_settings.maintenance_status.clone()),
                    )
                },
            )
            .when(!self.general_settings.status.is_empty(), |panel| {
                panel.child(
                    div()
                        .relative()
                        .when_some(
                            self.ax_marker("settings-general-status"),
                            |element, marker| element.child(marker),
                        )
                        .w_full()
                        .overflow_hidden()
                        .child(self.general_settings.status.clone()),
                )
            })
    }
}

fn control(
    browser: &Browser,
    id: &'static str,
    label: &'static str,
    enabled: bool,
    theme: &Theme,
) -> gpui::Stateful<Div> {
    let ax_id = match id {
        "general-settings-close" => "settings-close",
        "rating-decay-decrease" => "settings-rating-decay-decrease",
        "rating-decay-increase" => "settings-rating-decay-increase",
        "choose-trash-folder" => "settings-trash-choose",
        "clear-trash-folder" => "settings-trash-clear",
        "refresh-global-storage" => "settings-storage-refresh",
        "clear-rebuildable-cache" => "settings-cache-clear",
        _ => id,
    };
    div()
        .id(id)
        .relative()
        .when_some(browser.ax_focus(ax_id), |element, focus| {
            element.track_focus(&focus)
        })
        .when_some(browser.ax_marker(ax_id), |element, marker| {
            element.child(marker)
        })
        .flex_none()
        .flex()
        .items_center()
        .h(px(theme.spacing.control_height))
        .px(px(theme.spacing.control_gap))
        .border(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border_emphasis))
        .focus({
            let focus_color = color(theme.interaction.focus_outline);
            let focus_width = theme.interaction.focus_outline_width;
            move |style| style.border(px(focus_width)).border_color(focus_color)
        })
        .text_color(color(if enabled {
            theme.colors.text_primary
        } else {
            theme.colors.text_disabled
        }))
        .when(enabled, |button| {
            let hover = color(theme.colors.selection_hover_fill);
            let pressed = color(theme.interaction.pressed_fill);
            button
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
        })
        .child(label)
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
