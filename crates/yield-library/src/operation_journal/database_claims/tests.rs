use super::*;
use crate::durable_capacity::{PROTECTED_FREE_SPACE_FLOOR, VolumeIdentity};
use crate::operation_journal::full_capacity::FullCapacityAllocation;
use crate::operation_journal::{AdmissionClaim, AdmissionClaimId};

fn fixture(count: usize) -> DatabaseClaimInventory {
    let header = InventoryHeader {
        profile: "profile".into(),
        acquisition: "acquisition".into(),
        ownership_generation: 1,
        decision_nonce: Uuid::from_u128(1),
    };
    let bindings = (0..count)
        .map(|i| DatabaseBinding {
            id: Uuid::from_u128(i as u128 + 10),
            role: DatabaseRole::Source,
            volume: 7,
            allocation_unit: 4096,
            main_file: format!("file-{i:03}"),
            registration: Uuid::from_u128(i as u128 + 100),
            owner_generation: 1,
            lifecycle: 1,
            database_root: "database-root".into(),
            root_binding: "root-binding".into(),
            source: Some(SourceBinding {
                source_id: format!("source-{i}"),
                media_root: "media-root".into(),
                lease_owner: "owner".into(),
                lease_epoch: 1,
            }),
            current_revision: 0,
            participants: vec![DatabaseParticipant {
                id: Uuid::from_u128(i as u128 + 200),
                requirement: 1,
                revision_domain: RevisionDomain::SourceAuthoritative,
                command_digest: [1; 32],
                before: 0,
                after: 1,
            }],
        })
        .collect::<Vec<_>>();
    let slices = bindings
        .iter()
        .enumerate()
        .map(|(i, b)| DatabaseClaimSlice {
            id: Uuid::from_u128(i as u128 + 300),
            claim: Uuid::from_u128(2),
            binding: b.id,
            participant: b.participants[0].id,
            direction: ClaimDirection::RequiredForward,
            class: FullCapacityAllocationClass::SourceWalShm,
            ceiling: 8192,
            wal_ceiling: 4096,
            shm_ceiling: 4096,
        })
        .collect();
    DatabaseClaimInventory::new(header, bindings, slices).unwrap()
}
fn plan_and_claim(bytes: u64) -> (FullCapacityPlan, AdmissionClaimInventory) {
    let volume = VolumeIdentity { device: 7 };
    let plan = FullCapacityPlan::new(vec![
        FullCapacityAllocation::new(
            volume.clone(),
            4096,
            FullCapacityAllocationClass::SourceWalShm,
            bytes,
            bytes,
            PROTECTED_FREE_SPACE_FLOOR,
        )
        .unwrap(),
    ])
    .unwrap();
    let claims = AdmissionClaimInventory::new(vec![
        AdmissionClaim::new(
            AdmissionClaimId::from_uuid(Uuid::from_u128(2)).unwrap(),
            volume,
            FullCapacityAllocationClass::SourceWalShm,
            bytes,
        )
        .unwrap(),
    ])
    .unwrap();
    (plan, claims)
}
#[test]
fn two_databases_partition_one_aggregate_without_duplicating_ceiling() {
    let mut inv = fixture(2);
    let (plan, claims) = plan_and_claim(16384);
    inv.validate_against(&plan, &claims).unwrap();
    inv.slices[1].ceiling = 16384;
    inv.slices[1].wal_ceiling = 12288;
    assert!(inv.validate_against(&plan, &claims).is_err());
    inv.slices[1].ceiling = 4096;
    inv.slices[1].wal_ceiling = 0;
    assert!(inv.validate_against(&plan, &claims).is_err());
    inv.slices[1].ceiling = 8192;
    inv.slices[1].wal_ceiling = 4096;
    inv.bindings[1].volume = 8;
    assert!(inv.validate_against(&plan, &claims).is_err());
}
#[test]
fn canonical_wire_fixture_and_every_truncation_fail_closed() {
    let inv = fixture(2);
    let bytes = inv.encode().unwrap();
    assert_eq!(&bytes[..6], b"YWCB\x01\x00");
    assert_eq!(bytes.len(), inv.encoded_len().unwrap());
    assert_eq!(DatabaseClaimInventory::decode(&bytes).unwrap(), inv);
    for length in 0..bytes.len() {
        assert!(
            DatabaseClaimInventory::decode(&bytes[..length]).is_err(),
            "prefix {length}"
        );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(DatabaseClaimInventory::decode(&trailing).is_err());
    let mut version = bytes;
    version[4] = 2;
    assert!(DatabaseClaimInventory::decode(&version).is_err());
    assert!(DatabaseClaimInventory::decode(&vec![0; MAX_INVENTORY_BYTES + 1]).is_err());
    let mut malformed = inv.encode().unwrap();
    malformed[8] = 0xff;
    assert!(
        DatabaseClaimInventory::decode(&malformed).is_err(),
        "invalid UTF-8"
    );
    let mut malformed = inv.encode().unwrap();
    malformed[6..8].copy_from_slice(&65535u16.to_le_bytes());
    assert!(
        DatabaseClaimInventory::decode(&malformed).is_err(),
        "reject oversized identity before retention"
    );
    let mut malformed = inv.encode().unwrap();
    let role =
        6 + 2 + inv.header.profile.len() + 2 + inv.header.acquisition.len() + 8 + 16 + 4 + 16;
    malformed[role] = 3;
    assert!(
        DatabaseClaimInventory::decode(&malformed).is_err(),
        "unknown role"
    );
}
#[test]
fn unsorted_inputs_normalize_but_unsorted_wire_is_rejected() {
    let inv = fixture(2);
    let mut reversed = inv.clone();
    reversed.bindings.reverse();
    reversed.slices.reverse();
    let normalized =
        DatabaseClaimInventory::new(reversed.header, reversed.bindings, reversed.slices).unwrap();
    assert_eq!(normalized, inv);
    // Private corruption models hostile wire without going through the normalizing constructor.
    let mut reversed = inv.clone();
    reversed.slices.reverse();
    assert!(DatabaseClaimInventory::decode(&reversed.encode().unwrap()).is_err());
    assert_ne!(reversed.digest().unwrap(), inv.digest().unwrap());
}
#[test]
fn alias_roles_generations_directions_and_integer_ranges_are_not_interchangeable() {
    let inv = fixture(2);
    let bad: Vec<Box<dyn Fn(&mut DatabaseClaimInventory)>> = vec![
        Box::new(|v| v.bindings[1].main_file = v.bindings[0].main_file.clone()),
        Box::new(|v| v.bindings[1].id = v.bindings[0].id),
        Box::new(|v| v.bindings[1].participants[0].id = v.bindings[0].participants[0].id),
        Box::new(|v| v.bindings[0].registration = Uuid::nil()),
        Box::new(|v| v.header.ownership_generation = 0),
        Box::new(|v| v.header.decision_nonce = Uuid::nil()),
        Box::new(|v| v.bindings[0].owner_generation = u64::MAX),
        Box::new(|v| v.bindings[0].participants[0].after = u64::MAX),
        Box::new(|v| {
            v.bindings[0].participants[0].revision_domain = RevisionDomain::GlobalRegistry
        }),
        Box::new(|v| v.bindings[0].participants[0].before = 1),
        Box::new(|v| v.bindings[0].participants[0].requirement = 2),
        Box::new(|v| v.slices[0].class = FullCapacityAllocationClass::GlobalWalShm),
        Box::new(|v| v.slices[0].class = FullCapacityAllocationClass::JournalRecord),
        Box::new(|v| v.slices[0].wal_ceiling = 4097),
        Box::new(|v| v.slices[0].ceiling = 0),
        Box::new(|v| v.slices[0].participant = Uuid::from_u128(999)),
    ];
    for (i, f) in bad.into_iter().enumerate() {
        let mut value = inv.clone();
        f(&mut value);
        assert!(value.validate().is_err(), "case {i}");
    }
}
#[test]
fn maximum_counts_and_aggregate_identity_bytes_are_bounded() {
    let mut inv = fixture(64);
    assert!(inv.encode().unwrap().len() < MAX_INVENTORY_BYTES);
    for b in &mut inv.bindings {
        b.main_file = format!("{}{}", "x".repeat(250), b.main_file); // also exercises per-field rejection
    }
    assert!(inv.validate().is_err());
    let mut inv = fixture(64);
    for (i, b) in inv.bindings.iter_mut().enumerate() {
        b.main_file = format!("{}{:03}", "x".repeat(253), i);
        b.database_root = "d".repeat(256);
        b.root_binding = "r".repeat(256);
        let s = b.source.as_mut().unwrap();
        s.source_id = "s".repeat(256);
        s.media_root = "m".repeat(256);
        s.lease_owner = "l".repeat(256);
    }
    assert!(
        inv.validate().is_err(),
        "aggregate fields exceed64KiB while every field fits"
    );
    let mut inv = fixture(1);
    inv.slices = vec![inv.slices[0].clone(); MAX_SLICES + 1];
    assert!(inv.validate().is_err());
}
#[test]
fn global_common_counter_and_narrow_revision_domains_remain_distinct() {
    let mut inv = fixture(1);
    let b = &mut inv.bindings[0];
    b.role = DatabaseRole::Global;
    b.source = None;
    b.current_revision = 7;
    b.participants[0].revision_domain = RevisionDomain::GlobalVocabulary;
    b.participants[0].before = 4;
    b.participants[0].after = 5;
    inv.slices[0].class = FullCapacityAllocationClass::GlobalWalShm;
    inv.validate().unwrap();
    assert_eq!(
        DatabaseClaimInventory::decode(&inv.encode().unwrap()).unwrap(),
        inv
    );
    inv.bindings[0].participants[0].revision_domain = RevisionDomain::GlobalMutationGeneration;
    assert!(inv.validate().is_err());
    inv.bindings[0].participants[0].before = 7;
    inv.validate().unwrap();
    inv.slices[0].direction = ClaimDirection::LeaseMaintenance;
    assert!(inv.validate().is_err());
}
