#![deny(missing_docs)]
#![deny(warnings)]

//! Playback navigation history integration for the browser.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gpui::{Context, ScrollStrategy};
use yield_core::PlaybackHistoryEntry;

use super::direct_random::DirectRandomVisit;
use super::transport::{PlaybackIntent, PlaybackIntentOrigin};
use super::waveform::WaveformPlayRegion;
use super::{Browser, SelectionOrigin, SourceSession, tag_chips, tag_filter};

/// Direction used by the browser's playback history shortcuts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PlaybackHistoryDirection {
    /// Navigate to the previous accepted playback intent.
    Back,
    /// Navigate to the next accepted playback intent.
    Forward,
}

pub(super) fn history_shortcut_direction(
    key: &str,
    secondary: bool,
    shift: bool,
    alt: bool,
    function: bool,
    held: bool,
) -> Option<PlaybackHistoryDirection> {
    if !secondary || shift || alt || function || held {
        return None;
    }
    match key {
        "left" | "arrowleft" => Some(PlaybackHistoryDirection::Back),
        "right" | "arrowright" => Some(PlaybackHistoryDirection::Forward),
        _ => None,
    }
}

impl Browser {
    pub(super) fn replay_direct_random_visit(
        &mut self,
        visit: DirectRandomVisit,
        cx: &mut Context<Self>,
    ) {
        let path = visit.id.path();
        let Some((source_index, entry_index)) = cached_direct_random_target(
            &self.source_sessions,
            &self.source_registry_removed_roots,
            &visit,
        ) else {
            self.set_playback_status(format!("Random history unavailable: {}", path.display()));
            cx.notify();
            return;
        };
        if !path.is_file() {
            self.set_playback_status(format!("Random history unavailable: {}", path.display()));
            cx.notify();
            return;
        }
        self.stop_playback();
        self.cancel_pending_scan_for_history();
        let index = if self.active_source == Some(source_index)
            && self.view_model.visible_position_for(&visit.id).is_some()
        {
            Some(entry_index)
        } else {
            self.reveal_cached_history_entry(source_index, entry_index, path, cx)
        };
        let Some(index) = index else {
            self.set_playback_status(format!("Random history unavailable: {}", path.display()));
            cx.notify();
            return;
        };
        self.selection.focus_pointer(&visit.id);
        self.select_entry(index, None, SelectionOrigin::Passive, cx);
        self.start_playback_from_position(0.0, cx);
    }

    pub(super) fn record_playback_history_for_intent(&mut self, intent: &PlaybackIntent) {
        let (start, end) = playback_history_span(intent);
        self.playback_history
            .record(intent.path.clone(), start, end);
    }

    pub(super) fn navigate_playback_history(
        &mut self,
        direction: PlaybackHistoryDirection,
        cx: &mut Context<Self>,
    ) {
        self.reset_browser_focus_state();
        self.context_menu = None;
        let entry = match direction {
            PlaybackHistoryDirection::Back => self.playback_history.back(),
            PlaybackHistoryDirection::Forward => self.playback_history.forward(),
        };
        let Some(entry) = entry else {
            self.set_playback_status(
                match direction {
                    PlaybackHistoryDirection::Back => "No earlier playback history",
                    PlaybackHistoryDirection::Forward => "No later playback history",
                }
                .to_string(),
            );
            cx.notify();
            return;
        };

        self.stop_playback();

        let Some((source_index, entry_index)) = cached_history_target(
            &self.source_sessions,
            self.active_source,
            &self.source_registry_removed_roots,
            entry.path(),
        ) else {
            self.set_playback_status(format!(
                "Playback history unavailable: {} is not in a registered source",
                entry.path().display()
            ));
            cx.notify();
            return;
        };

        self.cancel_pending_scan_for_history();
        let same_selected_file = self.active_source == Some(source_index)
            && self
                .selected
                .and_then(|index| self.entries.get(index))
                .is_some_and(|current| current.path == entry.path());
        let same_selected_file_is_visible = history_target_can_replay_in_place(
            same_selected_file,
            self.view_model.query().is_empty(),
            self.selected
                .and_then(|index| self.entries.get(index))
                .and_then(|current| self.view_model.visible_position_for(&current.stable_id()))
                .is_some(),
        );
        if same_selected_file_is_visible {
            self.replay_loaded_history_entry(entry, cx);
            return;
        }

        let Some(selected_index) =
            self.reveal_cached_history_entry(source_index, entry_index, entry.path(), cx)
        else {
            self.set_playback_status(format!(
                "Playback history unavailable: {} is no longer visible",
                entry.path().display()
            ));
            cx.notify();
            return;
        };
        self.select_entry(selected_index, None, SelectionOrigin::Passive, cx);
        self.replay_loaded_history_entry(entry, cx);
    }

