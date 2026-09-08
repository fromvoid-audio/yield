#![deny(missing_docs)]
#![deny(warnings)]

//! The sample-row context menu and its revision-fenced actions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, ClipboardItem, Context, Edges, KeyDownEvent, MouseButton, MouseDownEvent, Pixels,
    Point, Window, anchored, deferred, div, prelude::*, px,
};
use wavecrate_core::{
    AudioFileCuration, AudioFileId, CurationAction, FolderScope, MAX_RATING, SortSpec,
};
use wavecrate_design::Theme;
use wavecrate_library::sample_sources::{SourceId, reconciliation::RootIdentity};

use crate::theme::{color, transparent};

use super::Browser;

const STALE_CONTEXT_MESSAGE: &str =
    "Context menu target is stale; refresh the catalog before editing";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextMenuAction {
    Reveal,
    CopyPath,
    Unlock,
    RemoveFromCollection { slot: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContextSelectionEffect {
    FocusClicked,
    PreserveSelection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuKeyEffect {
    Close,
    Previous,
    Next,
    Activate,
    Consume,
}

#[derive(Clone, Debug)]
struct ContextMenuTarget {
    source_id: SourceId,
    lifecycle_generation: u64,
    root: PathBuf,
    root_identity: RootIdentity,
    directory_generation: u64,
    revision: u64,
    entry_indices: Arc<HashMap<PathBuf, usize>>,
    scan_generation: u64,
    catalog_generation: u64,
    query: String,
    folder: Option<PathBuf>,
    folder_scope: FolderScope,
    collection_filter: Option<usize>,
    sort: SortSpec,
    entry_index: usize,
    visible_position: usize,
    id: AudioFileId,
    relative_path: PathBuf,
    absolute_path: PathBuf,
    selected_paths: Vec<PathBuf>,
    in_explicit_selection: bool,
    anchor: Point<Pixels>,
}

#[derive(Clone, Debug)]
pub(super) struct ContextMenuState {
    target: ContextMenuTarget,
    actions: Vec<ContextMenuAction>,
    selected_action: usize,
}

impl ContextMenuTarget {
    fn is_current(&self, browser: &Browser) -> bool {
        let Some(activation) = browser.catalog_activation.as_ref() else {
            return false;
        };
        let Some(context) = browser.catalog_metadata.context() else {
            return false;
        };
        projection_fence_matches(
            &self.source_id,
            self.lifecycle_generation,
            &self.root,
            &self.entry_indices,
            &activation.source_id,
            activation.lifecycle_generation,
            &activation.root,
            &context.entry_indices,
        ) && browser.root.as_deref() == Some(self.root.as_path())
            && activation.audit_finished
            && activation.audit_error.is_none()
            && browser.scan_generation == self.scan_generation
            && browser.catalog_next_generation == self.catalog_generation
            && context.source_id == self.source_id
            && context.lifecycle_generation == self.lifecycle_generation
            && context.root == self.root
            && context.root_identity == self.root_identity
            && context.directory_generation == self.directory_generation
            && context.revision == self.revision
            && Arc::ptr_eq(&context.entry_indices, &self.entry_indices)
            && browser.view_model.query() == self.query
            && browser.view_model.folder().map(Path::to_path_buf) == self.folder
            && browser.view_model.folder_scope() == self.folder_scope
            && browser.view_model.collection_filter() == self.collection_filter
            && browser.view_model.sort() == self.sort
            && browser
                .view_model
                .rows()
                .get(self.visible_position)
                .is_some_and(|row| row.entry_index == self.entry_index && row.id == self.id)
            && browser.entries.get(self.entry_index).is_some_and(|entry| {
                entry.stable_id() == self.id
                    && entry.relative_path == self.relative_path
                    && entry.path == self.absolute_path
            })
            && self.entry_indices.get(&self.relative_path).copied() == Some(self.entry_index)
    }
}

impl Browser {
    /// Open a sample row menu without changing the loaded waveform or transport selection.
    pub(super) fn open_context_menu(
        &mut self,
        entry_index: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Right {
            return;
        }
        let Some(entry) = self.entries.get(entry_index).cloned() else {
            return;
        };
        let Some(activation) = self.catalog_activation.as_ref() else {
            self.scan_status =
                String::from("Context menu requires a successful audited catalog projection");
            cx.notify();
            return;
        };
        if !activation.audit_finished || activation.audit_error.is_some() {
            self.scan_status =
                String::from("Context menu requires a successful audited catalog projection");
            cx.notify();
            return;
        }
        let Some(context) = self.catalog_metadata.context() else {
            self.scan_status = String::from(
                "Catalog metadata is still loading; try again after the catalog is ready",
            );
            cx.notify();
            return;
        };
        let id = entry.stable_id();
        let Some(visible_position) = self
            .view_model
            .rows()
            .iter()
            .position(|row| row.entry_index == entry_index && row.id == id)
        else {
            return;
        };
        let has_explicit_selection = !self.selection.is_empty();
        let in_explicit_selection = has_explicit_selection && self.selection.contains(&id);
        match context_selection_effect(has_explicit_selection, in_explicit_selection) {
            ContextSelectionEffect::FocusClicked => {
                self.selection.focus_pointer(&id);
                self.browser_focus_override = Some(id.clone());
            }
            ContextSelectionEffect::PreserveSelection => {
                if in_explicit_selection {
                    self.selection.move_anchor_to(&id);
                    self.browser_focus_override = Some(id.clone());
                }
            }
        }
        let selected_paths = self
            .view_model
            .rows()
            .iter()
            .filter(|row| self.selection.contains(&row.id))
            .filter_map(|row| self.entries.get(row.entry_index))
            .map(|entry| entry.relative_path.clone())
            .collect::<Vec<_>>();
        let actions = context_menu_actions(entry.curation, self.view_model.collection_filter());
        self.context_menu = Some(ContextMenuState {
            target: ContextMenuTarget {
                source_id: activation.source_id.clone(),
                lifecycle_generation: activation.lifecycle_generation,
                root: activation.root.clone(),
                root_identity: context.root_identity,
                directory_generation: context.directory_generation,
                revision: context.revision,
                entry_indices: context.entry_indices,
                scan_generation: self.scan_generation,
                catalog_generation: self.catalog_next_generation,
                query: self.view_model.query().to_string(),
                folder: self.view_model.folder().map(Path::to_path_buf),
                folder_scope: self.view_model.folder_scope(),
                collection_filter: self.view_model.collection_filter(),
                sort: self.view_model.sort(),
                entry_index,
                visible_position,
                id,
                relative_path: entry.relative_path,
                absolute_path: entry.path,
                selected_paths,
                in_explicit_selection,
                anchor: event.position,
            },
            actions,
            selected_action: 0,
        });
        window.focus(&self.focus_handle);
        cx.stop_propagation();
        cx.notify();
    }

    /// Dismiss the menu while retaining the passive row focus until normal browser navigation.
    pub(super) fn close_context_menu(&mut self, cx: &mut Context<Self>) {
        if dismiss_menu_state(&mut self.context_menu) {
            cx.notify();
        }
    }

    /// Route keyboard input to the open menu before any browser shortcut handling.
    pub(super) fn handle_context_menu_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.context_menu.as_ref() else {
            return false;
        };
        if !menu.target.is_current(self) {
            self.invalidate_context_menu(cx);
            return true;
        }
        let key = event.keystroke.key.as_str();
        match menu_key_effect(key, event.is_held) {
            MenuKeyEffect::Close => self.close_context_menu(cx),
            MenuKeyEffect::Previous => {
                if let Some(menu) = self.context_menu.as_mut() {
                    menu.selected_action =
                        previous_action_index(menu.selected_action, menu.actions.len());
                    cx.notify();
                }
            }
            MenuKeyEffect::Next => {
                if let Some(menu) = self.context_menu.as_mut() {
                    menu.selected_action =
                        next_action_index(menu.selected_action, menu.actions.len());
                    cx.notify();
                }
            }
            MenuKeyEffect::Activate => self.activate_context_menu_action(window, cx),
            MenuKeyEffect::Consume => {}
        }
        true
    }

    fn activate_context_menu_action(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(menu) = self.context_menu.as_ref() else {
            return;
        };
        if !menu.target.is_current(self) {
            self.invalidate_context_menu(cx);
            return;
        }
        let Some(action) = menu.actions.get(menu.selected_action).copied() else {
            self.close_context_menu(cx);
            return;
        };
        let target = menu.target.clone();
        self.close_context_menu(cx);
        match action {
            ContextMenuAction::Reveal => self.reveal_context_file(target.absolute_path, cx),
            ContextMenuAction::CopyPath => {
                cx.write_to_clipboard(ClipboardItem::new_string(format_copy_path(
                    &target.absolute_path,
                )));
                self.playback_status = String::from("Copied path");
                cx.notify();
            }
            ContextMenuAction::Unlock => {
                let paths = unlock_target_paths(&target);
                self.submit_curation_action(CurationAction::UnlockKeepRating, Some(paths), cx);
            }
            ContextMenuAction::RemoveFromCollection { slot } => {
                self.submit_curation_action(
                    CurationAction::RemoveCollection { slot },
                    Some(vec![target.relative_path]),
                    cx,
                );
            }
        }
    }

    fn invalidate_context_menu(&mut self, cx: &mut Context<Self>) {
        if self.context_menu.take().is_some() {
            self.browser_focus_override = None;
            self.playback_status = String::from(STALE_CONTEXT_MESSAGE);
            cx.notify();
        }
    }
}

/// Close a menu whose captured projection no longer matches the browser.
pub(super) fn invalidate_if_stale(browser: &mut Browser) {
    if browser
        .context_menu
        .as_ref()
        .is_some_and(|menu| !menu.target.is_current(browser))
    {
        browser.context_menu = None;
        browser.browser_focus_override = None;
        browser.playback_status = String::from(STALE_CONTEXT_MESSAGE);
    }
}

fn projection_fence_matches(
    captured_source_id: &SourceId,
    captured_lifecycle_generation: u64,
    captured_root: &Path,
    captured_entry_indices: &Arc<HashMap<PathBuf, usize>>,
    current_source_id: &SourceId,
    current_lifecycle_generation: u64,
    current_root: &Path,
    current_entry_indices: &Arc<HashMap<PathBuf, usize>>,
) -> bool {
    captured_source_id == current_source_id
        && captured_lifecycle_generation == current_lifecycle_generation
        && captured_root == current_root
        && Arc::ptr_eq(captured_entry_indices, current_entry_indices)
}

fn context_selection_effect(
    has_explicit_selection: bool,
    clicked_is_selected: bool,
) -> ContextSelectionEffect {
    if !has_explicit_selection || clicked_is_selected {
        ContextSelectionEffect::FocusClicked
    } else {
        ContextSelectionEffect::PreserveSelection
    }
}

fn context_menu_actions(
    curation: Option<AudioFileCuration>,
    collection_filter: Option<usize>,
) -> Vec<ContextMenuAction> {
    let mut actions = vec![ContextMenuAction::Reveal, ContextMenuAction::CopyPath];
    if curation.is_some_and(|curation| {
        curation.rating().value() == MAX_RATING && curation.rating().locked()
    }) {
        actions.push(ContextMenuAction::Unlock);
    }
    if let Some(slot) = collection_filter {
        actions.push(ContextMenuAction::RemoveFromCollection { slot });
    }
    actions
}

fn unlock_target_paths(target: &ContextMenuTarget) -> Vec<PathBuf> {
    unlock_paths(
        target.in_explicit_selection,
        &target.selected_paths,
        &target.relative_path,
    )
}

fn unlock_paths(
    in_explicit_selection: bool,
    selected_paths: &[PathBuf],
    clicked_path: &Path,
) -> Vec<PathBuf> {
    if in_explicit_selection {
        selected_paths.to_vec()
    } else {
        vec![clicked_path.to_path_buf()]
    }
}

fn dismiss_menu_state<T>(menu: &mut Option<T>) -> bool {
    menu.take().is_some()
}

fn menu_key_effect(key: &str, held: bool) -> MenuKeyEffect {
    if key.eq_ignore_ascii_case("escape") || key.eq_ignore_ascii_case("w") {
        MenuKeyEffect::Close
    } else if (key == "up" || key == "arrowup") && !held {
        MenuKeyEffect::Previous
    } else if (key == "down" || key == "arrowdown") && !held {
        MenuKeyEffect::Next
    } else if key == "enter" && !held {
        MenuKeyEffect::Activate
    } else {
        MenuKeyEffect::Consume
    }
}

fn previous_action_index(current: usize, action_count: usize) -> usize {
    current
        .checked_sub(1)
        .unwrap_or(action_count.saturating_sub(1))
}

fn next_action_index(current: usize, action_count: usize) -> usize {
    if action_count == 0 {
        0
    } else {
        (current + 1) % action_count
    }
}

/// Render the full-window dismiss layer and compact sample menu.
pub(super) fn render_overlay(
    browser: &Browser,
    theme: &Theme,
    cx: &mut Context<Browser>,
) -> AnyElement {
    let Some(menu) = browser.context_menu.as_ref() else {
        return div().into_any_element();
    };
    let target_label = menu
        .target
        .relative_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| menu.target.relative_path.display().to_string());
    let hover_fill = color(theme.interaction.hover_fill);
    let pressed_fill = color(theme.interaction.pressed_fill);
    let selected_fill = color(theme.colors.selection_hover_fill);
    let action_rows = menu
        .actions
        .iter()
        .enumerate()
        .map(|(index, action)| {
            let selected = index == menu.selected_action;
            let label = context_menu_action_label(*action).to_string();
            div()
                .id(("sample-context-action", index))
                .flex()
                .items_center()
                .w_full()
                .h(px(28.0))
                .px(px(8.0))
                .cursor_pointer()
                .rounded(px(3.0))
                .bg(if selected {
                    selected_fill
                } else {
                    transparent()
                })
                .hover(move |style| style.bg(hover_fill))
                .active(move |style| style.bg(pressed_fill))
                .text_size(px(theme.typography.body_size))
                .text_color(color(theme.colors.text_primary))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |view, _: &MouseDownEvent, window, cx| {
                        if let Some(menu) = view.context_menu.as_mut() {
                            menu.selected_action = index;
                        }
                        view.activate_context_menu_action(window, cx);
                    }),
                )
                .child(label)
        })
        .collect::<Vec<_>>();
    let panel = div()
        .id("sample-context-menu")
        .flex()
        .flex_col()
        .w(px(260.0))
        .p(px(6.0))
        .gap(px(2.0))
        .bg(color(theme.colors.surface_overlay))
        .border(px(theme.spacing.border_width))
        .border_color(color(theme.colors.border_emphasis))
        .rounded(px(4.0))
        .occlude()
        .font(gpui::font(theme.typography.font_family.clone()))
        .child(
            div()
                .w_full()
                .px(px(8.0))
                .pb(px(5.0))
                .text_size(px(theme.typography.small_size))
                .text_color(color(theme.colors.text_muted))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .child(format!("SAMPLE ROW · {target_label}")),
        )
        .children(action_rows);
    let dismiss = div()
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right_0()
        .occlude()
        .on_any_mouse_down(cx.listener(|view, _: &MouseDownEvent, _, cx| {
            view.close_context_menu(cx);
            cx.stop_propagation();
        }));
    div()
        .absolute()
        .size_full()
        .child(dismiss)
        .child(deferred(
            anchored()
                .position(menu.target.anchor)
                .snap_to_window_with_margin(Edges::all(px(8.0)))
                .child(panel),
        ))
        .into_any_element()
}

