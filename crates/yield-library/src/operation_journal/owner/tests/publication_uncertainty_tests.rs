//! Original runtime/admission tests. Scalar activation supplies no native authority.
#[cfg(test)]
mod integration {
    use super::super::super::resource_custody::UseError;
    use super::super::*;
    use crate::sample_sources::db::SourceRegistrationId;

    fn assert_uncertainty_retains_original_fence(error: OperationJournalStoreError) {
        let fixture = fixture("publication-uncertainty-original-runtime");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, _) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        runtime.attach_resource_custody(&shared).unwrap();
        let original = runtime.resource_admission.clone();
        assert!(original.activate(|| true));
        let operation_id = OperationId::new();
        let source = SourceRegistrationId::model_key_for_tests(1);
        let fence = runtime
            .publication_fences
            .issue_for_source(operation_id, source, &shared)
            .unwrap();
        assert!(
            runtime
                .publication_write_result::<()>(&fence, Err(error))
                .is_err()
        );
        drop(fence);
        assert!(runtime.publication_fences.unresolved());
        assert!(matches!(
            runtime.publication_fences.close_for_mutation(operation_id),
            Err(OperationJournalOwnerError::PublicationMutationPending { .. })
        ));
        assert!(matches!(
            runtime.publication_fences.close_for_source(source),
            Err(OperationJournalOwnerError::PublicationSourceRetirementPending { .. })
        ));
        runtime
            .publication_fences
            .close_for_mutation(OperationId::new())
            .unwrap();
        runtime
            .publication_fences
            .close_for_source(SourceRegistrationId::model_key_for_tests(2))
            .unwrap();
        assert_eq!(original.validate_open(), Err(UseError::Unknown));
        assert!(matches!(
            original.begin_read(|| true),
            Err(UseError::Unknown)
        ));
        assert!(!original.activate(|| true));
        assert!(!original.close_and_wait());
        assert_eq!(original.validate_open(), Err(UseError::Unknown));
    }

    #[test]
    fn publication_uncertainty_original_runtime_gate_is_sticky_and_shutdown_unclean() {
        assert_uncertainty_retains_original_fence(
            OperationJournalStoreError::StagingOutcomeUncertain {
                reason: "injected unconfirmed durable outcome".into(),
            },
        );
    }

    #[test]
    fn publication_uncertainty_namespace_refusal_retains_original_fence() {
        assert_uncertainty_retains_original_fence(
            OperationJournalStoreError::StagingNamespaceOutcomeUncertain,
        );
    }

    #[test]
    fn publication_uncertainty_known_rejection_does_not_close_original_runtime_gate() {
        let fixture = fixture("publication-known-original-runtime");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let mut runtime = OwnerRuntime::new(store).unwrap();
        let (commands, _) = OwnerCommandReceivers::channel();
        let shared = Arc::new(OwnerShared::new(commands));
        runtime.attach_resource_custody(&shared).unwrap();
        assert!(runtime.resource_admission.activate(|| true));
        let operation_id = OperationId::new();
        let fence = runtime
            .publication_fences
            .issue(operation_id, &shared)
            .unwrap();
        let result: Result<(), OperationJournalStoreError> =
            Err(OperationJournalStoreError::StagingCheckpoint {
                reason: "confirmed precommit rollback".into(),
            });
        assert!(runtime.publication_write_result(&fence, result).is_err());
        drop(fence);
        assert!(!runtime.publication_fences.unresolved());
        runtime
            .publication_fences
            .close_for_mutation(operation_id)
            .unwrap();
        runtime.resource_admission.validate_open().unwrap();
        let read = runtime.resource_admission.begin_read(|| true).unwrap();
        drop(read);
        assert!(runtime.resource_admission.close_and_wait());
    }
}
