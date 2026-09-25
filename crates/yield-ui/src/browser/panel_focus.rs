//! Keyboard focus routes for the four visible panels and the loaded sample.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use gpui::{Context, KeyDownEvent, ScrollStrategy, Window};
use yield_core::AudioFileId;

use super::Browser;

const CHORD_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Panel {
    Waveform,
    Browser,
    Folder,
    Sources,
}

fn panel_for_key(key: &str) -> Option<Panel> {
    match key.to_ascii_lowercase().as_str() {
        "w" => Some(Panel::Waveform),
        "b" => Some(Panel::Browser),
        "t" => Some(Panel::Folder),
        "s" => Some(Panel::Sources),
        _ => None,
    }
}

/// Keep browser row focus separate from the sample loaded in the waveform.
fn browser_target<'a>(
    visible: impl Iterator<Item = &'a AudioFileId> + Clone,
    focused: Option<&AudioFileId>,
    anchor: Option<&AudioFileId>,
    selected: &HashSet<AudioFileId>,
    loaded: Option<&AudioFileId>,
) -> Option<AudioFileId> {
    focused
        .filter(|id| visible.clone().any(|row| row == *id))
        .cloned()
        .or_else(|| {
            anchor
                .filter(|id| visible.clone().any(|row| row == *id))
                .cloned()
        })
        .or_else(|| visible.clone().find(|id| selected.contains(*id)).cloned())
        .or_else(|| {
            loaded
                .filter(|id| visible.clone().any(|row| row == *id))
                .cloned()
        })
        .or_else(|| visible.into_iter().next().cloned())
}

