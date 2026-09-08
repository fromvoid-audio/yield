//! Pure browser multi-selection state.
//!
//! Selection membership is stored by [`AudioFileId`] rather than by a visible-row index. The
//! caller supplies the current visible identity order when an operation needs a range, so sorting
//! and filtering can never silently retarget an existing selection.

use std::collections::HashSet;

use crate::AudioFileId;

/// Session-only selection state for the sample browser.
///
/// The selected set is the batch-operation target. The anchor is the identity from which the
/// next range is extended and is deliberately independent from the caller's focused sample. A
/// focused sample may move through the browser while the selected set remains unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowserSelection {
    selected: HashSet<AudioFileId>,
    anchor: Option<AudioFileId>,
}

impl BrowserSelection {
    /// Create an empty browser selection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the selected identities.
    pub fn selected(&self) -> &HashSet<AudioFileId> {
        &self.selected
    }

    /// Return the identity used as the next range anchor, if one exists.
    pub fn anchor(&self) -> Option<&AudioFileId> {
        self.anchor.as_ref()
    }

    /// Return the number of explicitly selected identities.
    pub fn len(&self) -> usize {
        self.selected.len()
    }

    /// Return whether no identities are explicitly selected.
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    /// Return whether an identity belongs to the explicit selection set.
    pub fn contains(&self, id: &AudioFileId) -> bool {
        self.selected.contains(id)
    }

    /// Clear explicit membership and make one pointer-target identity the anchor.
    ///
    /// This is the plain pointer behavior. A focused row with an empty explicit set is the
    /// browser's effective single target; the caller owns that focus fallback. This operation
    /// does not make any audio or scrolling decision.
    pub fn focus_pointer(&mut self, id: &AudioFileId) {
        self.selected.clear();
        self.anchor = Some(id.clone());
    }

    /// Move the range anchor to the focused identity while preserving membership.
    ///
    /// Plain arrow navigation uses this operation. Focus itself remains owned by the caller, so
    /// changing the anchor does not select or deselect the focused row.
    pub fn move_anchor_to(&mut self, focused_id: &AudioFileId) {
        self.anchor = Some(focused_id.clone());
    }

