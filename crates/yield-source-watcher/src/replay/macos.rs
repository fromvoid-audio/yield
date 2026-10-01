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
                            // Finder writes this exact metadata filename while browsing.
                            // Only ignore a known file event: a directory with this name,
                            // an audio filename beginning with it, or uncertain delivery
                            // must still reach authoritative reconciliation.
                            (name == ".DS_Store"
                                && flags
                                    & (fs::kFSEventStreamEventFlagItemCreated
                                        | fs::kFSEventStreamEventFlagItemRemoved
                                        | fs::kFSEventStreamEventFlagItemRenamed)
                                    == 0)
                                || ingress
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
        AdapterDisposition, BootstrapReplayEvidence, RawObservationLimits,
        ReconciliationAdmissionLimits, ReconciliationAdmissionOwner,
        ReconciliationAdmissionSupervisor, RootIdentity, WatcherGeneration,
    };
    use yield_library::sample_sources::{
        SourceDatabase, SourceId, SourceWriter, SourceWriterError,
    };

    use super::*;
    use crate::SourceWatchSpec;
    use crate::replay::{BackendStreamIdentity, ReplayUncertainty};

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
    fn finder_metadata_file_advances_cursor_without_reconciliation() {
        let ingress = ingress();
        record_callback_batch(
            &ingress,
            Path::new("/Volumes/Samples"),
            &[MacosReplayEvent {
                event_id: 108,
                volume_relative_path: Path::new("Library/.DS_Store"),
                flags: fs::kFSEventStreamEventFlagItemModified
                    | fs::kFSEventStreamEventFlagItemIsFile,
            }],
        );
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert!(candidate.events().is_empty());
        assert_eq!(candidate.last_seen(), Some(108));
        assert!(candidate.history_complete());
        assert!(candidate.uncertainty().is_empty());
    }

    #[test]
    fn finder_metadata_filter_preserves_audio_directories_and_uncertain_delivery() {
        for (path, flags) in [
            (
                "Library/.DS_Store.wav",
                fs::kFSEventStreamEventFlagItemIsFile,
            ),
            ("Library/.DS_Store", fs::kFSEventStreamEventFlagItemIsDir),
            (
                "Library/.DS_Store",
                fs::kFSEventStreamEventFlagItemIsFile | fs::kFSEventStreamEventFlagItemCreated,
            ),
            (
                "Library/.DS_Store",
                fs::kFSEventStreamEventFlagItemIsFile | fs::kFSEventStreamEventFlagItemRemoved,
            ),
            (
                "Library/.DS_Store",
                fs::kFSEventStreamEventFlagItemIsFile | fs::kFSEventStreamEventFlagItemRenamed,
            ),
            ("Library/.DS_Store", 0),
        ] {
            let ingress = ingress();
            record_callback_batch(
                &ingress,
                Path::new("/Volumes/Samples"),
                &[MacosReplayEvent {
                    event_id: 108,
                    volume_relative_path: Path::new(path),
                    flags: flags | fs::kFSEventStreamEventFlagItemModified,
                }],
            );
            assert_eq!(ingress.into_candidate().events().len(), 1, "{path}");
        }
        let ingress = ingress();
        record_callback_batch(
            &ingress,
            Path::new("/Volumes/Samples"),
            &[MacosReplayEvent {
                event_id: 108,
                volume_relative_path: Path::new("Library/.DS_Store"),
                flags: fs::kFSEventStreamEventFlagItemIsFile
                    | fs::kFSEventStreamEventFlagUserDropped,
            }],
        );
        assert!(
            ingress
                .into_candidate()
                .uncertainty()
                .contains(ReplayUncertainty::DELIVERY_LOST)
        );
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
                .admit_replay_with_durable_prior(batch.clone(), None, continuous)
                .expect("existing owner replay route");
            assert!(
                admission.audit_request().is_some(),
                "no durable prior: {action}"
            );
            if matches!(action, "create" | "duplicate" | "rename") {
                let repeated = owner
                    .admit_replay_with_durable_prior(batch, None, continuous)
                    .expect("repeated callback admission");
                assert_eq!(
                    repeated.admission().disposition(),
                    AdapterDisposition::DuplicateSuppressed,
                    "duplicate {action} delivery must not dispatch another scan"
                );
            }
        }
    }

    #[test]
    fn disposable_profile_reports_finder_replay_continuity_and_audit() {
        let _runtime = yield_library::test_runtime::TestRuntimeGuard::acquire();
        let temporary_root =
            std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp root");
        let base = tempfile::tempdir_in(temporary_root).expect("isolated config base");
        let _config_base = ConfigBaseGuard::set(base.path().to_path_buf());
        let _profile = PersistenceProfileGuard::named("finder-fixture-profile");
        let profile = WritableProfileGuard::acquire_current().expect("isolated profile owner");
        let source = tempfile::tempdir().expect("disposable volume root");
        let volume_root = std::fs::canonicalize(source.path()).expect("canonical volume root");
        let source_root = volume_root.join("Library");
        std::fs::create_dir(&source_root).expect("disposable Finder library root");
        let source_root = std::fs::canonicalize(source_root).expect("canonical source root");
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
        let (audit, _) = audit
            .commit_with_manifest_snapshot_and_audit(initial.root_identity().clone(), None)
            .expect("committed source revision");
        let fence = writer.current_scan_fence().expect("source fence");
        let stream = BackendStreamIdentity::from_fsevents_volume(17, [7; 16])
            .expect("synthetic fixture stream");
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
        let bootstrap = BootstrapReplayEvidence::from_certain_empty_capture(
            source_id.clone(),
            fence.root_identity().clone(),
            stream.clone(),
            7,
            lane.generation(),
            100,
            100,
            true,
            false,
            0,
        )
        .expect("complete-audit capture boundary");
        let prior = writer
            .commit_bootstrap_replay_checkpoint(&fence, &audit, &bootstrap)
            .expect("durable Finder replay prior");

        let fixture = include_str!("fixtures/finder_v1.tsv");
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
        .expect("disposable profile ingress");
        let mut fixture_events = 0usize;
        for line in fixture.lines().filter(|line| !line.starts_with('#')) {
            let columns: Vec<_> = line.split('\t').collect();
            assert_eq!(columns.len(), 5, "fixture schema");
            if columns[3] == "unknown-event" {
                continue;
            }
            let flags = match columns[3] {
                "created-file" => {
                    fs::kFSEventStreamEventFlagItemCreated | fs::kFSEventStreamEventFlagItemIsFile
                }
                "renamed-file" => {
                    fs::kFSEventStreamEventFlagItemRenamed | fs::kFSEventStreamEventFlagItemIsFile
                }
                "removed-file" => {
                    fs::kFSEventStreamEventFlagItemRemoved | fs::kFSEventStreamEventFlagItemIsFile
                }
                other => panic!("unknown Finder fixture flag shape: {other}"),
            };
            record_callback_batch(
                &ingress,
                &volume_root,
                &[MacosReplayEvent {
                    event_id: columns[1].parse().expect("fixture event ID"),
                    volume_relative_path: Path::new(columns[2]),
                    flags,
                }],
            );
            fixture_events += 1;
        }
        ingress.mark_history_complete();
        let candidate = ingress.into_candidate();
        assert_eq!(candidate.events().len(), fixture_events);
        assert!(candidate.uncertainty().is_empty());
        let handoff = candidate
            .into_observation_batch(7, lane.generation())
            .expect("supported Finder fixture reaches reconciliation");
        let (batch, continuous) = handoff.into_admission_parts();
        assert!(continuous);
        let admission = owner
            .admit_replay_with_durable_prior(batch.clone(), Some(&prior), continuous)
            .expect("durable-prior Finder replay admission");
        let ticket = match admission.admission().outcome() {
            yield_library::sample_sources::reconciliation::AdmissionOutcome::Accepted(ticket) => {
                *ticket
            }
            outcome => panic!("expected Finder replay ticket, got {outcome:?}"),
        };
        let dispatched = owner.dispatch_next().expect("dispatch Finder replay");
        let proof = dispatched
            .normalized()
            .proof()
            .watcher_continuity()
            .expect("source-bound replay proof")
            .clone();
        assert_eq!(proof.source_id(), &source_id);
        assert_eq!(proof.root_identity(), fence.root_identity());
        assert_eq!(proof.backend_stream_identity(), &stream);
        assert_eq!(proof.watcher_generation(), lane.generation());
        assert_eq!(proof.prior_acknowledgement().sequence(), 100);
        assert!(proof.replay_coverage().is_contiguous());
        assert_eq!(proof.replay_coverage().after_sequence(), 100);
        assert_eq!(proof.replay_coverage().through_sequence(), 106);
        owner
            .mark_dispatched(ticket)
            .expect("Finder replay dispatched");
        owner.mark_applied(ticket).expect("Finder replay applied");
        let mut manifest = writer
            .catalog_database()
            .write_batch()
            .expect("Finder manifest batch");
        let event_paths: Vec<_> = batch
            .observations()
            .iter()
            .flat_map(|observation| observation.paths())
            .collect();
        for (index, path) in event_paths.iter().enumerate() {
            manifest
                .upsert_file(path.path(), 100 + index as u64, 1)
                .expect("bounded fixture path");
        }
        let (committed_manifest, committed_prior) = writer
            .commit_watcher_replay_manifest_batch(&fence, manifest, &prior, &proof)
            .expect("atomic Finder facts and replay barrier");
        assert_eq!(committed_manifest.revision, fence.revision() + 1);
        assert_eq!(committed_prior.acknowledged_sequence(), 106);
        assert_eq!(event_paths.len(), 6);
        let revision_after_commit = writer
            .current_scan_fence()
            .expect("committed fence")
            .revision();
        assert_eq!(owner.supervisor().in_flight(), 1);

        // Restart after the transaction, before this process acknowledges or retires the ticket.
        drop(owner);
        drop(writer);
        let reopened = SourceDatabase::open_for_catalog_with_profile_guard(
            &source_root,
            &profile,
            Path::new("finder-fixture-owned-source"),
        )
        .expect("reopen after pre-ack restart");
        let writer = SourceWriter::new_for_scan(reopened, source_id.clone(), 7)
            .expect("reopened Finder source writer");
        let reopened_fence = writer.current_scan_fence().expect("reopened source fence");
        assert_eq!(reopened_fence.revision(), revision_after_commit);
        assert_eq!(
            writer
                .catalog_database()
                .read_durable_replay_prior(
                    &source_id,
                    fence.root_identity(),
                    WatcherGeneration::new(7),
                    lane.generation(),
                )
                .expect("reopened Finder prior")
                .expect("committed prior survives restart"),
            committed_prior
        );
        for path in &event_paths {
            assert!(
                writer
                    .catalog_database()
                    .entry_for_path(path.path())
                    .expect("read Finder fixture projection")
                    .is_some()
            );
        }
        let last_good_entry = writer
            .catalog_database()
            .entry_for_path(event_paths[0].path())
            .expect("snapshot last-good row")
            .expect("committed last-good row");
        let last_good_row = (
            last_good_entry.relative_path,
            last_good_entry.file_size,
            last_good_entry.modified_ns,
            last_good_entry.content_hash,
        );

        // A duplicate of the already-committed proof remains fenced after a real database
        // reopen. Use a fresh batch and assert both the checkpoint and last-good row survive.
        let mut duplicate = writer
            .catalog_database()
            .write_batch()
            .expect("duplicate replay batch after reopen");
        duplicate
            .upsert_file(event_paths[0].path(), 777, 1)
            .expect("duplicate fixture path");
        assert!(matches!(
            writer.commit_watcher_replay_manifest_batch(&reopened_fence, duplicate, &prior, &proof),
            Err(SourceWriterError::ReplayCheckpointMismatch)
        ));
        assert_eq!(
            writer
                .current_scan_fence()
                .expect("duplicate fence")
                .revision(),
            revision_after_commit
        );
        assert_eq!(
            writer
                .catalog_database()
                .read_durable_replay_prior(
                    &source_id,
                    fence.root_identity(),
                    WatcherGeneration::new(7),
                    lane.generation(),
                )
                .expect("duplicate checkpoint readback")
                .expect("committed prior survives duplicate"),
            committed_prior
        );
        let duplicate_row = writer
            .catalog_database()
            .entry_for_path(event_paths[0].path())
            .expect("last-good row after duplicate")
            .expect("duplicate leaves last-good row");
        assert_eq!(
            (
                duplicate_row.relative_path,
                duplicate_row.file_size,
                duplicate_row.modified_ns,
                duplicate_row.content_hash,
            ),
            last_good_row
        );

        // Prepare a current, contiguous 107..108 proof before introducing uncertainty. Its
        // later rejection therefore demonstrates the audit marker fence, not a stale cursor.
        let next = ReplayIngress::new(
            SourceWatchSpec::new(
                source_id.clone(),
                7,
                source_root.clone(),
                Some(fence.root_identity().clone()),
            ),
            17,
            [7; 16],
            committed_prior.acknowledged_sequence(),
        )
        .expect("contiguous next replay ingress");
        record_callback_batch(
            &next,
            &volume_root,
            &[
                MacosReplayEvent {
                    event_id: 107,
                    volume_relative_path: Path::new("Library/next-a.wav"),
                    flags: fs::kFSEventStreamEventFlagItemCreated
                        | fs::kFSEventStreamEventFlagItemIsFile,
                },
                MacosReplayEvent {
                    event_id: 108,
                    volume_relative_path: Path::new("Library/next-b.wav"),
                    flags: fs::kFSEventStreamEventFlagItemCreated
                        | fs::kFSEventStreamEventFlagItemIsFile,
                },
            ],
        );
        next.mark_history_complete();
        let next_candidate = next.into_candidate();
        assert!(next_candidate.uncertainty().is_empty());
        let next_handoff = next_candidate
            .into_observation_batch(7, lane.generation())
            .expect("current contiguous proof candidate");
        let (next_batch, next_continuous) = next_handoff.into_admission_parts();
        assert!(next_continuous);
        let mut next_owner =
            ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
        let next_lane = next_owner
            .begin_source(source_id.clone(), fence.root_identity().clone())
            .expect("current next-replay lane");
        assert_eq!(next_lane.generation(), lane.generation());
        let next_admission = next_owner
            .admit_replay_with_durable_prior(
                next_batch.clone(),
                Some(&committed_prior),
                next_continuous,
            )
            .expect("contiguous current replay admission");
        let next_ticket = match next_admission.admission().outcome() {
            yield_library::sample_sources::reconciliation::AdmissionOutcome::Accepted(ticket) => {
                *ticket
            }
            outcome => panic!("expected contiguous replay ticket, got {outcome:?}"),
        };
        let next_dispatched = next_owner
            .dispatch_next()
            .expect("dispatch contiguous replay");
        let next_proof = next_dispatched
            .normalized()
            .proof()
            .watcher_continuity()
            .expect("contiguous current proof")
            .clone();
        assert_eq!(next_proof.prior_acknowledgement().sequence(), 106);
        assert!(next_proof.replay_coverage().is_contiguous());
        assert_eq!(next_proof.replay_coverage().after_sequence(), 106);
        assert_eq!(next_proof.replay_coverage().through_sequence(), 108);
        next_owner
            .mark_dispatched(next_ticket)
            .expect("next replay dispatched");
        next_owner
            .mark_applied(next_ticket)
            .expect("next replay applied");

        // The unsupported Finder shape re-enters through the same isolated owner, widens to a
        // source audit, and cannot reuse the previous proof or advance the stored checkpoint.
        let unsupported = ReplayIngress::new(
            SourceWatchSpec::new(
                source_id.clone(),
                7,
                source_root.clone(),
                Some(fence.root_identity().clone()),
            ),
            17,
            [7; 16],
            committed_prior.acknowledged_sequence(),
        )
        .expect("uncertain replay ingress");
        let unsupported_row = fixture
            .lines()
            .filter(|line| !line.starts_with('#'))
            .map(|line| line.split('\t').collect::<Vec<_>>())
            .find(|columns| columns[3] == "unknown-event")
            .expect("unsupported Finder fixture row");
        let unsupported_event = MacosReplayEvent {
            // The redacted fixture supplies the unsupported shape; place it after the
            // independently prepared, otherwise-valid 107..108 candidate interval.
            event_id: unsupported_row[1]
                .parse::<u64>()
                .expect("unsupported event ID")
                + 2,
            volume_relative_path: Path::new(unsupported_row[2]),
            flags: 1 << 31,
        };
        record_callback_batch(&unsupported, &volume_root, &[unsupported_event]);
        unsupported.mark_history_complete();
        let candidate = unsupported.into_candidate();
        assert!(
            candidate
                .uncertainty()
                .contains(ReplayUncertainty::UNKNOWN_EVENT)
        );
        let uncertain_handoff = candidate
            .into_observation_batch(7, lane.generation())
            .expect("unsupported fixture reaches audit route");
        let (uncertain_batch, uncertain_continuous) = uncertain_handoff.into_admission_parts();
        assert!(!uncertain_continuous);
        let mut owner =
            ReconciliationAdmissionOwner::new(ReconciliationAdmissionSupervisor::new(limits));
        let current_lane = owner
            .begin_source(source_id.clone(), fence.root_identity().clone())
            .expect("current audit lane after restart");
        assert_eq!(current_lane.generation(), lane.generation());
        let admission = owner
            .admit_replay_with_durable_prior(
                uncertain_batch,
                Some(&committed_prior),
                uncertain_continuous,
            )
            .expect("unsupported replay is explicitly uncertain");
        let audit_request = admission
            .audit_request()
            .expect("unsupported shape requests source audit");
        assert_eq!(audit_request.source_id(), &source_id);
        assert_eq!(audit_request.root_identity(), fence.root_identity());
        let marker = owner
            .durable_audit_marker_for_replay(&admission)
            .expect("current-lane source-wide marker");
        let retained = writer
            .retain_watcher_replay_uncertainty(&reopened_fence, &marker)
            .expect("persist source-audit uncertainty");
        assert_eq!(retained.source_revision(), reopened_fence.revision());
        assert_eq!(retained.source_id(), &source_id);
        assert_eq!(retained.root_identity(), fence.root_identity());
        assert_eq!(
            retained.backend_stream_identity(),
            None,
            "the durable source-wide audit marker does not bind one callback stream"
        );
        assert_eq!(
            retained.watcher_generation(),
            Some(current_lane.generation())
        );
        assert_eq!(
            writer
                .catalog_database()
                .read_durable_replay_uncertainty()
                .expect("durable uncertainty state"),
            DurableReplayUncertaintyState::Retained(retained.clone())
        );
        let checkpoint = writer
            .catalog_database()
            .get_metadata(yield_library::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
            .expect("read checkpoint metadata")
            .expect("prior checkpoint remains stored");
        assert!(checkpoint.contains("\"event_id\":106"));
        assert!(
            writer
                .catalog_database()
                .read_durable_replay_prior(
                    &source_id,
                    fence.root_identity(),
                    WatcherGeneration::new(7),
                    current_lane.generation(),
                )
                .expect("uncertain prior is blocked")
                .is_none()
        );
        let last_good_revision = writer.current_scan_fence().expect("audit fence").revision();
        assert_eq!(last_good_revision, revision_after_commit);
        let mut forbidden = writer
            .catalog_database()
            .write_batch()
            .expect("rejected uncertainty publication batch");
        for observation in next_batch.observations() {
            for path in observation.paths() {
                forbidden
                    .upsert_file(path.path(), 900, 9)
                    .expect("otherwise-valid next source fact");
            }
        }
        assert!(matches!(
            writer.commit_watcher_replay_manifest_batch(
                &reopened_fence,
                forbidden,
                &committed_prior,
                &next_proof
            ),
            Err(SourceWriterError::ReplayCheckpointMismatch)
        ));
        assert_eq!(
            writer.current_scan_fence().unwrap().revision(),
            revision_after_commit
        );
        assert!(
            writer
                .catalog_database()
                .entry_for_path(Path::new("next-a.wav"))
                .expect("rejected next replay projection")
                .is_none()
        );
        let uncertain_row = writer
            .catalog_database()
            .entry_for_path(event_paths[0].path())
            .expect("last-good row after uncertainty")
            .expect("uncertainty leaves last-good row");
        assert_eq!(
            (
                uncertain_row.relative_path,
                uncertain_row.file_size,
                uncertain_row.modified_ns,
                uncertain_row.content_hash,
            ),
            last_good_row
        );
        assert_eq!(
            writer
                .catalog_database()
                .get_metadata(yield_library::sample_sources::db::META_SOURCE_WATCHER_CHECKPOINT)
                .expect("checkpoint after blocked valid proof"),
            Some(checkpoint.clone())
        );

        let valid_report = finder_evidence_json(
            "valid_finder_replay",
            lane.generation().get(),
            100,
            100,
            101,
            106,
            Some(106),
            106,
            true,
            fence.revision(),
            revision_after_commit,
            "none",
            "not_required",
            "not_applicable",
            "committed_once; durable_after_pre_ack_restart",
        );
        let uncertain_report = finder_evidence_json(
            "unsupported_finder_shape",
            current_lane.generation().get(),
            committed_prior.acknowledged_sequence(),
            committed_prior.acknowledged_sequence(),
            107,
            108,
            Some(108),
            committed_prior.acknowledged_sequence(),
            false,
            revision_after_commit,
            last_good_revision,
            "unknown_event",
            "retained_source_audit",
            "omitted_source_wide_marker",
            "checkpoint_not_advanced; last_good_projection_retained",
        );
        for report in [&valid_report, &uncertain_report] {
            assert!(report.len() <= 2048, "redacted evidence remains bounded");
            assert!(!report.contains(source_root.to_string_lossy().as_ref()));
            assert!(
                !report.contains("kick.wav"),
                "raw Finder paths are not reported"
            );
            println!("YIELD_FINDER_CONTINUITY_EVIDENCE={report}");
        }
        assert_eq!(
            writer
                .catalog_database()
                .read_durable_replay_uncertainty()
                .expect("source-owned readback"),
            DurableReplayUncertaintyState::Retained(retained)
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn finder_evidence_json(
        case: &str,
        watcher_generation: u64,
        prior_ack: u64,
        started_after: u64,
        first_sequence: u64,
        last_sequence: u64,
        covered_through: Option<u64>,
        ack_after: u64,
        barrier_advanced: bool,
        source_revision_before: u64,
        source_revision_after: u64,
        uncertainty: &str,
        audit_disposition: &str,
        audit_marker_stream_identity: &str,
        outcome: &str,
    ) -> String {
        let covered_region = covered_through.map_or_else(
            || "null".to_owned(),
            |through| {
                format!("{{\"after\":{prior_ack},\"through\":{through},\"contiguous\":true}}")
            },
        );
        format!(
            "{{\"schema\":\"yield.finder-continuity-evidence.v1\",\
             \"case\":\"{case}\",\
             \"source_identity_ref\":\"fixture-source-01\",\
             \"root_identity_ref\":\"fixture-root-01\",\
             \"stream_identity\":{{\"backend\":\"fsevents\",\"device\":17,\
             \"volume_uuid\":\"redacted-fixture-01\",\"matched\":true}},\
             \"audit_marker_stream_identity\":\"{audit_marker_stream_identity}\",\
             \"lifecycle_generation\":7,\"watcher_generation\":{watcher_generation},\
             \"cursor\":{{\"prior_ack\":{prior_ack},\"started_after\":{started_after},\
             \"first_sequence\":{first_sequence},\"last_sequence\":{last_sequence},\
             \"durable_ack_after\":{ack_after}}},\
             \"barrier\":{{\"before\":{prior_ack},\"after\":{ack_after},\
             \"advanced\":{barrier_advanced}}},\
             \"covered_region\":{covered_region},\
             \"source_revision\":{{\"before\":{source_revision_before},\
             \"after\":{source_revision_after}}},\
             \"uncertainty\":[\"{uncertainty}\"],\
             \"audit_disposition\":\"{audit_disposition}\",\
             \"outcome\":\"{outcome}\",\
             \"restart\":\"actual_profile_sqlite_reopen_before_case\",\
             \"native_fsevents\":\"blocked_not_exercised_synthetic_fixture_only\",\
             \"native_ui\":\"blocked_all_UI_controls_paused\"}}"
        )
    }
}
