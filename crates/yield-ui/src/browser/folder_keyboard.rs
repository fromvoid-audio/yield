//! Keyboard focus and explicit marks for the visible folder tree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gpui::{Context, KeyDownEvent, Window};

use super::{Browser, motion};

#[derive(Clone, Debug)]
pub(super) struct FolderKeyboardState {
    root: PathBuf,
    focused: PathBuf,
    anchor: PathBuf,
    marked: BTreeSet<PathBuf>,
    explicit: bool,
}

impl FolderKeyboardState {
    pub(super) fn new(root: PathBuf, selected: PathBuf) -> Self {
        Self {
            root,
            focused: selected.clone(),
            anchor: selected.clone(),
            marked: BTreeSet::from([selected]),
            explicit: false,
        }
    }

    pub(super) fn root(&self) -> &Path {
        &self.root
    }

    pub(super) fn focused(&self) -> &Path {
        &self.focused
    }

    pub(super) fn is_marked(&self, path: &Path) -> bool {
        self.marked.contains(path)
    }

    pub(super) fn explicit(&self) -> bool {
        self.explicit
    }

    pub(super) fn pointer_select(&mut self, path: PathBuf) {
        self.focused = path.clone();
        self.anchor = path.clone();
        self.marked = BTreeSet::from([path]);
        self.explicit = false;
    }

    pub(super) fn set_focus(&mut self, path: PathBuf) {
        self.focused = path;
    }

    /// Reconcile a tree replacement without carrying focus into a different source.
    pub(super) fn reconcile(
        &mut self,
        root: &Path,
        selected: &Path,
        visible: &[PathBuf],
        existing: &BTreeSet<PathBuf>,
    ) {
        if self.root != root {
            *self = Self::new(root.to_path_buf(), selected.to_path_buf());
            return;
        }
        if !visible.contains(&self.focused) {
            self.focused = visible
                .iter()
                .find(|path| path.as_path() == selected)
                .or_else(|| visible.first())
                .cloned()
                .unwrap_or_else(|| PathBuf::from("."));
        }
        if !visible.contains(&self.anchor) {
            self.anchor = self.focused.clone();
        }
        self.marked.retain(|path| existing.contains(path));
        if !self.explicit && self.marked.is_empty() {
            self.marked.insert(self.focused.clone());
        }
    }

    /// Return the next focus and whether the folder projection should follow it.
    pub(super) fn navigate(
        &mut self,
        delta: isize,
        extend: bool,
        preserve_selection: bool,
        visible: &[PathBuf],
    ) -> Option<(PathBuf, bool)> {
        let index = visible.iter().position(|path| path == &self.focused)?;
        let next = index.saturating_add_signed(delta).min(visible.len() - 1);
        if next == index {
            return None;
        }
        let target = visible[next].clone();
        if extend {
            let anchor = visible
                .iter()
                .position(|path| path == &self.anchor)
                .unwrap_or(index);
            let (start, end) = if anchor <= next {
                (anchor, next)
            } else {
                (next, anchor)
            };
            self.marked = visible[start..=end].iter().cloned().collect();
            self.explicit = true;
        } else if !preserve_selection && !self.explicit {
            self.anchor = target.clone();
            self.marked = BTreeSet::from([target.clone()]);
        }
        self.focused = target.clone();
        Some((target, extend || (!preserve_selection && !self.explicit)))
    }

    /// Toggle the focused folder's explicit mark and advance keyboard focus one row.
    pub(super) fn toggle_and_advance(&mut self, visible: &[PathBuf]) -> Option<(PathBuf, bool)> {
        let index = visible.iter().position(|path| path == &self.focused)?;
        let was_marked = self.explicit && self.marked.contains(&self.focused);
        let toggled = self.focused.clone();
        if was_marked {
            self.marked.remove(&toggled);
        } else {
            self.marked.insert(toggled);
        }
        self.explicit = true;
        self.focused = visible[(index + 1).min(visible.len() - 1)].clone();
        Some((self.focused.clone(), !was_marked))
    }
}

impl Browser {
    pub(super) fn folder_is_selected(&self, path: &Path) -> bool {
        let explicit = self
            .folder_keyboard
            .as_ref()
            .filter(|state| self.root.as_deref() == Some(state.root()) && state.explicit());
        explicit.map_or(self.selected_folder == path, |state| state.is_marked(path))
    }