fn context_menu_action_label(action: ContextMenuAction) -> &'static str {
    match action {
        ContextMenuAction::Reveal => file_manager_reveal_label(),
        ContextMenuAction::CopyPath => "Copy Path",
        ContextMenuAction::Unlock => "Unlock",
        ContextMenuAction::RemoveFromCollection { .. } => "Remove from collection",
    }
}

fn file_manager_reveal_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "Reveal in Explorer"
    } else if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else {
        "Reveal in File Manager"
    }
}

fn format_copy_path(path: &Path) -> String {
    let mut rendered = path.to_string_lossy().replace('\\', "/");
    if rendered.contains(' ') {
        rendered = format!("\"{rendered}\"");
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::{
        ContextMenuAction, ContextSelectionEffect, MenuKeyEffect, context_menu_actions,
        context_selection_effect, dismiss_menu_state, format_copy_path, menu_key_effect,
        next_action_index, previous_action_index, projection_fence_matches, unlock_paths,
    };
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use wavecrate_core::{AudioFileCuration, MAX_RATING};
    use wavecrate_library::sample_sources::SourceId;

    #[test]
    fn copy_path_normalizes_separators_and_quotes_spaces() {
        assert_eq!(
            format_copy_path(Path::new(r"C:\Samples\kick one.wav")),
            "\"C:/Samples/kick one.wav\""
        );
        assert_eq!(
            format_copy_path(Path::new("/tmp/kick.wav")),
            "/tmp/kick.wav"
        );
    }

    #[test]
    fn context_target_focus_rules_preserve_outside_multi_selection() {
        assert_eq!(
            context_selection_effect(false, false),
            ContextSelectionEffect::FocusClicked
        );
        assert_eq!(
            context_selection_effect(true, true),
            ContextSelectionEffect::FocusClicked
        );
        assert_eq!(
            context_selection_effect(true, false),
            ContextSelectionEffect::PreserveSelection
        );

        let selected = vec![PathBuf::from("a.wav"), PathBuf::from("b.wav")];
        assert_eq!(unlock_paths(true, &selected, Path::new("b.wav")), selected);
        assert_eq!(
            unlock_paths(false, &selected, Path::new("outside.wav")),
            vec![PathBuf::from("outside.wav")]
        );
    }

    #[test]
    fn menu_actions_conditionally_offer_unlock_and_active_removal() {
        let locked_keep = AudioFileCuration::from_parts(MAX_RATING, true, 0, None, None);
        let actions = context_menu_actions(locked_keep, Some(3));
        assert_eq!(
            actions,
            vec![
                ContextMenuAction::Reveal,
                ContextMenuAction::CopyPath,
                ContextMenuAction::Unlock,
                ContextMenuAction::RemoveFromCollection { slot: 3 },
            ]
        );

        let unlocked_keep = AudioFileCuration::from_parts(MAX_RATING, false, 0, None, None);
        assert!(!context_menu_actions(unlocked_keep, None).contains(&ContextMenuAction::Unlock));
    }

    #[test]
    fn keyboard_effects_dismiss_and_navigate_without_browser_shortcuts() {
        assert_eq!(menu_key_effect("escape", false), MenuKeyEffect::Close);
        assert_eq!(menu_key_effect("w", false), MenuKeyEffect::Close);
        assert_eq!(menu_key_effect("down", false), MenuKeyEffect::Next);
        assert_eq!(menu_key_effect("up", false), MenuKeyEffect::Previous);
        assert_eq!(menu_key_effect("down", true), MenuKeyEffect::Consume);
        assert_eq!(menu_key_effect("1", false), MenuKeyEffect::Consume);
        assert_eq!(menu_key_effect("enter", false), MenuKeyEffect::Activate);
        assert_eq!(next_action_index(2, 3), 0);
        assert_eq!(previous_action_index(0, 3), 2);
    }

    #[test]
    fn stale_projection_fence_rejects_source_or_index_map_changes() {
        let source = SourceId::from_string("source");
        let other_source = SourceId::from_string("other");
        let root = PathBuf::from("/samples");
        let indices = Arc::new(HashMap::from([(PathBuf::from("a.wav"), 0)]));
        let replacement_indices = Arc::new(HashMap::from([(PathBuf::from("a.wav"), 0)]));
        assert!(projection_fence_matches(
            &source, 4, &root, &indices, &source, 4, &root, &indices,
        ));
        assert!(!projection_fence_matches(
            &source,
            4,
            &root,
            &indices,
            &other_source,
            4,
            &root,
            &indices,
        ));
        assert!(!projection_fence_matches(
            &source,
            4,
            &root,
            &indices,
            &source,
            4,
            &root,
            &replacement_indices,
        ));
    }

    #[test]
    fn dismissing_menu_state_leaves_focus_owned_by_browser() {
        let mut menu = None;
        assert!(!dismiss_menu_state(&mut menu));
        menu = Some(());
        assert!(dismiss_menu_state(&mut menu));
        assert!(menu.is_none());
    }
}
