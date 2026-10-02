//! Synthetic OPT-2033 decision matrix. No production owner or completion authority.
use uuid::Uuid;
use yield_library::operation_journal::work_plan::{WorkUnitKey, WorkUnitRole};
use yield_library::operation_journal::{OperationId, ParticipantId};

#[derive(Clone, Debug, PartialEq, Eq)]
struct Link {
    operation: OperationId,
    global: WorkUnitKey,
    source: WorkUnitKey,
    source_participant: ParticipantId,
    command: Uuid,
    source_id: String,
    revision: u64,
    lifecycle: u64,
    historical_lease: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    CommitNew,
    ReplayOriginal,
    InvalidInput,
    StaleProfile,
    ReplayConflict,
    ReceiptCapacity,
}

// Models the decision order only; it neither writes SQLite nor returns a production receipt.
fn expectation(
    link: &Link,
    retained: Option<&Link>,
    request_owner: Uuid,
    live_owner: Uuid,
    full: bool,
) -> Outcome {
    if link.operation.as_uuid().is_nil()
        || link.command.is_nil()
        || link.global.participant_id().as_uuid().is_nil()
        || link.source.participant_id().as_uuid().is_nil()
        || link.global.role() != WorkUnitRole::GlobalLibrary
        || link.source.role() != WorkUnitRole::SourceAffect
        || link.source.participant_id() != link.source_participant
        || link.source_id.is_empty()
        || link.source_id.len() > 256
        || link.historical_lease == 0
        || link.revision > i64::MAX as u64
        || link.lifecycle > i64::MAX as u64
        || request_owner.is_nil()
    {
        return Outcome::InvalidInput;
    }
    if request_owner != live_owner {
        return Outcome::StaleProfile;
    }
    if let Some(old) = retained {
        return if old == link {
            Outcome::ReplayOriginal
        } else {
            Outcome::ReplayConflict
        };
    }
    if full {
        Outcome::ReceiptCapacity
    } else {
        Outcome::CommitNew
    }
}

fn link() -> Link {
    let source_participant = ParticipantId::from_uuid(Uuid::from_u128(2));
    Link {
        operation: OperationId::from_uuid(Uuid::from_u128(1)),
        global: WorkUnitKey::new(
            255,
            10,
            ParticipantId::from_uuid(Uuid::from_u128(3)),
            WorkUnitRole::GlobalLibrary,
        ),
        source: WorkUnitKey::new(0, 60, source_participant, WorkUnitRole::SourceAffect),
        source_participant,
        command: Uuid::from_u128(4),
        source_id: "source-a".into(),
        revision: 7,
        lifecycle: 1,
        historical_lease: 5,
    }
}

#[test]
fn new_duplicate_and_budget_order_matrix() {
    let l = link();
    let owner = Uuid::from_u128(10);
    for (retained, full, expected) in [
        (None, false, Outcome::CommitNew),
        (Some(&l), false, Outcome::ReplayOriginal),
        (Some(&l), true, Outcome::ReplayOriginal),
        (None, true, Outcome::ReceiptCapacity),
    ] {
        assert_eq!(expectation(&l, retained, owner, owner, full), expected);
    }
}

#[test]
fn reacquisition_preserves_historical_source_fence_but_rejects_stale_request() {
    let l = link();
    let old = Uuid::from_u128(10);
    let fresh = Uuid::from_u128(11);
    assert_eq!(
        expectation(&l, Some(&l), old, fresh, false),
        Outcome::StaleProfile
    );
    assert_eq!(
        expectation(&l, Some(&l), fresh, fresh, false),
        Outcome::ReplayOriginal
    );
    assert_eq!(l.historical_lease, 5);
}

#[test]
fn same_key_changed_source_evidence_conflicts() {
    let l = link();
    let owner = Uuid::from_u128(10);
    let mut changed_revision = l.clone();
    changed_revision.revision += 1;
    assert_eq!(
        expectation(&changed_revision, Some(&l), owner, owner, false),
        Outcome::ReplayConflict
    );
    let mut changed = l.clone();
    changed.lifecycle += 1;
    assert_eq!(
        expectation(&changed, Some(&l), owner, owner, false),
        Outcome::ReplayConflict
    );
    changed = l.clone();
    changed.historical_lease += 1;
    assert_eq!(
        expectation(&changed, Some(&l), owner, owner, false),
        Outcome::ReplayConflict
    );
}

#[test]
fn endpoint_and_bound_rejections() {
    let l = link();
    let owner = Uuid::from_u128(10);
    let mut wrong_participant = l.clone();
    wrong_participant.source_participant = ParticipantId::from_uuid(Uuid::from_u128(9));
    let mut wrong_role = l.clone();
    wrong_role.global = wrong_role.source;
    let mut oversized = l.clone();
    oversized.source_id = "é".repeat(129);
    let mut zero_lease = l.clone();
    zero_lease.historical_lease = 0;
    let mut overflow = l.clone();
    overflow.revision = u64::MAX;
    let mut nil = l.clone();
    nil.command = Uuid::nil();
    for invalid in [
        wrong_participant,
        wrong_role,
        oversized,
        zero_lease,
        overflow,
        nil,
    ] {
        assert_eq!(
            expectation(&invalid, None, owner, owner, false),
            Outcome::InvalidInput
        );
    }
}

#[test]
fn distinct_source_commands_do_not_collapse_to_one_operation_key() {
    let a = link();
    let mut b = a.clone();
    b.command = Uuid::from_u128(5);
    assert_eq!(a.operation, b.operation);
    assert_ne!(
        (a.operation, a.global, a.source, a.command),
        (b.operation, b.global, b.source, b.command)
    );
}
