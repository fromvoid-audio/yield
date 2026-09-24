#![deny(missing_docs)]
#![deny(warnings)]

//! Profile-owned tag-definition mutation state.

use std::sync::mpsc::TryRecvError;

use gpui::{AnyView, App, Context, Render, Window, div, prelude::*, px};
use yield_design::Theme;
use yield_library::sample_sources::{
    ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularySetReceipt,
};
use yield_session::{VocabularyRequest, VocabularySubmitError};

use crate::theme::color;

use super::Browser;
use super::source_registry::SourceRegistryGlobal;

/// Browser-owned state for one profile dictionary mutation and its bounded undo receipt.
pub(super) struct TagDictionaryState {
    pending: Option<DictionaryMutation>,
    last_removed: Option<LastRemovedDefinition>,
}

impl Default for TagDictionaryState {
    fn default() -> Self {
        Self {
            pending: None,
            last_removed: None,
        }
    }
}

enum DictionaryMutation {
    Remove {
        label: String,
        request: VocabularyRequest<ProfileTagVocabularyDeleteReceipt>,
    },
    Restore {
        label: String,
        category: ProfileTagCategory,
        request: VocabularyRequest<ProfileTagVocabularySetReceipt>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LastRemovedDefinition {
    revision: u64,
    label: String,
    category: ProfileTagCategory,
}

enum DictionaryCompletion {
    Removed,
    Missing,
    Restored,
}

struct RemoveDefinitionTooltip;

impl Render for RemoveDefinitionTooltip {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::theme::active(cx);
        div()
            .px(px(theme.spacing.control_gap))
            .py(px(theme.spacing.control_gap))
            .bg(color(theme.colors.surface_raised))
            .border(px(theme.spacing.border_width))
            .border_color(color(theme.colors.border_emphasis))
            .text_size(px(theme.typography.small_size))
            .text_color(color(theme.colors.text_primary))
            .child("Remove definition")
    }
}

/// Render the dictionary-only tooltip for a definition removal control.
pub(super) fn remove_definition_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| RemoveDefinitionTooltip).into()
}

impl TagDictionaryState {
    pub(super) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn has_undo(&self) -> bool {
        self.last_removed.is_some()
    }

    pub(super) fn submit_remove(
        &mut self,
        global: &SourceRegistryGlobal,
        revision: u64,
        label: String,
    ) -> Result<(), VocabularySubmitError> {
        let request = global.delete_profile_tag_vocabulary(revision, label.clone())?;
        self.pending = Some(DictionaryMutation::Remove { label, request });
        Ok(())
    }

