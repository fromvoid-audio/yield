// OPT-1641 contract fixtures. Private test completions deliberately model receipts;
// they neither execute physical effects nor qualify production owner adapters.

fn move_progress(cross_source: bool) -> WorkProgress {
    if cross_source {
        cross_source_optional_progress()
    } else {
        let (participants, source, _, source_id) = valid_topology(false);
        claimed_progress(
            participants,
            namespace_file_move_args(source, source_id),
            1641,
        )
    }
}

fn apply_contract_key(progress: &mut WorkProgress, key: WorkUnitKey) {
    // Both source revisions are covered, including the destination's initial revision 8.
    let evidence = evidence_for(progress, key, 8);
    progress
        .apply_verified_completion(completion(progress, key, evidence))
        .expect("modeled completion");
}

#[test]
fn destination_first_and_origin_retirement_contract_table() {
    for cross_source in [false, true] {
        let mut progress = move_progress(cross_source);
        let mut previous = progress.clone();
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::DestinationSourceReconcile,
            WorkUnitRole::OriginRemovalStart,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::OriginSourceReconcile,
        ] {
            let key = key_for(&progress, 0, role);
            if role != WorkUnitRole::SourceRead {
                // Omit each prerequisite independently, rather than just checking the plan edges.
                let mut omitted = previous.clone();
                let evidence = evidence_for(&omitted, key, 8);
                let before = omitted.clone();
                assert!(
                    matches!(
                        omitted.apply_verified_completion(completion(&omitted, key, evidence)),
                        Err(WorkProgressError::PrerequisiteIncomplete { .. })
                    ),
                    "cross_source={cross_source}, role={role:?}"
                );
                assert_eq!(omitted, before);
            }
            if matches!(
                role,
                WorkUnitRole::OriginRemovalStart
                    | WorkUnitRole::OriginAbsence
                    | WorkUnitRole::OriginSourceReconcile
            ) {
                assert_eq!(progress.phase(), OperationPhase::FilesystemPublished);
            }
            if role == WorkUnitRole::OriginSourceReconcile {
                for downstream in [
                    WorkUnitRole::GlobalLibrary,
                    WorkUnitRole::Harvest,
                    WorkUnitRole::Projection,
                    WorkUnitRole::Readiness,
                ] {
                    let key = progress
                        .steps()
                        .iter()
                        .find(|step| step.key().role() == downstream)
                        .expect("required downstream role")
                        .key();
                    let mut premature = progress.clone();
                    assert!(matches!(
                        premature.apply_verified_completion(completion(
                            &premature,
                            key,
                            evidence_for(&premature, key, 8)
                        )),
                        Err(WorkProgressError::PrerequisiteIncomplete { .. })
                    ));
                    assert_eq!(premature, progress);
                }
            }
            previous = progress.clone();
            apply_contract_key(&mut progress, key);
        }
        assert_eq!(progress.phase(), OperationPhase::SourceReconciled);
        // Required global/Harvest, projection, and readiness cannot be skipped for success.
        assert_eq!(progress.disposition(), FullProgressDisposition::None);
        assert!(progress.steps().iter().any(|step| step.requirement()
            == ParticipantRequirement::Required
            && step.status() == ProgressStepStatus::Pending));
        for (role, expected_phase) in [
            (
                WorkUnitRole::GlobalLibrary,
                OperationPhase::SourceReconciled,
            ),
            (WorkUnitRole::Harvest, OperationPhase::GlobalReconciled),
            (
                WorkUnitRole::Projection,
                OperationPhase::ProjectionPublished,
            ),
            (WorkUnitRole::Readiness, OperationPhase::ReadinessScheduled),
        ] {
            let keys = progress
                .steps()
                .iter()
                .filter(|step| step.key().role() == role)
                .map(WorkProgressStep::key)
                .collect::<Vec<_>>();
            for key in keys {
                apply_contract_key(&mut progress, key);
            }
            assert_eq!(progress.phase(), expected_phase);
        }
        // Observed completions alone cannot settle capacity or manufacture terminal success.
        assert_eq!(progress.disposition(), FullProgressDisposition::None);
    }
}

