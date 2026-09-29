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
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use yield_library::app_dirs::{ConfigBaseGuard, PersistenceProfileGuard, WritableProfileGuard};
    use yield_library::sample_sources::db::DurableReplayUncertaintyState;
    use yield_library::sample_sources::reconciliation::{
        AdapterDisposition, RawObservationLimits, ReconciliationAdmissionLimits,
        ReconciliationAdmissionOwner, ReconciliationAdmissionSupervisor, RootIdentity,
    };
    use yield_library::sample_sources::{SourceDatabase, SourceId, SourceWriter};

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
    fn overflow_restart_and_scan_overlap_replays_request_a_current_source_audit() {
        let cases = [
            (
                "overflow",
                fs::kFSEventStreamEventFlagUserDropped
                    | fs::kFSEventStreamEventFlagMustScanSubDirs
                    | fs::kFSEventStreamEventFlagItemIsDir,
                true,
            ),
            (
                "restart-before-history-complete",
                fs::kFSEventStreamEventFlagItemCreated | fs::kFSEventStreamEventFlagItemIsFile,
                false,
            ),
            (
                "scan-overlap-without-durable-prior",
                fs::kFSEventStreamEventFlagItemCreated | fs::kFSEventStreamEventFlagItemIsFile,
                true,
            ),
        ];
        for (scenario, flags, history_complete) in cases {
            let ingress = ReplayIngress::new(
                SourceWatchSpec::new(
                    SourceId::from_string("continuity-fixture-source"),
                    7,
                    PathBuf::from("/fixture-volume/Library"),
                    Some(RootIdentity::from_bytes(
                        b"continuity-fixture-root".to_vec(),
                    )),
                ),
                17,
                [7; 16],
                100,
            )
            .expect("bounded replay ingress");
            record_callback_batch(
                &ingress,
                Path::new("/fixture-volume"),
                &[MacosReplayEvent {
                    event_id: 101,
                    volume_relative_path: Path::new("Library/kick.wav"),
                    flags,
                }],
            );
            if history_complete {
                ingress.mark_history_complete();
            }
            let candidate = ingress.into_candidate();
            if scenario == "overflow" {
                assert!(
                    candidate
                        .uncertainty()
                        .contains(ReplayUncertainty::DELIVERY_LOST)
                );
                assert!(
                    candidate
                        .uncertainty()
                        .contains(ReplayUncertainty::MUST_SCAN_SUBDIRS)
                );
            }
            let limits = ReconciliationAdmissionLimits::new(
                1,
                RawObservationLimits::new(8, 4096, 4096).expect("lane bound"),
                RawObservationLimits::new(32, 16384, 16384).expect("global bound"),
                8,
                8,
                8,
            )
            .expect("owner bounds");
            let mut owner =
                ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
            let lane = owner
                .begin_source(
                    candidate.source_id().clone(),
                    candidate.root_identity().clone(),
                )
                .expect("current source lane");
            let handoff = candidate
                .into_observation_batch(7, lane.generation())
                .expect("raw event or uncertainty reaches owner");
            let (batch, continuous) = handoff.into_admission_parts();
            assert_eq!(
                continuous,
                scenario == "scan-overlap-without-durable-prior",
                "{scenario} continuity disposition"
            );
            let admitted = owner
                .admit_replay_with_durable_prior(batch.clone(), None, continuous)
                .expect("uncertain replay admission");
            assert!(admitted.audit_request().is_some(), "{scenario}");
            assert!(
                owner.durable_audit_marker_for_replay(&admitted).is_some(),
                "{scenario} must retain a durable source-wide barrier"
            );
            let repeated = owner
                .admit_replay_with_durable_prior(batch, None, continuous)
                .expect("duplicate replay admission");
            assert_eq!(
                repeated.admission().disposition(),
                AdapterDisposition::DuplicateSuppressed,
                "{scenario} duplicate must not dispatch another audit"
            );
        }
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

    #[test]
    fn redacted_finder_shapes_replay_through_current_source_owner() {
        let fixture = include_str!("fixtures/finder_v1.tsv");
        let mut actions: BTreeMap<&str, Vec<(&str, u64, &str, &str)>> = BTreeMap::new();
        for line in fixture.lines().filter(|line| !line.starts_with('#')) {
            let columns: Vec<_> = line.split('\t').collect();
            assert_eq!(columns.len(), 5, "fixture schema");
            let path = Path::new(columns[2]);
            assert!(!path.is_absolute(), "fixture must not retain machine paths");
            assert!(
                path.components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
            );
            actions.entry(columns[0]).or_default().push((
                columns[2],
                columns[1].parse().expect("stable event ID"),
                columns[3],
                columns[4],
            ));
        }
        assert_eq!(actions.len(), 6, "every Finder fixture action is present");

        for (action, events) in actions {
            let ingress = ReplayIngress::new(
                SourceWatchSpec::new(
                    SourceId::from_string("finder-fixture-source"),
                    3,
                    PathBuf::from("/fixture-volume/Library"),
                    Some(RootIdentity::from_bytes(b"finder-fixture-root".to_vec())),
                ),
                17,
                [7; 16],
                100,
            )
            .expect("disposable fixture ingress");
            for (path, id, shape, _) in &events {
                let flags = match *shape {
                    "created-file" => {
                        fs::kFSEventStreamEventFlagItemCreated
                            | fs::kFSEventStreamEventFlagItemIsFile
                    }
                    "renamed-file" => {
                        fs::kFSEventStreamEventFlagItemRenamed
                            | fs::kFSEventStreamEventFlagItemIsFile
                    }
                    "removed-file" => {
                        fs::kFSEventStreamEventFlagItemRemoved
                            | fs::kFSEventStreamEventFlagItemIsFile
                    }
                    "unknown-event" => 1 << 31,
                    other => panic!("unknown fixture flag shape: {other}"),
                };
                record_callback_batch(
                    &ingress,
                    Path::new("/fixture-volume"),
                    &[MacosReplayEvent {
                        event_id: *id,
                        volume_relative_path: Path::new(path),
                        flags,
                    }],
                );
            }
            ingress.mark_history_complete();
            let candidate = ingress.into_candidate();
            let expected: Vec<_> = events
                .iter()
                .filter(|(_, _, _, relative)| *relative != "-")
                .map(|(_, _, _, relative)| Path::new(relative))
                .collect();
            assert_eq!(candidate.events().len(), expected.len(), "{action}");
            for (event, relative) in candidate.events().iter().zip(expected) {
                assert_eq!(event.relative_path(), relative, "{action}");
                assert!(!event.relative_path().is_absolute(), "{action}");
            }
            assert_eq!(candidate.uncertainty().is_empty(), action != "unsupported");

            let limits = ReconciliationAdmissionLimits::new(
                1,
                RawObservationLimits::new(8, 4096, 4096).expect("lane bound"),
                RawObservationLimits::new(32, 16384, 16384).expect("global bound"),
                8,
                8,
                8,
            )
            .expect("owner bounds");
            let mut owner =
                ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
            let lane = owner
                .begin_source(
                    candidate.source_id().clone(),
                    candidate.root_identity().clone(),
                )
                .expect("current source lane");
            let handoff = candidate
                .into_observation_batch(3, lane.generation())
                .expect("callback paths or uncertainty must reach reconciliation");
            let (batch, continuous) = handoff.into_admission_parts();
            assert_eq!(continuous, action != "unsupported", "{action}");
            let admission = owner
                .admit_replay_with_durable_prior(batch, None, continuous)
                .expect("existing owner replay route");
            assert!(
                admission.audit_request().is_some(),
                "no durable prior: {action}"
            );
        }
    }

    #[test]
    fn disposable_profile_persists_owner_routed_finder_uncertainty() {
        let _runtime = yield_library::test_runtime::TestRuntimeGuard::acquire();
        let temporary_root =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = tempfile::tempdir_in(temporary_root).expect("isolated config base");
        let _config_base = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile = PersistenceProfileGuard::named("finder-fixture-profile");
        let profile = WritableProfileGuard::acquire_current().expect("isolated profile owner");
        let source = tempfile::tempdir().expect("disposable source root");
        let source_root = std::fs::canonicalize(source.path()).expect("canonical source root");
        let source_id = SourceId::from_string("finder-fixture-owned-source");
        let database = SourceDatabase::open_for_catalog_with_profile_guard(
            &source_root,
            &profile,
            Path::new("finder-fixture-owned-source"),
        )
        .expect("isolated source database");
        let writer = SourceWriter::new_for_scan(database, source_id.clone(), 7)
            .expect("source-owned writer");
        let initial = writer.current_scan_fence().expect("initial source fence");
        let mut audit = writer
            .catalog_database()
            .write_batch()
            .expect("isolated audit batch");
        audit.complete_manifest_audit(1).expect("complete audit");
        audit
            .commit_with_manifest_snapshot_and_audit(initial.root_identity().clone(), None)
            .expect("committed source revision");
        let fence = writer.current_scan_fence().expect("source fence");
        let ingress = ReplayIngress::new(
            SourceWatchSpec::new(
                source_id.clone(),
                7,
                source_root.clone(),
                Some(fence.root_identity().clone()),
            ),
            17,
            [7; 16],
            100,
        )
        .expect("physical source ingress");
        let changed_path = source_root.join("kick.wav");
        record_callback_batch(
            &ingress,
            Path::new("/"),
            &[MacosReplayEvent {
                event_id: 101,
                volume_relative_path: &changed_path,
                flags: fs::kFSEventStreamEventFlagItemCreated
                    | fs::kFSEventStreamEventFlagItemIsFile,
            }],
        );
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.events()[0].relative_path(), Path::new("kick.wav"));
        let limits = ReconciliationAdmissionLimits::new(
            1,
            RawObservationLimits::new(8, 4096, 4096).expect("lane bound"),
            RawObservationLimits::new(32, 16384, 16384).expect("global bound"),
            8,
            8,
            8,
        )
        .expect("owner bounds");
        let mut owner =
            ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
        let lane = owner
            .begin_source(source_id.clone(), fence.root_identity().clone())
            .expect("current source lane");
        let handoff = candidate
            .into_observation_batch(7, lane.generation())
            .expect("native callback reaches reconciliation");
        let (batch, continuous) = handoff.into_admission_parts();
        let admission = owner
            .admit_replay_with_durable_prior(batch, None, continuous)
            .expect("unproven owner replay admission");
        let marker = owner
            .durable_audit_marker_for_replay(&admission)
            .expect("source-wide current-lane audit marker");
        let retained = writer
            .retain_watcher_replay_uncertainty(&fence, &marker)
            .expect("source writer persists uncertainty");
        assert_eq!(retained.source_revision(), fence.revision());
        assert_eq!(retained.source_id(), &source_id);
        assert_eq!(
            writer
                .catalog_database()
                .read_durable_replay_uncertainty()
                .expect("source-owned readback"),
            DurableReplayUncertaintyState::Retained(retained)
        );
    }
}