    fn replay_loaded_history_entry(&mut self, entry: PlaybackHistoryEntry, cx: &mut Context<Self>) {
        let span = entry.span();
        self.clear_waveform_region();
        self.set_waveform_play_region(WaveformPlayRegion::new(
            f64::from(span.start()),
            f64::from(span.end()),
        ));
        let Some(selected_path) = self
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.clone())
        else {
            self.set_playback_status(format!(
                "Playback history unavailable: {} is not selected",
                entry.path().display()
            ));
            cx.notify();
            return;
        };
        if selected_path != entry.path() {
            self.set_playback_status(format!(
                "Playback history unavailable: {} is not selected",
                entry.path().display()
            ));
            cx.notify();
            return;
        }

        let intent = history_playback_intent(self.playback_policy.enabled(), self.volume, &entry);
        self.request_playback_intent(intent, cx);
    }

    fn cancel_pending_scan_for_history(&mut self) {
        let had_pending_scan = self.scan_cancel.is_some() || self.scan_task.is_some();
        if let Some(cancellation) = self.scan_cancel.take() {
            cancellation.store(true, std::sync::atomic::Ordering::Release);
        }
        self.scan_task.take();
        self.scan_generation =
            next_scan_generation_after_history_cancel(self.scan_generation, had_pending_scan);
        if had_pending_scan {
            let count = self.entries.len();
            self.set_scan_status(format!(
                "{count} audio file{} · cached",
                if count == 1 { "" } else { "s" }
            ));
        }
        self.pending_source_label = None;
        self.pending_source_return_index = None;
        self.pending_source_return_root = None;
        self.restore_selection_id = None;
        self.restore_selection_position = None;
        self.reset_focus();
        self.reset_scroll_edges();
    }

    fn reveal_cached_history_entry(
        &mut self,
        source_index: usize,
        entry_index: usize,
        target_path: &Path,
        cx: &mut Context<Self>,
    ) -> Option<usize> {
        let switching_source = self.active_source != Some(source_index);
        if switching_source {
            self.deactivate_catalog_source(cx);
            self.playback_policy.clear_file();
            self.reset_collection_filter();
        }
        let session = self.source_sessions.get(source_index)?;
        let target = session.entries.get(entry_index)?;
        if target.path != target_path {
            return None;
        }

        let folder = target
            .relative_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let state = session
            .tree
            .reconcile_state(Some(folder), &session.expanded_folders);
        let root = session.root.clone();
        let entries = session.entries.clone();
        let tree = session.tree.clone();
        let folder_scope = session.folder_scope;
        let expanded = state.expanded.clone();
        let selected_folder = state.selected.clone();
        let target_index = entries.iter().position(|entry| entry.path == target_path)?;

        self.active_source = Some(source_index);
        self.pending_source_label = None;
        self.pending_source_return_index = None;
        self.pending_source_return_root = None;
        self.root = Some(root);
        self.entries = entries;
        self.folder_tree = Some(tree);
        self.selected_folder = selected_folder.clone();
        self.expanded_folders = expanded.clone();
        self.folder_scope = folder_scope;
        self.folder_rows_cache.clear();
        self.rebuild_folder_rows_cache();
        if let Some(position) = self
            .folder_rows_cache
            .iter()
            .position(|row| row.path == self.selected_folder)
        {
            self.folder_scroll_handle
                .scroll_to_item(position, ScrollStrategy::Center);
        }
        self.selected = None;
        self.selection.clear();
        self.cancel_waveform_load();
        self.waveform_pending = false;
        self.waveform = None;
        self.waveform_error = None;
        self.invalidate_waveform_generation();
        self.view_model.set_entries(&self.entries);
        self.reschedule_playback_age_refresh(cx);
        if let Some(index) = self.active_source {
            self.view_model.set_curation_queue(
                &self.entries,
                self.source_sessions[index].curation_queue_mode,
            );
        }
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        tag_filter::reinstall_after_projection(self, cx);
        super::curation_queue::reinstall_after_projection(self, cx);
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.search_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.view_model.set_query(&self.entries, "");
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        if let Some(session) = self.source_sessions.get_mut(source_index) {
            session.selected_folder = selected_folder;
            session.expanded_folders = expanded;
            session.folder_scope = folder_scope;
            session.selected_audio_id = None;
        }
        if switching_source || self.catalog_activation.is_none() {
            // History owns the accepted replay request, so preserve its transport while
            // activating the catalog. Activation can reset the temporary reveal; install
            // that reveal afterwards so a filtered history target remains visible.
            let _ = self.begin_catalog_activation(source_index, false, true, false, cx);
        }
        let target_id = self.entries.get(target_index)?.stable_id();
        if self.view_model.visible_position_for(&target_id).is_none() {
            self.view_model
                .set_reveal(&self.entries, Some(target_id.clone()));
        }
        if self.view_model.visible_position_for(&target_id).is_none() {
            return None;
        }
        self.reconcile_selection();
        let count = self.entries.len();
        self.set_scan_status(format!(
            "{count} audio file{} · cached",
            if count == 1 { "" } else { "s" }
        ));
        tag_chips::invalidate(self);
        self.start_source_registry_poll(cx);
        Some(target_index)
    }
}

