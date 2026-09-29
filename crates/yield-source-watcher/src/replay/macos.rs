//! Bounded translation of per-device FSEvents callback records.
//!
//! Per-device callback paths are relative to the volume root. macOS may include or omit `/`.
//! This module translates them into the source root's namespace before the common ingress
//! validates source relativity. The sibling native worker starts the stream; translation alone
//! never grants replay proof.

use std::path::{Path, PathBuf};

use fsevent_sys as fs;

use super::{
    MAX_REPLAY_EVENTS, ReplayEventCondition, ReplayIngress, ReplayPathKind, ReplayUncertainty,
};

mod native;

pub use native::{BootstrapCapture, NativeReplayError, capture_history};

/// One borrowed native callback record, before source-relative validation.
#[derive(Clone, Copy, Debug)]
pub struct MacosReplayEvent<'a> {
    /// Native FSEvents event ID.
    pub event_id: u64,
    /// Path relative to the volume root, optionally with a leading `/`.
    pub volume_relative_path: &'a Path,
    /// Native FSEvents flags, retained without lossy notify translation.
    pub flags: fs::FSEventStreamEventFlags,
}

/// Translate one bounded native callback into a replay ingress.
///
/// `volume_root` is the canonical mount point of the exact device represented by the ingress's
/// UUID-bound stream identity. A callback larger than the ingress bound is marked uncertain before
/// processing its first bounded prefix; the remainder is never copied or scanned.
pub fn record_callback_batch(
    ingress: &ReplayIngress,
    volume_root: &Path,
    events: &[MacosReplayEvent<'_>],
) {
    if events.len() > MAX_REPLAY_EVENTS {
        ingress.mark_reason(ReplayUncertainty::EVENT_LIMIT);
    }
    for event in events.iter().take(MAX_REPLAY_EVENTS) {
        record_callback_event(ingress, volume_root, *event);
    }
}

pub(super) fn record_callback_event(
    ingress: &ReplayIngress,
    volume_root: &Path,
    event: MacosReplayEvent<'_>,
) {
    let flags = event.flags;
    let lost = flags
        & (fs::kFSEventStreamEventFlagUserDropped
            | fs::kFSEventStreamEventFlagKernelDropped
            | fs::kFSEventStreamEventFlagEventIdsWrapped)
        != 0;
    let must_scan = flags & fs::kFSEventStreamEventFlagMustScanSubDirs != 0;
    let root_changed = flags
        & (fs::kFSEventStreamEventFlagRootChanged
            | fs::kFSEventStreamEventFlagMount
            | fs::kFSEventStreamEventFlagUnmount)
        != 0;
    let unknown = flags & !KNOWN_FLAGS != 0;
    if lost {
        ingress.mark_reason(ReplayUncertainty::DELIVERY_LOST);
    }
    if must_scan {
        ingress.mark_reason(ReplayUncertainty::MUST_SCAN_SUBDIRS);
    }
    if root_changed {
        ingress.mark_reason(ReplayUncertainty::ROOT_CHANGED);
    }
    if unknown {
        ingress.mark_reason(ReplayUncertainty::UNKNOWN_EVENT);
    }
    if flags & fs::kFSEventStreamEventFlagHistoryDone != 0 {
        ingress.mark_history_complete();
        return;
    }
    let condition = if lost {
        ReplayEventCondition::DeliveryLost
    } else if must_scan {
        ReplayEventCondition::MustScanSubdirs
    } else if root_changed {
        ReplayEventCondition::RootChanged
    } else if unknown {
        ReplayEventCondition::Unknown
    } else {
        ReplayEventCondition::Changed
    };
    let Some(absolute) = source_absolute_path(volume_root, event.volume_relative_path) else {
        ingress.mark_reason(ReplayUncertainty::INVALID_PATH);
        ingress.record(Some(event.event_id), Path::new(""), condition);
        return;
    };
    if absolute == ingress.spec.root() {
        ingress.record(
            Some(event.event_id),
            &absolute,
            ReplayEventCondition::MustScanSubdirs,
        );
        return;
    }
    let kind_flags = flags
        & (fs::kFSEventStreamEventFlagItemIsFile
            | fs::kFSEventStreamEventFlagItemIsDir
            | fs::kFSEventStreamEventFlagItemIsSymlink);
    let kind = match kind_flags {
        fs::kFSEventStreamEventFlagItemIsFile => ReplayPathKind::File,
        fs::kFSEventStreamEventFlagItemIsDir => ReplayPathKind::Directory,
        fs::kFSEventStreamEventFlagItemIsSymlink => ReplayPathKind::Symlink,
        _ => ReplayPathKind::Unknown,
    };
    if condition == ReplayEventCondition::Changed
        && kind == ReplayPathKind::File
        && absolute
            .strip_prefix(ingress.spec.root())
            .ok()
            .is_some_and(|relative| {
                relative
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)))
                    && relative
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| {
                            ingress
                                .spec
                                .ignored_file_name_prefixes()
                                .iter()
                                .any(|prefix| name.starts_with(prefix))
                        })
            })
    {
        ingress.record_ignored_artifact(event.event_id);
        return;
    }
    ingress.record_with_kind(Some(event.event_id), &absolute, condition, kind);
}

