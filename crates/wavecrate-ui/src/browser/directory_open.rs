#![deny(missing_docs)]
#![deny(warnings)]

//! Bounded, revision-fenced opening of registered source and folder directories.
//!
//! Context-menu capture only retains source/folder identities. Filesystem validation and root
//! identity checks run on the background executor before the UI asks GPUI to launch the system
//! file manager. The launch request itself has no completion signal and remains subject to the
//! operating system's later path-open TOCTOU window.

use std::fs;
use std::path::{Component, Path, PathBuf};

use gpui::{Context, MouseDownEvent, Pixels, Point, Window};
use wavecrate_library::sample_sources::{SourceId, reconciliation::RootIdentity};
use wavecrate_session::resolve_source_root;

use super::Browser;

/// The kind of registered directory represented by a context-menu target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DirectoryOpenKind {
    /// A registered source root.
    Source,
    /// A folder in the active source's folder projection.
    Folder,
}

/// A bounded identity and projection fence for one source/folder open request.
#[derive(Clone, Debug)]
pub(super) struct DirectoryOpenTarget {
    pub(super) kind: DirectoryOpenKind,
    pub(super) source_index: usize,
    pub(super) source_id: SourceId,
    pub(super) root: PathBuf,
    pub(super) root_identity: Option<RootIdentity>,
    pub(super) folder: Option<PathBuf>,
    pub(super) scan_generation: u64,
    pub(super) handoff_generation: u64,
    pub(super) anchor: Point<Pixels>,
}

/// Browser-owned bounded lifecycle for one directory-open preflight.
pub(super) struct DirectoryOpenState {
    next_request_id: u64,
    epoch: u64,
    running: Option<RunningDirectoryOpen>,
    queued: Option<DirectoryOpenTarget>,
    task: Option<gpui::Task<()>>,
}

impl Default for DirectoryOpenState {
    fn default() -> Self {
        Self {
            next_request_id: 0,
            epoch: 0,
            running: None,
            queued: None,
            task: None,
        }
    }
}

struct RunningDirectoryOpen {
    request_id: u64,
    epoch: u64,
    target: DirectoryOpenTarget,
    cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl DirectoryOpenState {
    fn queue_latest(&mut self, target: DirectoryOpenTarget) {
        if let Some(running) = self.running.as_ref() {
            running
                .cancellation
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.queued = Some(target);
    }

    fn cancel(&mut self) {
        if let Some(running) = self.running.as_ref() {
            running
                .cancellation
                .store(true, std::sync::atomic::Ordering::Release);
        }
        self.epoch = self.epoch.wrapping_add(1);
        self.queued = None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DirectoryOpenError {
    Canceled,
    RootChanged,
    MissingDirectory,
    NonDirectory,
    OutsideRoot,
    InvalidRelativePath,
}

impl std::fmt::Display for DirectoryOpenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Canceled => formatter.write_str("directory open canceled"),
            Self::RootChanged => formatter.write_str("source changed while preparing the open"),
            Self::MissingDirectory => formatter.write_str("the directory is no longer available"),
            Self::NonDirectory => formatter.write_str("the target is no longer a directory"),
            Self::OutsideRoot => formatter.write_str("the folder resolves outside its source"),
            Self::InvalidRelativePath => formatter.write_str("the folder path is invalid"),
        }
    }
}

impl Browser {
    /// Capture a registered source row without changing the active projection.
    pub(super) fn open_source_context_menu(
        &mut self,
        index: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != gpui::MouseButton::Right {
            return;
        }
        let Some(target) = capture_source_target(self, index, event.position) else {
            self.playback_status = String::from("Source is not ready to open");
            cx.notify();
            return;
        };
        Browser::open_directory_context_menu(self, target, window, cx);
    }

    /// Capture an active folder row without changing folder selection or playback.
    pub(super) fn open_folder_context_menu(
        &mut self,
        folder: PathBuf,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != gpui::MouseButton::Right {
            return;
        }
        let Some(target) = capture_folder_target(self, folder, event.position) else {
            self.playback_status = String::from("Folder is not ready to open");
            cx.notify();
            return;
        };
        Browser::open_directory_context_menu(self, target, window, cx);
    }

    /// Start one background directory preflight and issue a best-effort system-open request.
    pub(super) fn start_directory_open(
        &mut self,
        target: DirectoryOpenTarget,
        cx: &mut Context<Self>,
    ) {
        if !directory_target_is_current(self, &target) {
            self.playback_status = String::from("Open canceled: the directory changed");
            cx.notify();
            return;
        }
        if self.directory_open.running.is_some() {
            self.directory_open.queue_latest(target);
            self.playback_status = String::from("Preparing the latest directory open…");
            cx.notify();
            return;
        }
        self.start_directory_open_preparation(target, cx);
    }

