//! Native accessibility controls owned by the browser window.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use async_channel::Receiver;
use gpui::{Context, FocusHandle, Task, Window, canvas, prelude::*};
use raw_window_handle::HasWindowHandle;
use yield_platform::accessibility::{AxAction, AxChild, AxEvent, AxFrame, AxRegistry, AxRole};

use super::{Browser, tag_library};

pub(super) const ADD_SOURCE: &str = "browser-add-source";
pub(super) const FOLDER_SCOPE: &str = "browser-folder-scope";
pub(super) const REFRESH_SOURCE: &str = "browser-refresh-source";
pub(super) const SEARCH: &str = "browser-search";

pub(super) fn source_id(root: &Path) -> String {
    format!("browser-source:{:?}", root.as_os_str())
}

pub(super) fn forget_id(root: &Path) -> String {
    format!("browser-forget-source:{:?}", root.as_os_str())
}

pub(super) fn folder_id(root: &Path, folder: &Path) -> String {
    format!(
        "browser-folder:{:?}:{:?}",
        root.as_os_str(),
        folder.as_os_str()
    )
}

pub(super) fn folder_toggle_id(root: &Path, folder: &Path) -> String {
    format!(
        "browser-folder-toggle:{:?}:{:?}",
        root.as_os_str(),
        folder.as_os_str()
    )
}

pub(super) fn sample_id(path: &Path) -> String {
    format!("browser-sample:{:?}", path.as_os_str())
}

pub(super) struct BrowserAccessibility {
    registry: Rc<RefCell<AxRegistry>>,
    last_controls: RefCell<Vec<AxChild>>,
    visible_sources: Vec<PathBuf>,
    visible_folders: Vec<PathBuf>,
    visible_samples: Vec<PathBuf>,
    control_focus: HashMap<String, FocusHandle>,
    add_focus: FocusHandle,
    scope_focus: FocusHandle,
    refresh_focus: FocusHandle,
    _events: Task<()>,
    _activation: gpui::Subscription,
}

