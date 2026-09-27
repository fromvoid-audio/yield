//! Inline display-name editing for stable collection slots.

use std::path::PathBuf;

use gpui::{Context, Window};
use yield_core::COLLECTION_COUNT;
use yield_library::sample_sources::SourceId;
use yield_session::{MAX_COLLECTION_NAME_BYTES, PreferencePatch};

use super::Browser;

/// The source and slot captured when one inline editor opens.
pub(super) struct CollectionRenameEditor {
    pub(super) slot: usize,
    source_id: SourceId,
    root: PathBuf,
}

impl Browser {
    pub(super) fn collection_name(&self, slot: usize) -> String {
        self.preferences
            .collection_name(slot)
            .unwrap_or_else(|| format!("Collection {}", slot + 1))
    }

    pub(super) fn collection_rename_is_current(&self, slot: usize) -> bool {
        self.collection_rename_editor
            .as_ref()
            .is_some_and(|editor| {
                editor.slot == slot
                    && editor.slot < COLLECTION_COUNT
                    && self.active_source_id().as_ref() == Some(&editor.source_id)
                    && self.root.as_ref() == Some(&editor.root)
            })
    }

    pub(super) fn begin_collection_rename(
        &mut self,
        slot: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(source_id) = self.active_source_id() else {
            return false;
        };
        let Some(root) = self.root.clone() else {
            return false;
        };
        if slot >= COLLECTION_COUNT || !self.preferences.loaded() || !self.preferences.can_edit() {
            return false;
        }
        let name = self.collection_name(slot);
        self.collection_rename_editor = Some(CollectionRenameEditor {
            slot,
            source_id,
            root,
        });
        self.collection_rename_input.update(cx, |input, cx| {
            input.set_content(name, cx);
            input.select_all_content(cx);
        });
        window.focus(&self.collection_rename_input.read(cx).focus_handle());
        cx.notify();
        true
    }

    pub(super) fn cancel_collection_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(editor) = self.collection_rename_editor.take() {
            let focus = self
                .ax_focus(&super::accessibility::collection_filter_id(editor.slot))
                .unwrap_or_else(|| self.focus_handle.clone());
            window.focus(&focus);
            cx.notify();
        }
    }

    /// Return can synthesize a keyboard click on the row after focus is restored. Suppress only
    /// that click; the next real keydown clears the fence for ordinary keyboard activation.
    fn finish_collection_rename_submission(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self
            .collection_rename_editor
            .as_ref()
            .map(|editor| editor.slot)
        else {
            return;
        };
        self.suppress_collection_keyboard_click = Some(slot);
        self.cancel_collection_rename(window, cx);
    }

    pub(super) fn commit_collection_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(slot) = self
            .collection_rename_editor
            .as_ref()
            .map(|editor| editor.slot)
        else {
            return;
        };
        if !self.collection_rename_is_current(slot) {
            self.finish_collection_rename_submission(window, cx);
            return;
        }
        let draft = self
            .collection_rename_input
            .read(cx)
            .content()
            .trim()
            .to_string();
        if draft.is_empty() {
            self.finish_collection_rename_submission(window, cx);
            return;
        }
        if draft.len() > MAX_COLLECTION_NAME_BYTES || draft.chars().any(char::is_control) {
            self.set_playback_status(String::from("Collection name is invalid"));
            cx.notify();
            return;
        }
        if self
            .submit_preference_patch(PreferencePatch::SetCollectionName { slot, name: draft }, cx)
        {
            self.finish_collection_rename_submission(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gpui::TestAppContext;
    use yield_library::sample_sources::SourceId;
    use yield_session::{PreferenceRevision, UiPreferences};

    use super::super::{Browser, SourceSession};

    #[gpui::test]
    fn inline_rename_cancels_empty_input_and_fences_source_switch(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, _| {
            let root = PathBuf::from("/tmp/collection-rename-source");
            let mut session = SourceSession::new(root.clone());
            session.source_id = Some(SourceId::from_string("collection-source"));
            view.source_sessions.push(session);
            view.active_source = Some(0);
            view.root = Some(root);
            assert!(
                view.preferences
                    .accept_snapshot(PreferenceRevision::default(), UiPreferences::default())
            );
        });
        cx.update(|window, app| {
            browser.update(app, |view, cx| {
                assert!(view.begin_collection_rename(2, window, cx));
                assert!(view.collection_rename_is_current(2));
                assert!(
                    view.collection_rename_input
                        .read(cx)
                        .focus_handle()
                        .is_focused(window)
                );
                assert_eq!(
                    view.collection_rename_input.read(cx).content(),
                    "Collection 3"
                );
                view.collection_rename_input
                    .update(cx, |input, cx| input.set_content("   ", cx));
                view.commit_collection_rename(window, cx);
                assert!(view.collection_rename_editor.is_none());
                assert_eq!(view.collection_name(2), "Collection 3");
                assert!(view.begin_collection_rename(2, window, cx));
                view.root = Some(PathBuf::from("/tmp/other-source"));
                assert!(!view.collection_rename_is_current(2));
                view.commit_collection_rename(window, cx);
                assert!(view.collection_rename_editor.is_none());
                assert_eq!(view.collection_name(2), "Collection 3");
            });
        });
    }
}