    pub(super) fn ensure_folder_keyboard(&mut self) -> Option<()> {
        let root = self.root.as_ref()?;
        let visible = self
            .folder_rows()
            .iter()
            .map(|row| row.path.clone())
            .collect::<Vec<_>>();
        let existing = self
            .folder_tree
            .as_ref()?
            .paths()
            .cloned()
            .collect::<BTreeSet<_>>();
        if visible.is_empty() {
            self.folder_keyboard = None;
            return None;
        }
        if self
            .folder_keyboard
            .as_ref()
            .is_none_or(|state| state.root() != root)
        {
            self.folder_keyboard = Some(FolderKeyboardState::new(
                root.clone(),
                self.selected_folder.clone(),
            ));
        }
        self.folder_keyboard
            .as_mut()
            .expect("initialized folder keyboard")
            .reconcile(root, &self.selected_folder, &visible, &existing);
        Some(())
    }

    fn focused_folder_path(&self, window: &Window) -> Option<PathBuf> {
        self.root.as_ref()?;
        let ax_focused = self
            .folder_rows()
            .iter()
            .find(|row| {
                self.ax_folder_ids(&row.path)
                    .0
                    .as_deref()
                    .and_then(|id| self.ax_focus(id))
                    .is_some_and(|focus| focus.is_focused(window))
            })
            .map(|row| row.path.clone());
        self.folder_keyboard
            .as_ref()
            .and_then(|state| {
                ((self.focus_handle.is_focused(window) || ax_focused.is_some())
                    && self.focus_matches_folder(state.focused()))
                .then(|| state.focused().to_path_buf())
            })
            .or(ax_focused)
    }

    pub(super) fn handle_folder_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(focused) = self.focused_folder_path(window) else {
            return false;
        };
        if self.ensure_folder_keyboard().is_none() {
            return false;
        }
        if let Some(state) = self.folder_keyboard.as_mut() {
            state.set_focus(focused);
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let (delta, directional) = match key {
            "up" | "arrowup" => (-1, true),
            "down" | "arrowdown" => (1, true),
            _ => (0, false),
        };
        if directional {
            if modifiers.alt || modifiers.function {
                return true;
            }
            let visible = self
                .folder_rows()
                .iter()
                .map(|row| row.path.clone())
                .collect::<Vec<_>>();
            let moved = self.folder_keyboard.as_mut().and_then(|state| {
                state.navigate(
                    delta,
                    modifiers.shift,
                    modifiers.platform || modifiers.control,
                    &visible,
                )
            });
            if let Some((path, follow_selection)) = moved {
                if follow_selection {
                    self.selected_folder = path.clone();
                    self.update_active_folder_projection(cx);
                }
                self.focus_folder_path(path, window, cx);
            }
            return true;
        }
        if matches!(key, "left" | "arrowleft" | "right" | "arrowright") {
            if !modifiers.platform
                && !modifiers.control
                && !modifiers.alt
                && !modifiers.shift
                && !modifiers.function
                && !event.is_held
            {
                let path = self
                    .folder_keyboard
                    .as_ref()
                    .expect("folder focus")
                    .focused();
                let row = self
                    .folder_rows()
                    .iter()
                    .find(|row| row.path == path)
                    .cloned();
                if let Some(row) = row
                    && row.path != Path::new(".")
                    && row.has_children
                    && row.expanded == matches!(key, "left" | "arrowleft")
                {
                    self.toggle_folder_expanded(row.path.clone(), cx);
                    self.focus_folder_path(row.path, window, cx);
                }
            }
            return true;
        }
        if key.eq_ignore_ascii_case("x") {
            if !modifiers.platform
                && !modifiers.control
                && !modifiers.alt
                && !modifiers.shift
                && !modifiers.function
                && !event.is_held
            {
                let visible = self
                    .folder_rows()
                    .iter()
                    .map(|row| row.path.clone())
                    .collect::<Vec<_>>();
                if let Some((path, _marked)) = self
                    .folder_keyboard
                    .as_mut()
                    .and_then(|state| state.toggle_and_advance(&visible))
                {
                    self.focus_folder_path(path, window, cx);
                    cx.notify();
                }
            }
            return true;
        }
        false
    }