    pub(super) fn submit_restore(
        &mut self,
        global: &SourceRegistryGlobal,
        revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<(), VocabularySubmitError> {
        let request = global.set_profile_tag_category(revision, label.clone(), category)?;
        self.pending = Some(DictionaryMutation::Restore {
            label,
            category,
            request,
        });
        Ok(())
    }

    pub(super) fn undo_details(&self) -> Option<(u64, String, ProfileTagCategory)> {
        self.last_removed
            .as_ref()
            .map(|removed| (removed.revision, removed.label.clone(), removed.category))
    }

    fn take_pending(&mut self) -> Option<DictionaryMutation> {
        self.pending.take()
    }

    fn retain(&mut self, mutation: DictionaryMutation) {
        self.pending = Some(mutation);
    }

    fn complete_remove(
        &mut self,
        receipt: ProfileTagVocabularyDeleteReceipt,
    ) -> DictionaryCompletion {
        let Some(entry) = receipt.removed else {
            return DictionaryCompletion::Missing;
        };
        self.last_removed = Some(LastRemovedDefinition {
            revision: receipt.revision,
            label: entry.label,
            category: entry.category,
        });
        DictionaryCompletion::Removed
    }

    fn complete_restore(&mut self) -> DictionaryCompletion {
        self.last_removed = None;
        DictionaryCompletion::Restored
    }
}

/// Render the explicit undo control for the most recent successful removal.
pub(super) fn render_undo(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    if !browser.tag_dictionary.has_undo() {
        return div().into_any_element();
    }
    let pending = browser.tag_dictionary.has_pending();
    let hover = color(theme.colors.selection_hover_fill);
    let pressed = color(theme.interaction.pressed_fill);
    let text_color = color(if pending {
        theme.colors.text_disabled
    } else {
        theme.colors.text_muted
    });
    div()
        .id("tag-library-undo-removal")
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .h(px(theme.spacing.control_height))
        .px(px(theme.spacing.control_gap))
        .text_size(px(theme.typography.small_size))
        .text_color(text_color)
        .when(!pending, |element| {
            element
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .active(move |style| style.bg(pressed))
        })
        .on_click(cx.listener(move |view, _, _, cx| {
            if !pending {
                view.undo_tag_library_definition(cx);
            }
        }))
        .child("Undo removal")
        .into_any_element()
}

/// Poll the one profile dictionary mutation, including after the panel closes.
pub(super) fn poll(browser: &mut Browser, cx: &mut Context<Browser>) -> bool {
    let Some(mutation) = browser.tag_dictionary.take_pending() else {
        return false;
    };
    match mutation {
        DictionaryMutation::Remove { label, mut request } => match request.try_recv() {
            Err(TryRecvError::Empty) => {
                browser
                    .tag_dictionary
                    .retain(DictionaryMutation::Remove { label, request });
                false
            }
            Err(TryRecvError::Disconnected) => {
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    String::from("Tag definition removal disconnected"),
                    cx,
                );
                true
            }
            Ok(Ok(receipt)) => {
                let completion = browser.tag_dictionary.complete_remove(receipt);
                let message = match completion {
                    DictionaryCompletion::Removed => {
                        "Tag definition removed · Undo removal available"
                    }
                    DictionaryCompletion::Missing => "Tag definition was already absent",
                    DictionaryCompletion::Restored => unreachable!(),
                };
                super::tag_library::dictionary_mutation_finished(browser, message, cx);
                true
            }
            Ok(Err(error)) => {
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    format!("Could not remove tag definition: {error}"),
                    cx,
                );
                true
            }
        },
        DictionaryMutation::Restore {
            label,
            category,
            mut request,
        } => match request.try_recv() {
            Err(TryRecvError::Empty) => {
                browser.tag_dictionary.retain(DictionaryMutation::Restore {
                    label,
                    category,
                    request,
                });
                false
            }
            Err(TryRecvError::Disconnected) => {
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    String::from("Tag definition restoration disconnected"),
                    cx,
                );
                true
            }
            Ok(Ok(_receipt)) => {
                let completion = browser.tag_dictionary.complete_restore();
                debug_assert!(matches!(completion, DictionaryCompletion::Restored));
                super::tag_library::dictionary_mutation_finished(
                    browser,
                    "Tag definition restored",
                    cx,
                );
                true
            }
            Ok(Err(error)) => {
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    format!("Could not restore tag definition: {error}"),
                    cx,
                );
                true
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{DictionaryCompletion, LastRemovedDefinition, TagDictionaryState};
    use yield_library::sample_sources::{
        ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularyEntry,
    };

    #[test]
    fn successful_removal_keeps_only_the_latest_bounded_beforeimage() {
        let mut state = TagDictionaryState::default();
        assert!(matches!(
            state.complete_remove(ProfileTagVocabularyDeleteReceipt {
                revision: 4,
                removed: Some(ProfileTagVocabularyEntry {
                    label: String::from("warm"),
                    category: ProfileTagCategory::Character,
                }),
            }),
            DictionaryCompletion::Removed
        ));
        assert_eq!(
            state.undo_details(),
            Some((4, String::from("warm"), ProfileTagCategory::Character,))
        );

        assert!(matches!(
            state.complete_remove(ProfileTagVocabularyDeleteReceipt {
                revision: 5,
                removed: Some(ProfileTagVocabularyEntry {
                    label: String::from("bright"),
                    category: ProfileTagCategory::SoundType,
                }),
            }),
            DictionaryCompletion::Removed
        ));
        assert_eq!(
            state.undo_details(),
            Some((5, String::from("bright"), ProfileTagCategory::SoundType,))
        );
        let expected = LastRemovedDefinition {
            revision: 5,
            label: String::from("bright"),
            category: ProfileTagCategory::SoundType,
        };
        assert_eq!(state.last_removed, Some(expected));
    }

    #[test]
    fn missing_removal_has_no_new_undo_and_restore_consumes_beforeimage() {
        let mut state = TagDictionaryState::default();
        assert!(matches!(
            state.complete_remove(ProfileTagVocabularyDeleteReceipt {
                revision: 4,
                removed: None,
            }),
            DictionaryCompletion::Missing
        ));
        assert_eq!(state.undo_details(), None);

        state.last_removed = Some(LastRemovedDefinition {
            revision: 7,
            label: String::from("restore-me"),
            category: ProfileTagCategory::Prefix,
        });
        assert!(matches!(
            state.complete_remove(ProfileTagVocabularyDeleteReceipt {
                revision: 7,
                removed: None,
            }),
            DictionaryCompletion::Missing
        ));
        assert_eq!(
            state.undo_details(),
            Some((7, String::from("restore-me"), ProfileTagCategory::Prefix,))
        );
        assert!(matches!(
            state.complete_restore(),
            DictionaryCompletion::Restored
        ));
        assert_eq!(state.undo_details(), None);
    }
}
