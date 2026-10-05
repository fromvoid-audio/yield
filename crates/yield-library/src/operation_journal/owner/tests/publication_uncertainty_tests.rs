//! Original runtime/admission tests. Scalar activation supplies no native authority.
#[cfg(test)]
mod integration {
    use super::super::super::resource_custody::UseError;
    use super::super::*;

    #[test]
    fn publication_uncertainty_original_runtime_gate_is_sticky_and_shutdown_unclean() {
        let fixture = fixture("publication-unknown-original-runtime");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let runtime = OwnerRuntime::new(store).unwrap();
        let original = runtime.resource_admission.clone();
        assert!(original.activate(|| true));
        let result: Result<(), OperationJournalStoreError> =
            Err(OperationJournalStoreError::StagingOutcomeUncertain {
                reason: "injected unconfirmed durable outcome".into(),
            });
        assert!(runtime.publication_write_result(result).is_err());
        assert_eq!(original.validate_open(), Err(UseError::Unknown));
        assert!(matches!(
            original.begin_read(|| true),
            Err(UseError::Unknown)
        ));
        runtime.publication_write_result(Ok(())).unwrap();
        assert!(!original.activate(|| true));
        assert!(!original.close_and_wait());
        assert_eq!(original.validate_open(), Err(UseError::Unknown));
    }

    #[test]
    fn publication_uncertainty_known_rejection_does_not_close_original_runtime_gate() {
        let fixture = fixture("publication-known-original-runtime");
        let store = OperationJournalStore::open(&fixture.owner).unwrap();
        let runtime = OwnerRuntime::new(store).unwrap();
        assert!(runtime.resource_admission.activate(|| true));
        let result: Result<(), OperationJournalStoreError> =
            Err(OperationJournalStoreError::StagingCheckpoint {
                reason: "confirmed precommit rollback".into(),
            });
        assert!(runtime.publication_write_result(result).is_err());
        runtime.resource_admission.validate_open().unwrap();
        let read = runtime.resource_admission.begin_read(|| true).unwrap();
        drop(read);
        assert!(runtime.resource_admission.close_and_wait());
    }
}
