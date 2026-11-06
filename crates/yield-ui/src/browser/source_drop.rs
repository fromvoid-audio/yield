use std::path::{Path, PathBuf};

use gpui::Context;
use yield_session::resolve_source_root;

use super::Browser;

const MAX_DROP_PATH_BYTES: usize = 128 * 1024;

impl Browser {
    pub(super) fn accept_external_source_drop(
        &mut self,
        paths: &[PathBuf],
        cx: &mut Context<Self>,
    ) {
        let [path] = paths else {
            self.report_source_drop("Drop one folder onto Sources", cx);
            return;
        };
        if !path.is_absolute() || path.as_os_str().as_encoded_bytes().len() > MAX_DROP_PATH_BYTES {
            self.report_source_drop("The dropped folder path is unavailable", cx);
            return;
        }
        if !self.source_registry_loaded {
            self.report_source_drop("Wait for Sources to finish loading", cx);
            return;
        }
        if self.scan_cancel.is_some() || self.external_source_drop_task.is_some() {
            self.report_source_drop("Wait for the current source scan or drop to finish", cx);
            return;
        }

        let path = path.clone();
        let scan_generation = self.scan_generation;
        self.report_source_drop("Checking the dropped folder…", cx);
        self.external_source_drop_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { prepare_dropped_folder(&path) })
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
        result: Result<PathBuf, String>,
        cx: &mut Context<Self>,
    ) {
        if self.scan_generation != scan_generation || self.scan_cancel.is_some() {
            self.report_source_drop("The source changed before the drop was ready", cx);
            return;
        }
        let root = match result {
            Ok(root) => root,
            Err(error) => {
                self.report_source_drop(error, cx);
                return;
            }
        };
        if self.source_registry_known_roots.contains(&root)
            || self
                .source_sessions
                .iter()
                .any(|session| session.root == root)
            || self
                .source_registry_pending_adds
                .values()
                .any(|pending| pending.root == root)
        {
            self.report_source_drop("That folder is already a source", cx);
            return;
        }
        self.start_scan(root, cx);
    }

    fn report_source_drop(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.set_playback_status(message);
        cx.notify();
    }
}

fn prepare_dropped_folder(path: &Path) -> Result<PathBuf, String> {
    resolve_source_root(path)
        .map(|root| root.canonical_root().to_path_buf())
        .map_err(|error| format!("Could not add the dropped folder. {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            view.finish_external_source_drop(generation, Ok(root.clone()), cx);
            assert!(view.scan_task.is_none());

            view.source_registry_known_roots.clear();
            view.scan_generation = generation.wrapping_add(1);
            view.finish_external_source_drop(generation, Ok(root), cx);
            assert!(view.scan_task.is_none());
        });
    }
}
