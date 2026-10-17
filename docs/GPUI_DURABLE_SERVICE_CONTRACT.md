# GPUI durable-service decision (OPT-1569)

GPUI submits bounded observations to the existing profile-owned journal/coordinator path in
`yield-library`; it does not own another coordinator, journal, SQLite connection, filesystem
root, source lease, capacity provider or progress machine. This document freezes the adapter
contract. It does not implement the service or a supported operation-family adapter. OPT-1473
remains open. Ordinary registry/preferences/vocabulary routes retain their separate
[global command policy](GLOBAL_COMMAND_REPLAY_CONTRACT.md); they are not durable file operations.

## Envelope and handoff

The future renderer-neutral family facade lives in `yield-library`, beside the existing owner
and typed command codecs. GPUI calls that facade through a retained session service handle.
The session retains lifecycle ownership; cloning a UI handle never clones teardown authority.
The existing `OperationJournalClient` is the journal handoff. `PreparedAdmissionInput` and
`admit_prepared` are crate-private today: the facade must construct valid input inside that
boundary, not expose the private constructor or substitute `persist_intent` as admission.
The latter persists observations without providing universal capacity/effect authorization.

The proposed version-one transport has these distinct values (not new implemented Rust types):

| Value | Required fields and meaning |
| --- | --- |
| Initial request | UI-local request correlation, original `ProfileAcquisitionId`, installed service generation, command codec/version, bounded typed family args, exact canonical payload identity, source IDs and observed `SourceRevision`/lifecycle context as applicable. No UI-allocated OperationId. |
| Accepted handle | Original request context plus one clone-free bounded owner result handle. Queue admission means pending, not IntentDurable or successful effect. |
| Completion | Original correlation/acquisition/generation/payload identity, exact owner outcome below, and returned OperationId/sequence/work-unit identities when present. No serialized handle or live capability. |
| Subsequent command | The admitted stable OperationId, expected durable sequence, exact participant/work-unit/action and bounded typed command, with freshly validated current owner/source context. Historical identity does not restore authority. |

The facade checks the installed profile acquisition before enqueueing and retains the original
request context for delivery. The owner revalidates its live profile/root/database binding,
source registration/revision/lifecycle, current capacity/recovery state and command shape before
allocating intent identity or entering effects. A stored relative path or source ID is an
observation; physical access requires retained owner capabilities. A current service cannot
redirect an old queued intent into a new acquisition. On UI delivery, mismatch with the pending
request or currently installed acquisition/generation is stale and cannot confirm current state.

No render callback waits on `recv_timeout`, joins a worker, opens a database, performs hashing
or prepares filesystem/capacity authority. Submission uses the existing nonblocking bounded
owner queue. The facade retains at most 64 outstanding requests and an encoded input budget
no greater than `OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES` (four MAX_RECORD_BYTES).
It must reserve its handle/context budget before submission and refund on rejection. Journal
command capacity is 32; queue/results/input exhaustion remains the existing typed rejection.
Poll with `OperationJournalRequest::try_result` through at most 16 pending handles per UI update;
Empty remains pending. Retain the original context until consumption or explicit detachment.
A worker bridge may schedule UI notifications but must not create an unbounded result queue.
Whole-source scans, native qualification, owner startup and blocking teardown occur off render.

## Completion and recovery vocabulary

| Actual owner result | Required UI/service interpretation |
| --- | --- |
| `OperationJournalOwnerQueueError` | Not enqueued; retain typed Initializing/Full/ResultsFull/InputBytesFull/InvalidInput/Unavailable/Closed. No fabricated OperationId or successful retry. |
| `PreparedAdmissionError` / `CapacityProviderUnavailable` | Pre-intent rejected. Preserve Unsupported, RecoveryRequired, reserve/disk pressure, descriptor refresh or claim conflict reason. No operation row/effect success. |
| `Ok(OperationId)` from prepared admission | Intent admitted by the existing owner. This is not publication, participant reconciliation or terminal success. Subsequent work stays on the derived frozen plan. |
| `PreparedAdmissionRetainedError::Committed` | Retain exact receipt, claim IDs and reconciliation reason; committed intent has an unresolved acknowledgement/disposition. Do not retry as a new operation. |
| `PreparedAdmissionRetainedError::Uncertain` | Retain original OperationId, claims, write stage and attempted sequence. Reconcile through the owner; timeout/disconnect must not release held capacity or allocate replacement work. |
| `PreparedAdmissionRolledBackError` | Identity was allocated, then rollback confirmed. Preserve that identity and diagnostic; do not mislabel it as a pre-ID rejection. |
| `JournalInsertOutcome::Committed` | Exact persistence acknowledgement for that record, not capacity admission, physical effect or full-operation completion. `Failed` retains its write-stage diagnostics. |
| Request Empty / Disconnected; worker panic | Empty stays pending; disconnected/panic is unconfirmed and triggers owner recovery observation. A lost response is not rollback, cancellation, unsupported completion or success. |

Recovery uses existing `get`/`scan_recovery_page` and the crate-private family-aware
`get_full`/`scan_coordinator_recovery_page`, through the facade, preserving operation ID,
sequence, codec family, cursor and retention reason. Combined pages are capped at 16 rows and
2 MiB. Consistent legacy and full-progress rows remain distinct; Retained rows require audit.
Decoded records are observations. Recovery reacquires live authority and verifies immutable
participant receipts before any private `VerifiedStepCompletion`. It never copies a stored
summary into a live grant, rewrites a historical receipt with a fresh lease, or resumes by path
alone. Cursor/generation from another installed service cannot acknowledge a current operation.
The full [participant contract](PARTICIPANT_CHECKPOINT_CONTRACT.md) governs ordered work,
exact replay, PartialNeedsRetry, AuditRequired and guarded terminal dispositions. Only the
existing owner can publish a validated terminal state after required work and settlement.

