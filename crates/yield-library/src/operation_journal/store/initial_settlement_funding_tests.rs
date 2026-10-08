// Original SQLite settlement and arithmetic fixtures, never physical qualification.
#[cfg(test)]
mod initial_settlement_funding {
    use super::*;
    use crate::operation_journal::owner::JournalNamespaceFundingProbe;

    #[test]
    fn funded_initial_settlement_commits_original_claims_without_owned_point_copy() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = provider_for_intent(&record);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let lease = provider.admit(&plan, &claims).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let baseline = funding.remaining_bytes();
        super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.set(0));
        let settled = store
            .settle_initial_claims_with_lease_for_input(
                &record,
                &claims,
                &provider,
                lease,
                Some(&funding.input),
            )
            .unwrap();
        assert!(matches!(
            settled.outcome(),
            JournalInsertOutcome::Committed(_)
        ));
        assert!(settled.lease().is_some());
        assert_eq!(
            super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.get()),
            0
        );
        assert_eq!(funding.remaining_bytes(), baseline);
        assert_eq!(store.get(record.operation_id).unwrap(), Some(record));
        assert_eq!(ordinary_claim_count(&store), 1);
        drop(settled);
        assert_eq!(provider.claim_count(), 1);
    }

    #[test]
    fn funded_initial_settlement_confirmed_rollback_releases_exact_original_lease() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = provider_for_intent(&record);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let lease = provider.admit(&plan, &claims).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: "original rollback boundary".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let settled = store
            .settle_initial_claims_with_lease_for_input(
                &record,
                &claims,
                &provider,
                lease,
                Some(&funding.input),
            )
            .unwrap();
        assert_eq!(
            settled.outcome().failure().unwrap().stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert!(settled.lease().is_none());
        assert!(store.connection.is_autocommit());
        assert_eq!(provider.claim_count(), 0);
        assert_eq!(ordinary_claim_count(&store), 0);
        let pool = funding.pool_bytes_observer_for_test();
        let (outcome, _) = settled.into_parts();
        let JournalInsertOutcome::Failed(failure) = outcome else {
            panic!("selected original failure")
        };
        let error = funding.retain_command_error_for_test(failure.into_error());
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(error);
        assert_eq!(pool().0, pool().1);
    }

    #[test]
    fn funded_initial_settlement_unknown_commit_keeps_original_charge_and_record() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = provider_for_intent(&record);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let lease = provider.admit(&plan, &claims).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        store.set_insert_test_hook(|boundary| {
            if boundary == InsertTestBoundary::Commit {
                InsertTestDirective::ExecuteThenFail(
                    OperationJournalStoreError::IdentityCheckUnavailable {
                        reason: "lost original commit acknowledgement".into(),
                    },
                )
            } else {
                InsertTestDirective::Continue
            }
        });
        let settled = store
            .settle_initial_claims_with_lease_for_input(
                &record,
                &claims,
                &provider,
                lease,
                Some(&funding.input),
            )
            .unwrap();
        assert_eq!(
            settled.outcome().failure().unwrap().stage(),
            JournalInsertWriteStage::CommitOutcomeUnknown
        );
        assert!(settled.lease().is_some());
        assert_eq!(store.get(record.operation_id).unwrap(), Some(record));
        assert_eq!(ordinary_claim_count(&store), 1);
        drop(settled);
        assert_eq!(provider.claim_count(), 1);
        assert!(provider.validate_recovery_state().is_err());
    }

    #[test]
    fn funded_initial_settlement_native_failure_survives_original_caller_drop() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = provider_for_intent(&record);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let lease = provider.admit(&plan, &claims).unwrap();
        store
            .connection
            .execute_batch("DROP TABLE operation_journal")
            .unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let pool = funding.pool_bytes_observer_for_test();
        let settled = store
            .settle_initial_claims_with_lease_for_input(
                &record,
                &claims,
                &provider,
                lease,
                Some(&funding.input),
            )
            .unwrap();
        assert!(store.connection.is_autocommit());
        assert_eq!(
            settled.outcome().failure().unwrap().stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert!(
            settled
                .outcome()
                .failure()
                .unwrap()
                .error()
                .to_string()
                .contains("no such table: operation_journal")
        );
        assert_eq!(provider.claim_count(), 0);
        let (outcome, _) = settled.into_parts();
        let JournalInsertOutcome::Failed(failure) = outcome else {
            panic!("selected original native failure")
        };
        let error = funding.retain_command_error_for_test(failure.into_error());
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(error);
        assert_eq!(pool().0, pool().1);
    }
    #[test]
    fn funded_initial_settlement_recording_commits_four_exact_control_rows() {
        use crate::operation_journal::recording_commands::recording_proposal_for_test;
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let proposal = recording_proposal_for_test(2, 100);
        let record = finite_recording_record(&proposal);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let claims = AdmissionClaimInventory::new(
            plan.allocations()
                .iter()
                .map(|row| {
                    AdmissionClaim::new(
                        AdmissionClaimId::new(),
                        row.volume().clone(),
                        row.class(),
                        row.allocated_bytes(),
                    )
                    .unwrap()
                })
                .collect(),
        )
        .unwrap();
        let mut facts = BTreeMap::new();
        for row in plan.allocations() {
            let fact = facts.entry(row.volume().clone()).or_insert(VolumeFacts {
                identity: row.volume().clone(),
                allocation_unit: row.allocation_unit(),
                free_bytes: PROTECTED_FREE_SPACE_FLOOR,
            });
            fact.free_bytes += row.allocated_bytes() * 8;
        }
        let provider = CapacityProvider::qualified_fixture(facts);
        let lease = provider.admit(&plan, &claims).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let baseline = funding.remaining_bytes();
        super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.set(0));
        let settled = store
            .settle_initial_claims_with_lease_for_input(
                &record,
                &claims,
                &provider,
                lease,
                Some(&funding.input),
            )
            .unwrap();
        assert!(matches!(
            settled.outcome(),
            JournalInsertOutcome::Committed(_)
        ));
        assert_eq!(
            super::super::POINT_ROW_MATERIALIZED_BYTES.with(|n| n.get()),
            0
        );
        assert_eq!(funding.remaining_bytes(), baseline);
        assert_eq!(store.recording_retained_totals().unwrap(), (4, 16384));
        assert!(store.verify_recording_reservation(&proposal, true).unwrap());
        assert_eq!(ordinary_claim_count(&store), 2);
        drop(settled);
        assert_eq!(provider.claim_count(), 1);
        assert_eq!(provider.ledger_claims().len(), 2);
    }

    #[test]
    fn funded_initial_settlement_schema_pressure_precedes_owned_text() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let scope = super::super::schema_backing::Scope::new(
            &store.connection,
            &store.database_path,
            &funding.input,
        )
        .unwrap();
        let pressure = funding
            .input
            .reserve_historical_scratch(funding.remaining_bytes() - 1)
            .unwrap();
        super::super::schema_backing::TEXT_COPIED_BYTES.with(|n| n.set(0));
        let error = scope.objects().unwrap_err();
        assert!(matches!(
            error,
            OperationJournalStoreError::HistoricalClaimResourceBound
        ));
        assert_eq!(
            super::super::schema_backing::TEXT_COPIED_BYTES.with(|n| n.get()),
            0
        );
        scope.finish(true);
        let pool = funding.pool_bytes_observer_for_test();
        let error = funding.retain_command_error_for_test(error);
        drop(pressure);
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(error);
        assert_eq!(pool().0, pool().1);
    }

    #[test]
    fn funded_initial_settlement_mandatory_rollback_runs_after_later_pressure() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        let record = intent_record(OperationId::new());
        let claims = claims_for_intent(&record);
        let provider = provider_for_intent(&record);
        let plan = full_capacity_plan_for_record(&record).unwrap();
        let lease = provider.admit(&plan, &claims).unwrap();
        let funding = JournalNamespaceFundingProbe::new();
        let loan_input = funding.input.reserve_historical_scratch(1).unwrap();
        let pool = funding.pool_bytes_observer_for_test();
        let pressure = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured = pressure.clone();
        store.set_insert_test_hook(move |boundary| {
            if boundary == InsertTestBoundary::BeforePrecommitValidation {
                *captured.lock().unwrap() =
                    Some(loan_input.reserve_historical_scratch(pool().0).unwrap());
                InsertTestDirective::Fail(OperationJournalStoreError::IdentityCheckUnavailable {
                    reason: "original mandatory rollback under pressure".into(),
                })
            } else {
                InsertTestDirective::Continue
            }
        });
        let settled = store
            .settle_initial_claims_with_lease_for_input(
                &record,
                &claims,
                &provider,
                lease,
                Some(&funding.input),
            )
            .unwrap();
        assert_eq!(
            settled.outcome().failure().unwrap().stage(),
            JournalInsertWriteStage::RolledBackBeforeCommit
        );
        assert!(store.connection.is_autocommit());
        assert_eq!(provider.claim_count(), 0);
        assert_eq!(ordinary_claim_count(&store), 0);
        drop(pressure.lock().unwrap().take());
    }
    #[test]
    fn funded_initial_settlement_recording_policy_keeps_original_temp_schema_refusal() {
        let f = fixture();
        let store = OperationJournalStore::open(&f.owner).unwrap();
        store
            .connection
            .execute_batch("CREATE TEMP TABLE original_policy_refusal(x INTEGER)")
            .unwrap();
        let gold = store
            .configure_recording_metadata_policy()
            .unwrap_err()
            .to_string();
        let funding = JournalNamespaceFundingProbe::new();
        let pool = funding.pool_bytes_observer_for_test();
        let error = store
            .configure_recording_metadata_policy_with_input(Some(&funding.input))
            .unwrap_err();
        assert_eq!(error.to_string(), gold);
        let error = funding.retain_command_error_for_test(error);
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(error);
        assert_eq!(pool().0, pool().1);
        assert!(store.connection.is_autocommit());
    }

    #[test]
    fn funded_initial_settlement_construction_credit_precedes_original_clone() {
        use crate::operation_journal::recording_commands::recording_proposal_for_test;
        let proposal = recording_proposal_for_test(2, 100);
        let original = finite_recording_record(&proposal);
        let funding = JournalNamespaceFundingProbe::new();
        let baseline = funding.remaining_bytes();
        let credit = OperationJournalStore::prepay_initial_record_construction(
            &(
                &original.intent,
                &original.universal_intent,
                original.universal_command_args(),
                &original.full_capacity_plan,
            ),
            &funding.input,
        )
        .unwrap();
        assert!(funding.remaining_bytes() < baseline);
        let cloned = StoredOperationRecord::new_v17_recording(
            original.operation_id,
            original.intent.clone(),
            original.universal_intent.as_ref().unwrap().clone(),
            original.universal_command_args().unwrap().clone(),
            original.full_capacity_plan.as_ref().unwrap().clone(),
            original.created_unix_ms,
        );
        assert_eq!(cloned, original);
        let pool = funding.pool_bytes_observer_for_test();
        drop(funding);
        assert!(pool().0 < pool().1);
        drop(cloned);
        assert!(pool().0 < pool().1);
        drop(credit);
        assert_eq!(pool().0, pool().1);
    }
}
