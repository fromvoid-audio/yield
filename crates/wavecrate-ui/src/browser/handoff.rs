//! Browser actions that hand selected files to the operating system.

use std::path::PathBuf;

use gpui::{ClickEvent, Context, MouseDownEvent, Window};
use wavecrate_core::{AudioFile, AudioFileId, BrowserRow, BrowserSelection};

use crate::external_clipboard::{
    FileClipboardError, PreparedFilePaths, copy_file_paths, prepare_file_paths,
};

use super::Browser;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HandoffCommand {
    Copy,
    Reveal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct HandoffContext {
    source_root: Option<PathBuf>,
    focused_id: Option<AudioFileId>,
    visible_ids: Vec<AudioFileId>,
    selected_ids: Vec<AudioFileId>,
}

impl Browser {
    /// Copy the current visible selection as native file clipboard data.
    pub(super) fn copy_selected_files(&mut self, cx: &mut Context<Self>) {
        self.start_file_handoff(HandoffCommand::Copy, cx);
    }

    /// Reveal the focused file in the platform file browser after validation.
    pub(super) fn reveal_focused_file(
        &mut self,
        _: &ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_file_handoff(HandoffCommand::Reveal, cx);
    }

    pub(super) fn invalidate_file_handoff_from_search_mouse(
        &mut self,
        _: &MouseDownEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.invalidate_file_handoff();
    }

    fn start_file_handoff(&mut self, command: HandoffCommand, cx: &mut Context<Self>) {
        self.handoff_generation = self.handoff_generation.wrapping_add(1);
        let generation = self.handoff_generation;
        self.handoff_task.take();
        let context = self.handoff_context();
        let paths = match command {
            HandoffCommand::Copy => file_handoff_paths(
                &self.entries,
                self.view_model.rows(),
                &self.selection,
                self.selected,
            ),
            HandoffCommand::Reveal => focused_file_path(&self.entries, self.selected)
                .into_iter()
                .collect(),
        };
        if paths.is_empty() {
            self.playback_status = match command {
                HandoffCommand::Copy => "No selected files to copy".to_string(),
                HandoffCommand::Reveal => "Select a sample to reveal".to_string(),
            };
            cx.notify();
            return;
        }

        self.playback_status = match command {
            HandoffCommand::Copy => "Preparing clipboard…".to_string(),
            HandoffCommand::Reveal => "Preparing reveal…".to_string(),
        };
        self.handoff_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { prepare_file_paths(&paths) })
                .await;
            let _ = this.update(cx, |view, cx| {
                if !handoff_is_current(
                    generation,
                    view.handoff_generation,
                    &context,
                    &view.handoff_context(),
                ) {
                    return;
                }
                view.handoff_task = None;
                match result {
                    Ok(prepared) => view.publish_file_handoff(command, prepared, cx),
                    Err(error) => view.handoff_failed(command, error),
                }
                cx.notify();
            });
        }));
    }

    fn publish_file_handoff(
        &mut self,
        command: HandoffCommand,
        prepared: PreparedFilePaths,
        cx: &mut Context<Self>,
    ) {
        match command {
            HandoffCommand::Copy => match copy_file_paths(&prepared) {
                Ok(()) => {
                    let count = prepared.as_slice().len();
                    self.playback_status =
                        format!("Copied {count} file{}", if count == 1 { "" } else { "s" });
                }
                Err(error) => self.handoff_failed(command, error),
            },
            HandoffCommand::Reveal => {
                let Some(path) = prepared.as_slice().first() else {
                    self.handoff_failed(command, FileClipboardError::NoExistingFiles);
                    return;
                };
                cx.reveal_path(path);
                self.playback_status = format!("Revealed {}", path.display());
            }
        }
    }

    fn handoff_failed(&mut self, command: HandoffCommand, error: FileClipboardError) {
        self.playback_status = match command {
            HandoffCommand::Copy => format!("Copy failed: {error}"),
            HandoffCommand::Reveal => format!("Reveal failed: {error}"),
        };
    }

    fn handoff_context(&self) -> HandoffContext {
        let visible_ids = self
            .view_model
            .rows()
            .iter()
            .map(|row| row.id.clone())
            .collect::<Vec<_>>();
        HandoffContext {
            source_root: self.root.clone(),
            focused_id: self.selected_id(),
            selected_ids: self.selection.selected_in_order(&visible_ids),
            visible_ids,
        }
    }

    pub(super) fn invalidate_file_handoff(&mut self) {
        self.handoff_generation = self.handoff_generation.wrapping_add(1);
        self.handoff_task.take();
    }
}

fn file_handoff_paths(
    entries: &[AudioFile],
    visible_rows: &[BrowserRow],
    selection: &BrowserSelection,
    focused_index: Option<usize>,
) -> Vec<PathBuf> {
    let visible_ids = visible_rows
        .iter()
        .map(|row| row.id.clone())
        .collect::<Vec<_>>();
    if !visible_ids.iter().any(|id| selection.contains(id)) {
        return focused_index
            .and_then(|index| entries.get(index))
            .map(|entry| entry.path.clone())
            .into_iter()
            .collect();
    }

    visible_rows
        .iter()
        .filter(|row| selection.contains(&row.id))
        .filter_map(|row| entries.get(row.entry_index))
        .map(|entry| entry.path.clone())
        .collect()
}

