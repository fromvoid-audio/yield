//! Pure folder-tree and folder-scope projections for the browser.
//!
//! The tree is built from scan output and never touches the filesystem. Paths are compared by
//! [`Path`] components so a folder named `kit` cannot accidentally match a sibling named
//! `kitchen`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use crate::AudioFile;

/// Which files belong to the currently selected folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FolderScope {
    /// Include files whose parent is exactly the selected folder.
    #[default]
    DirectOnly,
    /// Include files in the selected folder and every descendant folder.
    AllDescendants,
}

impl FolderScope {
    /// Return the compact label used by the browser control.
    pub const fn label(self) -> &'static str {
        match self {
            Self::DirectOnly => "Direct only",
            Self::AllDescendants => "All descendants",
        }
    }

    /// Toggle between direct-only and recursive folder listing.
    pub const fn toggled(self) -> Self {
        match self {
            Self::DirectOnly => Self::AllDescendants,
            Self::AllDescendants => Self::DirectOnly,
        }
    }
}

/// A pure folder row ready for a sidebar renderer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderTreeRow {
    /// Relative folder path, with `.` representing the source root.
    pub path: PathBuf,
    /// Display depth below the source root.
    pub depth: usize,
    /// Whether the folder has any direct or descendant children.
    pub has_children: bool,
    /// Whether the row is currently expanded in the supplied projection.
    pub expanded: bool,
    /// Whether an audio file exists in this folder or a descendant folder.
    pub contains_audio: bool,
    /// Number of audio files in this folder or its descendants.
    pub audio_file_count: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct FolderNode {
    children: BTreeSet<PathBuf>,
    contains_audio: bool,
    audio_file_count: usize,
}

/// Pure source folder tree built from relative directory and audio-file paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FolderTree {
    nodes: BTreeMap<PathBuf, FolderNode>,
}

impl FolderTree {
    /// Build a tree from relative directories and audio files.
    ///
    /// `directories` should contain every directory discovered during the scan, including empty
    /// directories. File paths are used to mark audio-containing folders and their ancestors. Any
    /// missing ancestors are created as a defensive measure so callers can provide only files.
    pub fn from_scan(directories: &[PathBuf], files: &[AudioFile]) -> Self {
        let mut tree = Self::default();
        tree.ensure_path(Path::new("."));
        for directory in directories {
            tree.ensure_path(directory);
        }
        for file in files {
            let Some(parent) = file.relative_path.parent() else {
                continue;
            };
            let parent = normalize_relative_path(parent);
            tree.ensure_path(&parent);
            let mut current = parent.as_path();
            loop {
                if let Some(node) = tree.nodes.get_mut(current) {
                    node.contains_audio = true;
                    node.audio_file_count = node.audio_file_count.saturating_add(1);
                }
                if current == Path::new(".") {
                    break;
                }
                current = current.parent().unwrap_or_else(|| Path::new("."));
            }
        }
        tree
    }

    /// Return true when the tree contains this relative directory.
    pub fn contains(&self, path: &Path) -> bool {
        self.nodes.contains_key(&normalize_relative_path(path))
    }

    /// Return the root-relative folder paths in deterministic order.
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.nodes.keys()
    }

    /// Return a flattened visible tree projection for the supplied expanded set.
    pub fn rows(&self, expanded: &BTreeSet<PathBuf>) -> Vec<FolderTreeRow> {
        let mut expanded = expanded.clone();
        expanded.insert(PathBuf::from("."));
        self.rows_with_expanded(&expanded)
    }

    fn rows_with_expanded(&self, expanded: &BTreeSet<PathBuf>) -> Vec<FolderTreeRow> {
        let mut rows = Vec::new();
        let root = PathBuf::from(".");
        self.append_rows(&root, 0, expanded, &mut rows);
        rows
    }

    fn append_rows(
        &self,
        path: &Path,
        depth: usize,
        expanded: &BTreeSet<PathBuf>,
        rows: &mut Vec<FolderTreeRow>,
    ) {
        let Some(node) = self.nodes.get(path) else {
            return;
        };
        let normalized = normalize_relative_path(path);
        let is_expanded = expanded.contains(&normalized);
        rows.push(FolderTreeRow {
            path: normalized.clone(),
            depth,
            has_children: !node.children.is_empty(),
            expanded: is_expanded,
            contains_audio: node.contains_audio,
            audio_file_count: node.audio_file_count,
        });
        if !is_expanded {
            return;
        }
        let mut children = node.children.iter().collect::<Vec<_>>();
        children.sort_by(|left, right| {
            folder_sort_key(left)
                .cmp(&folder_sort_key(right))
                .then_with(|| left.cmp(right))
        });
        for child in children {
            self.append_rows(child, depth.saturating_add(1), expanded, rows);
        }
    }

    /// Reconcile selected and expanded state after a scan refresh.
    ///
    /// Missing selected folders walk toward the nearest existing ancestor. Expanded entries that
    /// no longer exist are removed, while the root remains expanded to keep the tree usable.
    pub fn reconcile_state(
        &self,
        selected: Option<&Path>,
        expanded: &BTreeSet<PathBuf>,
    ) -> FolderTreeState {
        let selected = selected
            .map(normalize_relative_path)
            .and_then(|path| self.nearest_existing_ancestor(&path))
            .unwrap_or_else(|| PathBuf::from("."));
        let mut expanded = expanded
            .iter()
            .map(|path| normalize_relative_path(path))
            .filter(|path| self.nodes.contains_key(path))
            .collect::<BTreeSet<_>>();
        expanded.insert(PathBuf::from("."));
        let mut ancestor = selected.as_path();
        while ancestor != Path::new(".") {
            ancestor = ancestor.parent().unwrap_or_else(|| Path::new("."));
            expanded.insert(normalize_relative_path(ancestor));
        }
        FolderTreeState { selected, expanded }
    }

    fn nearest_existing_ancestor(&self, path: &Path) -> Option<PathBuf> {
        let mut candidate = path;
        loop {
            let normalized = normalize_relative_path(candidate);
            if self.nodes.contains_key(&normalized) {
                return Some(normalized);
            }
            if candidate == Path::new(".") {
                return None;
            }
            candidate = candidate.parent().unwrap_or_else(|| Path::new("."));
        }
    }

    fn ensure_path(&mut self, path: &Path) {
        let path = normalize_relative_path(path);
        if self.nodes.contains_key(&path) {
            return;
        }
        if path != Path::new(".") {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            let parent = normalize_relative_path(parent);
            self.ensure_path(&parent);
            if let Some(parent_node) = self.nodes.get_mut(&parent) {
                parent_node.children.insert(path.clone());
            }
        }
        self.nodes.insert(path, FolderNode::default());
    }
}

