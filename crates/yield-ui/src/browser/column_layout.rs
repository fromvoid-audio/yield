//! Session-scoped presentation layout for the sample browser table.

use yield_core::SortColumn;
use yield_design::Theme;

use super::view::BrowserColumnVisibility;

const MIN_WIDTH: f32 = 48.0;
const MAX_WIDTH: f32 = 420.0;
const MIN_NAME_WIDTH: f32 = 192.0;
pub(super) const DRAG_THRESHOLD: f32 = 5.0;

#[derive(Clone, Copy, Debug)]
pub(super) enum ColumnGesture {
    Reorder {
        source: BrowserColumn,
        start_x: f32,
        insertion: Option<usize>,
    },
    Resize {
        source: BrowserColumn,
        start_x: f32,
        start_width: f32,
        previous_override: Option<f32>,
        previous_name_override: Option<f32>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrowserColumn {
    Name,
    Rating,
    PlaybackType,
    Collection,
    Extension,
    Size,
    Duration,
    LastPlayed,
}

impl BrowserColumn {
    pub(super) const DEFAULT_ORDER: [Self; 8] = [
        Self::Name,
        Self::Rating,
        Self::PlaybackType,
        Self::Collection,
        Self::Extension,
        Self::Size,
        Self::Duration,
        Self::LastPlayed,
    ];

    const fn index(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Rating => 1,
            Self::PlaybackType => 2,
            Self::Collection => 3,
            Self::Extension => 4,
            Self::Size => 5,
            Self::Duration => 6,
            Self::LastPlayed => 7,
        }
    }

    pub(super) const fn sort_column(self) -> Option<SortColumn> {
        match self {
            Self::Name => Some(SortColumn::Name),
            Self::Rating => Some(SortColumn::Rating),
            Self::PlaybackType => Some(SortColumn::PlaybackType),
            Self::Collection => Some(SortColumn::Collection),
            Self::Extension => Some(SortColumn::Extension),
            Self::Size => Some(SortColumn::Size),
            Self::Duration => None,
            Self::LastPlayed => Some(SortColumn::LastPlayed),
        }
    }

    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Rating => "Rating",
            Self::PlaybackType => "Type",
            Self::Collection => "Col",
            Self::Extension => "Ext",
            Self::Size => "Size",
            Self::Duration => "Duration",
            Self::LastPlayed => "Last Played",
        }
    }

    fn default_width(self, theme: &Theme) -> f32 {
        match self {
            Self::Name => MIN_NAME_WIDTH,
            Self::Rating => theme.spacing.rating_column_width,
            Self::PlaybackType => theme.spacing.playback_type_column_width,
            Self::Collection => theme.spacing.collection_column_width,
            Self::Extension => theme.spacing.extension_column_width,
            Self::Size => theme.spacing.size_column_width,
            Self::Duration => theme.spacing.duration_column_width,
            Self::LastPlayed => theme.spacing.last_played_column_width,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ResolvedColumn {
    pub(super) id: BrowserColumn,
    pub(super) width: f32,
}

#[derive(Clone, Debug)]
pub(super) struct ColumnLayout {
    order: [BrowserColumn; 8],
    widths: [Option<f32>; 8],
}

impl Default for ColumnLayout {
    fn default() -> Self {
        Self {
            order: BrowserColumn::DEFAULT_ORDER,
            widths: [None; 8],
        }
    }
}

impl ColumnLayout {
    pub(super) fn visible_order(&self, visibility: BrowserColumnVisibility) -> Vec<BrowserColumn> {
        self.order
            .iter()
            .copied()
            .filter(|column| match column {
                BrowserColumn::PlaybackType => visibility.playback_type,
                BrowserColumn::LastPlayed => visibility.last_played,
                _ => true,
            })
            .collect()
    }

    pub(super) fn width(&self, column: BrowserColumn, theme: &Theme) -> f32 {
        self.widths[column.index()].unwrap_or_else(|| column.default_width(theme))
    }

    pub(super) fn width_override(&self, column: BrowserColumn) -> Option<f32> {
        self.widths[column.index()]
    }

    pub(super) fn restore_width_override(&mut self, column: BrowserColumn, value: Option<f32>) {
        self.widths[column.index()] = value;
    }

    pub(super) fn freeze_name_width(&mut self, rendered_width: f32) {
        if self.widths[BrowserColumn::Name.index()].is_none() {
            self.widths[BrowserColumn::Name.index()] = Some(rendered_width);
        }
    }

    pub(super) fn resolve(
        &self,
        theme: &Theme,
        visibility: BrowserColumnVisibility,
        available_width: f32,
    ) -> Vec<ResolvedColumn> {
        let order = self.visible_order(visibility);
        let fixed = order
            .iter()
            .filter(|column| **column != BrowserColumn::Name)
            .map(|column| self.width(*column, theme))
            .sum::<f32>();
        let name_width = self.widths[BrowserColumn::Name.index()]
            .unwrap_or_else(|| (available_width - fixed).max(MIN_NAME_WIDTH));
        order
            .into_iter()
            .map(|id| ResolvedColumn {
                id,
                width: if id == BrowserColumn::Name {
                    name_width
                } else {
                    self.width(id, theme)
                },
            })
            .collect()
    }

    pub(super) fn set_width(&mut self, column: BrowserColumn, width: f32) -> bool {
        if !width.is_finite() {
            return false;
        }
        let minimum = if column == BrowserColumn::Name {
            MIN_NAME_WIDTH
        } else {
            MIN_WIDTH
        };
        let width = width.clamp(minimum, MAX_WIDTH);
        let current = self.widths[column.index()];
        self.widths[column.index()] = Some(width);
        current != Some(width)
    }

    pub(super) fn reorder_visible(
        &mut self,
        source: BrowserColumn,
        insertion: usize,
        visibility: BrowserColumnVisibility,
    ) -> bool {
        let visible = self.visible_order(visibility);
        if !visible.contains(&source) || insertion > visible.len() - 1 {
            return false;
        }
        if visible.iter().position(|column| *column == source) == Some(insertion) {
            return false;
        }
        let mut without = visible;
        without.retain(|column| *column != source);
        let mut next = self.order.to_vec();
        next.retain(|column| *column != source);
        let insert_at = without
            .get(insertion)
            .and_then(|target| next.iter().position(|column| column == target))
            .unwrap_or_else(|| {
                without
                    .last()
                    .and_then(|target| next.iter().position(|column| column == target))
                    .map_or(0, |index| index + 1)
            });
        next.insert(insert_at, source);
        let next: [BrowserColumn; 8] = next.try_into().expect("one column reinserted");
        if next == self.order {
            return false;
        }
        self.order = next;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_columns_keep_their_relative_order_during_visible_reorder() {
        let mut layout = ColumnLayout::default();
        let narrow = BrowserColumnVisibility::default();
        assert!(!layout.reorder_visible(BrowserColumn::Rating, 1, narrow));
        assert_eq!(layout.order, BrowserColumn::DEFAULT_ORDER);
        assert!(layout.reorder_visible(BrowserColumn::Rating, 4, narrow));
        assert_eq!(
            layout.visible_order(narrow),
            vec![
                BrowserColumn::Name,
                BrowserColumn::Collection,
                BrowserColumn::Extension,
                BrowserColumn::Size,
                BrowserColumn::Rating,
                BrowserColumn::Duration,
            ]
        );
        assert_eq!(
            layout.visible_order(BrowserColumnVisibility {
                playback_type: true,
                last_played: true,
            }),
            vec![
                BrowserColumn::Name,
                BrowserColumn::PlaybackType,
                BrowserColumn::Collection,
                BrowserColumn::Extension,
                BrowserColumn::Size,
                BrowserColumn::Rating,
                BrowserColumn::Duration,
                BrowserColumn::LastPlayed,
            ]
        );
    }

    #[test]
    fn width_override_rejects_non_finite_and_clamps_to_reference_bounds() {
        let mut layout = ColumnLayout::default();
        assert!(!layout.set_width(BrowserColumn::Size, f32::NAN));
        assert!(layout.set_width(BrowserColumn::Size, -100.0));
        assert_eq!(layout.widths[BrowserColumn::Size.index()], Some(48.0));
        assert!(layout.set_width(BrowserColumn::Size, 900.0));
        assert_eq!(layout.widths[BrowserColumn::Size.index()], Some(420.0));
    }
}