fn focused_file_path(entries: &[AudioFile], focused_index: Option<usize>) -> Option<PathBuf> {
    focused_index
        .and_then(|index| entries.get(index))
        .map(|entry| entry.path.clone())
}

fn handoff_is_current(
    requested_generation: u64,
    current_generation: u64,
    requested_context: &HandoffContext,
    current_context: &HandoffContext,
) -> bool {
    requested_generation == current_generation && requested_context == current_context
}

pub(super) fn is_browser_copy_shortcut(
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
) -> bool {
    key.eq_ignore_ascii_case("c") && secondary && !shift && !alt && !function
}

pub(super) fn should_route_browser_copy(
    search_focused: bool,
    is_held: bool,
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
) -> bool {
    !search_focused && !is_held && is_browser_copy_shortcut(key, secondary, shift, alt, function)
}

#[cfg(test)]
mod tests {
    use super::{
        file_handoff_paths, handoff_is_current, is_browser_copy_shortcut, should_route_browser_copy,
    };
    use std::path::PathBuf;
    use wavecrate_core::{AudioFile, BrowserRow, BrowserSelection};

    fn entry(path: &str) -> AudioFile {
        AudioFile {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(path.rsplit('/').next().unwrap_or(path)),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            curation: None,
        }
    }

    fn rows(entries: &[AudioFile]) -> Vec<BrowserRow> {
        entries
            .iter()
            .enumerate()
            .map(|(entry_index, entry)| BrowserRow {
                entry_index,
                id: entry.stable_id(),
            })
            .collect()
    }

    #[test]
    fn handoff_preserves_current_visible_row_order() {
        let entries = vec![entry("/samples/a.wav"), entry("/samples/b.wav")];
        let all_rows = rows(&entries);
        let visible = vec![all_rows[1].clone(), all_rows[0].clone()];
        let mut selection = BrowserSelection::new();
        selection.toggle(&all_rows[1].id, Some(&all_rows[1].id));
        selection.toggle(&all_rows[0].id, Some(&all_rows[1].id));

        assert_eq!(
            file_handoff_paths(&entries, &visible, &selection, Some(0)),
            vec![
                PathBuf::from("/samples/b.wav"),
                PathBuf::from("/samples/a.wav")
            ]
        );
    }

    #[test]
    fn handoff_uses_focused_file_when_no_selected_id_is_visible() {
        let entries = vec![entry("/samples/hidden.wav"), entry("/samples/focused.wav")];
        let visible = vec![rows(&entries)[1].clone()];
        let mut selection = BrowserSelection::new();
        selection.toggle(&entries[0].stable_id(), Some(&entries[0].stable_id()));

        assert_eq!(
            file_handoff_paths(&entries, &visible, &selection, Some(1)),
            vec![PathBuf::from("/samples/focused.wav")]
        );
    }

    #[test]
    fn reveal_uses_focus_even_when_batch_membership_exists() {
        let entries = vec![entry("/samples/first.wav"), entry("/samples/focused.wav")];
        let visible = rows(&entries);
        let mut selection = BrowserSelection::new();
        selection.toggle(&visible[0].id, Some(&visible[0].id));

        assert_eq!(
            super::focused_file_path(&entries, Some(1)),
            Some(PathBuf::from("/samples/focused.wav"))
        );
    }

    #[test]
    fn handoff_empty_without_membership_or_focus_has_no_targets() {
        let entries = vec![entry("/samples/one.wav")];
        let visible = rows(&entries);
        assert!(file_handoff_paths(&entries, &visible, &BrowserSelection::new(), None).is_empty());
    }

    #[test]
    fn handoff_keeps_unicode_and_spaces() {
        let entries = vec![entry("/samples/é first take.wav")];
        let visible = rows(&entries);
        assert_eq!(
            file_handoff_paths(&entries, &visible, &BrowserSelection::new(), Some(0)),
            vec![PathBuf::from("/samples/é first take.wav")]
        );
    }

    #[test]
    fn handoff_fence_requires_newest_command_and_context() {
        let context = super::HandoffContext {
            source_root: Some(PathBuf::from("/samples")),
            focused_id: None,
            visible_ids: Vec::new(),
            selected_ids: Vec::new(),
        };
        assert!(handoff_is_current(4, 4, &context, &context));
        assert!(!handoff_is_current(4, 5, &context, &context));
        let changed = super::HandoffContext {
            source_root: Some(PathBuf::from("/other")),
            ..context.clone()
        };
        assert!(!handoff_is_current(4, 4, &context, &changed));
    }

    #[test]
    fn copy_shortcut_is_browser_only_and_preserves_text_modifiers() {
        assert!(is_browser_copy_shortcut("c", true, false, false, false));
        assert!(!is_browser_copy_shortcut("c", false, false, false, false));
        assert!(!is_browser_copy_shortcut("c", true, true, false, false));
        assert!(!is_browser_copy_shortcut("c", true, false, true, false));
        assert!(!is_browser_copy_shortcut("c", true, false, false, true));
        assert!(!is_browser_copy_shortcut("x", true, false, false, false));
        assert!(should_route_browser_copy(
            false, false, "c", true, false, false, false
        ));
        assert!(!should_route_browser_copy(
            true, false, "c", true, false, false, false
        ));
        assert!(!should_route_browser_copy(
            false, true, "c", true, false, false, false
        ));
    }
}
