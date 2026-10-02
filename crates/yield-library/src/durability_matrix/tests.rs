use super::*;

fn synthetic(platform: Platform, volume: VolumeClass) -> QualificationRow {
    QualificationRow {
        scope_reference: format!("synthetic-only/{platform:?}/{volume:?}/fixture-build"),
        platform,
        volume,
        provenance: Provenance::SyntheticFixture,
        layout: VolumeLayout::AllOneVolume,
        transfer: Transfer::QualifiedAtomic,
        evidence: ALL_EVIDENCE
            .into_iter()
            .map(|kind| EvidenceRecord {
                kind,
                status: EvidenceStatus::Verified,
                reference: format!("synthetic-only/{kind:?}"),
            })
            .collect(),
    }
}

fn change(row: &mut QualificationRow, kind: EvidenceKind, status: EvidenceStatus) {
    row.evidence
        .iter_mut()
        .find(|record| record.kind == kind)
        .unwrap()
        .status = status;
}

#[test]
fn every_platform_medium_requires_its_own_synthetic_evidence() {
    for platform in [Platform::MacOs, Platform::Windows, Platform::Linux] {
        for volume in [
            VolumeClass::LocalFixed,
            VolumeClass::Removable,
            VolumeClass::Remote,
        ] {
            let row = synthetic(platform, volume);
            let result = row.assess().unwrap();
            assert_eq!(result.disposition, AssessmentDisposition::FixtureComplete);
            assert!(
                result.visibility_verified
                    && result.atomic_namespace
                    && result.whole_publication_atomic
            );
            assert_eq!(
                result.synchronization,
                SynchronizationClaim::PowerLossSynchronized
            );
            assert!(result.unqualified.is_empty());
            let mut unobserved = row;
            unobserved.evidence.clear();
            let result = unobserved.assess().unwrap();
            assert_eq!(
                result.disposition,
                AssessmentDisposition::PublicationBlocked
            );
            assert_eq!(
                result.synchronization,
                SynchronizationClaim::SyncUnsupportedOrUnverified
            );
            assert_eq!(result.unqualified.len(), ALL_EVIDENCE.len());
        }
    }
}

#[test]
fn bytewise_and_cross_volume_never_inherit_whole_atomicity() {
    for (layout, transfer) in [
        (VolumeLayout::AllOneVolume, Transfer::Bytewise),
        (
            VolumeLayout::DestinationLocalStaging,
            Transfer::QualifiedAtomic,
        ),
        (VolumeLayout::DestinationLocalStaging, Transfer::Bytewise),
        (VolumeLayout::AllOneVolume, Transfer::Unqualified),
    ] {
        let mut row = synthetic(Platform::Windows, VolumeClass::LocalFixed);
        row.layout = layout;
        row.transfer = transfer;
        let result = row.assess().unwrap();
        assert!(result.visibility_verified && result.atomic_namespace);
        assert!(!result.whole_publication_atomic);
    }
    let mut row = synthetic(Platform::MacOs, VolumeClass::LocalFixed);
    row.layout = VolumeLayout::Unverified;
    let result = row.assess().unwrap();
    assert_eq!(
        result.disposition,
        AssessmentDisposition::PublicationBlocked
    );
    assert!(!result.atomic_namespace && !result.whole_publication_atomic);
}

#[test]
fn final_visibility_is_not_no_replace_or_sync_qualification() {
    let mut row = synthetic(Platform::MacOs, VolumeClass::LocalFixed);
    change(
        &mut row,
        EvidenceKind::NoReplaceFinalClaim,
        EvidenceStatus::Unsupported,
    );
    let result = row.assess().unwrap();
    assert!(result.visibility_verified);
    assert!(!result.atomic_namespace && !result.whole_publication_atomic);
    assert_eq!(
        result.disposition,
        AssessmentDisposition::PublicationBlocked
    );
    assert_eq!(
        result.synchronization,
        SynchronizationClaim::SyncUnsupportedOrUnverified
    );
}

#[test]
fn each_publication_barrier_blocks_on_missing_failed_or_unsupported_evidence() {
    for kind in [
        EvidenceKind::BoundObjectIdentity,
        EvidenceKind::OwnerFences,
        EvidenceKind::CopyValidatedCheckpoint,
        EvidenceKind::NoReplaceFinalClaim,
        EvidenceKind::ReopenedVisibility,
    ] {
        for status in [
            EvidenceStatus::Missing,
            EvidenceStatus::Failed,
            EvidenceStatus::Unsupported,
        ] {
            let mut row = synthetic(Platform::Windows, VolumeClass::LocalFixed);
            change(&mut row, kind, status);
            let result = row.assess().unwrap();
            assert_eq!(
                result.disposition,
                AssessmentDisposition::PublicationBlocked
            );
            assert!(!result.atomic_namespace && !result.whole_publication_atomic);
            assert!(result.unqualified.contains(&kind));
        }
    }
}

