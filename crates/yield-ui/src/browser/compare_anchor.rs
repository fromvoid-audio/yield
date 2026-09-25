use std::path::{Component, Path, PathBuf};

use gpui::Context;
use yield_library::sample_sources::SourceId;
use yield_session::{VerifiedSourceRoot, resolve_source_root};

use super::Browser;
use super::transport::{PlaybackIntent, PlaybackIntentOrigin};

/// A transient comparison target bound to one registered physical source root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CompareAnchor {
    source_id: SourceId,
    root: PathBuf,
    verified_root: VerifiedSourceRoot,
    relative_path: PathBuf,
}

impl CompareAnchor {
    pub(super) fn source_root(&self) -> &Path {
        &self.root
    }

    pub(super) fn path(&self) -> PathBuf {
        self.root.join(&self.relative_path)
    }
}

fn checked_file(root: &VerifiedSourceRoot, relative_path: &Path) -> Result<(), &'static str> {
    if relative_path.as_os_str().is_empty()
        || !relative_path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err("Compare anchor file is no longer available");
    }
    let candidate = root.canonical_root().join(relative_path);
    let resolved = candidate
        .canonicalize()
        .map_err(|_| "Compare anchor file is no longer available")?;
    if !resolved.starts_with(root.canonical_root())
        || !resolved.metadata().is_ok_and(|metadata| metadata.is_file())
    {
        return Err("Compare anchor file is no longer available");
    }
    Ok(())
}

impl Browser {
    fn compare_source_is_registered(&self, source_id: &SourceId, root: &Path) -> bool {
        self.source_registry_known_roots.contains(root)
            && !self.source_registry_removed_roots.contains(root)
            && self.source_sessions.iter().any(|session| {
                session.source_id.as_ref() == Some(source_id) && session.root == root
            })
    }

    pub(super) fn compare_anchor_path_is_current(&self, path: &Path) -> bool {
        self.compare_anchor.as_ref().is_some_and(|anchor| {
            anchor.path() == path
                && self.compare_source_is_registered(&anchor.source_id, &anchor.root)
        })
    }