    /// Extend a range to `target_id`, replacing or adding to the explicit selection.
    ///
    /// `visible_ids` must be an in-order, clonable iterator over the current browser display
    /// identities. The range is inclusive and works in either direction. The existing anchor is
    /// preferred; when it is absent or no longer visible, `focused_id` seeds the range, falling
    /// back to the target itself. The anchor remains at the range origin after the operation.
    ///
    /// Returns `false` when `target_id` is not visible and no selection changes are made.
    pub fn extend_range<'a, I>(
        &mut self,
        visible_ids: I,
        focused_id: Option<&AudioFileId>,
        target_id: &AudioFileId,
        additive: bool,
    ) -> bool
    where
        I: IntoIterator<Item = &'a AudioFileId> + Clone,
    {
        let Some(target_position) = visible_ids
            .clone()
            .into_iter()
            .position(|id| id == target_id)
        else {
            return false;
        };

        let anchor_id = self
            .anchor
            .as_ref()
            .filter(|id| {
                visible_ids
                    .clone()
                    .into_iter()
                    .any(|visible_id| visible_id == *id)
            })
            .or_else(|| {
                focused_id.filter(|id| {
                    visible_ids
                        .clone()
                        .into_iter()
                        .any(|visible_id| visible_id == *id)
                })
            })
            .unwrap_or(target_id);
        let anchor_position = visible_ids
            .clone()
            .into_iter()
            .position(|id| id == anchor_id)
            .unwrap_or(target_position);
        let start = anchor_position.min(target_position);
        let end = anchor_position.max(target_position);

        if !additive {
            self.selected.clear();
        }
        self.selected.extend(
            visible_ids
                .into_iter()
                .enumerate()
                .filter(|(position, _)| (*position >= start) && (*position <= end))
                .map(|(_, id)| id.clone()),
        );
        self.anchor = Some(anchor_id.clone());
        true
    }

    /// Toggle `target_id` in the explicit selection set.
    ///
    /// When membership is empty and the target differs from the focused identity, the focused
    /// identity is seeded into the set first. This preserves the row from which a Cmd/Ctrl click
    /// began while still toggling the clicked target. The existing anchor is preferred as that
    /// seed; when no anchor exists, `focused_id` is used, and finally the target itself.
    pub fn toggle(&mut self, target_id: &AudioFileId, focused_id: Option<&AudioFileId>) {
        let anchor_id = self.anchor.as_ref().or(focused_id).unwrap_or(target_id);
        if self.selected.is_empty() && anchor_id != target_id {
            self.selected.insert(anchor_id.clone());
        }
        self.anchor = Some(anchor_id.clone());
        if !self.selected.insert(target_id.clone()) {
            self.selected.remove(target_id);
        }
    }

    /// Select every identity in the current visible projection.
    ///
    /// The existing anchor is preserved, as is the caller's focused identity. An empty visible
    /// projection simply leaves the explicit set empty.
    pub fn select_all(&mut self, visible_ids: &[AudioFileId]) {
        self.selected = visible_ids.iter().cloned().collect();
    }

    /// Clear explicit membership and the range anchor.
    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    /// Reconcile membership and the anchor against the current visible identity projection.
    ///
    /// Identities hidden by a query or folder filter, removed by a rescan, or otherwise absent
    /// from `visible_ids` are dropped. Sorting only changes the order supplied to this method and
    /// therefore preserves identity membership. Returns whether state changed.
    pub fn reconcile(&mut self, visible_ids: &[AudioFileId]) -> bool {
        let visible = visible_ids.iter().collect::<HashSet<_>>();
        let previous_len = self.selected.len();
        self.selected.retain(|id| visible.contains(id));
        let previous_anchor = self.anchor.clone();
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !visible.contains(anchor))
        {
            self.anchor = None;
        }
        previous_len != self.selected.len() || previous_anchor != self.anchor
    }

    /// Return selected identities in the supplied visible order.
    ///
    /// This is useful for deterministic batch commands while keeping the underlying membership
    /// representation as a set.
    pub fn selected_in_order(&self, visible_ids: &[AudioFileId]) -> Vec<AudioFileId> {
        visible_ids
            .iter()
            .filter(|id| self.selected.contains(*id))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioFile;
    use std::path::PathBuf;

    fn id(name: &str) -> AudioFileId {
        AudioFile {
            path: PathBuf::from("/source").join(name),
            relative_path: PathBuf::from(name),
            byte_len: 0,
            duration_seconds: None,
            sample_rate: None,
            curation: None,
        }
        .stable_id()
    }

    fn rows(names: &[&str]) -> Vec<AudioFileId> {
        names.iter().map(|name| id(name)).collect()
    }

    #[test]
    fn plain_pointer_clears_membership_and_sets_anchor() {
        let visible = rows(&["a.wav", "b.wav", "c.wav"]);
        let mut selection = BrowserSelection::new();
        selection.toggle(&visible[0], Some(&visible[0]));
        selection.toggle(&visible[1], Some(&visible[0]));

        selection.focus_pointer(&visible[2]);

        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), Some(&visible[2]));
    }

    #[test]
    fn plain_pointer_then_arrow_keeps_batch_membership_empty() {
        let visible = rows(&["a.wav", "b.wav", "c.wav"]);
        let mut selection = BrowserSelection::new();
        selection.toggle(&visible[0], Some(&visible[0]));
        selection.toggle(&visible[1], Some(&visible[0]));
        selection.focus_pointer(&visible[0]);
        let before = selection.selected().clone();

        selection.move_anchor_to(&visible[2]);

        assert_eq!(selection.selected(), &before);
        assert_eq!(selection.anchor(), Some(&visible[2]));
    }

    #[test]
    fn reverse_range_replaces_selection_inclusively() {
        let visible = rows(&["a.wav", "b.wav", "c.wav", "d.wav"]);
        let mut selection = BrowserSelection::new();
        selection.focus_pointer(&visible[3]);

        assert!(selection.extend_range(&visible, None, &visible[1], false));

        assert_eq!(
            selection.selected_in_order(&visible),
            vec![visible[1].clone(), visible[2].clone(), visible[3].clone()]
        );
        assert_eq!(selection.anchor(), Some(&visible[3]));
    }

    #[test]
    fn additive_reverse_range_keeps_existing_membership() {
        let visible = rows(&["a.wav", "b.wav", "c.wav", "d.wav", "e.wav"]);
        let mut selection = BrowserSelection::new();
        selection.toggle(&visible[0], Some(&visible[0]));
        selection.move_anchor_to(&visible[4]);
        selection.toggle(&visible[1], Some(&visible[4]));

        assert!(selection.extend_range(&visible, None, &visible[2], true));

        assert_eq!(
            selection.selected_in_order(&visible),
            vec![
                visible[0].clone(),
                visible[1].clone(),
                visible[2].clone(),
                visible[3].clone(),
                visible[4].clone()
            ]
        );
        assert_eq!(selection.anchor(), Some(&visible[4]));
    }

    #[test]
    fn toggle_seeds_focused_anchor_when_membership_is_empty() {
        let visible = rows(&["a.wav", "b.wav", "c.wav"]);
        let mut selection = BrowserSelection::new();

        selection.toggle(&visible[2], Some(&visible[0]));

        assert_eq!(
            selection.selected_in_order(&visible),
            vec![visible[0].clone(), visible[2].clone()]
        );
        assert_eq!(selection.anchor(), Some(&visible[0]));
    }

    #[test]
    fn toggle_same_target_selects_target_while_setting_anchor() {
        let visible = rows(&["a.wav", "b.wav"]);
        let mut selection = BrowserSelection::new();
        selection.toggle(&visible[0], Some(&visible[0]));

        assert_eq!(
            selection.selected_in_order(&visible),
            vec![visible[0].clone()]
        );
        assert_eq!(selection.anchor(), Some(&visible[0]));
    }

    #[test]
    fn select_all_keeps_anchor_and_uses_current_sorted_order() {
        let original = rows(&["a.wav", "b.wav", "c.wav"]);
        let sorted = rows(&["c.wav", "b.wav", "a.wav"]);
        let mut selection = BrowserSelection::new();
        selection.move_anchor_to(&original[1]);

        selection.select_all(&sorted);

        assert_eq!(selection.anchor(), Some(&original[1]));
        assert_eq!(selection.selected_in_order(&sorted), sorted);
    }

    #[test]
    fn reconcile_preserves_ids_across_sort_and_drops_filtered_or_missing_rows() {
        let original = rows(&["a.wav", "b.wav", "c.wav"]);
        let mut selection = BrowserSelection::new();
        selection.focus_pointer(&original[0]);
        selection.toggle(&original[2], Some(&original[0]));

        let sorted = rows(&["c.wav", "a.wav", "b.wav"]);
        assert!(!selection.reconcile(&sorted));
        assert_eq!(
            selection.selected_in_order(&sorted),
            vec![original[2].clone(), original[0].clone()]
        );
        assert_eq!(selection.anchor(), Some(&original[0]));

        let filtered = rows(&["c.wav", "b.wav"]);
        assert!(selection.reconcile(&filtered));
        assert_eq!(
            selection.selected_in_order(&filtered),
            vec![original[2].clone()]
        );
        assert_eq!(selection.anchor(), None);
    }

    #[test]
    fn empty_projection_clears_membership_and_invalidates_anchor() {
        let visible = rows(&["a.wav"]);
        let mut selection = BrowserSelection::new();
        selection.focus_pointer(&visible[0]);

        assert!(selection.reconcile(&[]));
        assert!(selection.is_empty());
        assert_eq!(selection.anchor(), None);
        assert!(!selection.extend_range(&[], None, &visible[0], false));
    }
}