#[test]
fn cleanup_rollback_wal_and_replay_are_independent_required_evidence() {
    for kind in [
        EvidenceKind::IndependentCleanup,
        EvidenceKind::Rollback,
        EvidenceKind::WalRecovery,
        EvidenceKind::CrashRecovery,
    ] {
        let mut row = synthetic(Platform::Linux, VolumeClass::LocalFixed);
        change(&mut row, kind, EvidenceStatus::Unsupported);
        let result = row.assess().unwrap();
        assert!(result.visibility_verified && result.atomic_namespace);
        assert_eq!(
            result.disposition,
            AssessmentDisposition::RecoveryUnqualified
        );
        assert_ne!(
            result.synchronization,
            SynchronizationClaim::PowerLossSynchronized
        );
        assert!(result.unqualified.contains(&kind));
    }
}

#[test]
fn fsync_flush_or_remote_ack_does_not_prove_media_power_loss() {
    for volume in [
        VolumeClass::LocalFixed,
        VolumeClass::Removable,
        VolumeClass::Remote,
    ] {
        let mut row = synthetic(Platform::MacOs, volume);
        change(
            &mut row,
            EvidenceKind::MediaPowerLoss,
            EvidenceStatus::Missing,
        );
        assert_eq!(
            row.assess().unwrap().synchronization,
            SynchronizationClaim::BestEffortSync
        );
        change(
            &mut row,
            EvidenceKind::FileSynchronization,
            EvidenceStatus::BestEffort,
        );
        change(
            &mut row,
            EvidenceKind::NamespaceSynchronization,
            EvidenceStatus::Unsupported,
        );
        let result = row.assess().unwrap();
        assert_eq!(result.disposition, AssessmentDisposition::FixtureComplete);
        assert_eq!(result.synchronization, SynchronizationClaim::BestEffortSync);
        assert!(
            result
                .unqualified
                .contains(&EvidenceKind::NamespaceSynchronization)
        );
        change(
            &mut row,
            EvidenceKind::FileSynchronization,
            EvidenceStatus::Failed,
        );
        assert_eq!(
            row.assess().unwrap().synchronization,
            SynchronizationClaim::SyncUnsupportedOrUnverified
        );
    }
}

#[test]
fn synthetic_and_observed_rows_remain_review_records_without_authority() {
    let mut row = synthetic(Platform::Windows, VolumeClass::LocalFixed);
    assert_eq!(
        row.assess().unwrap().disposition,
        AssessmentDisposition::FixtureComplete
    );
    row.provenance = Provenance::Observed;
    assert_eq!(
        row.assess().unwrap().disposition,
        AssessmentDisposition::ReadyForEvidenceReview
    );
    let encoded = serde_json::to_value(&row).unwrap();
    let decoded: QualificationRow = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(decoded, row);
    let mut unexpected = encoded;
    unexpected["runtime_grant"] = serde_json::json!("forbidden");
    assert!(serde_json::from_value::<QualificationRow>(unexpected).is_err());
}

#[test]
fn malformed_duplicate_overlong_and_partial_identity_records_are_rejected() {
    let mut row = synthetic(Platform::Linux, VolumeClass::LocalFixed);
    row.evidence.push(row.evidence[0].clone());
    assert_eq!(row.assess(), Err(MatrixError::TooManyRecords));
    row.evidence.pop();
    row.evidence[1] = row.evidence[0].clone();
    assert_eq!(
        row.assess(),
        Err(MatrixError::DuplicateCategory(
            EvidenceKind::BoundObjectIdentity
        ))
    );
    for reference in [
        String::new(),
        " ".into(),
        "report\nsecret".into(),
        "x".repeat(MAX_EVIDENCE_REFERENCE_BYTES + 1),
    ] {
        let mut row = synthetic(Platform::Linux, VolumeClass::LocalFixed);
        row.evidence[0].reference = reference;
        assert_eq!(
            row.assess(),
            Err(MatrixError::InvalidReference(
                EvidenceKind::BoundObjectIdentity
            ))
        );
    }
    let mut row = synthetic(Platform::Linux, VolumeClass::LocalFixed);
    change(
        &mut row,
        EvidenceKind::BoundObjectIdentity,
        EvidenceStatus::BestEffort,
    );
    assert_eq!(
        row.assess(),
        Err(MatrixError::InvalidBestEffort(
            EvidenceKind::BoundObjectIdentity
        ))
    );
}

#[test]
fn scope_and_atomic_sequence_evidence_cannot_be_omitted() {
    let mut row = synthetic(Platform::MacOs, VolumeClass::LocalFixed);
    row.scope_reference.clear();
    assert_eq!(row.assess(), Err(MatrixError::InvalidScope));
    row.scope_reference = "synthetic-only/macos/local".into();
    change(
        &mut row,
        EvidenceKind::WholeAtomicSequence,
        EvidenceStatus::Missing,
    );
    let result = row.assess().unwrap();
    assert!(result.atomic_namespace);
    assert!(!result.whole_publication_atomic);
    assert!(
        result
            .unqualified
            .contains(&EvidenceKind::WholeAtomicSequence)
    );
}
