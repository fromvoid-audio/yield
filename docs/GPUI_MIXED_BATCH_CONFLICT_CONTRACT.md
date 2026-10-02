# One duplicate-plus-move conflict contract (OPT-1587)

Decision baseline: `18db2b075` on current main. This is a contract and a
**test-only executable fixture**, not a batch dispatcher, filesystem adapter,
new durable transport, or physical qualification. OPT-1595 implements the
consumer later; OPT-1479 and the physical admission/effect tasks remain open.

## Exact caller and reference inventory

The archived `reference/radiant/src/sample_sources/duplicate_file_ops.rs`
`execute_duplicate_context_sample_same` chooses a numbered non-colliding name,
then copies and registers metadata. Its pathname copy/removal behavior is not
an accepted capability-relative backend. The archived
`native_app/sample_library/folder_browser/file_move_conflicts.rs` retains
`FileMoveConflictBatch`, advances `current_index`, and distinguishes moved,
skipped and remaining items. `apply_failed_file_move_conflict` preserves the
already moved prefix and pending batch; `status_with_metadata_error` reports a
metadata error separately. Cancellation skips the remaining conflicts; it does
not undo the completed prefix. Legacy choices are Overwrite, Rename and Skip.
They supply UX vocabulary, not current ownership or atomicity guarantees.

Current GPUI `browser/handoff.rs` handles pathname clipboard/reveal handoff,
not a duplicate/move batch producer. There is no production mixed-batch caller.
`NamespaceCommand::Move`, the ordinary duplicate admission inventory in
`GPUI_PRODUCTION_ADMISSION_INVENTORY.md`, `FileOperationOwner` request/results,
and `FullProgressDisposition` are existing integration shapes. Mutation workers
remain Unsupported unless their exact producer and backend are qualified.
`EffectOutcome::Applied` can be read-only staging validation; it is not item
completion. A status label, watcher echo or displayed destination path is not a
publication or cleanup receipt.

## Bounded plan, identity and conflict decisions

This decision covers exactly two **disjoint** captured regular files in one
source: slot D duplicates A to A-copy; slot M moves B to a different parent.
Order is D then M. No item consumes another item's output, aliases its input,
shares a target, or depends on a successful predecessor. Reject such plans and
replan a dependency-aware contract; cross-source retirement uses OPT-1641 and
its consumers rather than inferring it from this same-source fixture.

The batch correlation key plus fixed slot identifies a logical item before
admission. It is not an OperationId or authority. Each admitted item has its own
persisted OperationId and immutable command fingerprint, source/profile/lease
fences and descriptor-bound target. Pre-intent rejection leaves OperationId
absent. No aggregate OperationId substitutes for per-item admission. Duplicate
operation IDs across the two items, conflicting fingerprints, foreign receipts,
and a receipt for the sibling slot are rejected without advancing either item.

| Choice | Exact behavior |
| --- | --- |
| Skip | Set only that unstarted item to Skipped; no effect or cleanup claim. |
| Rename | Choose a contained leaf and revalidate captured source/target fences. A collision returns to that item's decision; never overwrite by fallback. |
| Overwrite | Blocked/Unsupported in this initial slice. Requires a separately qualified expected-target replacement and independent cleanup contract. |
| Cancel remaining | Stop admission of unstarted items. Completed prefix stays confirmed; an admitted/in-flight item needs its own closed-barrier/outcome settlement. |

Before Intent, a changed destination/choice replaces the proposed plan and
requires fresh validation. After Intent, retry preserves the same OperationId,
fingerprint, captured target and choice. A changed choice is a conflicting new
intent, not a retry; settle the old item and explicitly create a new intent if
safe. Reopen/restart must load persisted per-item records and retain uncertain
rows. Array order, paths or a fresh UUID cannot reconstruct a lost identity.
The consumer must persist the batch-to-item correlation before reporting restart
support; this fixture does not implement that persistence.

## Per-item outcomes and truthful aggregation

| Per-item state/evidence | Reporting and next action |
| --- | --- |
| Planned / decision pending | No success. Only separately validated admission may start work. |
| Superseded, unsupported, conflict or retryable before effect | Report the exact rejection/decision/retry reason; no publication or source removal. |
| Admitted/in flight, including cancel request | Pending settlement; cancellation is not a terminal no-effect proof. |
| Destination published but metadata/checkpoint incomplete | PartialNeedsRetry with the same item identity; retained evidence and charge custody remain. |
| RetainedUncertain / foreign or ambiguous recovery evidence | AuditRequired/retained; no automatic retry effect, path rollback or charge release. |
| Synthetic fully settled receipt (fixture only) | Confirm only its matching item after destination verification, required source/global checkpoints and charge settlement; D preserves its input, M requires exact origin-retirement proof after destination verification. |

The report always carries two item rows and counts of confirmed, pending,
skipped/cancelled-before-intent, rejected and retained items. All-item success
requires **two independently confirmed complete items**, never one successful
prefix, no remaining conflict dialog, two low-level Applied results, or a
terminal failure/cancellation label. One confirmed item plus any other outcome
is a partial batch report. Cancellation never deletes D's successful output to
simulate transaction rollback. Any compensation is a separately authorized
per-item operation with independent cleanup/restoration evidence.

The synthetic receipt fields in `mixed_batch_contract_tests.rs` encode these
preconditions for review; they cannot construct a live owner capability or a
verified filesystem checkpoint. Production integration must validate actual
sealed receipts, exact object provenance, durable checkpoints and source/global
publication before replacing those fixture inputs. Repeated identical receipts
are idempotent; a mismatched receipt cannot overwrite a confirmed row.

## Verification and unresolved implementation

Portable fixtures exercise prefix failure, unsupported/retained owner outcomes,
foreign/sibling identity, duplicate IDs, immutable retries and changed choices,
incomplete publication/source/retirement/settlement evidence, duplicate delivery,
and cancel before admission versus in flight. They use existing OperationId,
EffectOutcome and FullProgressDisposition vocabulary and open no filesystem.

OPT-1595 still owns persisted batch correlation, real per-item admission,
conflict command routing and recovery/aggregate projections. OPT-1694/1701 and
their qualified backend/proof gates remain open. Native conflict UI/pointer/AX
acceptance belongs to OPT-1953; no native or physical result is claimed here.
