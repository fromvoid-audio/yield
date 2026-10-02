//! Synthetic decision fixtures only: no production receipts, source commits or file qualification.
#[derive(Clone, Copy, Debug)]
enum Family {
    DeriveCopy,
    CopyThenRemoveMove,
}
#[derive(Clone, Copy, Debug, Default)]
struct Observations {
    parent_identity: bool,
    destination_metadata_commit: bool,
    publication_checkpoint: bool,
    origin_intent: bool,
    qualified_absence: bool,
    origin_retirement: bool,
    required_global_applied: bool,
    exact_plan_and_owner_context: bool,
}
// Proposed policy matrix. These booleans are not capability constructors and must never
// be exposed as a production Harvest admission or private journal completion API.
fn policy_ready(family: Family, o: Observations) -> bool {
    o.parent_identity
        && o.destination_metadata_commit
        && o.publication_checkpoint
        && o.required_global_applied
        && o.exact_plan_and_owner_context
        && match family {
            Family::DeriveCopy => true,
            Family::CopyThenRemoveMove => {
                o.origin_intent && o.qualified_absence && o.origin_retirement
            }
        }
}
#[test]
fn copy_keeps_origin_and_needs_real_destination_global_and_context() {
    let complete = Observations {
        parent_identity: true,
        destination_metadata_commit: true,
        publication_checkpoint: true,
        required_global_applied: true,
        exact_plan_and_owner_context: true,
        ..Default::default()
    };
    assert!(policy_ready(Family::DeriveCopy, complete));
    assert!(!policy_ready(Family::CopyThenRemoveMove, complete));
    for missing in [
        Observations {
            parent_identity: false,
            ..complete
        },
        Observations {
            destination_metadata_commit: false,
            ..complete
        },
        Observations {
            publication_checkpoint: false,
            ..complete
        },
        Observations {
            required_global_applied: false,
            ..complete
        },
        Observations {
            exact_plan_and_owner_context: false,
            ..complete
        },
    ] {
        assert!(!policy_ready(Family::DeriveCopy, missing));
    }
}
#[test]
fn move_cannot_replace_origin_chain_with_reference_receipt_or_copy_policy() {
    let complete = Observations {
        parent_identity: true,
        destination_metadata_commit: true,
        publication_checkpoint: true,
        origin_intent: true,
        qualified_absence: true,
        origin_retirement: true,
        required_global_applied: true,
        exact_plan_and_owner_context: true,
    };
    assert!(policy_ready(Family::CopyThenRemoveMove, complete));
    for missing in [
        Observations {
            origin_intent: false,
            ..complete
        },
        Observations {
            qualified_absence: false,
            ..complete
        },
        Observations {
            origin_retirement: false,
            ..complete
        },
        Observations {
            destination_metadata_commit: false,
            ..complete
        },
    ] {
        assert!(!policy_ready(Family::CopyThenRemoveMove, missing));
    }
}
#[test]
fn metadata_and_read_observations_do_not_supply_private_completion() {
    let metadata_only = Observations {
        parent_identity: true,
        destination_metadata_commit: true,
        required_global_applied: true,
        ..Default::default()
    };
    assert!(!policy_ready(Family::DeriveCopy, metadata_only));
    assert!(!policy_ready(Family::CopyThenRemoveMove, metadata_only));
    let caller_dto = Observations {
        publication_checkpoint: true,
        exact_plan_and_owner_context: true,
        ..Default::default()
    };
    assert!(!policy_ready(Family::DeriveCopy, caller_dto));
}