#[test]
fn move_checkpoint_exact_replay_and_conflicting_receipt_contract() {
    for cross_source in [false, true] {
        let mut progress = move_progress(cross_source);
        for role in [
            WorkUnitRole::SourceRead,
            WorkUnitRole::Prepare,
            WorkUnitRole::Stage,
            WorkUnitRole::CopyValidated,
            WorkUnitRole::Publish,
            WorkUnitRole::DestinationSourceReconcile,
            WorkUnitRole::OriginRemovalStart,
            WorkUnitRole::OriginAbsence,
            WorkUnitRole::OriginSourceReconcile,
        ] {
            let key = key_for(&progress, 0, role);
            let evidence = evidence_for(&progress, key, 8);
            progress
                .apply_verified_completion(completion(&progress, key, evidence.clone()))
                .expect("first receipt");
            let before = progress.clone();
            assert_eq!(
                progress.apply_verified_completion(completion(&progress, key, evidence.clone())),
                Ok(ProgressApplyOutcome::Replay)
            );
            assert_eq!(progress, before);
            let mut conflict = completion(&progress, key, evidence);
            conflict.execution_epoch += 1;
            assert_eq!(
                progress.apply_verified_completion(conflict),
                Err(WorkProgressError::CompletionReplayConflict { key })
            );
            assert_eq!(progress, before);
        }
    }
}

#[test]
fn origin_removal_intent_restart_retains_preconditions_without_authority() {
    let (participants, source, _, source_id) = valid_topology(false);
    let args = namespace_file_move_args(source, source_id);
    let mut progress = claimed_progress(participants.clone(), args.clone(), 1642);
    for role in [
        WorkUnitRole::SourceRead,
        WorkUnitRole::Prepare,
        WorkUnitRole::Stage,
        WorkUnitRole::CopyValidated,
        WorkUnitRole::Publish,
        WorkUnitRole::DestinationSourceReconcile,
        WorkUnitRole::OriginRemovalStart,
    ] {
        let key = key_for(&progress, 0, role);
        apply_contract_key(&mut progress, key);
    }
    let intent = snapshot_intent_for(&progress, args, participants);
    let snapshot = progress
        .capture_full_progress_snapshot(&intent)
        .expect("snapshot");
    let record = super::super::super::full_progress_codec::StoredFullProgressRecord::new(snapshot)
        .expect("record");
    let bytes = super::super::super::full_progress_codec::encode(&record).expect("encode");
    let decoded = super::super::super::full_progress_codec::decode(&bytes).expect("decode");
    let mut restored = decoded
        .snapshot()
        .rehydrate_observation()
        .expect("retained observation");
    assert_eq!(restored.steps(), progress.steps());
    assert_eq!(restored.phase(), OperationPhase::FilesystemPublished);
    let retirement = key_for(&restored, 0, WorkUnitRole::OriginSourceReconcile);
    let before = restored.clone();
    assert!(matches!(
        restored.apply_verified_completion(completion(
            &restored,
            retirement,
            evidence_for(&restored, retirement, 8)
        )),
        Err(WorkProgressError::PrerequisiteIncomplete { .. })
    ));
    assert_eq!(restored, before);
    // Rehydration grants no filesystem, source lease, or completion authority. Only this
    // cfg(test) fixture can fabricate the modeled private completion used above.
}

#[test]
fn watcher_gap_audits_move_without_retiring_origin_or_losing_evidence() {
    let mut progress = move_progress(true);
    for role in [
        WorkUnitRole::SourceRead,
        WorkUnitRole::Prepare,
        WorkUnitRole::Stage,
        WorkUnitRole::CopyValidated,
        WorkUnitRole::Publish,
        WorkUnitRole::DestinationSourceReconcile,
        WorkUnitRole::OriginRemovalStart,
    ] {
        let key = key_for(&progress, 0, role);
        apply_contract_key(&mut progress, key);
    }
    let steps = progress.steps().to_vec();
    progress
        .require_audit(ProgressAuditObservation::new(
            ProgressAuditReason::WatcherContinuityUnproven,
        ))
        .expect("audit hold");
    assert_eq!(progress.steps(), steps);
    assert_eq!(progress.phase(), OperationPhase::FilesystemPublished);
    assert_eq!(
        progress.disposition(),
        FullProgressDisposition::AuditRequired
    );
    let absence = key_for(&progress, 0, WorkUnitRole::OriginAbsence);
    assert!(matches!(
        progress.apply_verified_completion(completion(
            &progress,
            absence,
            evidence_for(&progress, absence, 8)
        )),
        Err(WorkProgressError::ProgressSuspended { .. })
    ));
    assert_eq!(progress.steps(), steps);
}
