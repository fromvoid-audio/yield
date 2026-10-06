// Included in actual original-owner SQLite integration tests; no fabricated issuer.
#[test]
fn retirement_inventory_actual_original_consumer_projects_six_slices() {
    original_retirement_custody_with_inventory(0, None, 1);
}
#[test]
fn retirement_inventory_actual_original_consumer_busy_preserves_decision_and_input() {
    original_retirement_custody_with_inventory(0, None, 2);
}
#[test]
fn retirement_inventory_actual_original_consumer_refuses_insufficient_shared_credit() {
    original_retirement_custody_with_inventory(0, None, 3);
}
#[test]
fn retirement_inventory_actual_original_consumer_uses_restarted_global_lifetime() {
    original_retirement_custody_with_inventory(0, None, 4);
}
#[test]
fn retirement_inventory_actual_detached_lease_report_retains_journal_input() {
    original_retirement_custody_with_inventory(0, None, 6);
}
#[test]
fn retirement_inventory_actual_original_consumer_preserves_completed_audit_codec() {
    original_retirement_custody_with_inventory(0, None, 5);
}
fn concurrent_inventory_drop<T: Send + 'static>(first: T, second: T) {
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let other = Arc::clone(&barrier);
    let a = thread::spawn(move || {
        barrier.wait();
        drop(first);
    });
    let b = thread::spawn(move || {
        other.wait();
        drop(second);
    });
    a.join().unwrap();
    b.join().unwrap();
}
fn assert_actual_retirement_inventory(
    inventory: &crate::operation_journal::database_claims::DatabaseClaimInventory,
    encoded: &[u8],
    profile: &crate::app_dirs::WritableProfileGuard,
    source: &crate::sample_sources::wal_transport::SourceLeaseWalTransportObservation,
    global: &crate::sample_sources::library::GlobalPreparationObservation,
    retirement:&crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs,
    record: &StoredOperationRecord,
    claims: &AdmissionClaimInventory,
) {
    use crate::operation_journal::database_claims::{self, ClaimDirection, DatabaseRole};
    assert_eq!(inventory.header().profile, profile.profile_root_identity());
    assert_eq!(inventory.header().acquisition, profile.owner_identity());
    assert_eq!(
        inventory.header().ownership_generation,
        profile.ownership_generation()
    );
    assert!(!inventory.header().decision_nonce.is_nil());
    assert_eq!(
        inventory.source_registration_generation(),
        Some(source.original_binding().registration_generation())
    );
    assert_eq!(inventory.bindings().len(), 2);
    assert_eq!(inventory.slices().len(), 6);
    assert_eq!(&encoded[..6], b"YWCB\x04\x00");
    assert_eq!(
        database_claims::DatabaseClaimInventory::decode(encoded).unwrap(),
        *inventory
    );
    let mut canonical = Vec::new();
    retirement
        .write_codec_normalized_command(&mut canonical)
        .unwrap();
    assert_eq!(canonical, database_claims::command_bytes(record).unwrap());
    let digest = database_claims::command_digest(record, 1).unwrap();
    assert_eq!(retirement.canonical_command_digest().unwrap(), digest);
    inventory.validate_record(record).unwrap();
    inventory
        .validate_against(record.full_capacity_plan.as_ref().unwrap(), claims)
        .unwrap();
    let source_binding = inventory
        .bindings()
        .iter()
        .find(|b| b.role == DatabaseRole::Source)
        .unwrap();
    let global_binding = inventory
        .bindings()
        .iter()
        .find(|b| b.role == DatabaseRole::Global)
        .unwrap();
    let fence = source.original_binding().stored_fence();
    assert_eq!(
        source_binding.source.as_ref().unwrap().lease_fence.as_ref(),
        Some(fence)
    );
    assert_ne!(
        fence.preparation_nonce(),
        retirement.lease_fence().preparation_nonce()
    );
    assert!(fence.matches_initial_policy(retirement.lease_fence()));
    assert_eq!(
        source_binding.owner_generation,
        fence.subject().lease_epoch()
    );
    assert_eq!(
        source_binding.lifecycle,
        fence.subject().lifecycle_generation()
    );
    assert_eq!(
        source_binding.registration,
        fence.subject().registration().unwrap().registration_id()
    );
    assert_eq!(global_binding.registration, global.owner().registration());
    assert_eq!(global_binding.owner_generation, global.owner().generation());
    assert_eq!(global_binding.lifecycle, global.owner().generation());
    assert_eq!(global_binding.current_revision, global.mutation_revision());
    assert_eq!(
        global_binding.database_root,
        global.wal_scope().unwrap().root_identity()
    );
    assert_eq!(global_binding.root_binding, global_binding.database_root);
    assert_ne!(source_binding.id, global_binding.id);
    assert_eq!(source_binding.volume, global_binding.volume);
    for binding in inventory.bindings() {
        assert!(
            binding
                .participants
                .iter()
                .all(|p| p.command_digest == digest)
        );
    }
    let family = retirement.initial_forward_policy_digest().unwrap();
    let mut ids = std::collections::HashSet::new();
    for (work, binding, direction, classes, per) in [
        (
            retirement.source_work(),
            source_binding,
            ClaimDirection::LeaseMaintenance,
            [Class::SourceDatabaseCommit, Class::SourceWalShm],
            source
                .original_binding()
                .retirement_forward_policy()
                .unwrap()
                .component_ceilings(),
        ),
        (
            retirement.draining_work(),
            global_binding,
            ClaimDirection::RequiredForward,
            [Class::GlobalDatabaseCommit, Class::GlobalWalShm],
            global
                .wal_scope()
                .unwrap()
                .retirement_forward_policy()
                .unwrap()
                .per_transition_ceilings(),
        ),
        (
            retirement.retired_work(),
            global_binding,
            ClaimDirection::RequiredForward,
            [Class::GlobalDatabaseCommit, Class::GlobalWalShm],
            global
                .wal_scope()
                .unwrap()
                .retirement_forward_policy()
                .unwrap()
                .per_transition_ceilings(),
        ),
    ] {
        for (component, class) in classes.into_iter().enumerate() {
            let claim = claims
                .claims()
                .iter()
                .find(|c| c.allocation_class() == class)
                .unwrap()
                .claim_id()
                .as_uuid();
            let expected = database_claims::retirement_forward_policy::registered_slice_identity(
                inventory.header(),
                binding,
                inventory.source_registration_generation().unwrap(),
                claim,
                &family,
                work,
                direction,
                class,
            )
            .unwrap();
            let slice = inventory
                .slices()
                .iter()
                .find(|s| s.id == expected)
                .unwrap();
            assert!(ids.insert(slice.id));
            assert_eq!(slice.claim, claim);
            assert_eq!(slice.binding, binding.id);
            assert_eq!(slice.participant, work.participant_id().as_uuid());
            assert_eq!(slice.direction, direction);
            assert_eq!(slice.class, class);
            assert_eq!(
                slice.ceiling,
                if component == 0 {
                    per[0]
                } else {
                    per[1] + per[2]
                }
            );
            assert_eq!(
                (slice.wal_ceiling, slice.shm_ceiling),
                if component == 0 {
                    (0, 0)
                } else {
                    (per[1], per[2])
                }
            );
        }
    }
    assert_retirement_codec_variants(retirement);
    assert_registered_retirement_nonce_correlation(inventory, record, retirement);
}
fn assert_retirement_codec_variants(
    retirement:&crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs,
) {
    use crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs;
    // Serializer compatibility only, not acceptance or production provenance. Start with
    // the exact actual frozen family, then exercise every closed optional/enum arm.
    let original = serde_json::to_value(retirement).unwrap();
    for phase in [
        None,
        Some("prepared"),
        Some("registered_phase_observed"),
        Some("draining"),
        Some("retired"),
        Some("abandoned"),
    ] {
        for captured in [false, true] {
            let mut changed = original.clone();
            let subject = &mut changed["lease_fence"]["subject"];
            subject["source_id"] = serde_json::json!("source-\"\\é☃");
            subject["registration"]=phase.map(|phase|serde_json::json!({"phase":phase,"registration_id":Uuid::from_u128(17)})).unwrap_or(serde_json::Value::Null);
            subject["audit"] = if captured {
                serde_json::json!({"status":"captured","revision":17,"directory_generation":19})
            } else {
                serde_json::json!({"status":"unavailable"})
            };
            let args: SourceRegistryRetirementIntentArgs = serde_json::from_value(changed).unwrap();
            let expected = serde_json::to_vec(
                &serde_json::to_value(UniversalCommandArgs::SourceRegistryRetirement(args.clone()))
                    .unwrap(),
            )
            .unwrap();
            let mut actual = Vec::new();
            args.write_codec_normalized_command(&mut actual).unwrap();
            assert_eq!(actual, expected);
            let mut hash = blake3::Hasher::new_derive_key("yield.database-command-family.v1");
            hash.update(&crate::operation_journal::model::SCHEMA_V18.to_le_bytes());
            hash.update(&[1]);
            hash.update(&expected);
            assert_eq!(
                args.canonical_command_digest().unwrap(),
                *hash.finalize().as_bytes()
            );
        }
    }
}
fn assert_registered_retirement_nonce_correlation(
    inventory: &crate::operation_journal::database_claims::DatabaseClaimInventory,
    record: &StoredOperationRecord,
    retirement:&crate::operation_journal::source_registry_retirement::SourceRegistryRetirementIntentArgs,
) {
    use crate::operation_journal::{
        database_claims::{DatabaseClaimInventory, DatabaseRole},
        source_lease_binding::StoredSourceLeaseFence,
    };
    let actual = inventory
        .bindings()
        .iter()
        .find(|b| b.role == DatabaseRole::Source)
        .unwrap()
        .source
        .as_ref()
        .unwrap()
        .lease_fence
        .as_ref()
        .unwrap();
    let image = serde_json::to_value(actual).unwrap();
    let paths: &[&[&str]] = &[
        &["version"],
        &["page_size"],
        &["maximum_pages"],
        &["subject", "source_id"],
        &["subject", "lifecycle_generation"],
        &["subject", "profile_acquisition"],
        &["subject", "lease_owner"],
        &["subject", "lease_epoch"],
        &["subject", "media_root_identity"],
        &["subject", "database_root_identity"],
        &["subject", "database_identity"],
        &["subject", "root_binding_identity"],
        &["subject", "scan_revision"],
        &["subject", "audit"],
        &["subject", "registration", "registration_id"],
        &["subject", "registration", "phase"],
    ];
    let reject = |changed: serde_json::Value| {
        if let Ok(fence) = serde_json::from_value::<StoredSourceLeaseFence>(changed) {
            assert!(!fence.matches_initial_policy(retirement.lease_fence()));
            let mut bindings = inventory.bindings().to_vec();
            bindings
                .iter_mut()
                .find(|b| b.role == DatabaseRole::Source)
                .unwrap()
                .source
                .as_mut()
                .unwrap()
                .lease_fence = Some(fence);
            if let Ok(tampered) = DatabaseClaimInventory::new_registered_retirement_forward(
                inventory.header().clone(),
                bindings,
                inventory.slices().to_vec(),
                inventory.source_registration_generation().unwrap(),
            ) {
                assert!(tampered.validate_record(record).is_err());
            }
        }
    };
    for path in paths {
        let mut changed = image.clone();
        let mut field = &mut changed;
        for key in *path {
            field = &mut field[*key];
        }
        *field = if matches!(
            path.last().copied(),
            Some("profile_acquisition" | "lease_owner" | "registration_id")
        ) {
            serde_json::json!(Uuid::from_u128(1))
        } else if path.last().copied() == Some("phase") {
            serde_json::json!("draining")
        } else if path.last().copied() == Some("audit") {
            if field["status"] == "captured" {
                serde_json::json!({"status":"unavailable"})
            } else {
                serde_json::json!({"status":"captured","revision":1,"directory_generation":1})
            }
        } else if let Some(n) = field.as_u64() {
            serde_json::json!(n + 1)
        } else if field.is_string() {
            serde_json::json!(format!("{}-foreign", field.as_str().unwrap()))
        } else {
            serde_json::json!([1, 2, 3])
        };
        reject(changed);
    }
    for object in 0..3 {
        for field in ["identity", "device", "unit", "bytes", "ceiling"] {
            let mut changed = image.clone();
            let item = &mut changed["objects"][object][field];
            *item = if let Some(n) = item.as_u64() {
                serde_json::json!(n + 4096)
            } else {
                serde_json::json!(format!("{}-foreign", item.as_str().unwrap()))
            };
            reject(changed);
        }
    }
    let legacy = DatabaseClaimInventory::new_retirement_forward(
        inventory.header().clone(),
        inventory.bindings().to_vec(),
        inventory.slices().to_vec(),
    )
    .unwrap();
    assert!(
        legacy
            .validate_record(record)
            .unwrap_err()
            .to_string()
            .contains("immutable lease fence mismatch")
    );
    let original = record.universal_intent.as_ref().unwrap().participants();
    let mut descriptors = original.descriptors().to_vec();
    *descriptors
        .iter_mut()
        .find(|d| d.id() == retirement.source_participant())
        .unwrap() = ParticipantDescriptor::source_lease_maintenance(
        retirement.source_participant(),
        actual.clone(),
    )
    .unwrap();
    let mut changed = record.clone();
    changed.universal_intent = Some(
        UniversalIntent::new(
            None,
            crate::operation_journal::model::UniversalCommandKind::SourceRegistryRetirement,
            "operation.status.source_registry_retirement",
            ParticipantSet::new(descriptors, original.checkpoints().to_vec()).unwrap(),
        )
        .unwrap(),
    );
    assert!(inventory.validate_record(&changed).is_err());
}