impl Browser {
    pub(super) fn handle_panel_focus_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if self.theme_settings_open
            || self.audio_settings_open
            || super::tag_library::is_open(self)
            || modifiers.platform
            || modifiers.control
            || modifiers.alt
            || modifiers.shift
            || modifiers.function
            || event.is_held
        {
            self.focus_chord_started = None;
            return false;
        }
        if let Some((started, generation, root)) = self.focus_chord_started.take()
            && started.elapsed() <= CHORD_TIMEOUT
            && generation == self.scan_generation
            && root == self.root
            && let Some(panel) = panel_for_key(key)
        {
            self.focus_panel(panel, window, cx);
            return true;
        }
        if key.eq_ignore_ascii_case("g") {
            self.focus_chord_started =
                Some((Instant::now(), self.scan_generation, self.root.clone()));
            return true;
        }
        if key.eq_ignore_ascii_case("f") && self.browser_surface_focused(window) {
            self.focus_loaded_sample_in_browser(window, cx);
            return true;
        }
        false
    }

    fn browser_surface_focused(&self, window: &Window) -> bool {
        if self.focus_handle.is_focused(window) {
            return true;
        }
        self.view_model.rows().iter().any(|row| {
            self.entries
                .get(row.entry_index)
                .and_then(|entry| self.ax_sample_id(&entry.path))
                .and_then(|id| self.ax_focus(&id))
                .is_some_and(|focus| focus.is_focused(window))
        })
    }

    fn focus_panel(&mut self, panel: Panel, window: &mut Window, cx: &mut Context<Self>) {
        match panel {
            Panel::Waveform => {
                if self.waveform.is_some() {
                    window.focus(&self.waveform_focus_handle);
                    self.sync_panel_focus_ax(window, cx);
                    cx.notify();
                } else {
                    self.playback_status = "Load a sample to focus its waveform".into();
                    cx.notify();
                }
            }
            Panel::Browser => {
                let rows = self.view_model.rows();
                let target = browser_target(
                    rows.iter().map(|row| &row.id),
                    self.browser_focus_id().as_ref(),
                    self.selection.anchor(),
                    self.selection.selected(),
                    self.selected_id().as_ref(),
                );
                if let Some(id) = target {
                    self.focus_sample_id(id, window, cx);
                } else {
                    self.playback_status = "Add a source with samples to focus the browser".into();
                    cx.notify();
                }
            }
            Panel::Folder => {
                let folder = self
                    .folder_rows()
                    .iter()
                    .find(|row| row.path == self.selected_folder)
                    .or_else(|| self.folder_rows().first())
                    .map(|row| row.path.clone());
                if let Some(path) = folder {
                    self.focus_folder_path(path, window, cx);
                } else {
                    self.playback_status = "No source folders available to focus".into();
                    cx.notify();
                }
            }
            Panel::Sources => {
                let rows = self.source_rows();
                let source = rows
                    .iter()
                    .find(|row| Some(row.0) == self.active_source)
                    .or_else(|| rows.first())
                    .map(|row| row.1.clone());
                if let Some(root) = source {
                    self.focus_source_root(root, window, cx);
                } else {
                    self.playback_status = "Add a source to focus the sources list".into();
                    cx.notify();
                }
            }
        }
    }

    fn focus_loaded_sample_in_browser(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.waveform.is_none() {
            self.playback_status = "Load a sample to focus it".into();
            cx.notify();
            return;
        }
        let Some(id) = self.selected_id() else {
            self.playback_status = "Load a sample to focus it".into();
            cx.notify();
            return;
        };
        if self.view_model.visible_position_for(&id).is_none() {
            self.playback_status = "Loaded sample is not visible in the browser".into();
            cx.notify();
            return;
        }
        self.focus_sample_id(id, window, cx);
    }

    fn focus_sample_id(&mut self, id: AudioFileId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.view_model.visible_position_for(&id) else {
            return;
        };
        let Some(path) = self
            .entries
            .iter()
            .find(|entry| entry.stable_id() == id)
            .map(|entry| entry.path.clone())
        else {
            return;
        };
        let root = self.root.clone();
        let scan_generation = self.scan_generation;
        let origin_focus = window.focused(cx);
        let previous_focus = self.browser_focus_override.clone();
        let previous_detached = self.browser_focus_detached;
        self.set_browser_focus_override(id.clone());
        self.browser_focus_detached = false;
        self.list_scroll_handle
            .scroll_to_item(position, ScrollStrategy::Center);
        cx.notify();
        cx.on_next_frame(window, move |view, window, cx| {
            if window.focused(cx) != origin_focus {
                view.browser_focus_override = previous_focus;
                view.browser_focus_detached = previous_detached;
                cx.notify();
                return;
            }
            if view.root != root
                || view.scan_generation != scan_generation
                || view.view_model.visible_position_for(&id).is_none()
            {
                return;
            }
            if !view.focus_ax_row(view.ax_sample_id(&path), window, cx) {
                view.browser_focus_override = previous_focus;
                view.browser_focus_detached = previous_detached;
                cx.notify();
            }
        });
    }

    fn focus_folder_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let Some(position) = self.folder_rows().iter().position(|row| row.path == path) else {
            return;
        };
        let root = self.root.clone();
        let scan_generation = self.scan_generation;
        let origin_focus = window.focused(cx);
        self.folder_scroll_handle
            .scroll_to_item(position, ScrollStrategy::Center);
        cx.notify();
        cx.on_next_frame(window, move |view, window, cx| {
            if window.focused(cx) != origin_focus
                || view.root != root
                || view.scan_generation != scan_generation
                || !view.folder_rows().iter().any(|row| row.path == path)
            {
                return;
            }
            view.focus_ax_row(view.ax_folder_ids(&path).0, window, cx);
        });
    }

    fn focus_source_root(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.source_rows();
        let Some(position) = rows.iter().position(|row| row.1 == root) else {
            return;
        };
        let scan_generation = self.scan_generation;
        let origin_focus = window.focused(cx);
        self.source_scroll_handle
            .scroll_to_item(position, ScrollStrategy::Center);
        cx.notify();
        cx.on_next_frame(window, move |view, window, cx| {
            if window.focused(cx) != origin_focus
                || view.scan_generation != scan_generation
                || !view.source_rows().iter().any(|row| row.1 == root)
            {
                return;
            }
            view.focus_ax_row(view.ax_source_ids(&root).0, window, cx);
        });
    }

    fn focus_ax_row(
        &mut self,
        id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if let Some(focus) = id.as_deref().and_then(|id| self.ax_focus(id)) {
            window.focus(&focus);
            self.sync_panel_focus_ax(window, cx);
            cx.notify();
            true
        } else {
            self.playback_status = "Panel focus is unavailable".into();
            cx.notify();
            false
        }
    }

    #[cfg(target_os = "macos")]
    fn sync_panel_focus_ax(&self, window: &Window, cx: &mut Context<Self>) {
        self.sync_browser_accessibility(window, cx);
    }

    #[cfg(not(target_os = "macos"))]
    fn sync_panel_focus_ax(&self, _: &Window, _: &mut Context<Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, TestAppContext};
    use yield_core::{AudioFile, BrowserViewModel};

    fn fixture(root: &PathBuf, name: &str) -> AudioFile {
        AudioFile {
            path: root.join(name),
            relative_path: PathBuf::from(name),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
    }

    #[test]
    fn panel_keys_are_case_insensitive_and_reject_unrelated_keys() {
        assert_eq!(panel_for_key("W"), Some(Panel::Waveform));
        assert_eq!(panel_for_key("b"), Some(Panel::Browser));
        assert_eq!(panel_for_key("T"), Some(Panel::Folder));
        assert_eq!(panel_for_key("s"), Some(Panel::Sources));
        assert_eq!(panel_for_key("escape"), None);
    }

    #[test]
    fn browser_target_prefers_visible_focus_then_anchor_then_selection() {
        let root = PathBuf::from("/tmp/yield-focus-target");
        let ids = [
            fixture(&root, "a.wav").stable_id(),
            fixture(&root, "b.wav").stable_id(),
        ];
        let hidden = fixture(&root, "hidden.wav").stable_id();
        let selected = HashSet::from([ids[1].clone()]);
        assert_eq!(
            browser_target(ids.iter(), Some(&ids[0]), Some(&ids[1]), &selected, None),
            Some(ids[0].clone())
        );
        assert_eq!(
            browser_target(ids.iter(), Some(&hidden), Some(&ids[0]), &selected, None),
            Some(ids[0].clone())
        );
        assert_eq!(
            browser_target(ids.iter(), Some(&hidden), Some(&hidden), &selected, None),
            Some(ids[1].clone())
        );
        assert_eq!(
            browser_target(ids.iter(), None, None, &HashSet::new(), Some(&hidden)),
            Some(ids[0].clone())
        );
    }

    #[gpui::test]
    fn browser_chord_changes_row_focus_without_loading_another_sample(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let (loaded, focused) = browser.update(cx, |view, _| {
            let root = PathBuf::from("/tmp/yield-focus-chord");
            view.root = Some(root.clone());
            view.entries = [fixture(&root, "a.wav"), fixture(&root, "b.wav")].into();
            view.view_model = BrowserViewModel::new(&view.entries);
            view.selected = Some(0);
            (view.entries[0].stable_id(), view.entries[1].stable_id())
        });
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                for key in ["g", "b"] {
                    let event = KeyDownEvent {
                        keystroke: Keystroke::parse(key).expect("focus chord key"),
                        is_held: false,
                    };
                    view.handle_key_down(&event, window, cx);
                }
                assert_eq!(view.browser_focus_id(), Some(loaded.clone()));
                assert_eq!(view.selected_id(), Some(loaded.clone()));
                view.detach_browser_focus();
                view.selection.toggle(&focused, None);
                for key in ["g", "b"] {
                    let event = KeyDownEvent {
                        keystroke: Keystroke::parse(key).expect("focus chord key"),
                        is_held: false,
                    };
                    view.handle_key_down(&event, window, cx);
                }
                assert_eq!(view.browser_focus_id(), Some(focused));
                assert_eq!(view.selected_id(), Some(loaded));
            });
        });
    }

    #[gpui::test]
    fn input_and_source_change_do_not_complete_a_pending_chord(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                let input = view.search_input.read(cx).focus_handle();
                window.focus(&input);
                view.handle_key_down(
                    &KeyDownEvent {
                        keystroke: Keystroke::parse("g").expect("G"),
                        is_held: false,
                    },
                    window,
                    cx,
                );
                assert!(view.focus_chord_started.is_none());
                window.focus(&view.focus_handle);
                view.handle_key_down(
                    &KeyDownEvent {
                        keystroke: Keystroke::parse("g").expect("G"),
                        is_held: false,
                    },
                    window,
                    cx,
                );
                assert!(view.focus_chord_started.is_some());
                view.scan_generation += 1;
                view.handle_key_down(
                    &KeyDownEvent {
                        keystroke: Keystroke::parse("b").expect("B"),
                        is_held: false,
                    },
                    window,
                    cx,
                );
                assert!(view.focus_chord_started.is_none());
                assert!(view.browser_focus_override.is_none());
            });
        });
    }
}
