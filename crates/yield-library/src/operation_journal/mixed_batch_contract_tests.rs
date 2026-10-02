//! Executable decision fixture for OPT-1587. No production dispatcher or authority.
use super::{FullProgressDisposition, OperationId};
use crate::file_operation_owner::EffectOutcome;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Duplicate,
    Move,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Planned,
    Decision,
    InFlight,
    Confirmed,
    Skipped,
    Rejected,
    Retry,
    Retained,
    CancelledBeforeIntent,
    CancelPending,
}
#[derive(Clone, Copy, Debug)]
enum Choice {
    Skip,
    Rename,
    Overwrite,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    slot: u8,
    operation: OperationId,
    fingerprint: &'static str,
}
#[derive(Clone, Copy, Debug)]
struct Item {
    kind: Kind,
    identity: Option<Identity>,
    state: State,
}
#[derive(Clone, Copy, Debug)]
struct SyntheticSettledReceipt {
    identity: Identity,
    destination_verified: bool,
    source_checkpoint: bool,
    required_global_checkpoint: bool,
    charges_settled: bool,
    input_preserved: bool,
    origin_retired_after_destination: bool,
}
impl Item {
    fn admitted(kind: Kind, slot: u8, operation: OperationId) -> Self {
        Self {
            kind,
            identity: Some(Identity {
                slot,
                operation,
                fingerprint: "captured-intent-v1",
            }),
            state: State::InFlight,
        }
    }
    fn settle_synthetic(&mut self, receipt: SyntheticSettledReceipt) -> Result<(), &'static str> {
        if self.identity != Some(receipt.identity) {
            return Err("foreign or changed identity");
        }
        if !receipt.destination_verified
            || !receipt.source_checkpoint
            || !receipt.required_global_checkpoint
            || !receipt.charges_settled
            || match self.kind {
                Kind::Duplicate => !receipt.input_preserved,
                Kind::Move => !receipt.origin_retired_after_destination,
            }
        {
            return Err("incomplete per-item settlement");
        }
        if !matches!(
            self.state,
            State::InFlight | State::Retry | State::CancelPending | State::Confirmed
        ) {
            return Err("state requires reconciliation");
        }
        self.state = State::Confirmed;
        Ok(())
    }
    fn observe_owner(&mut self, outcome: EffectOutcome) -> Result<(), &'static str> {
        if matches!(self.state, State::Confirmed | State::Retained) {
            return Err("closed or audit-only item requires explicit reconciliation");
        }
        self.state = match outcome {
            EffectOutcome::Unsupported { .. } | EffectOutcome::SupersededBeforeEffect { .. } => {
                State::Rejected
            }
            EffectOutcome::Collision { .. } | EffectOutcome::Conflict { .. } => State::Decision,
            EffectOutcome::RetryableBeforeEffect { .. } => State::Retry,
            EffectOutcome::RetainedUncertain { .. } => State::Retained,
            // Applied may be validation only; a safe stop still needs settlement.
            EffectOutcome::Applied { .. } | EffectOutcome::StoppedAtSafeBoundary { .. } => {
                State::InFlight
            }
        };
        Ok(())
    }
    fn cancel_remaining(&mut self) {
        self.state = match self.state {
            State::Planned if self.identity.is_none() => State::CancelledBeforeIntent,
            State::Decision if self.identity.is_none() => State::CancelledBeforeIntent,
            State::InFlight | State::Retry | State::Decision => State::CancelPending,
            state => state,
        };
    }
    fn choose_before_intent(&mut self, choice: Choice) -> Result<(), &'static str> {
        if self.identity.is_some() || self.state != State::Decision {
            return Err("choice requires a settled old intent or unadmitted decision");
        }
        self.state = match choice {
            Choice::Skip => State::Skipped,
            Choice::Rename => State::Planned, // requires fresh bounded target/fence validation
            Choice::Overwrite => State::Rejected, // no qualified replacement authority
        };
        Ok(())
    }
    fn retry(&self, identity: Identity) -> Result<Identity, &'static str> {
        if self.identity == Some(identity) && matches!(self.state, State::Retry | State::Decision) {
            Ok(identity)
        } else {
            Err("not the same retry intent or requires reconciliation")
        }
    }
}
fn admitted_batch() -> [Item; 2] {
    [
        Item::admitted(
            Kind::Duplicate,
            0,
            OperationId::parse("00000000-0000-0000-0000-000000000001").unwrap(),
        ),
        Item::admitted(
            Kind::Move,
            1,
            OperationId::parse("00000000-0000-0000-0000-000000000002").unwrap(),
        ),
    ]
}
fn validate_identities(items: &[Item; 2]) -> Result<(), &'static str> {
    if items[0].kind != Kind::Duplicate || items[1].kind != Kind::Move {
        return Err("not the bounded duplicate-plus-move plan");
    }
    for (slot, item) in items.iter().enumerate() {
        if let Some(identity) = item.identity {
            if usize::from(identity.slot) != slot {
                return Err("wrong slot binding");
            }
        } else if item.state == State::Confirmed {
            return Err("missing admitted identity");
        }
    }
    if let [Some(a), Some(b)] = items.map(|item| item.identity) {
        if a.operation == b.operation {
            return Err("reused operation identity");
        }
    }
    Ok(())
}
fn synthetic_complete(item: Item) -> SyntheticSettledReceipt {
    SyntheticSettledReceipt {
        identity: item.identity.unwrap(),
        destination_verified: true,
        source_checkpoint: true,
        required_global_checkpoint: true,
        charges_settled: true,
        input_preserved: true,
        origin_retired_after_destination: true,
    }
}
#[derive(Debug, Eq, PartialEq)]
struct Report {
    confirmed: usize,
    pending: usize,
    skipped_or_cancelled: usize,
    rejected: usize,
    retained: usize,
}
fn report(items: &[Item; 2]) -> Report {
    let mut report = Report {
        confirmed: 0,
        pending: 0,
        skipped_or_cancelled: 0,
        rejected: 0,
        retained: 0,
    };
    for item in items {
        match item.state {
            State::Confirmed => report.confirmed += 1,
            State::Skipped | State::CancelledBeforeIntent => report.skipped_or_cancelled += 1,
            State::Rejected => report.rejected += 1,
            State::Retained => report.retained += 1,
            _ => report.pending += 1,
        }
    }
    report
}
fn all_confirmed(items: &[Item; 2]) -> bool {
    validate_identities(items).is_ok() && report(items).confirmed == 2
}