    pub(super) fn set_compare_anchor_from_focus(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self
            .active_source
            .and_then(|index| self.source_sessions.get(index))
        else {
            self.set_playback_status("Select a source before setting a compare anchor");
            cx.notify();
            return;
        };
        let Some(source_id) = session.source_id.clone() else {
            self.set_playback_status("Compare anchor source is not registered");
            cx.notify();
            return;
        };
        let root = session.root.clone();
        let Some(entry) = self
            .browser_focus_index()
            .and_then(|index| self.entries.get(index))
        else {
            self.set_playback_status("Focus a sample to set a compare anchor");
            cx.notify();
            return;
        };
        let Ok(relative_path) = entry.path.strip_prefix(&root) else {
            self.set_playback_status("Focused sample is no longer available");
            cx.notify();
            return;
        };
        let relative_path = relative_path.to_path_buf();
        if !self.compare_source_is_registered(&source_id, &root) {
            self.set_playback_status("Compare anchor source is not registered");
            cx.notify();
            return;
        }
        self.compare_anchor_generation = self.compare_anchor_generation.wrapping_add(1);
        let generation = self.compare_anchor_generation;
        self.compare_anchor_task.take();
        self.compare_anchor_play_pending = false;
        self.compare_anchor = None;
        self.set_playback_status("Checking compare anchor…");
        self.compare_anchor_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let verified_root = resolve_source_root(&root)
                        .map_err(|_| "Compare anchor source is no longer available")?;
                    checked_file(&verified_root, &relative_path)?;
                    Ok::<_, &'static str>((root, relative_path, verified_root))
                })
                .await;
            let _ = this.update(cx, |browser, cx| {
                if browser.compare_anchor_generation != generation {
                    return;
                }
                browser.compare_anchor_task = None;
                match result {
                    Ok((root, relative_path, verified_root))
                        if browser.compare_source_is_registered(&source_id, &root) =>
                    {
                        let label = relative_path.display();
                        browser.set_playback_status(format!("Compare anchor set: {label}"));
                        browser.compare_anchor = Some(CompareAnchor {
                            source_id,
                            root,
                            verified_root,
                            relative_path,
                        });
                    }
                    Ok(_) => {
                        browser.set_playback_status("Compare anchor source is no longer available");
                    }
                    Err(message) => browser.set_playback_status(message),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn play_compare_anchor(&mut self, cx: &mut Context<Self>) {
        let Some(anchor) = self.compare_anchor.clone() else {
            self.set_playback_status("Set a compare anchor first");
            cx.notify();
            return;
        };
        if !self.compare_source_is_registered(&anchor.source_id, &anchor.root) {
            self.compare_anchor = None;
            self.set_playback_status("Compare anchor source is no longer available");
            cx.notify();
            return;
        }
        self.compare_anchor_generation = self.compare_anchor_generation.wrapping_add(1);
        let generation = self.compare_anchor_generation;
        self.compare_anchor_task.take();
        self.compare_anchor_play_pending = true;
        self.set_playback_status("Checking compare anchor…");
        let root = anchor.root.clone();
        let relative_path = anchor.relative_path.clone();
        let expected_root = anchor.verified_root.clone();
        self.compare_anchor_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let verified = resolve_source_root(&root)
                        .map_err(|_| "Compare anchor source is no longer available")?;
                    if verified != expected_root {
                        return Err("Compare anchor source has changed");
                    }
                    checked_file(&verified, &relative_path)
                })
                .await;
            let _ = this.update(cx, |browser, cx| {
                if browser.compare_anchor_generation != generation
                    || browser.compare_anchor.as_ref() != Some(&anchor)
                {
                    return;
                }
                browser.compare_anchor_task = None;
                browser.compare_anchor_play_pending = false;
                if !browser.compare_source_is_registered(&anchor.source_id, &anchor.root) {
                    browser.compare_anchor = None;
                    browser.set_playback_status("Compare anchor source is no longer available");
                } else if let Err(message) = result {
                    browser.compare_anchor = None;
                    browser.set_playback_status(message);
                } else {
                    let intent = PlaybackIntent {
                        start: 0.0,
                        region: None,
                        looped: browser.playback_policy.enabled(),
                        volume: super::volume::quantize_volume(browser.volume),
                        path: anchor.path(),
                        origin: PlaybackIntentOrigin::CompareAnchor,
                        resume_desired: None,
                    };
                    browser.request_playback_intent(intent, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::super::SourceSession;
    use super::*;
    use gpui::AppContext;
    use yield_core::AudioFile;

    #[test]
    fn checked_file_rejects_missing_and_escaped_paths() {
        let source = tempfile::tempdir().expect("source");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(source.path().join("anchor.wav"), b"sample").expect("sample");
        std::fs::write(outside.path().join("other.wav"), b"sample").expect("outside sample");
        let verified = resolve_source_root(source.path()).expect("verified root");
        assert_eq!(checked_file(&verified, Path::new("anchor.wav")), Ok(()));
        assert!(checked_file(&verified, Path::new("missing.wav")).is_err());
        assert!(checked_file(&verified, Path::new("../other.wav")).is_err());

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                outside.path().join("other.wav"),
                source.path().join("escape.wav"),
            )
            .expect("symlink");
            assert!(checked_file(&verified, Path::new("escape.wav")).is_err());
        }
    }

    #[test]
    fn physical_source_replacement_invalidates_anchor_proof() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("source");
        std::fs::create_dir(&root).expect("source");
        let first = resolve_source_root(&root).expect("first proof");
        std::fs::rename(&root, parent.path().join("retired")).expect("retire source");
        std::fs::create_dir(&root).expect("replacement");
        let second = resolve_source_root(&root).expect("second proof");
        assert_ne!(first, second);
    }

    #[gpui::test]
    fn anchor_survives_other_source_navigation_without_changing_focus_or_selection(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.set_global(crate::theme::ActiveTheme::default());
        let first = tempfile::tempdir().expect("first source");
        let second = tempfile::tempdir().expect("second source");
        std::fs::write(first.path().join("anchor.wav"), b"sample").expect("anchor");
        std::fs::write(first.path().join("selected.wav"), b"sample").expect("selection");
        std::fs::write(second.path().join("other.wav"), b"sample").expect("other");
        let browser = cx.new(Browser::new);
        browser.update(cx, |view, cx| {
            let mut first_session = SourceSession::new(first.path().to_path_buf());
            first_session.source_id = Some(SourceId::from_string("first"));
            let mut second_session = SourceSession::new(second.path().to_path_buf());
            second_session.source_id = Some(SourceId::from_string("second"));
            view.source_sessions = vec![first_session, second_session];
            view.source_registry_known_roots =
                [first.path().to_path_buf(), second.path().to_path_buf()]
                    .into_iter()
                    .collect();
            view.active_source = Some(0);
            view.entries = ["anchor.wav", "selected.wav"]
                .into_iter()
                .map(|name| AudioFile {
                    path: first.path().join(name),
                    relative_path: PathBuf::from(name),
                    byte_len: 6,
                    duration_seconds: None,
                    sample_rate: None,
                    playback_type: None,
                    curation: None,
                })
                .collect();
            view.selected = Some(1);
            let focus = view.entries[0].stable_id();
            view.set_browser_focus_override(focus.clone());
            view.set_compare_anchor_from_focus(cx);
            assert_eq!(view.selected, Some(1));
            assert_eq!(view.browser_focus_id(), Some(focus));
        });
        cx.run_until_parked();
        browser.update(cx, |view, cx| {
            assert_eq!(
                view.compare_anchor.as_ref().map(CompareAnchor::path),
                Some(first.path().join("anchor.wav"))
            );
            view.active_source = Some(1);
            view.entries = vec![AudioFile {
                path: second.path().join("other.wav"),
                relative_path: PathBuf::from("other.wav"),
                byte_len: 6,
                duration_seconds: None,
                sample_rate: None,
                playback_type: None,
                curation: None,
            }];
            view.selected = Some(0);
            let focus = view.entries[0].stable_id();
            view.set_browser_focus_override(focus.clone());
            assert!(view.compare_anchor_path_is_current(&first.path().join("anchor.wav")));
            view.play_compare_anchor(cx);
            assert_eq!(view.selected, Some(0));
            assert_eq!(view.browser_focus_id(), Some(focus));
        });
        cx.run_until_parked();
        browser.update(cx, |view, cx| {
            assert_eq!(view.selected, Some(0));
            assert_eq!(view.browser_focus_id(), Some(view.entries[0].stable_id()));
            assert_eq!(
                view.playback_init_intent
                    .as_ref()
                    .map(|intent| intent.path.as_path()),
                Some(first.path().join("anchor.wav").as_path())
            );
            view.stop_playback();
            view.source_sessions.remove(0);
            view.play_compare_anchor(cx);
            assert!(view.compare_anchor.is_none());
            assert_eq!(
                view.playback_status,
                "Compare anchor source is no longer available"
            );
        });
    }
}