fn playback_history_span(intent: &PlaybackIntent) -> (f32, f32) {
    if let Some(region) = intent.region.filter(|region| region.is_valid()) {
        return (region.start as f32, region.end as f32);
    }
    if intent.looped {
        (0.0, 1.0)
    } else {
        (intent.start as f32, 1.0)
    }
}

fn history_playback_intent(
    looped: bool,
    volume: f32,
    entry: &PlaybackHistoryEntry,
) -> PlaybackIntent {
    let span = entry.span();
    let start = f64::from(span.start());
    let region = WaveformPlayRegion::new(start, f64::from(span.end()));
    PlaybackIntent {
        start,
        region,
        looped,
        volume: super::volume::quantize_volume(volume),
        path: entry.path().to_path_buf(),
        origin: PlaybackIntentOrigin::HistoryReplay,
        resume_desired: None,
    }
}

fn cached_history_target(
    sessions: &[SourceSession],
    active_source: Option<usize>,
    removed_roots: &BTreeSet<PathBuf>,
    target_path: &Path,
) -> Option<(usize, usize)> {
    let mut candidates = active_source.into_iter().chain(0..sessions.len());
    while let Some(source_index) = candidates.next() {
        let Some(session) = sessions.get(source_index) else {
            continue;
        };
        if removed_roots.contains(&session.root) {
            continue;
        }
        if let Some(entry_index) = session
            .entries
            .iter()
            .position(|entry| entry.path == target_path)
        {
            return Some((source_index, entry_index));
        }
    }
    None
}

fn cached_direct_random_target(
    sessions: &[SourceSession],
    removed_roots: &BTreeSet<PathBuf>,
    visit: &DirectRandomVisit,
) -> Option<(usize, usize)> {
    sessions
        .iter()
        .enumerate()
        .find_map(|(source_index, session)| {
            if session.source_id != visit.source.0
                || session.root != visit.source.1
                || removed_roots.contains(&session.root)
            {
                return None;
            }
            session
                .entries
                .iter()
                .position(|entry| entry.path == visit.id.path())
                .map(|entry_index| (source_index, entry_index))
        })
}

