use std::{
    collections::{BTreeSet, VecDeque},
    path::{Path, PathBuf},
};

use gpui::Context;
use yield_session::resolve_source_root;

use super::Browser;

const MAX_DROP_FOLDERS: usize = 16;
const MAX_DROP_BYTES: usize = 512 * 1024;

#[derive(Default)]
pub(super) struct SourceDropBatch {
    folders: VecDeque<PathBuf>,
    active: Option<ActiveSourceDrop>,
    added: usize,
    skipped: usize,
    first_rejection: Option<String>,
}

struct ActiveSourceDrop {
    root: PathBuf,
    scan_generation: u64,
}

impl SourceDropBatch {
    fn reject(&mut self, message: impl Into<String>) {
        self.skipped += 1;
        self.first_rejection.get_or_insert_with(|| message.into());
    }
}

impl Browser {
    pub(super) fn accept_external_source_drop(
        &mut self,
        paths: &[PathBuf],
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() || paths.len() > MAX_DROP_FOLDERS {
            self.report_source_drop(
                format!("Drop up to {MAX_DROP_FOLDERS} folders onto Sources"),
                cx,
            );
            return;
        }
        let bytes = paths.iter().fold(0usize, |total, path| {
            total.saturating_add(path.as_os_str().as_encoded_bytes().len())
        });
        if bytes > MAX_DROP_BYTES {
            self.report_source_drop("The dropped folder paths exceed the byte limit", cx);
            return;
        }
        if !self.source_registry_loaded {
            self.report_source_drop("Wait for Sources to finish loading", cx);
            return;
        }
        if self.scan_cancel.is_some()
            || self.external_source_drop_task.is_some()
            || self.external_source_drop_batch.is_some()
        {
            self.report_source_drop("Wait for the current source scan or drop to finish", cx);
            return;
        }

        let paths = paths.to_vec();
        let scan_generation = self.scan_generation;
        self.report_source_drop("Checking dropped folders…", cx);
        self.external_source_drop_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { prepare_dropped_folders(paths) })
                .await;
            let _ = this.update(cx, |view, cx| {
                view.external_source_drop_task.take();
                view.finish_external_source_drop(scan_generation, result, cx);
            });
        }));
    }

    fn finish_external_source_drop(
        &mut self,
        scan_generation: u64,
        batch: SourceDropBatch,
        cx: &mut Context<Self>,
    ) {
        if self.scan_generation != scan_generation || self.scan_cancel.is_some() {
            self.report_source_drop("The source changed before the drop was ready", cx);
            return;
        }
        self.external_source_drop_batch = Some(batch);
        self.advance_external_source_drop(cx);
    }

    pub(super) fn dropped_scan_root_matches(&self, generation: u64, root: &Path) -> bool {
        self.external_source_drop_batch
            .as_ref()
            .and_then(|batch| batch.active.as_ref())
            .is_none_or(|active| active.scan_generation != generation || active.root == root)
    }

    pub(super) fn advance_external_source_drop(&mut self, cx: &mut Context<Self>) {
        let Some(mut batch) = self.external_source_drop_batch.take() else {
            return;
        };
        if let Some(active) = batch.active.take() {
            if active.scan_generation != self.scan_generation {
                self.report_source_drop("Stopped the folder drop when another scan started", cx);
                return;
            }
            if self.scan_cancel.is_some()
                || self
                    .source_registry_pending_adds
                    .values()
                    .any(|pending| pending.root == active.root)
            {
                batch.active = Some(active);
                self.external_source_drop_batch = Some(batch);
                return;
            }
            // The registry keeps a mutation pending until it has the persisted revision or
            // a terminal failure. Only a retained root with no source error counts as added.
            if self.source_registry_known_roots.contains(&active.root)
                && self.source_sessions.iter().any(|session| {
                    session.root == active.root
                        && session.source_id.is_some()
                        && session.last_error.is_none()
                })
            {
                batch.added += 1;
            } else {
                batch.reject("A dropped folder could not be saved");
            }
        }
        while let Some(root) = batch.folders.pop_front() {
            if self.source_registry_known_roots.contains(&root)
                || self.source_registry_removed_roots.contains(&root)
                || self
                    .source_sessions
                    .iter()
                    .any(|session| session.root == root)
                || self
                    .source_registry_pending_adds
                    .values()
                    .any(|pending| pending.root == root)
            {
                batch.reject("A dropped folder is already a source or is being changed");
                continue;
            }
            self.start_scan(root.clone(), cx);
            batch.active = Some(ActiveSourceDrop {
                root,
                scan_generation: self.scan_generation,
            });
            self.external_source_drop_batch = Some(batch);
            return;
        }
        let noun = if batch.added == 1 {
            "source"
        } else {
            "sources"
        };
        let mut outcome = format!("Added {} {noun}. Skipped {}.", batch.added, batch.skipped);
        if let Some(reason) = batch.first_rejection {
            outcome.push(' ');
            outcome.push_str(&reason);
        }
        self.report_source_drop(outcome, cx);
    }

    pub(super) fn stop_external_source_drop(&mut self, cx: &mut Context<Self>) {
        let preparing = self.external_source_drop_task.take().is_some();
        let queued = self.external_source_drop_batch.take().is_some();
        if preparing || queued {
            self.report_source_drop(
                "Stopped adding dropped folders because Sources is unavailable",
                cx,
            );
        }
    }

    fn report_source_drop(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.set_playback_status(message);
        cx.notify();
    }
}

