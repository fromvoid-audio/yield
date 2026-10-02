#![deny(missing_docs)]
#![deny(warnings)]

//! Profile-owned tag-definition mutation state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::TryRecvError;

use gpui::{AnyView, App, Context, Render, Window, div, prelude::*, px};
use yield_design::Theme;
use yield_library::sample_sources::{
    ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularySetReceipt,
};
use yield_session::{
    CatalogEvent, CatalogTagAssignmentPreview, VocabularyRequest, VocabularySubmitError,
};

use crate::theme::color;

use super::Browser;
use super::source_registry::SourceRegistryGlobal;

/// Browser-owned state for one profile dictionary mutation and its bounded undo receipt.
pub(super) struct TagDictionaryState {
    pending: Option<DictionaryMutation>,
    last_removed: Option<LastRemovedDefinition>,
    undo_acquisition: Option<yield_session::ProfileAcquisitionId>,
    preview: Option<AssignmentPreviewState>,
}

static NEXT_PREVIEW_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

impl Default for TagDictionaryState {
    fn default() -> Self {
        Self {
            pending: None,
            last_removed: None,
            undo_acquisition: None,
            preview: None,
        }
    }
}

enum AssignmentPreviewState {
    Loading { request_id: u64, label: String },
    Ready(CatalogTagAssignmentPreview),
    Failed { label: String, message: String },
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
            .child("Remove definition only · assignments stay")
    }
}

/// Render the dictionary-only tooltip for a definition removal control.
pub(super) fn remove_definition_tooltip(_: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|_| RemoveDefinitionTooltip).into()
}

impl TagDictionaryState {
    pub(super) fn has_preview(&self) -> bool {
        self.preview.is_some()
    }

    pub(super) fn preview_pending(&self) -> bool {
        matches!(
            self.preview.as_ref(),
            Some(AssignmentPreviewState::Loading { .. })
        )
    }

    pub(super) fn next_preview_request_id(&self) -> u64 {
        NEXT_PREVIEW_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
    }

    pub(super) fn begin_preview(&mut self, request_id: u64, label: String) {
        self.preview = Some(AssignmentPreviewState::Loading { request_id, label });
    }

    pub(super) fn close_preview(&mut self) {
        self.preview = None;
    }

