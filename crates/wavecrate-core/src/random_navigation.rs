//! Identity-based random navigation over the current visible browser rows.

use std::collections::HashSet;

use crate::AudioFileId;

/// Random sample navigation state, distinct from random playback audition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RandomNavigationState {
    enabled: bool,
    result_ids: Vec<AudioFileId>,
    visited: HashSet<AudioFileId>,
    history: Vec<AudioFileId>,
}

impl RandomNavigationState {
    /// Return whether Up and Down use random navigation.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Toggle the mode and seed its first visit from the focused visible row.
    pub fn set_enabled(
        &mut self,
        enabled: bool,
        focused: Option<&AudioFileId>,
        visible_ids: &[AudioFileId],
    ) {
        if self.enabled == enabled {
            return;
        }
        self.enabled = enabled;
        self.reset_for_selection(focused, visible_ids);
    }

    /// Reset visits when the ordered visible result set changes.
    pub fn reconcile(&mut self, focused: Option<&AudioFileId>, visible_ids: &[AudioFileId]) {
        if self.result_ids != visible_ids {
            self.reset_for_selection(focused, visible_ids);
        }
    }

    /// Choose an unvisited visible row using a caller-supplied random unit.
    ///
    /// The unit is clamped to `0.0..=1.0`; nonfinite values choose the first candidate. After
    /// exhausting a result set, a new cycle begins with the current focus already visited.
    pub fn next(
        &mut self,
        focused: Option<&AudioFileId>,
        visible_ids: &[AudioFileId],
        random_unit: f32,
    ) -> Option<AudioFileId> {
        if !self.enabled {
            return None;
        }
        self.reconcile(focused, visible_ids);
        if visible_ids.len() <= 1 {
            return None;
        }
        if let Some(focused) = focused.filter(|id| visible_ids.contains(id)) {
            self.record(focused);
        }

        let target = self
            .choose_unvisited(visible_ids, random_unit)
            .or_else(|| {
                self.visited.clear();
                self.history.clear();
                if let Some(focused) = focused.filter(|id| visible_ids.contains(id)) {
                    self.record(focused);
                }
                self.choose_unvisited(visible_ids, random_unit)
            })?;
        self.record(&target);
        Some(target)
    }

    /// Walk backward through the actual random visits for this visible result set.
    pub fn previous(
        &mut self,
        focused: Option<&AudioFileId>,
        visible_ids: &[AudioFileId],
    ) -> Option<AudioFileId> {
        if !self.enabled {
            return None;
        }
        self.reconcile(focused, visible_ids);
        if self.history.len() <= 1 {
            return None;
        }
        self.history.pop();
        self.history.last().cloned()
    }

    fn reset_for_selection(&mut self, focused: Option<&AudioFileId>, visible_ids: &[AudioFileId]) {
        self.result_ids = visible_ids.to_vec();
        self.visited.clear();
        self.history.clear();
        if let Some(focused) = focused.filter(|id| visible_ids.contains(id)) {
            self.record(focused);
        }
    }

    fn choose_unvisited(
        &self,
        visible_ids: &[AudioFileId],
        random_unit: f32,
    ) -> Option<AudioFileId> {
        let candidates = visible_ids
            .iter()
            .filter(|id| !self.visited.contains(*id))
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }
        let random_unit = if random_unit.is_finite() {
            random_unit.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let index =
            ((random_unit * candidates.len() as f32).floor() as usize).min(candidates.len() - 1);
        candidates.get(index).map(|id| (*id).clone())
    }

    fn record(&mut self, id: &AudioFileId) {
        self.visited.insert(id.clone());
        if self.history.last() != Some(id) {
            self.history.push(id.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{AudioFileId, RandomNavigationState};

    fn id(name: &str) -> AudioFileId {
        AudioFileId(PathBuf::from(name))
    }

    #[test]
    fn visits_each_visible_identity_before_starting_another_cycle() {
        let ids = [id("a"), id("b"), id("c")];
        let mut state = RandomNavigationState::default();
        assert_eq!(state.next(Some(&ids[0]), &ids, 0.0), None);
        state.set_enabled(true, Some(&ids[0]), &ids);
        assert_eq!(state.next(Some(&ids[0]), &ids, 0.0), Some(ids[1].clone()));
        assert_eq!(state.next(Some(&ids[1]), &ids, 0.0), Some(ids[2].clone()));
        assert_eq!(state.next(Some(&ids[2]), &ids, 1.0), Some(ids[1].clone()));
    }

    #[test]
    fn previous_walks_visits_and_result_change_resets_history() {
        let ids = [id("a"), id("b"), id("c")];
        let mut state = RandomNavigationState::default();
        state.set_enabled(true, Some(&ids[0]), &ids);
        assert_eq!(state.next(Some(&ids[0]), &ids, 0.0), Some(ids[1].clone()));
        assert_eq!(state.next(Some(&ids[1]), &ids, 0.0), Some(ids[2].clone()));
        assert_eq!(state.previous(Some(&ids[2]), &ids), Some(ids[1].clone()));
        assert_eq!(state.previous(Some(&ids[1]), &ids), Some(ids[0].clone()));
        assert_eq!(state.previous(Some(&ids[0]), &ids), None);

        let filtered = [ids[0].clone(), ids[2].clone()];
        assert_eq!(state.previous(Some(&ids[2]), &filtered), None);
        assert_eq!(
            state.next(Some(&ids[2]), &filtered, 0.0),
            Some(ids[0].clone())
        );
    }

    #[test]
    fn empty_singleton_and_disabled_mode_do_not_jump() {
        let ids = [id("a")];
        let mut state = RandomNavigationState::default();
        state.set_enabled(true, Some(&ids[0]), &ids);
        assert_eq!(state.next(Some(&ids[0]), &ids, 0.0), None);
        assert_eq!(state.next(None, &[], 0.0), None);
        assert_eq!(state.previous(None, &[]), None);
        state.set_enabled(false, Some(&ids[0]), &ids);
        assert!(!state.enabled());
        assert_eq!(state.next(Some(&ids[0]), &ids, 0.0), None);
    }

    #[test]
    fn unit_bounds_choose_existing_candidates() {
        let ids = [id("a"), id("b"), id("c")];
        let mut state = RandomNavigationState::default();
        state.set_enabled(true, Some(&ids[0]), &ids);
        assert_eq!(
            state.next(Some(&ids[0]), &ids, f32::NAN),
            Some(ids[1].clone())
        );
        assert_eq!(
            state.next(Some(&ids[1]), &ids, f32::INFINITY),
            Some(ids[2].clone())
        );
    }
}