    pub(super) fn mark_folder_keyboard_focus(&mut self, path: PathBuf) {
        if self.ensure_folder_keyboard().is_some() {
            if let Some(state) = self.folder_keyboard.as_mut() {
                state.set_focus(path.clone());
            }
            self.start_focus(
                motion::FocusTarget::Folder(path),
                motion::FocusTrigger::Keyboard,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, TestAppContext};
    use yield_core::FolderTree;

    fn paths() -> Vec<PathBuf> {
        [".", "a", "a/child", "b"]
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }

    #[test]
    fn explicit_marks_keep_navigation_focus_independent() {
        let visible = paths();
        let mut state = FolderKeyboardState::new(PathBuf::from("/source"), visible[0].clone());
        assert_eq!(
            state.navigate(1, false, false, &visible),
            Some((visible[1].clone(), true))
        );
        assert!(state.is_marked(&visible[1]));
        assert_eq!(
            state.navigate(1, true, false, &visible),
            Some((visible[2].clone(), true))
        );
        assert!(state.is_marked(&visible[1]));
        assert!(state.is_marked(&visible[2]));
        assert_eq!(
            state.navigate(1, false, false, &visible),
            Some((visible[3].clone(), false))
        );
        assert!(!state.is_marked(&visible[3]));
        assert_eq!(
            state.toggle_and_advance(&visible),
            Some((visible[3].clone(), true))
        );
        assert!(state.is_marked(&visible[3]));
        assert_eq!(
            state.toggle_and_advance(&visible),
            Some((visible[3].clone(), false))
        );
        assert!(!state.is_marked(&visible[3]));
    }

    #[test]
    fn replacement_root_cannot_reuse_old_keyboard_focus() {
        let visible = paths();
        let mut state = FolderKeyboardState::new(PathBuf::from("/source-a"), visible[1].clone());
        state.navigate(1, false, true, &visible);
        state.reconcile(
            Path::new("/source-b"),
            &visible[0],
            &visible,
            &visible.iter().cloned().collect(),
        );
        assert_eq!(state.root(), Path::new("/source-b"));
        assert_eq!(state.focused(), Path::new("."));
    }

    #[test]
    fn collapsed_child_keeps_mark_but_moves_focus_to_visible_row() {
        let visible = paths();
        let mut state = FolderKeyboardState::new(PathBuf::from("/source"), visible[1].clone());
        state.navigate(1, true, false, &visible);
        state.reconcile(
            Path::new("/source"),
            &visible[1],
            &[visible[0].clone(), visible[1].clone(), visible[3].clone()],
            &visible.iter().cloned().collect(),
        );
        assert_eq!(state.focused(), Path::new("a"));
        assert!(state.is_marked(Path::new("a/child")));
    }

    #[gpui::test]
    fn folder_keys_route_selection_disclosure_and_marks(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            view.root = Some(PathBuf::from("/tmp/yield-folder-key-test"));
            view.folder_tree = Some(FolderTree::from_scan(
                &[
                    PathBuf::from("a"),
                    PathBuf::from("a/child"),
                    PathBuf::from("b"),
                ],
                &[],
            ));
            view.show_empty_folders = true;
            view.expanded_folders.insert(PathBuf::from("a"));
            view.rebuild_folder_rows_cache();
        });
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                window.focus(&view.focus_handle);
                for key in ["g", "t"] {
                    view.handle_key_down(
                        &KeyDownEvent {
                            keystroke: Keystroke::parse(key).unwrap(),
                            is_held: false,
                        },
                        window,
                        cx,
                    );
                }
            });
        });
        cx.run_until_parked();
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                for key in ["down", "left", "right", "shift-down", "x"] {
                    view.handle_key_down(
                        &KeyDownEvent {
                            keystroke: Keystroke::parse(key).unwrap(),
                            is_held: false,
                        },
                        window,
                        cx,
                    );
                    if key == "left" {
                        assert!(!view.expanded_folders.contains(Path::new("a")));
                    } else if key == "right" {
                        assert!(view.expanded_folders.contains(Path::new("a")));
                    }
                }
                assert_eq!(view.selected_folder, Path::new("a/child"));
                let state = view.folder_keyboard.as_ref().unwrap();
                assert_eq!(state.focused(), Path::new("b"));
                assert!(state.is_marked(Path::new("a")));
                assert!(!state.is_marked(Path::new("a/child")));
                assert_eq!(view.selected, None);
            });
        });
    }
}