    fn start_directory_open_preparation(
        &mut self,
        target: DirectoryOpenTarget,
        cx: &mut Context<Self>,
    ) {
        self.directory_open.next_request_id = self.directory_open.next_request_id.wrapping_add(1);
        let request_id = self.directory_open.next_request_id;
        let epoch = self.directory_open.epoch;
        let cancellation = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.directory_open.running = Some(RunningDirectoryOpen {
            request_id,
            epoch,
            target: target.clone(),
            cancellation: std::sync::Arc::clone(&cancellation),
        });
        let preparation_target = target.clone();
        self.playback_status = format!(
            "Preparing {} open…",
            match target.kind {
                DirectoryOpenKind::Source => "source",
                DirectoryOpenKind::Folder => "folder",
            }
        );
        self.directory_open.task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { prepare_directory_open(preparation_target, cancellation) })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.finish_directory_open_preparation(request_id, epoch, result, cx);
            });
        }));
        cx.notify();
    }

    fn finish_directory_open_preparation(
        &mut self,
        request_id: u64,
        epoch: u64,
        result: Result<PathBuf, DirectoryOpenError>,
        cx: &mut Context<Self>,
    ) {
        let Some(running) = self.directory_open.running.take() else {
            return;
        };
        if running.request_id != request_id || running.epoch != epoch {
            self.directory_open.running = Some(running);
            return;
        }
        self.directory_open.task.take();
        let canceled = running
            .cancellation
            .load(std::sync::atomic::Ordering::Acquire);
        if let Some(queued) = self.directory_open.queued.take() {
            // A queued request is always newer than this running request. It may have been
            // submitted after a handoff invalidated the old epoch, so start it against the
            // current epoch once the bounded old preflight has drained.
            if directory_target_is_current(self, &queued) {
                self.start_directory_open_preparation(queued, cx);
            } else {
                self.playback_status = String::from("Open canceled: the directory changed");
                cx.notify();
            }
            return;
        }
        if canceled || self.directory_open.epoch != epoch {
            return;
        }
        let target = running.target;
        if !directory_target_is_current(self, &target) {
            self.playback_status = String::from("Open canceled: the directory changed");
            cx.notify();
            return;
        }
        match result {
            Ok(path) => {
                cx.open_with_system(&path);
                self.playback_status = format!("Open requested: {}", path.display());
            }
            Err(error) => {
                self.playback_status = format!("Open unavailable: {error}");
            }
        }
        cx.notify();
    }

    /// Cancel any background directory-open preflight.
    pub(super) fn cancel_directory_open(&mut self) {
        self.directory_open.cancel();
    }

    /// Release a directory-open task during browser teardown.
    pub(super) fn shutdown_directory_open(&mut self) {
        self.directory_open.cancel();
        self.directory_open.running = None;
        self.directory_open.task.take();
    }
}

fn capture_source_target(
    browser: &Browser,
    source_index: usize,
    anchor: Point<Pixels>,
) -> Option<DirectoryOpenTarget> {
    let session = browser.source_sessions.get(source_index)?;
    let source_id = session.source_id.clone()?;
    if browser
        .source_registry_removed_roots
        .contains(&session.root)
    {
        return None;
    }
    let root = session.root.clone();
    Some(DirectoryOpenTarget {
        kind: DirectoryOpenKind::Source,
        source_index,
        source_id: source_id.clone(),
        root: root.clone(),
        root_identity: browser
            .catalog_activation
            .as_ref()
            .filter(|activation| activation.source_id == source_id && activation.root == root)
            .and_then(|activation| activation.root_identity.clone()),
        folder: None,
        scan_generation: browser.scan_generation,
        handoff_generation: browser.handoff_generation,
        anchor,
    })
}

fn capture_folder_target(
    browser: &Browser,
    folder: PathBuf,
    anchor: Point<Pixels>,
) -> Option<DirectoryOpenTarget> {
    if !is_safe_relative_folder(&folder) {
        return None;
    }
    let source_index = browser.active_source?;
    let session = browser.source_sessions.get(source_index)?;
    let source_id = session.source_id.clone()?;
    let root = browser.root.clone()?;
    if session.root != root
        || browser
            .source_registry_removed_roots
            .contains(&session.root)
        || !browser.folder_tree.as_ref()?.contains(&folder)
    {
        return None;
    }
    Some(DirectoryOpenTarget {
        kind: DirectoryOpenKind::Folder,
        source_index,
        source_id: source_id.clone(),
        root: root.clone(),
        root_identity: browser
            .catalog_activation
            .as_ref()
            .filter(|activation| activation.source_id == source_id && activation.root == root)
            .and_then(|activation| activation.root_identity.clone()),
        folder: Some(folder),
        scan_generation: browser.scan_generation,
        handoff_generation: browser.handoff_generation,
        anchor,
    })
}