fn source_absolute_path(volume_root: &Path, volume_relative_path: &Path) -> Option<PathBuf> {
    if !volume_root.is_absolute() {
        return None;
    }
    let relative = if volume_relative_path.is_absolute() {
        volume_relative_path.strip_prefix("/").ok()?
    } else {
        volume_relative_path
    };
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(volume_root.join(relative))
}

const KNOWN_FLAGS: fs::FSEventStreamEventFlags = fs::kFSEventStreamEventFlagMustScanSubDirs
    | fs::kFSEventStreamEventFlagUserDropped
    | fs::kFSEventStreamEventFlagKernelDropped
    | fs::kFSEventStreamEventFlagEventIdsWrapped
    | fs::kFSEventStreamEventFlagHistoryDone
    | fs::kFSEventStreamEventFlagRootChanged
    | fs::kFSEventStreamEventFlagMount
    | fs::kFSEventStreamEventFlagUnmount
    | fs::kFSEventStreamEventFlagItemCreated
    | fs::kFSEventStreamEventFlagItemRemoved
    | fs::kFSEventStreamEventFlagItemInodeMetaMod
    | fs::kFSEventStreamEventFlagItemRenamed
    | fs::kFSEventStreamEventFlagItemModified
    | fs::kFSEventStreamEventFlagItemFinderInfoMod
    | fs::kFSEventStreamEventFlagItemChangeOwner
    | fs::kFSEventStreamEventFlagItemXattrMod
    | fs::kFSEventStreamEventFlagItemIsFile
    | fs::kFSEventStreamEventFlagItemIsDir
    | fs::kFSEventStreamEventFlagItemIsSymlink
    | fs::kFSEventStreamEventFlagOwnEvent
    | fs::kFSEventStreamEventFlagItemIsHardlink
    | fs::kFSEventStreamEventFlagItemIsLastHardlink
    | fs::kFSEventStreamEventFlagItemCloned;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use yield_library::sample_sources::SourceId;
    use yield_library::sample_sources::reconciliation::RootIdentity;

    use super::*;
    use crate::SourceWatchSpec;

    fn ingress() -> ReplayIngress {
        let spec = SourceWatchSpec::new(
            SourceId::from_string("macos-replay"),
            3,
            PathBuf::from("/Volumes/Samples/Library"),
            Some(RootIdentity::from_bytes(b"root".to_vec())),
        );
        ReplayIngress::new(spec, 17, [2; 16], 100).expect("replay ingress")
    }

    #[test]
    fn translates_volume_relative_paths_and_ignores_history_sentinel_path() {
        let ingress = ingress();
        let events = [
            MacosReplayEvent {
                event_id: 107,
                volume_relative_path: Path::new("/Library/Kick.wav"),
                flags: fs::kFSEventStreamEventFlagItemModified
                    | fs::kFSEventStreamEventFlagItemIsFile,
            },
            MacosReplayEvent {
                event_id: 107,
                volume_relative_path: Path::new("/"),
                flags: fs::kFSEventStreamEventFlagHistoryDone,
            },
        ];
        record_callback_batch(&ingress, Path::new("/Volumes/Samples"), &events);
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.events().len(), 1);
        assert_eq!(candidate.events()[0].relative_path(), Path::new("Kick.wav"));
        assert_eq!(candidate.last_seen(), Some(107));
        assert!(candidate.history_complete());
        assert!(candidate.uncertainty().is_empty());
    }

    #[test]
    fn accepts_native_volume_paths_without_leading_slash() {
        let ingress = ingress();
        record_callback_batch(
            &ingress,
            Path::new("/Volumes/Samples"),
            &[MacosReplayEvent {
                event_id: 108,
                volume_relative_path: Path::new("Library/Kick.wav"),
                flags: fs::kFSEventStreamEventFlagItemModified,
            }],
        );
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.events()[0].relative_path(), Path::new("Kick.wav"));
        assert!(candidate.uncertainty().is_empty());
    }

    #[test]
    fn source_local_database_file_advances_cursor_without_a_replay_path() {
        let ingress = ingress();
        record_callback_batch(
            &ingress,
            Path::new("/Volumes/Samples"),
            &[MacosReplayEvent {
                event_id: 108,
                volume_relative_path: Path::new("Library/.yield.db-wal"),
                flags: fs::kFSEventStreamEventFlagItemModified
                    | fs::kFSEventStreamEventFlagItemIsFile,
            }],
        );
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert!(candidate.events().is_empty());
        assert_eq!(candidate.last_seen(), Some(108));
        assert!(candidate.uncertainty().is_empty());
    }

    #[test]
    fn combined_drop_and_root_flags_remain_distinct_uncertainty() {
        let ingress = ingress();
        record_callback_batch(
            &ingress,
            Path::new("/Volumes/Samples"),
            &[MacosReplayEvent {
                event_id: 110,
                volume_relative_path: Path::new("/Library/Kick.wav"),
                flags: fs::kFSEventStreamEventFlagUserDropped
                    | fs::kFSEventStreamEventFlagRootChanged
                    | fs::kFSEventStreamEventFlagMustScanSubDirs,
            }],
        );
        let candidate = ingress.into_candidate();
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::DELIVERY_LOST)
        );
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::ROOT_CHANGED)
        );
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::MUST_SCAN_SUBDIRS)
        );
        assert!(candidate.events().is_empty());
    }

    #[test]
    fn malformed_volume_path_and_large_callback_fail_closed() {
        let first_ingress = ingress();
        let record = MacosReplayEvent {
            event_id: 111,
            volume_relative_path: Path::new("/Library/../Other.wav"),
            flags: 0,
        };
        record_callback_batch(&first_ingress, Path::new("/Volumes/Samples"), &[record]);
        let candidate = first_ingress.into_candidate();
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::INVALID_PATH)
        );

        let ingress = ingress();
        let record = MacosReplayEvent {
            event_id: 111,
            volume_relative_path: Path::new("/Library/Kick.wav"),
            flags: 0,
        };
        let large: Vec<_> = (0..=MAX_REPLAY_EVENTS)
            .map(|index| MacosReplayEvent {
                event_id: record.event_id + index as u64,
                ..record
            })
            .collect();
        record_callback_batch(&ingress, Path::new("/Volumes/Samples"), &large);
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.events().len(), MAX_REPLAY_EVENTS);
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::EVENT_LIMIT)
        );
    }
}
