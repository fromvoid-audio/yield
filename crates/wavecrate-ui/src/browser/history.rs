#![deny(missing_docs)]
#![deny(warnings)]

//! Playback navigation history integration for the browser.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use gpui::{Context, ScrollStrategy};
use wavecrate_core::PlaybackHistoryEntry;

use super::transport::{PlaybackIntent, PlaybackIntentOrigin};
use super::waveform::WaveformPlayRegion;
use super::{Browser, SelectionOrigin, SourceSession};

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
        let entry = match direction {
            PlaybackHistoryDirection::Back => self.playback_history.back(),
            PlaybackHistoryDirection::Forward => self.playback_history.forward(),
        };
        let Some(entry) = entry else {
            self.playback_status = match direction {
                PlaybackHistoryDirection::Back => "No earlier playback history",
                PlaybackHistoryDirection::Forward => "No later playback history",
            }
            .to_string();
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
            self.playback_status = format!(
                "Playback history unavailable: {} is not in a registered source",
                entry.path().display()
            );
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
            self.playback_status = format!(
                "Playback history unavailable: {} is no longer visible",
                entry.path().display()
            );
            cx.notify();
            return;
        };
        self.select_entry(selected_index, None, SelectionOrigin::Passive, cx);
        self.replay_loaded_history_entry(entry, cx);
    }

    fn replay_loaded_history_entry(&mut self, entry: PlaybackHistoryEntry, cx: &mut Context<Self>) {
        let span = entry.span();
        self.clear_waveform_region();
        self.waveform_play_region =
            WaveformPlayRegion::new(f64::from(span.start()), f64::from(span.end()));
        let Some(selected_path) = self
            .selected
            .and_then(|index| self.entries.get(index))
            .map(|entry| entry.path.clone())
        else {
            self.playback_status = format!(
                "Playback history unavailable: {} is not selected",
                entry.path().display()
            );
            cx.notify();
            return;
        };
        if selected_path != entry.path() {
            self.playback_status = format!(
                "Playback history unavailable: {} is not selected",
                entry.path().display()
            );
            cx.notify();
            return;
        }

        let intent = history_playback_intent(self.loop_enabled, self.volume, &entry);
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
            self.scan_status = format!(
                "{count} audio file{} · cached",
                if count == 1 { "" } else { "s" }
            );
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
        self.view_model.set_folder_scope(
            &self.entries,
            Some(self.selected_folder.clone()),
            self.folder_scope,
        );
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        self.search_input
            .update(cx, |input, cx| input.set_content("", cx));
        self.view_model.set_query(&self.entries, "");
        self.refresh_collection_filter_counts();
        self.reconcile_selection();
        let count = self.entries.len();
        self.scan_status = format!(
            "{count} audio file{} · cached",
            if count == 1 { "" } else { "s" }
        );
        if let Some(session) = self.source_sessions.get_mut(source_index) {
            session.selected_folder = selected_folder;
            session.expanded_folders = expanded;
            session.folder_scope = folder_scope;
            session.selected_audio_id = None;
        }
        if switching_source || self.catalog_activation.is_none() {
            // History already owns the accepted replay request. Preserve its transport while
            // handing the newly active source to the catalog; a catalog failure keeps this
            // cached projection usable through the existing fallback path.
            let _ = self.begin_catalog_activation(source_index, false, true, false, cx);
        }
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

    use super::{
        PlaybackHistoryDirection, cached_history_target, history_playback_intent,
        history_shortcut_direction, history_target_can_replay_in_place,
        next_scan_generation_after_history_cancel, playback_history_span,
    };
    use crate::browser::SourceSession;
    use crate::browser::transport::{PlaybackIntent, PlaybackIntentOrigin};
    use crate::browser::waveform::WaveformPlayRegion;
    use wavecrate_core::{AudioFile, PlaybackHistoryEntry};

    fn audio(path: &str, relative_path: &str) -> AudioFile {
        AudioFile {
            path: PathBuf::from(path),
            relative_path: PathBuf::from(relative_path),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
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
}