fn history_target_can_replay_in_place(
    same_selected_file: bool,
    query_is_empty: bool,
    target_is_visible: bool,
) -> bool {
    same_selected_file && query_is_empty && target_is_visible
}

fn next_scan_generation_after_history_cancel(current_generation: u64, pending: bool) -> u64 {
    pending
        .then(|| current_generation.wrapping_add(1))
        .unwrap_or(current_generation)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::PathBuf};

    use gpui::TestAppContext;

    use super::{
        PlaybackHistoryDirection, cached_direct_random_target, cached_history_target,
        history_playback_intent, history_shortcut_direction, history_target_can_replay_in_place,
        next_scan_generation_after_history_cancel, playback_history_span,
    };
    use crate::browser::direct_random::DirectRandomVisit;
    use crate::browser::transport::{PlaybackIntent, PlaybackIntentOrigin};
    use crate::browser::waveform::WaveformPlayRegion;
    use crate::browser::{Browser, SourceSession};
    use yield_core::{
        AudioFile, AudioFileCuration, BrowserViewModel, FolderTree, PlaybackHistoryEntry,
    };
    use yield_library::sample_sources::SourceId;

    fn audio(path: &str, relative_path: &str) -> AudioFile {
        AudioFile {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(relative_path),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
    }

    #[test]
    fn accepted_intent_history_span_matches_resolved_playback_mode() {
        let plain = PlaybackIntent {
            start: 0.25,
            region: None,
            looped: false,
            volume: 1.0,
            path: PathBuf::from("plain.wav"),
            origin: PlaybackIntentOrigin::Explicit,
            resume_desired: None,
        };
        assert_eq!(playback_history_span(&plain), (0.25, 1.0));

        let looped = PlaybackIntent {
            looped: true,
            ..plain.clone()
        };
        assert_eq!(playback_history_span(&looped), (0.0, 1.0));

        let region = PlaybackIntent {
            region: WaveformPlayRegion::new(0.2, 0.7),
            ..plain
        };
        assert_eq!(playback_history_span(&region), (0.2, 0.7));
    }

    #[test]
    fn history_replay_intent_uses_current_loop_and_volume_without_recording_origin() {
        let entry = PlaybackHistoryEntry::new("sample.wav", 0.75, 0.25);
        let intent = history_playback_intent(true, 0.375, &entry);
        assert_eq!(intent.start, 0.25);
        assert_eq!(intent.region, WaveformPlayRegion::new(0.25, 0.75));
        assert!(intent.looped);
        assert_eq!(intent.volume, 0.375);
        assert_eq!(intent.origin, PlaybackIntentOrigin::HistoryReplay);
    }

    #[test]
    fn cached_history_target_prefers_active_source_then_falls_back_to_registered_sources() {
        let mut first = SourceSession::new(PathBuf::from("/first"));
        first.entries.push(audio("/first/a.wav", "a.wav"));
        first.entries.push(audio("/first/only.wav", "only.wav"));
        let mut second = SourceSession::new(PathBuf::from("/second"));
        second.entries.push(audio("/second/b.wav", "b.wav"));
        second.entries.push(audio("/first/a.wav", "shared.wav"));
        let sessions = vec![first, second];

        assert_eq!(
            cached_history_target(
                &sessions,
                Some(1),
                &BTreeSet::new(),
                PathBuf::from("/first/a.wav").as_path(),
            ),
            Some((1, 1))
        );
        assert_eq!(
            cached_history_target(
                &sessions,
                Some(0),
                &BTreeSet::new(),
                PathBuf::from("/second/b.wav").as_path(),
            ),
            Some((1, 0))
        );
        assert_eq!(
            cached_history_target(
                &sessions,
                Some(0),
                &BTreeSet::new(),
                PathBuf::from("/gone.wav").as_path(),
            ),
            None
        );
        let removed = BTreeSet::from([PathBuf::from("/first")]);
        assert_eq!(
            cached_history_target(
                &sessions,
                Some(0),
                &removed,
                PathBuf::from("/first/only.wav").as_path(),
            ),
            None
        );
    }

    #[test]
    fn direct_random_replay_requires_exact_cached_source_and_unretired_path() {
        let mut first = SourceSession::new(PathBuf::from("/first"));
        first.entries.push(audio("/first/a.wav", "a.wav"));
        let mut second = SourceSession::new(PathBuf::from("/second"));
        second.entries.push(audio("/second/a.wav", "a.wav"));
        let visit = DirectRandomVisit {
            source: (None, PathBuf::from("/first")),
            id: first.entries[0].stable_id(),
        };
        let sessions = vec![first, second];
        assert_eq!(
            cached_direct_random_target(&sessions, &BTreeSet::new(), &visit),
            Some((0, 0))
        );
        assert_eq!(
            cached_direct_random_target(
                &sessions,
                &BTreeSet::from([PathBuf::from("/first")]),
                &visit
            ),
            None
        );
        let mut missing = visit.clone();
        missing.id = audio("/first/missing.wav", "missing.wav").stable_id();
        assert_eq!(
            cached_direct_random_target(&sessions, &BTreeSet::new(), &missing),
            None
        );
    }

    #[gpui::test]
    fn direct_random_previous_reveals_a_cached_prior_source_and_requests_playback(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext;
        let first_root = tempfile::tempdir().expect("first source");
        let second_root = tempfile::tempdir().expect("second source");
        let first_path = first_root.path().join("a.wav");
        std::fs::write(&first_path, b"fixture").expect("first sample");
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(crate::browser::Browser::new);
        browser.update(cx, |view, cx| {
            let mut first = SourceSession::new(first_root.path().to_path_buf());
            first
                .entries
                .push(audio(first_path.to_str().unwrap(), "a.wav"));
            let visit = DirectRandomVisit {
                source: (None, first.root.clone()),
                id: first.entries[0].stable_id(),
            };
            let mut second = SourceSession::new(second_root.path().to_path_buf());
            second.entries.push(audio(
                second_root.path().join("b.wav").to_str().unwrap(),
                "b.wav",
            ));
            view.source_sessions = vec![first, second];
            view.active_source = Some(1);
            view.root = Some(second_root.path().to_path_buf());
            view.entries = view.source_sessions[1].entries.clone();
            view.view_model.set_entries(&view.entries);
            view.selected = Some(0);
            view.replay_direct_random_visit(visit, cx);
            assert_eq!(view.active_source, Some(0));
            assert_eq!(
                view.selected
                    .and_then(|index| view.entries.get(index))
                    .map(|entry| entry.path.as_path()),
                Some(first_path.as_path())
            );
            assert_eq!(
                view.playback_init_intent
                    .as_ref()
                    .map(|intent| intent.path.as_path()),
                Some(first_path.as_path())
            );
        });
    }

    #[gpui::test]
    fn direct_random_previous_rejects_a_deleted_cached_file_without_changing_focus(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext;
        let first_root = tempfile::tempdir().expect("first source");
        let second_root = tempfile::tempdir().expect("second source");
        let missing = first_root.path().join("missing.wav");
        cx.set_global(crate::theme::ActiveTheme::default());
        let browser = cx.new(crate::browser::Browser::new);
        browser.update(cx, |view, cx| {
            let mut first = SourceSession::new(first_root.path().to_path_buf());
            first
                .entries
                .push(audio(missing.to_str().unwrap(), "missing.wav"));
            let visit = DirectRandomVisit {
                source: (None, first.root.clone()),
                id: first.entries[0].stable_id(),
            };
            let mut second = SourceSession::new(second_root.path().to_path_buf());
            second.entries.push(audio(
                second_root.path().join("current.wav").to_str().unwrap(),
                "current.wav",
            ));
            view.source_sessions = vec![first, second];
            view.active_source = Some(1);
            view.root = Some(second_root.path().to_path_buf());
            view.entries = view.source_sessions[1].entries.clone();
            view.view_model.set_entries(&view.entries);
            view.selected = Some(0);
            view.replay_direct_random_visit(visit, cx);
            assert_eq!(view.active_source, Some(1));
            assert_eq!(view.selected, Some(0));
            assert!(view.playback_init_intent.is_none());
            assert!(view.playback_status.contains("Random history unavailable"));
        });
    }

    #[test]
    fn history_shortcuts_require_plain_secondary_navigation() {
        assert_eq!(
            history_shortcut_direction("left", true, false, false, false, false),
            Some(PlaybackHistoryDirection::Back)
        );
        assert_eq!(
            history_shortcut_direction("arrowright", true, false, false, false, false),
            Some(PlaybackHistoryDirection::Forward)
        );
        for (shift, alt, function, held) in [
            (true, false, false, false),
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
        ] {
            assert_eq!(
                history_shortcut_direction("left", true, shift, alt, function, held),
                None
            );
        }
        assert_eq!(
            history_shortcut_direction("left", false, false, false, false, false),
            None
        );
        assert_eq!(
            history_shortcut_direction("home", true, false, false, false, false),
            None
        );
    }

    #[test]
    fn history_activation_rejects_completion_from_canceled_scan() {
        let started_generation = 41;
        let canceled_generation =
            next_scan_generation_after_history_cancel(started_generation, true);
        assert!(!crate::browser::scan_completion_is_current(
            started_generation,
            canceled_generation
        ));
        assert_eq!(
            next_scan_generation_after_history_cancel(started_generation, false),
            started_generation
        );
    }

    #[test]
    fn filtered_or_hidden_same_file_uses_cached_reveal_before_replay() {
        assert!(history_target_can_replay_in_place(true, true, true));
        assert!(!history_target_can_replay_in_place(true, false, true));
        assert!(!history_target_can_replay_in_place(true, true, false));
        assert!(!history_target_can_replay_in_place(false, true, true));
    }

    #[gpui::test]
    fn history_back_reveals_filtered_cached_file_and_queues_its_region(cx: &mut TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            let root = PathBuf::from("/tmp/opt1765-history-fixture");
            let mut hidden = audio(
                "/tmp/opt1765-history-fixture/sub/hidden.wav",
                "sub/hidden.wav",
            );
            hidden.curation = AudioFileCuration::from_parts(0, false, 0, None, None);
            let mut current = audio("/tmp/opt1765-history-fixture/current.wav", "current.wav");
            current.curation = AudioFileCuration::from_parts(1, false, 0, None, None);
            let entries = vec![hidden.clone(), current.clone()];
            let mut session = SourceSession::new(root.clone());
            session.source_id = Some(SourceId::from_string("opt-1765-history-fixture"));
            session.tree = FolderTree::from_scan(&[], &entries);
            session.entries = entries.clone();
            view.source_sessions = vec![session];
            view.active_source = Some(0);
            view.root = Some(root);
            view.folder_tree = Some(FolderTree::from_scan(&[], &entries));
            view.entries = entries;
            view.view_model = BrowserViewModel::new(&view.entries);
            assert!(view.view_model.toggle_rating_filter(&view.entries, 1));
            view.selected = Some(1);
            let hidden_id = hidden.stable_id();
            assert!(view.view_model.visible_position_for(&hidden_id).is_none());
            view.playback_history.record(hidden.path.clone(), 0.2, 0.6);
            view.playback_history.record(current.path.clone(), 0.0, 1.0);

            view.navigate_playback_history(PlaybackHistoryDirection::Back, cx);

            assert_eq!(view.selected_audio_path(), Some(hidden.path.as_path()));
            assert_eq!(view.view_model.reveal_id(), Some(&hidden_id));
            assert!(view.view_model.visible_position_for(&hidden_id).is_some());
            assert_eq!(
                view.waveform_play_region,
                WaveformPlayRegion::new(f64::from(0.2_f32), f64::from(0.6_f32))
            );
            let pending = view
                .playback_init_intent
                .as_ref()
                .expect("history replay intent waits for preferences or audio output");
            assert_eq!(pending.origin, PlaybackIntentOrigin::HistoryReplay);
            assert_eq!(pending.path, hidden.path);
        });
    }
}