#[test]
fn mixed_batch_contract_success_requires_two_independent_settled_items() {
    let mut items = admitted_batch();
    for i in 0..2 {
        items[i]
            .settle_synthetic(synthetic_complete(items[i]))
            .unwrap();
    }
    assert!(all_confirmed(&items));
    let before = report(&items);
    assert!(
        items[0]
            .observe_owner(EffectOutcome::Unsupported {
                reason: "late duplicate".into()
            })
            .is_err()
    );
    assert_eq!(report(&items), before);
    items[0]
        .settle_synthetic(synthetic_complete(items[0]))
        .unwrap();
    assert_eq!(report(&items), before);
    assert!(
        items[0]
            .settle_synthetic(synthetic_complete(items[1]))
            .is_err()
    );
    assert_eq!(report(&items), before);
    items[1].identity.as_mut().unwrap().operation = items[0].identity.unwrap().operation;
    assert!(validate_identities(&items).is_err());
    assert!(!all_confirmed(&items));
    let mut missing = admitted_batch();
    for item in &mut missing {
        item.identity = None;
        item.state = State::Confirmed;
    }
    assert!(!all_confirmed(&missing));
}

#[test]
fn mixed_batch_contract_prefix_is_preserved_for_every_unsettled_sibling() {
    for state in [
        State::Planned,
        State::Decision,
        State::InFlight,
        State::Skipped,
        State::Rejected,
        State::Retry,
        State::Retained,
        State::CancelledBeforeIntent,
        State::CancelPending,
    ] {
        let mut items = admitted_batch();
        items[0]
            .settle_synthetic(synthetic_complete(items[0]))
            .unwrap();
        items[1].state = state;
        if matches!(
            state,
            State::Planned | State::Skipped | State::CancelledBeforeIntent
        ) {
            items[1].identity = None;
        }
        let r = report(&items);
        assert_eq!(r.confirmed, 1);
        assert_eq!(
            r.confirmed + r.pending + r.skipped_or_cancelled + r.rejected + r.retained,
            2
        );
        assert!(!all_confirmed(&items));
        items[0].cancel_remaining();
        assert_eq!(items[0].state, State::Confirmed);
    }
}

