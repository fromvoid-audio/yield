//! Direct whole-sample random audition history, independent of sticky arrow navigation.

use std::collections::HashSet;
use std::path::PathBuf;

use yield_core::AudioFileId;
use yield_library::sample_sources::SourceId;

const HISTORY_LIMIT: usize = 20;

pub(super) type SourceScope = (Option<SourceId>, PathBuf);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DirectRandomVisit {
    pub(super) source: SourceScope,
    pub(super) id: AudioFileId,
}

/// No-repeat state for the current list and a bounded cross-source visit history.
#[derive(Default)]
pub(super) struct DirectRandomHistory {
    scope: Option<SourceScope>,
    visible_ids: Vec<AudioFileId>,
    visited: HashSet<AudioFileId>,
    history: Vec<DirectRandomVisit>,
    cursor: Option<usize>,
}

impl DirectRandomHistory {
    fn reconcile(&mut self, scope: Option<SourceScope>, visible_ids: &[AudioFileId]) -> bool {
        if self.scope != scope || self.visible_ids != visible_ids {
            self.scope = scope;
            self.visible_ids = visible_ids.to_vec();
            self.visited.clear();
        }
        self.scope.is_some()
    }

    /// Choose a whole visible sample, avoiding repeats until the playable list is exhausted.
    pub(super) fn next(
        &mut self,
        scope: Option<SourceScope>,
        visible_ids: &[AudioFileId],
        current: Option<&AudioFileId>,
        random_unit: f32,
    ) -> Option<AudioFileId> {
        if !self.reconcile(scope.clone(), visible_ids) || visible_ids.is_empty() {
            return None;
        }
        let candidates = |visited: &HashSet<AudioFileId>| {
            visible_ids
                .iter()
                .filter(|id| !visited.contains(*id))
                .filter(|id| visible_ids.len() == 1 || current != Some(*id))
                .collect::<Vec<_>>()
        };
        let mut available = candidates(&self.visited);
        if available.is_empty() {
            self.visited.clear();
            available = candidates(&self.visited);
        }
        let unit = if random_unit.is_finite() {
            random_unit.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let index =
            ((unit * available.len() as f32).floor() as usize).min(available.len().checked_sub(1)?);
        let target = (*available.get(index)?).clone();
        self.visited.insert(target.clone());
        if let Some(cursor) = self.cursor {
            self.history.truncate(cursor + 1);
        }
        self.history.push(DirectRandomVisit {
            source: scope?,
            id: target.clone(),
        });
        if self.history.len() > HISTORY_LIMIT {
            self.history.remove(0);
        }
        self.cursor = Some(self.history.len() - 1);
        Some(target)
    }

    /// Return the previous direct random visit, including a prior source or filtered row.
    pub(super) fn previous(&mut self) -> Option<DirectRandomVisit> {
        let cursor = self.cursor?.checked_sub(1)?;
        self.cursor = Some(cursor);
        self.history.get(cursor).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yield_core::AudioFile;

    fn id(name: &str) -> AudioFileId {
        AudioFile {
            path: PathBuf::from(name),
            relative_path: PathBuf::from(name),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
        .stable_id()
    }

    fn scope(name: &str) -> Option<SourceScope> {
        Some((None, PathBuf::from(name)))
    }

    #[test]
    fn direct_visits_exclude_current_and_replay_only_actual_random_history() {
        let ids = [id("a.wav"), id("b.wav"), id("c.wav")];
        let mut history = DirectRandomHistory::default();
        assert_eq!(
            history.next(scope("one"), &ids, Some(&ids[0]), 0.0),
            Some(ids[1].clone())
        );
        assert_eq!(history.previous(), None);
        assert_eq!(
            history.next(scope("one"), &ids, Some(&ids[1]), 0.0),
            Some(ids[0].clone())
        );
        assert_eq!(
            history.next(scope("one"), &ids, Some(&ids[0]), 0.0),
            Some(ids[2].clone())
        );
        assert_eq!(
            history.previous().map(|visit| visit.id),
            Some(ids[0].clone())
        );
        assert_eq!(
            history.previous().map(|visit| visit.id),
            Some(ids[1].clone())
        );
        assert_eq!(history.previous(), None);
    }

    #[test]
    fn source_or_visible_list_change_resets_no_repeat_but_preserves_visits() {
        let ids = [id("a.wav"), id("b.wav"), id("c.wav")];
        let mut history = DirectRandomHistory::default();
        history.next(scope("one"), &ids, Some(&ids[0]), 0.0);
        history.next(scope("one"), &ids, Some(&ids[1]), 0.0);
        history.next(scope("two"), &ids, Some(&ids[0]), 0.0);
        let prior = history.previous().expect("prior source visit");
        assert_eq!(prior.source, scope("one").unwrap());
        assert_eq!(prior.id, ids[0]);
        history.next(scope("two"), &ids[..2], Some(&ids[0]), 0.0);
        assert_eq!(history.next(None, &ids, Some(&ids[0]), 0.0), None);
    }
}