pub(super) fn directory_target_is_current(browser: &Browser, target: &DirectoryOpenTarget) -> bool {
    let Some(session) = browser.source_sessions.get(target.source_index) else {
        return false;
    };
    if browser.source_registry_removed_roots.contains(&target.root)
        || session.source_id.as_ref() != Some(&target.source_id)
        || session.root != target.root
    {
        return false;
    }
    match target.kind {
        DirectoryOpenKind::Source => true,
        DirectoryOpenKind::Folder => folder_projection_fence_matches(
            target,
            browser.active_source,
            browser.root.as_deref(),
            browser.scan_generation,
            browser.handoff_generation,
            target.folder.as_ref().is_some_and(|folder| {
                browser
                    .folder_tree
                    .as_ref()
                    .is_some_and(|tree| tree.contains(folder))
            }),
        ),
    }
}

fn folder_projection_fence_matches(
    target: &DirectoryOpenTarget,
    active_source: Option<usize>,
    current_root: Option<&Path>,
    scan_generation: u64,
    handoff_generation: u64,
    folder_present: bool,
) -> bool {
    active_source == Some(target.source_index)
        && current_root == Some(target.root.as_path())
        && scan_generation == target.scan_generation
        && handoff_generation == target.handoff_generation
        && target.folder.is_some()
        && folder_present
}