impl BrowserAccessibility {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Browser>) -> Option<Self> {
        let handle = match window.window_handle() {
            Ok(handle) => handle,
            Err(error) => {
                eprintln!("browser accessibility window handle: {error}");
                return None;
            }
        };
        let (registry, receiver) = match AxRegistry::new(handle) {
            Ok(result) => result,
            Err(error) => {
                eprintln!("browser accessibility registry: {error:?}");
                return None;
            }
        };
        let activation = cx.observe_window_activation(window, |view, window, cx| {
            view.sync_browser_accessibility(window, cx);
            window.refresh();
            cx.notify();
        });
        Some(Self {
            registry: Rc::new(RefCell::new(registry)),
            last_controls: RefCell::new(Vec::new()),
            visible_sources: Vec::new(),
            visible_folders: Vec::new(),
            visible_samples: Vec::new(),
            control_focus: HashMap::new(),
            add_focus: cx.focus_handle(),
            scope_focus: cx.focus_handle(),
            refresh_focus: cx.focus_handle(),
            _events: Self::receive(receiver, window, cx),
            _activation: activation,
        })
    }

    fn receive(
        receiver: Receiver<AxEvent>,
        window: &Window,
        cx: &mut Context<Browser>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                if this
                    .update_in(cx, |view, window, cx| {
                        let Some(ax) = view.accessibility.as_ref() else {
                            return;
                        };
                        if tag_library::is_open(view) || !ax.registry.borrow().contains(&event.id) {
                            return;
                        }
                        let focus = if event.id == SEARCH {
                            view.search_input.read(cx).focus_handle()
                        } else if let Some(focus) = ax.focus_handle(&event.id) {
                            focus
                        } else {
                            return;
                        };
                        let source_action =
                            view.source_rows()
                                .into_iter()
                                .find_map(|(index, root, _, _, _)| {
                                    ax.visible_sources
                                        .contains(&root)
                                        .then(|| {
                                            if event.id == source_id(&root) {
                                                Some((index, false))
                                            } else if event.id == forget_id(&root) {
                                                Some((index, true))
                                            } else {
                                                None
                                            }
                                        })
                                        .flatten()
                                });
                        let folder_action = view.root.as_deref().and_then(|root| {
                            view.folder_rows().into_iter().find_map(|row| {
                                if !ax.visible_folders.contains(&row.path) {
                                    return None;
                                }
                                if event.id == folder_id(root, &row.path) {
                                    Some((row.path.clone(), false))
                                } else if row.has_children
                                    && row.path != Path::new(".")
                                    && event.id == folder_toggle_id(root, &row.path)
                                {
                                    Some((row.path.clone(), true))
                                } else {
                                    None
                                }
                            })
                        });
                        let sample_action = view.view_model.rows().iter().find_map(|row| {
                            let entry = view.entries.get(row.entry_index)?;
                            (ax.visible_samples.contains(&entry.path)
                                && event.id == sample_id(&entry.path))
                            .then_some(row.entry_index)
                        });
                        if !matches!(
                            event.id.as_str(),
                            ADD_SOURCE | FOLDER_SCOPE | REFRESH_SOURCE | SEARCH
                        ) && source_action.is_none()
                            && folder_action.is_none()
                            && sample_action.is_none()
                        {
                            return;
                        }
                        if sample_action.is_some() && super::tag_filter::scope_pending(view) {
                            return;
                        }
                        cx.activate(true);
                        window.activate_window();
                        window.focus(&focus);
                        if event.id == SEARCH {
                            match event.action {
                                AxAction::SetValue(value) => {
                                    let search_input = view.search_input.clone();
                                    window.defer(cx, move |_, cx| {
                                        search_input
                                            .update(cx, |input, cx| input.set_content(value, cx));
                                    });
                                }
                                AxAction::Focus => {}
                                AxAction::Press => return,
                            }
                        } else if event.action == AxAction::Press {
                            match event.id.as_str() {
                                ADD_SOURCE => view.prompt_for_source(cx),
                                FOLDER_SCOPE => view.toggle_folder_scope(cx),
                                REFRESH_SOURCE => view.refresh_active_source(cx),
                                _ => match source_action {
                                    Some((index, true)) => {
                                        view.forget_source(index, window, cx);
                                        if let Some(add_focus) = view.ax_focus(ADD_SOURCE) {
                                            window.focus(&add_focus);
                                        }
                                    }
                                    Some((index, false)) => {
                                        view.select_source(index, window, cx);
                                        window.focus(&focus);
                                    }
                                    None => match folder_action {
                                        Some((path, true)) => view.toggle_folder_expanded(path, cx),
                                        Some((path, false)) => {
                                            view.select_folder(path, window, cx);
                                            window.focus(&focus);
                                        }
                                        None => match sample_action {
                                            Some(index) => {
                                                view.select_entry_from_accessibility(index, cx)
                                            }
                                            _ => return,
                                        },
                                    },
                                },
                            }
                        }
                        view.sync_browser_accessibility(window, cx);
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

    fn focus_handle(&self, id: &str) -> Option<FocusHandle> {
        match id {
            ADD_SOURCE => Some(self.add_focus.clone()),
            FOLDER_SCOPE => Some(self.scope_focus.clone()),
            REFRESH_SOURCE => Some(self.refresh_focus.clone()),
            _ => self.control_focus.get(id).cloned(),
        }
    }
}

impl Browser {
    pub(super) fn ax_source_ids(&self, root: &Path) -> (Option<String>, Option<String>) {
        if self.accessibility.is_some() {
            (Some(source_id(root)), Some(forget_id(root)))
        } else {
            (None, None)
        }
    }

    pub(super) fn ax_folder_ids(&self, folder: &Path) -> (Option<String>, Option<String>) {
        match (self.accessibility.as_ref(), self.root.as_deref()) {
            (Some(_), Some(root)) => (
                Some(folder_id(root, folder)),
                Some(folder_toggle_id(root, folder)),
            ),
            _ => (None, None),
        }
    }

    pub(super) fn ax_sample_id(&self, path: &Path) -> Option<String> {
        self.accessibility.as_ref().map(|_| sample_id(path))
    }

    pub(super) fn update_ax_source_range(
        &mut self,
        range: std::ops::Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.source_rows();
        let Some(ax) = self.accessibility.as_mut() else {
            return;
        };
        let visible_sources = range
            .filter_map(|position| rows.get(position).map(|row| row.1.clone()))
            .collect::<Vec<_>>();
        let mut visible_ids = HashSet::new();
        for root in &visible_sources {
            for id in [source_id(root), forget_id(root)] {
                visible_ids.insert(id.clone());
                ax.control_focus
                    .entry(id)
                    .or_insert_with(|| cx.focus_handle());
            }
        }
        ax.control_focus.retain(|id, _| {
            (!id.starts_with("browser-source:") && !id.starts_with("browser-forget-source:"))
                || visible_ids.contains(id)
        });
        ax.visible_sources = visible_sources;
        self.sync_browser_accessibility(window, cx);
    }

    pub(super) fn update_ax_folder_range(
        &mut self,
        range: std::ops::Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let rows = self.folder_rows().to_vec();
        let Some(root) = self.root.as_deref() else {
            return;
        };
        let Some(ax) = self.accessibility.as_mut() else {
            return;
        };
        let visible_folders = range
            .filter_map(|position| rows.get(position).map(|row| row.path.clone()))
            .collect::<Vec<_>>();
        let mut visible_ids = HashSet::new();
        for folder in &visible_folders {
            let has_toggle = rows
                .iter()
                .any(|row| row.path == *folder && row.has_children && row.path != Path::new("."));
            let id = folder_id(root, folder);
            visible_ids.insert(id.clone());
            ax.control_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle());
            if has_toggle {
                let id = folder_toggle_id(root, folder);
                visible_ids.insert(id.clone());
                ax.control_focus
                    .entry(id)
                    .or_insert_with(|| cx.focus_handle());
            }
        }
        ax.control_focus.retain(|id, _| {
            (!id.starts_with("browser-folder:") && !id.starts_with("browser-folder-toggle:"))
                || visible_ids.contains(id)
        });
        ax.visible_folders = visible_folders;
        self.sync_browser_accessibility(window, cx);
    }

    pub(super) fn update_ax_sample_range(
        &mut self,
        range: std::ops::Range<usize>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let visible_samples = range
            .filter_map(|position| {
                let row = self.view_model.rows().get(position)?;
                self.entries
                    .get(row.entry_index)
                    .map(|entry| entry.path.clone())
            })
            .collect::<Vec<_>>();
        let Some(ax) = self.accessibility.as_mut() else {
            return;
        };
        let mut visible_ids = HashSet::new();
        for path in &visible_samples {
            let id = sample_id(path);
            visible_ids.insert(id.clone());
            ax.control_focus
                .entry(id)
                .or_insert_with(|| cx.focus_handle());
        }
        ax.control_focus
            .retain(|id, _| !id.starts_with("browser-sample:") || visible_ids.contains(id));
        ax.visible_samples = visible_samples;
        self.sync_browser_accessibility(window, cx);
    }

    pub(super) fn sync_browser_accessibility(&self, window: &Window, cx: &mut Context<Self>) {
        let Some(ax) = self.accessibility.as_ref() else {
            return;
        };
        let controls = if tag_library::is_open(self) {
            Vec::new()
        } else {
            let mut controls = vec![
                AxChild {
                    id: SEARCH.into(),
                    role: AxRole::TextField,
                    label: "Search samples".into(),
                    value: Some(self.search_input.read(cx).content().to_owned()),
                    enabled: true,
                },
                AxChild {
                    id: ADD_SOURCE.into(),
                    role: AxRole::Button,
                    label: "Add source folder".into(),
                    value: None,
                    enabled: true,
                },
            ];
            let source_rows = self.source_rows();
            for root in &ax.visible_sources {
                let Some((_, _, label, selected, error)) =
                    source_rows.iter().find(|row| &row.1 == root)
                else {
                    continue;
                };
                let value = match (*selected, error.as_deref()) {
                    (true, Some(error)) => format!("Selected, error: {error}"),
                    (false, Some(error)) => format!("Error: {error}"),
                    (true, None) => "Selected".into(),
                    (false, None) => "Not selected".into(),
                };
                controls.push(AxChild {
                    id: source_id(root),
                    role: AxRole::Row,
                    label: label.clone(),
                    value: Some(value),
                    enabled: true,
                });
                controls.push(AxChild {
                    id: forget_id(root),
                    role: AxRole::Button,
                    label: format!("Forget source {label}"),
                    value: None,
                    enabled: true,
                });
            }
            if let Some(root) = self.root.as_deref() {
                let folder_rows = self.folder_rows();
                for path in &ax.visible_folders {
                    let Some(row) = folder_rows.iter().find(|row| &row.path == path) else {
                        continue;
                    };
                    let label = if row.path == Path::new(".") {
                        "All folders".to_owned()
                    } else {
                        row.path
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_else(|| row.path.display().to_string())
                    };
                    controls.push(AxChild {
                        id: folder_id(root, path),
                        role: AxRole::Row,
                        label: label.clone(),
                        value: Some(format!(
                            "{}, {} audio {}",
                            if self.selected_folder == row.path {
                                "Selected"
                            } else {
                                "Not selected"
                            },
                            row.audio_file_count,
                            if row.audio_file_count == 1 {
                                "file"
                            } else {
                                "files"
                            }
                        )),
                        enabled: true,
                    });
                    if row.has_children && row.path != Path::new(".") {
                        controls.push(AxChild {
                            id: folder_toggle_id(root, path),
                            role: AxRole::Button,
                            label: format!(
                                "{} folder {label}",
                                if row.expanded { "Collapse" } else { "Expand" }
                            ),
                            value: Some(
                                if row.expanded {
                                    "Expanded"
                                } else {
                                    "Collapsed"
                                }
                                .into(),
                            ),
                            enabled: true,
                        });
                    }
                }
            }
            controls.push(AxChild {
                id: FOLDER_SCOPE.into(),
                role: AxRole::Button,
                label: "Folder scope".into(),
                value: Some(self.folder_scope.label().into()),
                enabled: true,
            });
            controls.push(AxChild {
                id: REFRESH_SOURCE.into(),
                role: AxRole::Button,
                label: "Refresh source".into(),
                value: None,
                enabled: true,
            });
            let browser_focus_id = self.browser_focus_id();
            for path in &ax.visible_samples {
                let Some(row) = self.view_model.rows().iter().find(|row| {
                    self.entries
                        .get(row.entry_index)
                        .is_some_and(|entry| &entry.path == path)
                }) else {
                    continue;
                };
                let Some(entry) = self.entries.get(row.entry_index) else {
                    continue;
                };
                let (label, _) = self.sample_name_display(&entry.relative_path);
                let selected =
                    self.sample_row_is_selected_with_focus(&row.id, browser_focus_id.as_ref());
                let value = format!(
                    "{}, {}",
                    if selected { "Selected" } else { "Not selected" },
                    yield_core::format_duration(entry.duration_seconds)
                );
                controls.push(AxChild {
                    id: sample_id(path),
                    role: AxRole::Row,
                    label,
                    value: Some(value),
                    enabled: !super::tag_filter::scope_pending(self),
                });
            }
            controls
        };
        if *ax.last_controls.borrow() != controls {
            if let Err(error) = ax.registry.borrow_mut().reconcile(controls.clone()) {
                eprintln!("browser accessibility reconcile: {error:?}");
                return;
            }
            *ax.last_controls.borrow_mut() = controls;
        }
        let registry = ax.registry.borrow();
        for (id, focus) in [
            (ADD_SOURCE, &ax.add_focus),
            (FOLDER_SCOPE, &ax.scope_focus),
            (REFRESH_SOURCE, &ax.refresh_focus),
        ] {
            if registry.contains(id) {
                let _ =
                    registry.set_focused(id, focus.is_focused(window) && window.is_window_active());
            }
        }
        if registry.contains(SEARCH) {
            let search_focus = self.search_input.read(cx).focus_handle();
            let _ = registry.set_focused(
                SEARCH,
                search_focus.is_focused(window) && window.is_window_active(),
            );
        }
        for (id, focus) in &ax.control_focus {
            if registry.contains(id) {
                let _ =
                    registry.set_focused(id, focus.is_focused(window) && window.is_window_active());
            }
        }
    }

    pub(super) fn ax_focus(&self, id: &str) -> Option<FocusHandle> {
        self.accessibility.as_ref()?.focus_handle(id)
    }

    pub(super) fn ax_marker(&self, id: impl Into<String>) -> Option<gpui::AnyElement> {
        let registry = self.accessibility.as_ref()?.registry.clone();
        let id = id.into();
        Some(
            canvas(
                move |bounds, _, _| {
                    let _ = registry.borrow().set_frame(
                        &id,
                        AxFrame {
                            x: f64::from(f32::from(bounds.origin.x)),
                            y: f64::from(f32::from(bounds.origin.y)),
                            width: f64::from(f32::from(bounds.size.width)),
                            height: f64::from(f32::from(bounds.size.height)),
                        },
                    );
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
            .into_any_element(),
        )
    }
}

impl Drop for BrowserAccessibility {
    fn drop(&mut self) {
        let _ = self.registry.borrow_mut().reconcile(Vec::new());
    }
}
