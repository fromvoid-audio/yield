//! Consume bounded native OS-entry refusals in the Browser UI.

use gpui::Context;

use super::Browser;
use crate::os_entry::{OsEntrySignal, UNSUPPORTED_OPEN_MESSAGE};

impl Browser {
    pub(super) fn start_os_entry_consumer(&mut self, cx: &mut Context<Self>) {
        if self.os_entry_task.is_some() || !cx.has_global::<OsEntrySignal>() {
            return;
        }
        let receiver = cx.global::<OsEntrySignal>().receiver();
        self.os_entry_task = Some(cx.spawn(async move |this, cx| {
            while receiver.recv().await.is_ok() {
                if this
                    .update(cx, |browser, cx| {
                        browser.set_os_entry_status(UNSUPPORTED_OPEN_MESSAGE);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }
}

#[cfg(test)]
mod tests {
    use gpui::AppContext;

    use super::*;

    #[gpui::test]
    fn queued_open_refusal_reaches_browser_without_changing_source_or_scan(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let signal = OsEntrySignal::new();
        cx.set_global(signal.clone());

        // An open event may arrive before Browser finishes starting. The ingress
        // retains only its capacity-one signal until the UI consumer is ready.
        signal.receive_open_urls(vec![String::from(
            "file:///private/example/do-not-retain/sample.wav",
        )]);

        let browser = cx.new(Browser::new);
        let (
            before_root,
            before_scan_status,
            before_scan_generation,
            before_entry_paths,
            before_source_roots,
            before_active_source,
            before_registry_revision,
            before_registry_roots,
            before_registry_status,
            before_registry_loaded,
            before_registry_pending_adds,
            before_registry_pending_removes,
            before_removed_roots,
        ) = browser.read_with(cx, |before, _| {
            (
                before.root.clone(),
                before.scan_status.clone(),
                before.scan_generation,
                before
                    .entries
                    .iter()
                    .map(|entry| entry.path.clone())
                    .collect::<Vec<_>>(),
                before
                    .source_sessions
                    .iter()
                    .map(|source| (source.root.clone(), source.source_id.clone()))
                    .collect::<Vec<_>>(),
                before.active_source,
                before.source_registry_revision,
                before.source_registry_known_roots.clone(),
                before.source_registry_status.clone(),
                before.source_registry_loaded,
                before.source_registry_pending_adds.len(),
                before.source_registry_pending_removes.len(),
                before.source_registry_removed_roots.clone(),
            )
        });
        let before_has_registry =
            cx.read(|app| app.has_global::<super::super::source_registry::SourceRegistryGlobal>());

        cx.run_until_parked();

        let (
            after_os_entry_status,
            after_root,
            after_scan_status,
            after_scan_generation,
            after_entry_paths,
            after_source_roots,
            after_active_source,
            after_registry_revision,
            after_registry_roots,
            after_registry_status,
            after_registry_loaded,
            after_registry_pending_adds,
            after_registry_pending_removes,
            after_removed_roots,
            status_log,
        ) = browser.read_with(cx, |after, _| {
            (
                after.os_entry_status,
                after.root.clone(),
                after.scan_status.clone(),
                after.scan_generation,
                after
                    .entries
                    .iter()
                    .map(|entry| entry.path.clone())
                    .collect::<Vec<_>>(),
                after
                    .source_sessions
                    .iter()
                    .map(|source| (source.root.clone(), source.source_id.clone()))
                    .collect::<Vec<_>>(),
                after.active_source,
                after.source_registry_revision,
                after.source_registry_known_roots.clone(),
                after.source_registry_status.clone(),
                after.source_registry_loaded,
                after.source_registry_pending_adds.len(),
                after.source_registry_pending_removes.len(),
                after.source_registry_removed_roots.clone(),
                after.status_log.text(),
            )
        });
        let after_has_registry =
            cx.read(|app| app.has_global::<super::super::source_registry::SourceRegistryGlobal>());

        assert_eq!(after_os_entry_status, Some(UNSUPPORTED_OPEN_MESSAGE));
        assert_eq!(after_root, before_root);
        assert_eq!(after_scan_status, before_scan_status);
        assert_eq!(after_scan_generation, before_scan_generation);
        assert_eq!(after_entry_paths, before_entry_paths);
        assert_eq!(after_source_roots, before_source_roots);
        assert_eq!(after_active_source, before_active_source);
        assert_eq!(after_registry_revision, before_registry_revision);
        assert_eq!(after_registry_roots, before_registry_roots);
        assert_eq!(after_registry_status, before_registry_status);
        assert_eq!(after_registry_loaded, before_registry_loaded);
        assert_eq!(after_registry_pending_adds, before_registry_pending_adds);
        assert_eq!(
            after_registry_pending_removes,
            before_registry_pending_removes
        );
        assert_eq!(after_removed_roots, before_removed_roots);
        assert_eq!(after_has_registry, before_has_registry);

        assert!(status_log.contains(UNSUPPORTED_OPEN_MESSAGE));
        assert!(!status_log.contains("do-not-retain"));
        assert!(!status_log.contains("sample.wav"));
    }
}