fn prepare_directory_open(
    target: DirectoryOpenTarget,
    cancellation: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<PathBuf, DirectoryOpenError> {
    check_canceled(&cancellation)?;
    if target
        .folder
        .as_ref()
        .is_some_and(|folder| !is_safe_relative_folder(folder))
    {
        return Err(DirectoryOpenError::InvalidRelativePath);
    }
    let before = resolve_source_root(&target.root).map_err(|_| DirectoryOpenError::RootChanged)?;
    check_canceled(&cancellation)?;
    if before.canonical_root() != target.root
        || target
            .root_identity
            .as_ref()
            .is_some_and(|identity| identity != before.root_identity())
    {
        return Err(DirectoryOpenError::RootChanged);
    }
    let path = target
        .folder
        .as_ref()
        .map(|folder| target.root.join(folder))
        .unwrap_or_else(|| target.root.clone());
    check_canceled(&cancellation)?;
    let _ = validate_directory_path(&path, before.canonical_root(), &cancellation)?;
    check_canceled(&cancellation)?;
    let after = resolve_source_root(&target.root).map_err(|_| DirectoryOpenError::RootChanged)?;
    check_canceled(&cancellation)?;
    if after.canonical_root() != before.canonical_root()
        || after.root_identity() != before.root_identity()
        || after.canonical_root() != target.root
    {
        return Err(DirectoryOpenError::RootChanged);
    }
    let canonical = validate_directory_path(&path, after.canonical_root(), &cancellation)?;
    check_canceled(&cancellation)?;
    let final_root =
        resolve_source_root(&target.root).map_err(|_| DirectoryOpenError::RootChanged)?;
    check_canceled(&cancellation)?;
    if final_root.canonical_root() != before.canonical_root()
        || final_root.root_identity() != before.root_identity()
        || final_root.canonical_root() != target.root
    {
        return Err(DirectoryOpenError::RootChanged);
    }
    check_canceled(&cancellation)?;
    Ok(canonical)
}

fn validate_directory_path(
    path: &Path,
    root: &Path,
    cancellation: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<PathBuf, DirectoryOpenError> {
    check_canceled(cancellation)?;
    let metadata = fs::symlink_metadata(path).map_err(|_| DirectoryOpenError::MissingDirectory)?;
    check_canceled(cancellation)?;
    if !metadata.file_type().is_dir() {
        return Err(DirectoryOpenError::NonDirectory);
    }
    let canonical = fs::canonicalize(path).map_err(|_| DirectoryOpenError::MissingDirectory)?;
    check_canceled(cancellation)?;
    if !canonical.starts_with(root) {
        return Err(DirectoryOpenError::OutsideRoot);
    }
    Ok(canonical)
}

fn check_canceled(
    cancellation: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), DirectoryOpenError> {
    if cancellation.load(std::sync::atomic::Ordering::Acquire) {
        Err(DirectoryOpenError::Canceled)
    } else {
        Ok(())
    }
}

fn is_safe_relative_folder(path: &Path) -> bool {
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::CurDir | Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn cancellation() -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
    }

    fn target(root: &Path, folder: Option<PathBuf>) -> DirectoryOpenTarget {
        DirectoryOpenTarget {
            kind: if folder.is_some() {
                DirectoryOpenKind::Folder
            } else {
                DirectoryOpenKind::Source
            },
            source_index: 0,
            source_id: SourceId::from_string("source"),
            root: root.to_path_buf(),
            root_identity: None,
            folder,
            scan_generation: 1,
            handoff_generation: 1,
            anchor: Point::default(),
        }
    }

    #[test]
    fn relative_folder_validation_rejects_escape_components() {
        assert!(is_safe_relative_folder(Path::new(".")));
        assert!(is_safe_relative_folder(Path::new("nested/deeper")));
        assert!(!is_safe_relative_folder(Path::new("../outside")));
        assert!(!is_safe_relative_folder(Path::new("/absolute")));
    }

    #[test]
    fn preparation_accepts_source_and_nested_folder() {
        let directory = tempdir().unwrap();
        let nested = directory.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();

        assert_eq!(
            prepare_directory_open(target(&root, None), cancellation()).unwrap(),
            root
        );
        assert_eq!(
            prepare_directory_open(target(&root, Some(PathBuf::from("nested"))), cancellation())
                .unwrap(),
            fs::canonicalize(nested).unwrap()
        );
    }

    #[test]
    fn preparation_rejects_missing_or_non_directory_target() {
        let directory = tempdir().unwrap();
        let file = directory.path().join("file");
        fs::write(&file, b"file").unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();

        assert_eq!(
            prepare_directory_open(
                target(&root, Some(PathBuf::from("missing"))),
                cancellation()
            ),
            Err(DirectoryOpenError::MissingDirectory)
        );
        assert_eq!(
            prepare_directory_open(target(&root, Some(PathBuf::from("file"))), cancellation()),
            Err(DirectoryOpenError::NonDirectory)
        );
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_parent_symlink_escape() {
        let directory = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let linked = directory.path().join("linked");
        fs::create_dir_all(outside.path().join("folder")).unwrap();
        std::os::unix::fs::symlink(outside.path(), &linked).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();

        assert_eq!(
            prepare_directory_open(
                target(&root, Some(PathBuf::from("linked/folder"))),
                cancellation(),
            ),
            Err(DirectoryOpenError::OutsideRoot)
        );
    }

    #[cfg(unix)]
    #[test]
    fn preparation_rejects_final_directory_symlink() {
        let directory = tempdir().unwrap();
        let nested = directory.path().join("nested");
        let link = directory.path().join("link");
        fs::create_dir(&nested).unwrap();
        std::os::unix::fs::symlink(&nested, &link).unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();

        assert_eq!(
            prepare_directory_open(target(&root, Some(PathBuf::from("link"))), cancellation()),
            Err(DirectoryOpenError::NonDirectory)
        );
    }

    #[test]
    fn canceled_preparation_stops_before_filesystem_work() {
        let directory = tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        let cancellation = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));

        assert_eq!(
            prepare_directory_open(target(&root, None), cancellation),
            Err(DirectoryOpenError::Canceled)
        );
    }

    #[test]
    fn queued_directory_open_cancels_running_preparation() {
        let first_cancellation = cancellation();
        let first = target(Path::new("/source-a"), None);
        let second = target(Path::new("/source-b"), None);
        let mut state = DirectoryOpenState {
            next_request_id: 1,
            epoch: 7,
            running: Some(RunningDirectoryOpen {
                request_id: 1,
                epoch: 7,
                target: first,
                cancellation: std::sync::Arc::clone(&first_cancellation),
            }),
            queued: None,
            task: None,
        };

        state.queue_latest(second.clone());

        assert!(first_cancellation.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            state.queued.as_ref().map(|target| &target.root),
            Some(&second.root)
        );
        assert!(state.running.is_some());
    }

    #[test]
    fn non_selected_folder_uses_projection_fence_only() {
        let target = target(Path::new("/source"), Some(PathBuf::from("nested")));

        assert!(folder_projection_fence_matches(
            &target,
            Some(0),
            Some(Path::new("/source")),
            1,
            1,
            true,
        ));
        assert!(!folder_projection_fence_matches(
            &target,
            Some(0),
            Some(Path::new("/source")),
            1,
            2,
            true,
        ));
        assert!(!folder_projection_fence_matches(
            &target,
            Some(0),
            Some(Path::new("/source")),
            1,
            1,
            false,
        ));
    }
}