#[test]
fn mixed_batch_contract_checkpoint_and_retirement_gaps_never_confirm() {
    for slot in 0..2 {
        for gap in 0..6 {
            let mut items = admitted_batch();
            let mut receipt = synthetic_complete(items[slot]);
            match gap {
                0 => receipt.destination_verified = false,
                1 => receipt.source_checkpoint = false,
                2 => receipt.required_global_checkpoint = false,
                3 => receipt.charges_settled = false,
                4 => receipt.identity.fingerprint = "changed-choice",
                _ => match items[slot].kind {
                    Kind::Duplicate => receipt.input_preserved = false,
                    Kind::Move => receipt.origin_retired_after_destination = false,
                },
            }
            assert!(items[slot].settle_synthetic(receipt).is_err());
            assert_eq!(items[slot].state, State::InFlight);
            assert!(!all_confirmed(&items));
        }
    }
}

#[test]
fn mixed_batch_contract_owner_outcomes_retain_per_item_reason_class() {
    for (outcome, state, disposition) in [
        (
            EffectOutcome::Unsupported {
                reason: "unqualified".into(),
            },
            State::Rejected,
            FullProgressDisposition::FailedPreservingData,
        ),
        (
            EffectOutcome::Collision {
                reason: "occupied".into(),
            },
            State::Decision,
            FullProgressDisposition::BlockedByUser,
        ),
        (
            EffectOutcome::SupersededBeforeEffect {
                reason: "stale".into(),
            },
            State::Rejected,
            FullProgressDisposition::FailedPreservingData,
        ),
        (
            EffectOutcome::RetryableBeforeEffect {
                reason: "busy".into(),
            },
            State::Retry,
            FullProgressDisposition::RetryPending,
        ),
        (
            EffectOutcome::RetainedUncertain {
                reason: "ack lost".into(),
            },
            State::Retained,
            FullProgressDisposition::AuditRequired,
        ),
        (
            EffectOutcome::StoppedAtSafeBoundary {
                checkpoint: "closed".into(),
            },
            State::InFlight,
            FullProgressDisposition::None,
        ),
    ] {
        let mut items = admitted_batch();
        items[0]
            .settle_synthetic(synthetic_complete(items[0]))
            .unwrap();
        items[1].observe_owner(outcome).unwrap();
        assert_eq!(items[1].state, state);
        assert_ne!(disposition, FullProgressDisposition::Succeeded);
        assert!(!all_confirmed(&items));
        if state == State::Retained {
            assert!(
                items[1]
                    .settle_synthetic(synthetic_complete(items[1]))
                    .is_err()
            );
        }
    }
}

#[test]
fn mixed_batch_contract_retry_and_cancel_preserve_admitted_identity() {
    let mut items = admitted_batch();
    items[0]
        .settle_synthetic(synthetic_complete(items[0]))
        .unwrap();
    items[1].state = State::Retry;
    let identity = items[1].identity.unwrap();
    assert_eq!(items[1].retry(identity).unwrap(), identity);
    let persisted: OperationId =
        serde_json::from_str(&serde_json::to_string(&identity.operation).unwrap()).unwrap();
    assert_eq!(persisted, identity.operation);
    assert!(
        items[1]
            .retry(Identity {
                fingerprint: "different-target",
                ..identity
            })
            .is_err()
    );
    assert!(
        items[1]
            .retry(Identity {
                operation: items[0].identity.unwrap().operation,
                ..identity
            })
            .is_err()
    );
    items[1].cancel_remaining();
    assert_eq!(items[1].state, State::CancelPending);
    assert_eq!(items[1].identity, Some(identity));
    assert!(!all_confirmed(&items));
    let mut unstarted = Item {
        kind: Kind::Move,
        identity: None,
        state: State::Planned,
    };
    unstarted.cancel_remaining();
    assert_eq!(unstarted.state, State::CancelledBeforeIntent);
    assert!(unstarted.identity.is_none());
}

#[test]
fn mixed_batch_contract_conflict_choices_do_not_rewrite_an_admitted_intent() {
    for (choice, expected) in [
        (Choice::Skip, State::Skipped),
        (Choice::Rename, State::Planned),
        (Choice::Overwrite, State::Rejected),
    ] {
        let mut items = admitted_batch();
        items[0]
            .settle_synthetic(synthetic_complete(items[0]))
            .unwrap();
        assert!(items[1].choose_before_intent(choice).is_err());
        let identity = items[1].identity;
        assert_eq!(items[1].state, State::InFlight);
        assert!(identity.is_some());
        items[1] = Item {
            kind: Kind::Move,
            identity: None,
            state: State::Decision,
        };
        items[1].choose_before_intent(choice).unwrap();
        assert_eq!(items[1].state, expected);
        assert!(items[1].identity.is_none());
        assert_eq!(report(&items).confirmed, 1);
        assert!(!all_confirmed(&items));
    }
}