## Cancellation and lifetime

Before submission, cancellation drops the local proposal. After queue acceptance, dropping the
result handle releases its result permit but does not cancel accepted work. Detaching a view is
not cancellation. The service retains sufficient correlation for owner recovery on lost results.

A durable cancellation request uses the existing `request_cancel_before_publish` control path
with exact OperationId, expected sequence and bounded `CancellationRequestObservation` where
that record family supports it. Its receipt acknowledges the request, not completed cleanup.
Full-progress cancellation must use its own operation/work-plan disposition; it cannot be
projected into legacy after-publish vocabulary. Entered work must quiesce, staging absence and
capacity settlement must be verified before CancelledBeforePublish. After observed effects,
preserve phase/receipts and continue cancellation-scoped reconciliation or verified compensation.
The family adapter must implement that bridge; unsupported cancellation stays explicitly retained.

Shutdown closes UI admission, drains family/entered work and participant consumers before the
journal/profile custody. It occurs off render and uses existing owners. The
[global drain report](GLOBAL_LIBRARY_SHUTDOWN_CONTRACT.md) is only joined teardown observation;
it does not mark durable operation dispositions complete. A deadline with pending work retains
custody and pending status. Profile switching creates a new generation/acquisition and clears
current UI confirmation; old durable work remains recoverable through the original journal.

## Focused typed success/rejection note

The following uses actual interfaces; it requires no second coordinator or private UI database:

1. The existing `owner_persists_and_reads_through_cloneable_client` fixture sends a bounded
   record through `OperationJournalClient::persist_intent`, reads a Committed receipt and then
   the exact stored record through the same owner. Its success is explicitly **record persistence**.
   The UI projection may observe that receipt for its matching context; it cannot call it a
   successful create or construct a `VerifiedStepCompletion`.
2. `full_admission_fails_closed_without_mutating_either_journal_family` queues typed family
   arguments to the same owner, receives CapacityProviderUnavailable and reopens both legacy
   and full-progress recovery rows unchanged. Queue success therefore produces a typed rejected
   admission, not an OperationId, second journal or filesystem effect.
3. `owner_reads_full_progress_and_combined_recovery_through_one_queue` observes both record
   families through the same owner. A transport can retain family/sequence/audit results without
   reconstructing another progress machine in GPUI.
4. Actual prepared-capacity success/retained/fault fixtures are macOS-gated and separate from
   these portable transport checks. Their native execution is not asserted by this decision.
   A future adapter's create-success evidence must include actual owner admission plus all its
   required publication/source/global/Harvest receipts, not the persistence fixture above.

## Concrete consumer gates retained

The original owner now supplies `owner::request_inbox::JournalRequestInbox<T>`
as a renderer-neutral delivery foundation. It prepays its fixed 64 slots from
the existing journal input pool before construction. A clone-free reservation
accounts bounded proposal bytes before family submission; rejection refunds
that accounting. Adoption compares the original result permit's owner Arc,
and never redirects a foreign or already consumed handle. Each installation
uses a checked, non-reused generation issued by that same owner, paired with
its original profile acquisition and original owner incarnation. Restarting
the journal under the same retained profile acquisition cannot make an old
delivery current. The incarnation is equality data, not permission; adoption
still requires the actual original result permit's owner Arc. Polling rotates through at most 16 actual
handles, retains Empty, and delivers exact owner outcomes or Unconfirmed
channel loss with original correlation/codec/payload context. Replacement
installation comparison is explicit; stale delivery cannot confirm its state.
Detachment returns the original context and does not cancel accepted work.

This foundation does not submit or prepare a create operation. The family
adapter must compute canonical payload identity and its actual encoded bound,
construct private prepared admission inside the existing owner boundary, and
retain the session's installation context. The inbox's real SQLite persistence
and pre-intent refusal tests verify delivery semantics only. Supported create
success, GPUI/session wiring, source/claim/physical authority, cancellation and
full OPT-1583 acceptance remain open behind their concrete implementation and
native qualification prerequisites.

- OPT-1583 owns the renderer-neutral supported create facade, bounded GPUI pending/result
  correlation, worker handoff and actual supported-operation success/pre-intent rejection.
  It must follow the first unresolved backend leaf, rather than make private admission public.
- OPT-1692 owns actual create admission; its concrete folder producer/owner and publication
  prerequisites remain authoritative. OPT-1634 owns verified filesystem checkpoint bridging;
  missing production bridges cannot be deferred as native tests.
- OPT-1592 owns restore-family wiring; OPT-1597 owns crashed-phase replay. This decision does
  not complete either. Current-format pending recovery rows and transaction integrity
  remain protected. Earlier alpha family migrations/legacy readers are not required
  under the [active alpha policy](../AGENTS.md#active-alpha-compatibility-policy).
- Full participant/lifecycle/physical acceptance remains in OPT-1341, OPT-1357, OPT-1670,
  OPT-1473 and their concrete leaves. Native handle/effect qualification retains OPT-1843's
  matrix and scoped platform issues. No unavailable platform is needed to finish this decision.

This note resolves the decision only. It names real owners, freezes the envelope and result
semantics, and records the missing facade/consumer implementations instead of inventing a
parallel coordinator or pretending the transport fixtures supply production effect authority.