    fn accept_preview_event(&mut self, event: &CatalogEvent) -> bool {
        let Some(AssignmentPreviewState::Loading { request_id, label }) = self.preview.as_ref()
        else {
            return false;
        };
        let replacement = match event {
            CatalogEvent::TagAssignmentPreviewReady {
                request_id: event_id,
                preview,
            } if event_id == request_id && preview.label == *label => {
                AssignmentPreviewState::Ready(preview.clone())
            }
            CatalogEvent::TagAssignmentPreviewFailed {
                request_id: event_id,
                error,
            } if event_id == request_id => AssignmentPreviewState::Failed {
                label: label.clone(),
                message: error.to_string(),
            },
            _ => return false,
        };
        self.preview = Some(replacement);
        true
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn has_undo(&self) -> bool {
        self.last_removed.is_some()
    }

    pub(super) fn submit_remove(
        &mut self,
        global: &SourceRegistryGlobal,
        acquisition: yield_session::ProfileAcquisitionId,
        revision: u64,
        label: String,
    ) -> Result<(), VocabularySubmitError> {
        let request = global.delete_profile_tag_vocabulary(acquisition, revision, label.clone())?;
        self.pending = Some(DictionaryMutation::Remove { label, request });
        Ok(())
    }

    pub(super) fn submit_restore(
        &mut self,
        global: &SourceRegistryGlobal,
        acquisition: yield_session::ProfileAcquisitionId,
        revision: u64,
        label: String,
        category: ProfileTagCategory,
    ) -> Result<(), VocabularySubmitError> {
        let request =
            global.set_profile_tag_category(acquisition, revision, label.clone(), category)?;
        self.pending = Some(DictionaryMutation::Restore {
            label,
            category,
            request,
        });
        Ok(())
    }

    fn clear_undo_for_changed_acquisition(
        &mut self,
        current: Option<&yield_session::ProfileAcquisitionId>,
    ) -> bool {
        if self
            .undo_acquisition
            .as_ref()
            .is_some_and(|acquisition| Some(acquisition) != current)
        {
            self.invalidate_unconfirmed();
            return true;
        }
        false
    }

    pub(super) fn undo_acquisition(&self) -> Option<&yield_session::ProfileAcquisitionId> {
        self.undo_acquisition.as_ref()
    }

    fn invalidate_unconfirmed(&mut self) {
        self.last_removed = None;
        self.undo_acquisition = None;
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
        self.undo_acquisition = None;
        DictionaryCompletion::Restored
    }
}

pub(super) fn apply_preview_event(browser: &mut Browser, event: &CatalogEvent) -> bool {
    browser.tag_dictionary.accept_preview_event(event)
}

pub(super) fn render_preview(
    browser: &Browser,
    theme: Theme,
    cx: &mut Context<Browser>,
) -> impl gpui::IntoElement {
    let title = match browser.tag_dictionary.preview.as_ref() {
        Some(AssignmentPreviewState::Loading { label, .. })
        | Some(AssignmentPreviewState::Failed { label, .. }) => label.clone(),
        Some(AssignmentPreviewState::Ready(preview)) => preview.label.clone(),
        None => String::new(),
    };
    let close = div()
        .id("tag-assignment-preview-close")
        .cursor_pointer()
        .text_color(color(theme.colors.text_muted))
        .on_click(cx.listener(|view, _, _, cx| {
            view.tag_dictionary.close_preview();
            super::tag_library::invalidate(view);
            view.start_source_registry_poll(cx);
            cx.notify();
        }))
        .child("Close");
    let body = match browser.tag_dictionary.preview.as_ref() {
        Some(AssignmentPreviewState::Loading { .. }) => div()
            .child("Reading affected assignments…")
            .into_any_element(),
        Some(AssignmentPreviewState::Failed { message, .. }) => div()
            .text_color(color(theme.colors.warning))
            .child(format!("Preview unavailable: {message}"))
            .into_any_element(),
        Some(AssignmentPreviewState::Ready(preview)) => {
            let sources = preview
                .affected_sources
                .iter()
                .map(|source| {
                    let source_label = browser
                        .source_sessions
                        .iter()
                        .find(|session| session.source_id.as_ref() == Some(&source.source_id))
                        .map(|session| session.label.clone())
                        .unwrap_or_else(|| source.source_id.to_string());
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(theme.spacing.control_gap))
                        .child(format!(
                            "{source_label} · {} assignments",
                            source.paths.len()
                        ))
                        .children(source.paths.iter().map(|path| {
                            div()
                                .pl(px(theme.spacing.control_gap))
                                .text_color(color(theme.colors.text_muted))
                                .child(path.display().to_string())
                        }))
                })
                .collect::<Vec<_>>();
            div()
                .flex()
                .flex_col()
                .gap(px(theme.spacing.control_gap))
                .child(format!(
                    "{} assignments in {} of {} sources",
                    preview.total_assignments,
                    preview.affected_sources.len(),
                    preview.checked_sources
                ))
                .child("Preview only · assignment-wide changes are not available yet")
                .children(sources)
                .into_any_element()
        }
        None => div().into_any_element(),
    };
    div()
        .id("tag-assignment-preview")
        .flex()
        .flex_col()
        .flex_1()
        .min_h_0()
        .w_full()
        .gap(px(theme.spacing.control_gap))
        .p(px(theme.spacing.panel_padding))
        .text_size(px(theme.typography.small_size))
        .text_color(color(theme.colors.text_primary))
        .child(
            div()
                .flex()
                .items_center()
                .child(format!("Assignments: {title}"))
                .child(div().flex_1())
                .child(close),
        )
        .child(
            div()
                .id("tag-assignment-preview-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(body),
        )
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
    let current_acquisition = cx
        .has_global::<SourceRegistryGlobal>()
        .then(|| {
            cx.global::<SourceRegistryGlobal>()
                .vocabulary_acquisition_id()
                .ok()
        })
        .flatten();
    let undo_invalidated = browser
        .tag_dictionary
        .clear_undo_for_changed_acquisition(current_acquisition.as_ref());
    let Some(mutation) = browser.tag_dictionary.take_pending() else {
        return undo_invalidated;
    };
    let acquisition = match &mutation {
        DictionaryMutation::Remove { request, .. } => request.acquisition_id(),
        DictionaryMutation::Restore { request, .. } => request.acquisition_id(),
    };
    if !super::source_registry::vocabulary_acquisition_is_current(acquisition, cx) {
        browser.tag_dictionary.invalidate_unconfirmed();
        super::tag_library::dictionary_mutation_failed(
            browser,
            String::from(
                "Profile changed; dictionary operation unconfirmed. Refresh and try again",
            ),
            cx,
        );
        return true;
    }
    match mutation {
        DictionaryMutation::Remove { label, mut request } => match request.try_recv() {
            Err(TryRecvError::Empty) => {
                browser
                    .tag_dictionary
                    .retain(DictionaryMutation::Remove { label, request });
                false
            }
            Err(TryRecvError::Disconnected) => {
                browser.tag_dictionary.invalidate_unconfirmed();
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    String::from(
                        "Tag definition removal unconfirmed; refreshing authoritative tags",
                    ),
                    cx,
                );
                true
            }
            Ok(Ok(receipt)) => {
                let removed = receipt.removed.is_some();
                let completion = browser.tag_dictionary.complete_remove(receipt);
                if removed {
                    browser.tag_dictionary.undo_acquisition =
                        Some(request.acquisition_id().clone());
                }
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
                if matches!(error, yield_library::sample_sources::LibraryError::GlobalMutationCompletionMismatch | yield_library::sample_sources::LibraryError::GlobalMutationStaleAcquisition) {
                    browser.tag_dictionary.invalidate_unconfirmed();
                }
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    if super::source_registry::vocabulary_mutation_is_unconfirmed(&error) {
                        format!("Tag definition removal unconfirmed; refreshing tags: {error}")
                    } else {
                        format!("Could not remove tag definition: {error}")
                    },
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
                browser.tag_dictionary.invalidate_unconfirmed();
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    String::from(
                        "Tag definition restoration unconfirmed; refreshing authoritative tags",
                    ),
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
                if matches!(error, yield_library::sample_sources::LibraryError::GlobalMutationCompletionMismatch | yield_library::sample_sources::LibraryError::GlobalMutationStaleAcquisition) {
                    browser.tag_dictionary.invalidate_unconfirmed();
                }
                super::tag_library::dictionary_mutation_failed(
                    browser,
                    if super::source_registry::vocabulary_mutation_is_unconfirmed(&error) {
                        format!("Tag definition restoration unconfirmed; refreshing tags: {error}")
                    } else {
                        format!("Could not restore tag definition: {error}")
                    },
                    cx,
                );
                true
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        AssignmentPreviewState, DictionaryCompletion, LastRemovedDefinition, TagDictionaryState,
    };
    use yield_library::sample_sources::SourceId;
    use yield_library::sample_sources::reconciliation::RootIdentity;
    use yield_library::sample_sources::{
        ProfileTagCategory, ProfileTagVocabularyDeleteReceipt, ProfileTagVocabularyEntry,
    };
    use yield_session::{
        CatalogEvent, CatalogTagAssignmentPreview, CatalogTagAssignmentPreviewSource,
    };

    #[test]
    fn affected_assignment_preview_ignores_stale_events_and_remains_immutable_until_closed() {
        let mut state = TagDictionaryState::default();
        state.begin_preview(42, String::from("shared"));
        let preview = CatalogTagAssignmentPreview {
            label: String::from("shared"),
            checked_sources: 2,
            affected_sources: vec![CatalogTagAssignmentPreviewSource {
                source_id: SourceId::from_string("first"),
                lifecycle_generation: 7,
                root_identity: RootIdentity::from_bytes(vec![1; 16]),
                revision: 9,
                paths: vec![PathBuf::from("one.wav"), PathBuf::from("two.wav")],
            }],
            total_assignments: 2,
        };
        assert!(
            !state.accept_preview_event(&CatalogEvent::TagAssignmentPreviewReady {
                request_id: 41,
                preview: preview.clone(),
            })
        );
        assert!(state.preview_pending());
        assert!(
            state.accept_preview_event(&CatalogEvent::TagAssignmentPreviewReady {
                request_id: 42,
                preview: preview.clone(),
            })
        );
        assert!(
            !state.accept_preview_event(&CatalogEvent::TagAssignmentPreviewReady {
                request_id: 42,
                preview: CatalogTagAssignmentPreview {
                    total_assignments: 0,
                    ..preview.clone()
                },
            })
        );
        assert!(
            matches!(state.preview.as_ref(), Some(AssignmentPreviewState::Ready(ready)) if *ready == preview)
        );
        state.close_preview();
        assert!(!state.has_preview());
    }

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

    #[test]
    fn unconfirmed_removal_and_profile_change_never_create_or_redirect_undo() {
        use yield_library::app_dirs::{
            ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard,
        };
        use yield_library::sample_sources::GlobalLibraryWriter;
        let _runtime = yield_library::test_runtime::TestRuntimeGuard::acquire();
        let temp = tempfile::tempdir().unwrap();
        let _base = ConfigBaseGuard::set(std::fs::canonicalize(temp.path()).unwrap());
        let _profile = PersistenceProfileGuard::named("ui-vocabulary-context-fixture");
        let capture = || {
            let guard = WritableProfileGuard::acquire_current().unwrap();
            let mut writer = GlobalLibraryWriter::start(&guard).unwrap();
            let acquisition = writer.profile_tag_vocabulary_client().acquisition_id();
            writer.shutdown();
            acquisition
        };
        let original = capture();
        let replacement = capture();
        assert_ne!(original, replacement);
        let mut state = TagDictionaryState::default();
        state.complete_remove(ProfileTagVocabularyDeleteReceipt {
            revision: 7,
            removed: Some(ProfileTagVocabularyEntry {
                label: "keep".into(),
                category: ProfileTagCategory::Character,
            }),
        });
        state.undo_acquisition = Some(original.clone());
        assert!(!state.clear_undo_for_changed_acquisition(Some(&original)));
        assert!(state.has_undo());
        assert!(state.clear_undo_for_changed_acquisition(Some(&replacement)));
        assert!(!state.has_undo());
        state.complete_remove(ProfileTagVocabularyDeleteReceipt {
            revision: 7,
            removed: None,
        });
        assert!(
            state.undo_details().is_none(),
            "a fresh no-op cannot fabricate the prior tombstone"
        );
        state.complete_remove(ProfileTagVocabularyDeleteReceipt {
            revision: 8,
            removed: Some(ProfileTagVocabularyEntry {
                label: "other".into(),
                category: ProfileTagCategory::Prefix,
            }),
        });
        state.undo_acquisition = Some(replacement);
        state.invalidate_unconfirmed();
        assert!(state.undo_details().is_none());
        assert!(state.undo_acquisition().is_none());
    }
}