/// Per-source folder selection and expansion state retained by the UI session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderTreeState {
    /// Currently selected relative folder.
    pub selected: PathBuf,
    /// Expanded relative folders.
    pub expanded: BTreeSet<PathBuf>,
}

/// Return whether an audio file's relative path belongs to a selected folder.
pub fn folder_contains_relative_path(
    folder: &Path,
    relative_file: &Path,
    scope: FolderScope,
) -> bool {
    let folder = normalize_relative_path(folder);
    let Some(parent) = relative_file.parent() else {
        return folder == Path::new(".");
    };
    let parent = normalize_relative_path(parent);
    match scope {
        FolderScope::DirectOnly => parent == folder,
        FolderScope::AllDescendants => {
            folder == Path::new(".") || parent == folder || parent.strip_prefix(&folder).is_ok()
        }
    }
}

fn normalize_relative_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                normalized.pop();
            }
            Component::RootDir | Component::Prefix(_) => return PathBuf::from("."),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn folder_sort_key(path: &Path) -> String {
    path.file_name()
        .unwrap_or_else(|| path.as_os_str())
        .to_string_lossy()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(relative_path: &str) -> AudioFile {
        AudioFile {
            path: PathBuf::from("/source").join(relative_path),
            relative_path: PathBuf::from(relative_path),
            byte_len: 1,
            duration_seconds: None,
            sample_rate: None,
            playback_type: None,
            curation: None,
        }
    }

    #[test]
    fn tree_contains_empty_folders_and_marks_audio_ancestors() {
        let tree = FolderTree::from_scan(
            &[
                PathBuf::from("."),
                PathBuf::from("drums"),
                PathBuf::from("drums/kicks"),
                PathBuf::from("empty"),
            ],
            &[file("drums/kicks/one.wav")],
        );
        let rows = tree
            .rows(&BTreeSet::from([PathBuf::from(".")]))
            .into_iter()
            .map(|row| (row.path, row.contains_audio))
            .collect::<Vec<_>>();
        assert_eq!(rows[0], (PathBuf::from("."), true));
        assert_eq!(rows[1], (PathBuf::from("drums"), true));
        assert_eq!(rows[2], (PathBuf::from("empty"), false));
    }

    #[test]
    fn rows_follow_expansion_and_path_component_order() {
        let tree = FolderTree::from_scan(
            &[
                PathBuf::from("."),
                PathBuf::from("kit"),
                PathBuf::from("kitchen"),
            ],
            &[],
        );
        let rows = tree
            .rows(&BTreeSet::from([PathBuf::from(".")]))
            .into_iter()
            .map(|row| row.path)
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                PathBuf::from("."),
                PathBuf::from("kit"),
                PathBuf::from("kitchen")
            ]
        );
        assert!(!folder_contains_relative_path(
            Path::new("kit"),
            Path::new("kitchen/one.wav"),
            FolderScope::AllDescendants
        ));
    }

    #[test]
    fn reconciliation_falls_back_to_nearest_existing_ancestor() {
        let tree = FolderTree::from_scan(&[PathBuf::from("."), PathBuf::from("drums")], &[]);
        let state = tree.reconcile_state(
            Some(Path::new("drums/kicks")),
            &BTreeSet::from([PathBuf::from("."), PathBuf::from("drums/kicks")]),
        );
        assert_eq!(state.selected, PathBuf::from("drums"));
        assert_eq!(state.expanded, BTreeSet::from([PathBuf::from(".")]));
    }

    #[test]
    fn reconciliation_expands_ancestors_of_a_restored_selection() {
        let tree = FolderTree::from_scan(
            &[
                PathBuf::from("."),
                PathBuf::from("drums"),
                PathBuf::from("drums/kicks"),
            ],
            &[],
        );
        let state = tree.reconcile_state(
            Some(Path::new("drums/kicks")),
            &BTreeSet::from([PathBuf::from(".")]),
        );
        assert_eq!(state.selected, PathBuf::from("drums/kicks"));
        assert_eq!(
            state.expanded,
            BTreeSet::from([PathBuf::from("."), PathBuf::from("drums")])
        );
    }

    #[test]
    fn folder_scope_distinguishes_direct_and_descendant_files() {
        assert!(folder_contains_relative_path(
            Path::new("."),
            Path::new("one.wav"),
            FolderScope::DirectOnly
        ));
        assert!(!folder_contains_relative_path(
            Path::new("."),
            Path::new("nested/one.wav"),
            FolderScope::DirectOnly
        ));
        assert!(folder_contains_relative_path(
            Path::new("."),
            Path::new("nested/one.wav"),
            FolderScope::AllDescendants
        ));
    }
}