fn prepare_dropped_folders(paths: Vec<PathBuf>) -> SourceDropBatch {
    let mut batch = SourceDropBatch::default();
    let mut seen = BTreeSet::new();
    for path in paths {
        if !path.is_absolute() {
            batch.reject("A dropped folder needs an absolute path");
            continue;
        }
        match resolve_source_root(&path) {
            Ok(root) => {
                let canonical = root.canonical_root().to_path_buf();
                if seen.insert(canonical.clone()) {
                    batch.folders.push_back(canonical);
                } else {
                    batch.reject("A folder appeared more than once in the drop");
                }
            }
            Err(error) => batch.reject(format!("Could not add a dropped folder. {error}")),
        }
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::super::source_registry::SourceRegistryGlobal;
    use super::*;
    use gpui::BorrowAppContext;
    use std::time::{Duration, Instant};

    #[gpui::test]
    fn valid_external_folder_reaches_the_existing_source_scan(cx: &mut gpui::TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let fixture = tempfile::tempdir().unwrap();
        let file = fixture.path().join("sample.wav");
        std::fs::write(&file, b"fixture").unwrap();
        let canonical = fixture.path().canonicalize().unwrap();
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));

        browser.update(cx, |view, cx| {
            view.source_registry_loaded = true;
            view.accept_external_source_drop(&[file], cx);
        });
        cx.run_until_parked();
        browser.update(cx, |view, cx| {
            assert!(view.root.is_none());
            assert!(view.external_source_drop_task.is_none());
            view.accept_external_source_drop(&[canonical.clone()], cx);
        });
        cx.run_until_parked();
        browser.update(cx, |view, _| {
            assert_eq!(view.root.as_deref(), Some(canonical.as_path()));
            assert_eq!(view.entries.len(), 1);
        });
    }

    #[gpui::test]
    fn duplicate_and_stale_drop_results_do_not_start_a_scan(cx: &mut gpui::TestAppContext) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        browser.update(cx, |view, cx| {
            let root = std::env::temp_dir();
            view.source_registry_known_roots.insert(root.clone());
            let generation = view.scan_generation;
            view.finish_external_source_drop(
                generation,
                SourceDropBatch {
                    folders: VecDeque::from([root.clone()]),
                    ..Default::default()
                },
                cx,
            );
            assert!(view.scan_task.is_none());

            view.source_registry_known_roots.clear();
            view.scan_generation = generation.wrapping_add(1);
            view.finish_external_source_drop(
                generation,
                SourceDropBatch {
                    folders: VecDeque::from([root]),
                    ..Default::default()
                },
                cx,
            );
            assert!(view.scan_task.is_none());
        });
    }

    #[gpui::test]
    fn multi_folder_drop_saves_each_root_in_order(cx: &mut gpui::TestAppContext) {
        let fixture = tempfile::tempdir().unwrap();
        let first = fixture.path().join("z-first");
        let second = fixture.path().join("a-second");
        let non_folder = fixture.path().join("sample.wav");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        std::fs::write(&non_folder, b"fixture").unwrap();
        let first = first.canonicalize().unwrap();
        let second = second.canonicalize().unwrap();
        cx.set_global(crate::theme::ActiveTheme::default());
        cx.set_global(SourceRegistryGlobal::for_preferences_fixture(
            fixture.path().join("profiles").join("source-drop"),
        ));
        let (browser, cx) = cx.add_window_view(|_, cx| Browser::new(cx));
        let deadline = Instant::now() + Duration::from_millis(600);
        let mut submitted = false;
        loop {
            cx.executor().advance_clock(Duration::from_millis(25));
            cx.run_until_parked();
            let done = browser.update(cx, |view, cx| {
                if !submitted && view.source_registry_loaded {
                    view.accept_external_source_drop(
                        &[
                            first.clone(),
                            non_folder.clone(),
                            second.clone(),
                            first.clone(),
                        ],
                        cx,
                    );
                    submitted = true;
                    return false;
                }
                submitted
                    && view.external_source_drop_task.is_none()
                    && view.external_source_drop_batch.is_none()
            });
            if done {
                break;
            }
            assert!(Instant::now() < deadline, "Source drop should settle");
            std::thread::sleep(Duration::from_millis(1));
        }
        cx.update(|_, app| {
            let view = browser.read(app);
            assert!(view.source_registry_pending_adds.is_empty());
            assert_eq!(
                view.source_sessions
                    .iter()
                    .map(|session| {
                        assert!(session.source_id.is_some());
                        &session.root
                    })
                    .collect::<Vec<_>>(),
                vec![&first, &second]
            );
            app.update_global::<SourceRegistryGlobal, _>(|registry, _| registry.shutdown());
        });
    }
}
