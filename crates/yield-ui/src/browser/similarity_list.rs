//! Explicit bounded sample similarity and memory-only projection fences.
use super::{Browser, catalog, catalog_metadata::CatalogMetadataContext};
use gpui::{AnyElement, Context, SharedString, div, prelude::*, px};
use std::{path::PathBuf, sync::Arc};
use yield_core::{AudioFileId, FolderScope, PlaybackTypeFilter, RatingFilter, SortSpec};
use yield_design::Theme;
use yield_library::sample_sources::SourcePublicationFence;
use yield_session::{
    CatalogCommand, CatalogEvent, SimilarityAspect, SimilarityCancellation,
    SourceSimilarityRequest, SourceSimilarityResult, SourceSimilarityRow,
};

pub(super) const ASPECTS: [(SimilarityAspect, &str); 5] = [
    (SimilarityAspect::Overall, "Overall"),
    (SimilarityAspect::Spectrum, "Spectrum"),
    (SimilarityAspect::Timbre, "Timbre"),
    (SimilarityAspect::Pitch, "Pitch proxy"),
    (SimilarityAspect::Amplitude, "Amplitude"),
];

#[derive(Default)]
pub(super) struct SimilarityListState {
    next_generation: u64,
    active: Option<Projection>,
    settings: AspectSettings,
}
#[derive(Clone, Copy)]
struct AspectControl {
    enabled: bool,
    weight: f32,
}
struct AspectSettings {
    weighting_enabled: bool,
    controls: [AspectControl; 5],
}
impl Default for AspectSettings {
    fn default() -> Self {
        Self {
            weighting_enabled: false,
            controls: [AspectControl {
                enabled: true,
                weight: 1.0,
            }; 5],
        }
    }
}
impl AspectSettings {
    fn effective_score(&self, scores: &[Option<f32>; 5]) -> Option<f32> {
        let finite =
            |score: Option<f32>| score.filter(|s| s.is_finite()).map(|s| s.clamp(-1.0, 1.0));
        let raw = finite(scores[SimilarityAspect::Overall.index()])?;
        if !self.weighting_enabled {
            return Some(raw);
        }
        let mut sum = 0.0;
        let mut weight = 0.0;
        for (index, control) in self.controls.iter().enumerate() {
            if control.enabled && control.weight > f32::EPSILON {
                if let Some(score) = finite(scores[index]) {
                    sum += score * control.weight;
                    weight += control.weight;
                }
            }
        }
        Some(if weight <= f32::EPSILON {
            raw
        } else {
            (sum / weight).clamp(-1.0, 1.0)
        })
    }
}
fn normalized_weight(weight: f32) -> f32 {
    if weight.is_finite() {
        weight.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

#[derive(Clone)]
struct ViewBinding {
    context: CatalogMetadataContext,
    scan_generation: u64,
    catalog_generation: u64,
    query: String,
    folder: Option<PathBuf>,
    folder_scope: FolderScope,
    collection: Option<usize>,
    rating: RatingFilter,
    playback: PlaybackTypeFilter,
    sort: SortSpec,
    age: yield_core::PlaybackAgeFilter,
    curation_queue: yield_core::CurationQueueMode,
    enabled_filters: [bool; 3],
    reveal: Option<AudioFileId>,
}
struct Projection {
    binding: ViewBinding,
    request: SourceSimilarityRequest,
    cancellation: SimilarityCancellation,
    fence: Option<SourcePublicationFence>,
    result: Option<SourceSimilarityResult>,
    status: String,
    terminal: bool,
}
impl Drop for Projection {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}
impl SimilarityListState {
    pub(super) fn cancel(&mut self) {
        self.active = None;
    }
}
impl ViewBinding {
    fn capture(browser: &Browser) -> Option<Self> {
        Some(Self {
            context: browser.catalog_metadata.context()?,
            scan_generation: browser.scan_generation,
            catalog_generation: browser.catalog_next_generation,
            query: browser.view_model.query().to_owned(),
            folder: browser.view_model.folder().map(PathBuf::from),
            folder_scope: browser.view_model.folder_scope(),
            collection: browser.view_model.collection_filter(),
            rating: browser.view_model.rating_filter(),
            playback: browser.view_model.playback_type_filter(),
            sort: browser.view_model.sort(),
            age: browser.view_model.playback_age_filter(),
            curation_queue: browser.view_model.curation_queue(),
            enabled_filters: filter_enablement(browser),
            reveal: browser.view_model.reveal_id().cloned(),
        })
    }
    fn is_current(&self, browser: &Browser) -> bool {
        let Some(context) = browser.catalog_metadata.context() else {
            return false;
        };
        let Some(activation) = browser.catalog_activation.as_ref() else {
            return false;
        };
        let session = browser
            .active_source
            .and_then(|index| browser.source_sessions.get(index));
        browser.root.as_ref() == Some(&self.context.root)
            && session.is_some_and(|session| {
                session.root == self.context.root
                    && session.source_id.as_ref() == Some(&self.context.source_id)
            })
            && activation.source_id == self.context.source_id
            && activation.root == self.context.root
            && activation.lifecycle_generation == self.context.lifecycle_generation
            && activation.registered
            && activation.registration_admitted
            && activation.audit_finished
            && activation.audit_error.is_none()
            && context.source_id == self.context.source_id
            && context.root == self.context.root
            && context.root_identity == self.context.root_identity
            && context.lifecycle_generation == self.context.lifecycle_generation
            && context.directory_generation == self.context.directory_generation
            && context.revision == self.context.revision
            && Arc::ptr_eq(&context.entry_indices, &self.context.entry_indices)
            && browser.scan_generation == self.scan_generation
            && browser.catalog_next_generation == self.catalog_generation
            && browser.view_model.query() == self.query
            && browser.view_model.folder() == self.folder.as_deref()
            && browser.view_model.folder_scope() == self.folder_scope
            && browser.view_model.collection_filter() == self.collection
            && browser.view_model.rating_filter() == self.rating
            && browser.view_model.playback_type_filter() == self.playback
            && browser.view_model.sort() == self.sort
            && browser.view_model.playback_age_filter() == self.age
            && browser.view_model.curation_queue() == self.curation_queue
            && filter_enablement(browser) == self.enabled_filters
            && browser.view_model.reveal_id() == self.reveal.as_ref()
            && !super::tag_filter::scope_pending(browser)
    }
}
impl Projection {
    fn is_current(&self, browser: &Browser) -> bool {
        !self.cancellation.is_cancelled()
            && self
                .fence
                .as_ref()
                .is_none_or(SourcePublicationFence::is_live)
            && self.binding.is_current(browser)
            && self.candidates_match(browser)
    }
    fn candidates_match(&self, browser: &Browser) -> bool {
        let mut index = 0;
        for row in browser.view_model.rows().iter().take(65) {
            let Some(entry) = browser.entries.get(row.entry_index) else {
                return false;
            };
            let Some(path) = entry.relative_path.to_str() else {
                return false;
            };
            if path == self.request.anchor() {
                continue;
            }
            if index == 64 {
                break;
            }
            if self.request.candidates().get(index).map(String::as_str) != Some(path) {
                return false;
            }
            index += 1;
        }
        index == self.request.candidates().len()
    }
    fn accepts_result(&self, result: &SourceSimilarityResult) -> bool {
        let query = result.query();
        let revision = query.revision();
        revision.source_id() == self.request.source_id()
            && revision.lifecycle_generation() == self.request.lifecycle_generation()
            && revision.revision() == self.request.revision()
            && revision.root_identity() == &self.binding.context.root_identity
            && query.anchor().relative_path.as_deref() == Some(self.request.anchor())
            && query.aspect() == self.request.aspect()
            && query.generation() == self.request.generation()
            && result.rows().len() == self.request.candidates().len()
            && result.rows().iter().all(|row| {
                row.target()
                    .relative_path
                    .as_ref()
                    .is_some_and(|path| self.request.candidates().contains(path))
            })
    }
}
impl Browser {
    fn set_similarity_aspect_enabled(
        &mut self,
        aspect: SimilarityAspect,
        enabled: bool,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if control_is_current(self, generation) {
            self.similarity_list.settings.controls[aspect.index()].enabled = enabled;
            cx.notify();
        }
    }
    fn set_similarity_aspect_weight(
        &mut self,
        aspect: SimilarityAspect,
        weight: f32,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if control_is_current(self, generation) {
            self.similarity_list.settings.controls[aspect.index()].weight =
                normalized_weight(weight);
            cx.notify();
        }
    }
    fn set_similarity_aspect_weighting_enabled(
        &mut self,
        enabled: bool,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        if control_is_current(self, generation) {
            self.similarity_list.settings.weighting_enabled = enabled;
            cx.notify();
        }
    }
    /// Toggle one audited row's anchor; every aspect change gets a fresh request.
    pub(super) fn toggle_similarity_anchor(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if self.similarity_list.active.as_ref().is_some_and(|p| {
            p.is_current(self) && p.request.anchor() == path.to_str().unwrap_or("")
        }) {
            self.similarity_list.cancel();
            cx.notify();
            return;
        }
        self.request_similarity(path, SimilarityAspect::Overall, cx);
    }
    fn request_similarity(
        &mut self,
        path: PathBuf,
        aspect: SimilarityAspect,
        cx: &mut Context<Self>,
    ) {
        self.similarity_list.cancel();
        let Some(context) = self.catalog_metadata.context() else {
            self.set_playback_status("Similarity unavailable: refresh the source catalog");
            cx.notify();
            return;
        };
        if !context.entry_indices.contains_key(&path) {
            self.set_playback_status("Similarity anchor is no longer in the catalog");
            cx.notify();
            return;
        }
        let Some(anchor) = path.to_str().map(str::to_owned) else {
            self.set_playback_status("Similarity unavailable for this pathname");
            cx.notify();
            return;
        };
        // Inspect at most 65 cached visible rows; no full-source pass or filesystem work.
        let candidates = self
            .view_model
            .rows()
            .iter()
            .take(65)
            .filter_map(|row| self.entries.get(row.entry_index))
            .filter(|entry| entry.relative_path != path)
            .take(64)
            .map(|entry| entry.relative_path.to_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>();
        let Some(candidates) = candidates else {
            self.set_playback_status("Similarity unavailable for a visible pathname");
            cx.notify();
            return;
        };
        let Some(generation) = self.similarity_list.next_generation.checked_add(1) else {
            self.set_playback_status("Similarity request generation exhausted");
            cx.notify();
            return;
        };
        self.similarity_list.next_generation = generation;
        let Ok(request) = SourceSimilarityRequest::new(
            context.source_id.clone(),
            context.lifecycle_generation,
            context.revision,
            generation,
            anchor,
            &candidates,
            aspect,
        ) else {
            self.set_playback_status("Similarity input exceeds supported limits");
            cx.notify();
            return;
        };
        let cancellation = SimilarityCancellation::new();
        let mut projection = Projection {
            binding: ViewBinding::capture(self).expect("captured catalog context"),
            request: request.clone(),
            cancellation: cancellation.clone(),
            fence: None,
            result: None,
            status: "Finding similar samples…".into(),
            terminal: false,
        };
        if !projection.is_current(self) {
            self.set_playback_status("Similarity source is stale");
            cx.notify();
            return;
        }
        match catalog::submit_catalog_command(
            CatalogCommand::RequestSimilarity {
                request,
                cancellation,
            },
            cx,
        ) {
            Ok(()) => {
                self.start_source_registry_poll(cx);
            }
            Err(error) => {
                projection.status = format!("Similarity unavailable: {error}");
                projection.terminal = true;
            }
        }
        self.similarity_list.active = Some(projection);
        cx.notify();
    }
}
/// Discard changed contexts before any cached row can be rendered.
pub(super) fn invalidate_if_stale(browser: &mut Browser) {
    if browser
        .similarity_list
        .active
        .as_ref()
        .is_some_and(|p| !p.is_current(browser))
    {
        browser.similarity_list.cancel();
        browser.set_playback_status("Similarity results are stale; choose the anchor again");
    }
}
/// Consume exact-current events; successful rows always retain physical publication custody.
pub(super) fn apply_catalog_event(
    browser: &mut Browser,
    event: &mut CatalogEvent,
    fence: Option<SourcePublicationFence>,
    cx: &mut Context<Browser>,
) -> bool {
    let CatalogEvent::SimilarityCompleted {
        request,
        cancellation,
        result,
    } = event
    else {
        return false;
    };
    invalidate_if_stale(browser);
    let Some(projection) = browser.similarity_list.active.as_mut() else {
        return true;
    };
    if projection.terminal || projection.request != *request || cancellation.is_cancelled() {
        return true;
    }
    match result {
        Ok(rows)
            if fence.as_ref().is_some_and(SourcePublicationFence::is_live)
                && projection.accepts_result(rows) =>
        {
            // Move the bounded immutable result; no payload or SQL reads on GPUI.
            let Ok(result) = std::mem::replace(
                result,
                Err(yield_session::SimilarityDispatchFailure::Cancelled),
            ) else {
                unreachable!()
            };
            projection.status = format!("{} candidates · current visible set", result.rows().len());
            projection.result = Some(result);
            projection.fence = fence;
        }
        Ok(_) => {
            return true;
        }
        Err(error) => {
            projection.status = format!("Similarity unavailable: {}", failure_message(*error));
        }
    }
    projection.terminal = true;
    cx.notify();
    true
}
struct AnchorTarget {
    binding: Option<ViewBinding>,
    path: PathBuf,
    index: usize,
}
impl AnchorTarget {
    fn capture(browser: &Browser, path: &std::path::Path, index: usize) -> Self {
        Self {
            binding: ViewBinding::capture(browser),
            path: path.to_path_buf(),
            index,
        }
    }
    fn activate(&self, browser: &mut Browser, cx: &mut Context<Browser>) {
        if self.binding.as_ref().is_some_and(|binding| {
            binding.is_current(browser)
                && binding.context.entry_indices.get(&self.path).copied() == Some(self.index)
                && browser
                    .entries
                    .get(self.index)
                    .is_some_and(|entry| entry.relative_path == self.path)
        }) {
            browser.toggle_similarity_anchor(self.path.clone(), cx);
        } else {
            browser.set_playback_status("Similarity anchor is stale; refresh the catalog");
            cx.notify();
        }
    }
}
/// Leading row anchor control keeps pointer activation separate from row selection.
pub(super) fn anchor_button(
    browser: &Browser,
    path: &std::path::Path,
    index: usize,
    cx: &mut Context<Browser>,
) -> AnyElement {
    let active = browser
        .similarity_list
        .active
        .as_ref()
        .is_some_and(|p| p.is_current(browser) && Some(p.request.anchor()) == path.to_str());
    let target = AnchorTarget::capture(browser, path, index);
    div()
        .id(SharedString::from(format!(
            "sample-similarity-anchor-{index}"
        )))
        .debug_selector(move || format!("sample-similarity-anchor-{index}"))
        .flex_none()
        .w(px(20.))
        .cursor_pointer()
        .child(if active { "●" } else { "○" })
        .on_click(cx.listener(move |view, _, _, cx| {
            cx.stop_propagation();
            target.activate(view, cx);
        }))
        .into_any_element()
}
/// Render only bounded cached rows under the same cheap current-context check.
pub(super) fn render(browser: &Browser, theme: &Theme, cx: &mut Context<Browser>) -> AnyElement {
    let Some(projection) = browser
        .similarity_list
        .active
        .as_ref()
        .filter(|p| p.is_current(browser))
    else {
        return div().into_any_element();
    };
    let generation = projection.request.generation();
    let anchor = projection.request.anchor().to_owned();
    let mut panel = div()
        .id("sample-similarity-panel")
        .absolute()
        .top(px(70.))
        .right(px(20.))
        .w(px(440.))
        .max_h(px(520.))
        .overflow_y_scroll()
        .occlude()
        .p(px(12.))
        .bg(crate::theme::color(theme.colors.surface_raised))
        .text_color(crate::theme::color(theme.colors.text_primary))
        .child(
            div()
                .flex()
                .justify_between()
                .child(format!("Similar samples · {}", label(&anchor)))
                .child(
                    div()
                        .id("sample-similarity-close")
                        .debug_selector(|| "sample-similarity-close".into())
                        .cursor_pointer()
                        .child("Close")
                        .on_click(cx.listener(move |view, _, _, cx| {
                            if control_is_current(view, generation) {
                                view.similarity_list.cancel();
                                cx.notify();
                            }
                        })),
                ),
        );
    let mut controls = div().flex().gap(px(8.));
    for (index, (aspect, name)) in ASPECTS.into_iter().enumerate() {
        let path = PathBuf::from(&anchor);
        controls = controls.child(
            div()
                .id(SharedString::from(format!(
                    "sample-similarity-aspect-{index}"
                )))
                .debug_selector(move || format!("sample-similarity-aspect-{index}"))
                .cursor_pointer()
                .child(if aspect == projection.request.aspect() {
                    format!("[{name}]")
                } else {
                    name.to_owned()
                })
                .on_click(cx.listener(move |view, _, _, cx| {
                    if control_is_current(view, generation) {
                        view.request_similarity(path.clone(), aspect, cx);
                    }
                })),
        );
    }
    let enabled = browser.similarity_list.settings.weighting_enabled;
    panel = panel.child(controls).child(
        div()
            .id("sample-similarity-weighting")
            .debug_selector(|| "sample-similarity-weighting".into())
            .cursor_pointer()
            .child(if enabled {
                "Aspect weighting: on"
            } else {
                "Aspect weighting: off"
            })
            .on_click(cx.listener(move |view, _, _, cx| {
                view.set_similarity_aspect_weighting_enabled(!enabled, generation, cx)
            })),
    );
    for (index, (aspect, name)) in ASPECTS.into_iter().enumerate() {
        let control = browser.similarity_list.settings.controls[index];
        let mut row = div()
            .flex()
            .gap(px(8.))
            .child(
                div()
                    .id(SharedString::from(format!(
                        "sample-similarity-enable-{index}"
                    )))
                    .debug_selector(move || format!("sample-similarity-enable-{index}"))
                    .cursor_pointer()
                    .child(format!(
                        "{} {name}",
                        if control.enabled { "☑" } else { "☐" }
                    ))
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.set_similarity_aspect_enabled(aspect, !control.enabled, generation, cx)
                    })),
            )
            .child(format!("{:.2}", control.weight));
        for (label, delta) in [("minus", -0.25), ("plus", 0.25)] {
            row = row.child(
                div()
                    .id(SharedString::from(format!(
                        "sample-similarity-weight-{label}-{index}"
                    )))
                    .debug_selector(move || format!("sample-similarity-weight-{label}-{index}"))
                    .cursor_pointer()
                    .child(if delta < 0.0 { "−" } else { "+" })
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.set_similarity_aspect_weight(
                            aspect,
                            control.weight + delta,
                            generation,
                            cx,
                        )
                    })),
            );
        }
        panel = panel.child(row);
    }
    panel = panel
        .child(if enabled {
            "Rank by weighted aspects"
        } else {
            "Rank by Overall"
        })
        .child(projection.status.clone());
    if let Some(result) = &projection.result {
        let rows = ranked_rows(&browser.similarity_list.settings, result);
        for (index, (row, value)) in rows.iter().enumerate() {
            let score = value.map_or_else(|| "Unavailable".into(), |score| format!("{score:.3}"));
            panel = panel.child(
                div()
                    .id(SharedString::from(format!("sample-similarity-row-{index}")))
                    .debug_selector(move || format!("sample-similarity-row-{index}"))
                    .flex()
                    .justify_between()
                    .child(label(row.target().relative_path.as_deref().unwrap_or("")))
                    .child(score),
            );
        }
    }
    panel.into_any_element()
}
fn ranked_rows<'a>(
    settings: &AspectSettings,
    result: &'a SourceSimilarityResult,
) -> Vec<(&'a SourceSimilarityRow, Option<f32>)> {
    // At most64 borrowed rows; preserve immutable owner result and its publication custody.
    let mut rows = result
        .rows()
        .iter()
        .map(|row| (row, settings.effective_score(row.aspect_scores())))
        .collect::<Vec<_>>();
    rows.sort_by(|(a, sa), (b, sb)| match (sa, sb) {
        (Some(a_score), Some(b_score)) => b_score
            .total_cmp(a_score)
            .then_with(|| a.target().relative_path.cmp(&b.target().relative_path)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.target().relative_path.cmp(&b.target().relative_path),
    });
    rows
}
fn control_is_current(browser: &Browser, generation: u64) -> bool {
    browser
        .similarity_list
        .active
        .as_ref()
        .is_some_and(|p| p.request.generation() == generation && p.is_current(browser))
}
fn filter_enablement(browser: &Browser) -> [bool; 3] {
    use yield_core::BrowserFilterFamily::{Name, PlaybackType, Rating};
    [Name, Rating, PlaybackType].map(|family| browser.view_model.filter_family_enabled(family))
}
fn failure_message(error: yield_session::SimilarityDispatchFailure) -> &'static str {
    use yield_session::SimilarityDispatchFailure::*;
    match error {
        InvalidInput => "input exceeds supported limits",
        StaleSource => "the source changed; choose the anchor again",
        AnchorUnavailable => "analyze the anchor first",
        IdentityUnavailable => "file identity could not be verified",
        OwnerUnavailable => "current feature data could not be read",
        Cancelled => "request cancelled",
    }
}
fn label(path: &str) -> String {
    path.chars().take(80).collect()
}

#[cfg(test)]
#[path = "similarity_list_tests.rs"]
mod tests;
#[cfg(test)]
use yield_session::SimilarityCandidateOutcome;
