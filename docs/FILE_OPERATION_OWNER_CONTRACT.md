# FileOperationOwner command, result, and capability-fence contract

Status: [OPT-1629](https://linear.app/boostnlvp/issue/OPT-1629) decision artifact, with the bounded queue foundation from [OPT-1630](https://linear.app/boostnlvp/issue/OPT-1630) implemented in [`file_operation_owner.rs`](../crates/yield-library/src/file_operation_owner.rs). The module owns command validation, owner-local registration observations, lifecycle, bounded receipts, and fail-closed result handling. It does not add a filesystem backend, enable production admission, or provide a production root/live-write/custody producer; those adapters remain explicit follow-up work.

Related contracts: [IO architecture target](IO_ARCHITECTURE_TARGET.md), [capacity authority contract](CAPACITY_AUTHORITY_CONTRACT.md), [OPT-1337](https://linear.app/boostnlvp/issue/OPT-1337), [OPT-1338](https://linear.app/boostnlvp/issue/OPT-1338), and [OPT-1554](https://linear.app/boostnlvp/issue/OPT-1554).

## Decision summary

The coordinator owns the logical operation. A future `FileOperationOwner` owns only registered target roots, staging descriptors, and filesystem effects performed under those registrations. The operation journal/source database owner remains responsible for SQLite connections, source leases, source transactions, durable operation dispositions, and checkpoint persistence. UI and adapters submit bounded intent; they do not turn a path, snapshot, UUID, or decoded receipt into authority.

The owners communicate with bounded commands and effect-specific receipts. Neither owner synchronously waits for the other while holding an owner command or SQLite transaction. A source lease, source control connection, publication fence, profile guard, or borrowed database connection never crosses into the file owner. A future source/file adapter may carry a sealed `Send`-safe authority request derived by the source owner, following the existing source-authority pattern, but the source owner retains the lease and its control connection.

This is a contract decision rather than a claim that the boundary already exists. The current [operation-journal owner](../crates/yield-library/src/operation_journal/owner.rs) is a bounded storage owner and is useful as a queue/result-permit baseline. The current [source authority](../crates/yield-library/src/sample_sources/db/source_authority.rs) illustrates the sealed request/owner-held lease split. The current [`file_ops_journal`](../crates/yield-library/src/sample_sources/db/file_ops_journal/) is source-database-local recovery evidence, not the proposed general file owner.

## OPT-1630 implementation boundary

`yield_library::file_operation_owner::FileOperationOwner` starts one background owner thread with a 32-command synchronous queue. It reserves one result permit before queueing and retains the complete canonical request bytes, input permit, and receipt identity until each result alias is acknowledged. Submission normalizes owned variable-capacity buffers and charges a checked `2 * canonical_bytes + authorization_backing_bytes` amount: two canonical-record units cover the command backing and canonical copy, while every normalized live-authorization source-fence vector and its source/root/database string backing is charged separately because live proofs are intentionally excluded from the fingerprint. The input permit is held while any representation remains retained, including terminal tombstone history. Exact completed requests remain collision-safe owner-local tombstones after acknowledgement so a duplicate cannot execute again; tombstones share the 64-receipt bound and retain their input permits. When that bounded history is full, new identities receive backpressure rather than causing eviction, and a failed queue send refunds the full checked charge. A confirmed coordinator-custody acknowledgement is scoped to this owner and the exact retained identity/fingerprint set; it is the only testable seam that permits the owner to release this retained history at shutdown.

The public command and effect types have private fields. Root registrations, live intent/capacity authorization, and custody acknowledgements have no production constructors in this milestone; the module's `cfg(test)` fixtures are synthetic validation seams only. Every accepted production-shaped command therefore returns typed `Unsupported` evidence because no qualified producer or filesystem backend is available. `ObservationOnly` authorization is accepted only for inspection-shaped requests and can never be used as a mutation fallback. The owner rejects stale owner/generation/fence values, invalid locators, overflow, duplicate identity conflicts, and more than two roots or source fences before any effect could run.

The lifecycle is owner-local and nonblocking across owners: `Running` accepts work, `Draining` closes the effect front door while already accepted commands and acknowledgements drain, and `shutdown` reports `DrainedAck` only when no retained receipt remains or confirmed custody has been supplied. Otherwise it returns `RetirementDeferred` and leaves the owner explicitly `Draining` with its command, receipt, fingerprint, and input evidence retained for reconciliation; deferred shutdown is not a clean `Stopped` state. Panic or implicit drop remains sticky `Unavailable`/`RetirementUncertain` with bounded effect identities and retained state, and normal worker exit cannot overwrite that failure state. No source lease, SQLite connection, profile guard, filesystem handle, or physical effect crosses this module.

## Ownership and lifetime

| Owner or participant | Owns | Must not own or authorize |
| --- | --- | --- |
| Coordinator and journal owner | `OperationId`, participant ordering, durable intent/disposition, durable checkpoint acknowledgement, claim/release identity | Target-root capabilities, source SQLite transactions while a file effect runs, UI projections, or a filesystem effect based on an observation |
| `FileOperationOwner` (proposed) | Retained target-root and staging descriptors, root registration generations, bounded filesystem effects, effect receipts | SQLite connections, source/global database transactions, source leases/control connections, UI state, or a durable operation terminal disposition |
| Source/database owner | Source lease, control connection, source/database root descriptors, source transactions, source lifecycle generation and lease epoch, source retirement | File-owner root registration, target publication, or a cross-owner wait while its transaction is held |
| UI and adapters | User intent and bounded command construction | Path-based authorization, root capabilities, capacity authority, or operation success |

Root capabilities and live write authority are owner-local and non-serializable. A queue reference identifies a retained registration but does not itself grant a write. Registration, drain, retirement, cancellation-at-boundary, and shutdown are control messages separate from effect commands. A registration control does not carry a durable operation identity because it manages an owner resource; the coordinator acknowledges the registration and later binds effects to an operation and participant.

The proposed lifecycle is `Running -> Draining -> Stopped`. `Running` accepts new registered work. `Draining` rejects new effects, finishes an accepted effect at its safe boundary, and retains unresolved evidence and charges. `Stopped` accepts no work. A successful `DrainedAck` is distinct from `RetirementDeferred` and `RetirementUncertain`: it requires that accepted effects have reached a safe boundary and that pending receipts/evidence have transferred to confirmed durable coordinator custody. A bounded drain with unresolved effects or receipts cannot claim clean retirement or clean `Stopped`; it reports deferred/audit state and retains charges. Drop or panic never reports a clean handoff, removes staged data, releases a charge, or fabricates a retirement acknowledgement. Unexpected stop may leave only confirmed intent recovery evidence; it cannot claim a checkpoint that was observed but not durably acknowledged. Root/source retirement waits for an exact drained acknowledgement and matching generation.

## Proposed command envelope

The following is the proposed shape, not a production type declaration. Names may be refined when the producer adapters are implemented, but the fields and authority boundaries are fixed by this decision.

```text
FileOperationCommand {
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: EffectId,                 // stable work-unit/effect identity
    source_fences: bounded Vec<SourceFence>,
    target_registration: RootRegistrationRef,
    expected: ExpectedObject,            // exact identity or explicit absence
    effect: EffectRequest,
    authorization: EffectAuthorization,
}

SourceFence {
    source_id: SourceId,
    lifecycle_generation: Generation,
    source_revision: Option<SourceRevision>,
    lease_epoch: PositiveLeaseEpoch,
}

EffectAuthorization {
    Mutating { live_intent: LiveIntentAuthorization, live_capacity: LiveCapacityAuthorization },
    ObservationOnly { live_root_registration: RootRegistrationRef,
                      source_fences: bounded Vec<SourceFence> },
}

RootRegistrationRef {
    registration_id: OpaqueRegistrationId,
    generation: Generation,
}

enum RootAuthorityFence {
    Source(SourceFence),
    Profile(ProfileAuthorityFence),
}

RootRoleBinding {
    registration: RootRegistrationRef,
    authority: RootAuthorityFence,        // matching SourceFence or ProfileAuthorityFence
    locator: BoundedRelativeLocator,
    expected: ObjectIdentityPredicate,    // exact identity or explicit absence
}

SourceRoot { binding: RootRoleBinding }       // authority must be Source(SourceFence)
DestinationRoot { binding: RootRoleBinding }  // source or profile authority as qualified
StagingRoot { binding: RootRoleBinding }      // retained owner/profile authority
```

Every command also carries one explicit effect payload. The payload owns the root
roles and bounded data for that effect; an envelope with only a target registration
or locators is incomplete:

```text
enum EffectRequest {
    CreateStaging { destination: DestinationRoot, expected_absent: AbsenceProof, bytes: PositiveBoundedBytes },
    CopyIntoStaging { source: SourceRoot, staging: StagingRoot, source_range: BoundedRange, bytes: PositiveBoundedBytes },
    WriteStagingChunk { staging: StagingRoot, chunk_id: ChunkId, issue_id: IssueId,
                        output: OutputIdentity, offset: BoundedOffset,
                        length: PositiveBoundedBytes, payload: BoundedBytes },
    FlushStaging { staging: StagingRoot, object: OwnedObjectIdentity },
    ValidateStaging { staging: StagingRoot, object: OwnedObjectIdentity, expected: StagingValidation },
    PublishNoReplace { staging: StagingRoot, target: DestinationRoot,
                       staged: CopyValidatedEvidence, expected_absent: AbsenceProof },
    MoveNoReplace { source: SourceRoot, destination: DestinationRoot,
                    source_object: OwnedObjectIdentity, expected_absent: AbsenceProof },
    CreateDirectoryNoReplace { destination: DestinationRoot, expected_absent: AbsenceProof },
    ReplaceExpectedTarget { staging: StagingRoot, target: DestinationRoot,
                            staged: CopyValidatedEvidence, expected_target: OwnedObjectIdentity },
    InspectOwnedObject { root: OwnedRoot, expected: ObjectIdentityPredicate },
    RemoveOwnedObject { root: OwnedRoot, expected: OwnedObjectIdentity, cleanup: CleanupProof },
    RestoreOwnedObject { root: OwnedRoot, expected: RetainedObjectIdentity, restore: RestoreProof },
}
```

The role table freezes what each payload must bind. Every role wrapper carries an
opaque registration identity and generation, its matching source or profile authority
fence, a bounded relative locator, and an exact object identity/absence predicate.
`SourceRoot` must bind a `SourceFence`; `DestinationRoot` and `StagingRoot` bind an
owner-local registration and generation plus an explicitly qualified source or profile
authority as applicable. Independent roles may share one registration when each role
still carries its own locator and object predicate. No target registration can authorize a source read or
a move's source half.

| Effect payload | Root roles and fences | Required predicate, proof, and data |
| --- | --- | --- |
| `CreateStaging` | One destination/staging root; profile authority may qualify the destination | Exact absence, positive bounded byte reservation, bounded staging locator, and live intent/capacity authorization |
| `CopyIntoStaging` | Independent source root + source fence and destination/staging root | Source object identity/range, destination absence or exact staging identity, positive bounded byte bound, and live source/capacity authorization |
| `WriteStagingChunk` | One retained staging root plus the source/producer fence required by its live grant | `ChunkId`, `IssueId`, exact output/artifact identity, checked offset, positive bounded length, bounded payload, exact expected staging identity, and a live write authorization; a copied chunk/issue UUID is insufficient |
| `FlushStaging` | One staging root | Exact owned staging identity and backend-specific flush/sync request; no namespace mutation |
| `ValidateStaging` | One staging root | Exact owned identity, byte/format/metadata validation requirements, and a live staging registration |
| `PublishNoReplace` | Independent staging root + destination root | `CopyValidated` evidence, exact staged identity, exact target absence, final no-replace proof, and required live intent/capacity authorization |
| `MoveNoReplace` | Independent source root + source fence and destination root | Exact source identity, exact target absence, qualified same-volume/no-replace primitive, and source/destination ownership proofs |
| `CreateDirectoryNoReplace` | One destination root or profile-authorized root | Exact target absence, bounded relative locator, and no-replace directory primitive |
| `ReplaceExpectedTarget` | Independent staging root + destination root | `CopyValidated` evidence, exact staged identity, exact expected target identity, qualified replace primitive, and live ownership proof |
| `InspectOwnedObject` | One retained owned root | Exact object predicate and owner-held descriptor; observation only, with no positive write-capacity charge or write authorization |
| `RemoveOwnedObject` | One retained owned root | Exact live object identity, independently qualified cleanup primitive, and cleanup proof; no path-only or recursive fallback |
| `RestoreOwnedObject` | One retained owned root | Exact retained object identity, qualified restore primitive, expected target predicate, and live ownership proof |

Each primitive is limited to at most two distinct registered roots and two source
fences. A workflow requiring more roots or fences is split into coordinator-owned
steps with separate receipts and durable checkpoints. `CopyIntoStaging`, `MoveNoReplace`,
and the publish/replace variants therefore cannot hide an unbound source or destination
half inside one target-root command.

The envelope's `target_registration` is the primary destination registration for
common routing. Effect payloads carry independent source and staging registrations
when their role requires them; those references are included in the request fingerprint
and are never inferred from the primary target.

`source_fences` is bounded and exact. A source fence must include the source identity, lifecycle generation, and a positive, nonzero authority lease epoch; a source revision is included when the source contract provides one. The owner revalidates every fence immediately before an effect. Lifecycle-generation values retain their existing domain; checked advancement rejects overflow or wrap, exact fence equality includes maximal generations, and freshness is never inferred from numeric wrap or ordering alone. `RootRegistrationRef` is an opaque bounded reference to an owner-local registration identity and generation. It contains no absolute path and is not a capability by itself.

`ExpectedObject` is an identity predicate, never a pathname assertion. It can require an exact object identity, an exact absence, or another explicitly qualified no-replace condition. Relative locators are bounded and interpreted only below the retained registered root. Absolute paths, serialized root capabilities, `FullProgress` snapshots, claim or issue UUIDs, and decoded receipts are observations; none can authorize an effect.

`EffectAuthorization::Mutating` is a proposed sealed pair. A future producer must obtain
the live intent and capacity proof through the coordinator/journal and source-owner
boundaries immediately before queueing or execution. `EffectAuthorization::ObservationOnly`
is accepted only by `InspectOwnedObject`; it carries a live root registration and any
source fences needed for that read, never a positive write-capacity fallback. A stored
claim, an old observation, or the synthetic owner-local chunk entry from OPT-1564 cannot
be presented as a general file write grant. The cross-owner adapter that would produce a
live file-write authorization is currently missing; its absence yields `Unsupported`,
never a path or UUID fallback.

Every effect variant except `InspectOwnedObject` requires `EffectAuthorization::Mutating`;
the owner rejects `ObservationOnly` for writes, publication, movement, replacement, or
cleanup rather than treating it as a weaker write grant.

The live authorization proofs and the full command envelope are intentionally non-`Clone` and
non-serializable in production. Duplicate identity uses the authority-free canonical request
projection, while the owner consumes live proofs only at its boundary. The current `cfg(test)`
fixtures can recreate synthetic proofs for boundary tests; they do not expose production
constructors or turn a serialized observation into authority.

## Effect vocabulary and responsibilities

The following vocabulary is intentionally small. Each effect is a filesystem-owner step; database/UI work remains a separate coordinator action and is acknowledged separately.

| Effect | File-owner responsibility | Coordinator/source/UI responsibility | Required boundary |
| --- | --- | --- | --- |
| `CreateStaging` | Create an owner-managed staging object below a registered root, only when the expected absence predicate still holds | Persist durable intent before the effect; retain staging identity and charge on uncertainty | Root registration, absence predicate, live intent/capacity authorization |
| `CopyIntoStaging` | Copy a bounded source range into owned staging and report exact bytes/identity | Supply a live source fence and claimed byte bound; persist checkpoint/evidence | Source fence and target registration are revalidated immediately before copying |
| `WriteStagingChunk` | Execute a bounded write using a future live cross-owner write adapter | Bind the effect to a stable participant/chunk identity and journal charge | OPT-1564's synthetic callback is insufficient physical progress evidence |
| `FlushStaging` | Flush the retained staged descriptor and report sync/media evidence | Persist the corresponding checkpoint; do not infer publication | Flush is a separate barrier from validation and publication |
| `ValidateStaging` | Reopen and validate owned staging identity, bytes, and required format/metadata | Persist `CopyValidated` or the equivalent checkpoint | Validation does not publish and does not complete the operation |
| `PublishNoReplace` | Make the final no-replace namespace claim and return observed target identity | Commit the coordinator's publication/participant transition after the receipt | Collision retains evidence; no overwrite or fallback namespace mutation |
| `MoveNoReplace` | Perform a specifically qualified same-volume/no-replace move | Coordinate source/database changes separately and in explicit order | Never silently become copy-then-delete across volumes |
| `CreateDirectoryNoReplace` | Create a directory only under a registered root and explicit absence predicate | Persist directory participant state and UI result | Collision is a retained, typed result |
| `ReplaceExpectedTarget` | Replace only with a qualified backend primitive and exact expected identity | Own the logical replace/disposition and rollback decision | Unsupported when the live primitive/proof is unavailable; never generic overwrite |
| `InspectOwnedObject` | Read identity/size/metadata only for an object proven below an owned root | Treat the response as observation, not execution authority | Stale or ambiguous identity is retained evidence |
| `RemoveOwnedObject` | Remove only with a specifically qualified cleanup capability and exact live ownership proof | Coordinate logical cleanup and durable disposition | No recursive cleanup, cross-volume fallback, or deletion from a path alone |
| `RestoreOwnedObject` | Restore only with an exact retained-object identity and qualified primitive | Coordinate the restore operation and its durable checkpoint | Unsupported and retained when proof or primitive is absent |

Compound rename, trash, history, destructive-edit, and cross-source workflows are coordinator compositions of these steps. The file owner does not invent an ordering, perform an implicit cross-volume copy/delete, overwrite a collision, or mutate a fallback namespace.

Control messages are separate from the effect vocabulary:

| Control | Result and lifetime rule |
| --- | --- |
| `RegisterRoot` | Returns an opaque registration identity/generation after the owner validates and retains the root descriptor. It has no `OperationId` and grants no effect by itself. |
| `RetireRoot` | Drains the exact registration generation; returns `DrainedAck` only after confirmed coordinator custody, otherwise `RetirementDeferred` or `RetirementUncertain`. It cannot release a charge on its own. |
| `CancelAtBoundary` | Records a coordinator cancellation request for an exact registration/effect boundary; it does not erase an accepted effect or infer physical cleanup. |
| `Drain` | Moves the owner to `Draining`, rejects new effects, and finishes accepted work only at a safe boundary. |
| `Shutdown` | Returns `DrainedAck` and permits clean `Stopped` only after confirmed coordinator custody; unresolved effects return `RetirementDeferred` or `RetirementUncertain`, with no clean handoff or unconfirmed recoverable-checkpoint claim. Drop/panic is never a successful retirement. |

## Fence, identity, and authority fields

| Field | Purpose | Observation that cannot substitute for it |
| --- | --- | --- |
| `OperationId` | Binds an effect to one coordinator operation | A filesystem journal row or UUID copied from a receipt |
| `ParticipantId` and `EffectId` | Makes each bounded effect/retry idempotent and identifies the work unit | A path, UI event, or a reordered command without stable identity |
| `SourceFence` | Binds source identity to lifecycle generation, optional revision, and positive lease epoch | `SourcePublicationFence`, a stale source snapshot, or a source path |
| `RootRegistrationRef` | Selects an owner-local retained root and generation | A serialized root/path or a registration UUID read from storage |
| `RootRoleBinding` | Binds registration/generation, matching source/profile authority fence, bounded relative locator, and exact identity/absence predicate | A root ID, path, or copied registration record |
| `ExpectedObject` | Prevents replace/remove against a changed object or non-absent target | A locator, old size, or an unverified directory listing |
| Bounded relative locators | Names source/staging/target entries below retained roots | Absolute paths or path normalization performed by a caller |
| `EffectAuthorization::Mutating` | Carries the live intent and capacity proofs required by mutating effects | `IntentDurable`, a claim/issue UUID, `FullProgress`, or a prior observation |
| `EffectAuthorization::ObservationOnly` | Carries only live root registration/source fences for `InspectOwnedObject` | Any attempt to treat an inspection result as write authority |

All fences are echoed in the result. Root and source identities are revalidated at the consuming boundary and directly before any effect. If a fence is stale before an effect, the owner returns `SupersededBeforeEffect` and performs no filesystem mutation. If state may have changed after an effect could have started, the owner returns a retained uncertainty/audit result; it never turns uncertainty into generic success or failure.

## Results and durable evidence

An effect result is a step receipt, not operation success. The coordinator alone maps receipts to durable operation dispositions. A result echoes the operation, participant, effect, source lifecycle/revision/epoch, root registration/generation, and expected-object fences. It carries separate observations for exact object identity, byte count, staging validation, namespace claim, reopen verification, visibility, and sync/media evidence. A copied result cannot mint a root, write grant, publish-ready receipt, or cleanup permit.

The proposed result shape is likewise a contract sketch, not a production type:

```text
FileOperationResult {
    operation_id: OperationId,
    participant_id: ParticipantId,
    effect_id: EffectId,
    source_fences: bounded Vec<SourceFence>,
    root_bindings: bounded Vec<RootBindingObservation>, // every source/staging/destination role
    target_registration: RootRegistrationRef,
    expected: ExpectedObject,
    observed: EffectEvidence,             // identity, bytes, stage, namespace, reopen, sync/media
    outcome: EffectOutcome,
}

RootBindingObservation {
    role: RootRole,
    registration_id: OpaqueRegistrationId,
    generation: Generation,
    authority_fence: AuthorityFenceObservation,
    locator: BoundedRelativeLocator,
    expected: ObjectIdentityPredicate,
    observed_identity: Option<OwnedObjectIdentity>,
}
```

`EffectEvidence` keeps bytes/staging validation, namespace claim, reopened verification,
visibility, and sync/media observations separate. It is not a live capability. The result
echoes every role binding and source fence as authority-free observations (including
registration/generation, matching authority-fence observation, locator, expected predicate,
and observed identity), rather than returning a live root or grant. A control ack echoes
the registration identity/generation and any retirement fence but has no durable operation
identity unless a coordinator separately binds it to one.

| Result | Meaning and required retention |
| --- | --- |
| `Applied(step_receipt)` | This effect completed with the receipt's exact identity and evidence. Later barriers remain outstanding unless separately acknowledged. |
| `SupersededBeforeEffect` | A live fence was stale before any effect; no effect was attempted. The coordinator may schedule a fresh generation. |
| `Collision(retained_evidence)` | The expected absence or identity predicate failed. Preserve the observed object and do not overwrite or delete it. |
| `Unsupported(no_effect)` | No qualified primitive or producer authority exists. Preserve staged data/claims and the reason; do not fall back to paths or UUIDs. |
| `RetryableBeforeEffect` | The effect was not attempted and bounded retry may be made with a fresh live authorization. |
| `RetainedUncertain/AuditRequired` | The effect may have occurred or durable acknowledgement is uncertain. Preserve exact identities, charges, and staged evidence until reconciliation. |
| `StoppedAtSafeBoundary(retained_checkpoint)` | Drain/shutdown stopped after a known safe barrier whose checkpoint acknowledgement is confirmed. Retain that checkpoint and any unresolved evidence; otherwise use `RetainedUncertain/AuditRequired` and do not claim publication or cleanup. |

Dropping a result receiver does not cancel an accepted command and does not release its charge. A late result is reconciled by its operation/participant/effect identity. Duplicate execution is idempotent only when the complete request fingerprint is identical: effect identity, operation/participant binding, root registrations, source fences, expected identities, payload bounds, and either immutable payload content or an immutable payload identity. An in-flight identical duplicate joins the existing bounded receipt rather than executing again. A completed identical duplicate returns the same receipt; a changed payload, fence, root, expected identity, or bound is a typed conflict with no replacement or second effect.

The owner retains an unacknowledged receipt until the coordinator confirms durable acknowledgement or takes confirmed durable custody of the evidence. Restart observations cannot reauthorize an effect whose completion or acknowledgement was uncertain. Receipt lifetime is bounded by the owner result/receipt cap below; reaching that cap applies backpressure before a new effect and never evicts uncertain evidence.

The barriers have this required partial order:

1. Durable intent and the necessary live capacity authorization in `EffectAuthorization::Mutating` occur before any mutating filesystem effect.
2. Staging and bounded writes occur under their live-bound source, staging, and destination roots.
3. Staged identity/content validation completes and the coordinator durably acknowledges `CopyValidated` (or its equivalent) before the final namespace claim.
4. Backend-required staging flush/sync prerequisites occur at their qualified positions before the final namespace claim. OPT-1631 and the existing macOS publication decision define the exact validation/flush ordering for each backend; this contract does not impose a universal flush-then-validate sequence.
5. The final no-replace namespace claim occurs only after the required validation, durable checkpoint, and staging backend prerequisites succeed.
6. After the final claim, the target is reopened and content-verified, and required destination-directory/target sync, visibility, and media evidence are collected before a verified-publication receipt is issued.

An incomplete write or staging flush never creates publish readiness. Target reopen/visibility
evidence does not precede the final claim. Partial staging is retained and is never
publish-ready. `CopyValidated` is not an operation terminal state. The existing source-local
`FileOpJournal` stages (`Intent`, `Staged`, `TargetDb`, `SourceDb`) describe recovery
observations and cannot be used as this universal owner contract without the missing
operation/participant/fence adapter.

## Queue, backpressure, and shutdown baseline

The current operation-journal owner establishes the bounded queue conventions that a future file owner should reuse:

| Current journal baseline | Value and behavior |
| --- | --- |
| Command queue capacity | `OPERATION_JOURNAL_OWNER_COMMAND_CAPACITY = 32` |
| Result permits | `OPERATION_JOURNAL_OWNER_RESULT_CAPACITY = 64` |
| Queued input-byte bound | `OPERATION_JOURNAL_OWNER_MAX_QUEUED_INPUT_BYTES = MAX_RECORD_BYTES * 4` |
| Queue admission | `try_send` and explicit `Full`, `ResultsFull`, `InputBytesFull`, `RecordTooLarge`, invalid-input, unavailable, and closed errors |
| Result ownership | `OperationJournalRequest::try_result`/`recv_timeout`; result permits are acquired before queueing |
| Dropped receiver | Does not cancel an admitted command; the owner retains the command's durable evidence |
| Owner lifecycle | Existing journal owner has Initializing/Available/Unavailable/Closing/Closed; the file owner contract adds Running/Draining/Stopped semantics for registered roots |

These are the observed baseline limits, not a claim that a file-owner queue is already implemented. A future file owner must apply explicit pre-queue backpressure to command bytes and result permits, keep pending evidence bounded, and avoid an unbounded retry loop. A drain rejects new effects, allows an accepted effect to stop at a safe barrier, and retains its exact evidence. Shutdown must not synchronously wait on a source or journal owner while holding a command or transaction.

The proposed file-owner limits are explicit contract bounds, separate from the
observed journal-owner constants above:

| File-owner bound | Required limit |
| --- | --- |
| Queued commands | At most 32 commands |
| Result permits | At most 64 result permits |
| One command input | Encoded/buffered payload at most `MAX_RECORD_BYTES` |
| Queued plus executing input | At most `MAX_RECORD_BYTES * 4` retained input bytes |
| Unacknowledged effect receipts | At most 64 receipts, including pending/uncertain receipts |

The input permit remains held until the owner no longer retains the command payload
or its executing buffer, including acknowledged tombstone history. The implemented
submission path normalizes variable-capacity owned buffers and reserves the checked
`2 * canonical_bytes + authorization_backing_bytes` charge described above. Result
permits are acquired before queueing. Cancellation is recorded against an already-admitted
identity under the admission gate; a queued cancellation is consumed before the execution
boundary and returns a stable no-effect result, while an executing identity returns
`AlreadyStarted` and a completed identity returns `AlreadyCompleted`. Cancellation is
nonblocking, never creates an unbounded tombstone, and never waits on another owner. When
the pending receipt cap is reached, the owner returns bounded backpressure
before starting an effect; it never evicts, overwrites, or forgets uncertain evidence. These limits are
implemented by the OPT-1630 queue foundation for its owner-local receipt and input
state. The physical effect and custody producers remain unavailable, so an `Unsupported`
receipt is still retained until its result alias is acknowledged and then remains in the
bounded duplicate tombstone history. Acknowledge does not replenish deduplication history
capacity; only explicit custody-backed owner shutdown may release that history.

## Caller and producer inventory

The issue-listed application paths below are a historical/reference inventory. They are absent from this current GPUI worktree; the names describe the producer adapters that a later implementation must supply, not callable current production APIs.

| Historical/reference producer | Effects to map | Separate coordinator/DB/UI work | Current contract gap |
| --- | --- | --- | --- |
| `folder_browser/rename_execution::execute_rename_commit_request` | `MoveNoReplace`, `CreateDirectoryNoReplace` | Source/catalog transaction, operation disposition, UI refresh | No current GPUI adapter that carries operation/effect/source/root fences |
| `file_move_execution::{execute_folder_move_request_with_progress, execute_file_drop}` | `CreateStaging`, `CopyIntoStaging`, `FlushStaging`, `ValidateStaging`, `PublishNoReplace` | Source/target DB ordering, progress/checkpoint persistence, UI result | Copy/write owner and cross-owner capacity authorization are absent |
| `file_move_transaction::rename_files_with_rollback_and_progress` | `MoveNoReplace` or staged copy composition | Transaction/rollback disposition and source updates | Existing rollback is not a file-owner receipt protocol |
| `drag_drop_actions/{folder_moves::submit_folder_move_input, external::copy_selected_files}` | Adapter input to the same staging/copy vocabulary | UI intent only; coordinator creates durable operation/participant | Adapter path and live intent producer are absent |
| `trash_actions/movement::move_path_to_configured_trash` | `MoveNoReplace`, possibly `InspectOwnedObject` | Trash policy, durable participant result, UI update | Existing cross-device recursive fallback is unsafe for this contract and must be `Unsupported` until qualified |
| `waveform_edits/worker::{execute_destructive_edit, execute_destructive_edit_write}` | `CreateStaging`, `WriteStagingChunk`, `FlushStaging`, `ValidateStaging`, `ReplaceExpectedTarget` | Recovery/edit transaction, checkpoint and UI state | No qualified writer/replace adapter; no overwrite fallback |
| `waveform_edits/worker/atomic_write::write_wav_atomically` | Stage/flush/validate/publish composition | Format/recovery metadata and operation disposition | Atomic-write helper is not a registered-root owner |
| `app/controller/ui/drag_drop_controller/drag_effects/source_moves/worker::run_source_move_task` and folder-move workers/finalize | `MoveNoReplace`, staging/copy composition | Source DB transaction, catalog/UI updates | Current source move and file effects are not separated by this owner boundary |
| Reference `committed_file_mutations` (`CommittedMutationFence`, `SourceMutationRequest`) | Source-fence concepts only | Coordinator/source transaction | No durable `OperationId`; this is useful precedent, not a file-owner command |

The current built code has narrower, separate responsibilities. [`file_ops_journal`](../crates/yield-library/src/sample_sources/db/file_ops_journal/) persists source-local `Move`/`Copy` observations; [`reconcile.rs`](../crates/yield-library/src/sample_sources/db/file_ops_journal/reconcile.rs) consumes descriptor-bound [`recovery_io`](../crates/yield-library/src/sample_sources/db/file_ops_journal/recovery_io.rs) helpers (`RecoveryRoot`, `OpenedFile`, `StagedFileFinalizer`). It is owned by a source database and is not a general `FileOperationOwner`; its non-Windows production finalizer can return `Unsupported` while retaining journal/staged evidence. `yield-catalog` owns source/catalog lifecycle and database transactions, `yield-session` owns source registry and shutdown, and `yield-ui` supplies browser/clipboard intent. None currently provides the proposed owner command/result or a live cross-owner write authorization.

Platform availability is not workflow qualification. The [macOS publication decision](GPUI_MACOS_PUBLICATION_DECISION.md) records that `renameatx_np(RENAME_EXCL)` is name-bound, `fclonefileat` does not return the destination handle/provenance required by this contract, and pathname unlink is unsafe for new cleanup effects. The Windows `Available` state in `sample_sources/finalization.rs` describes primitive availability only. The non-Windows recovery finalizer remains fail-closed, and its test finalizer cannot qualify production. Until OPT-1631 supplies a qualified primitive and live ownership proof, publication, replace, and cleanup commands return `Unsupported` with retained evidence.

## Fail-closed rules and unresolved producer adapters

- A path, locator, UUID, claim/issue identity, `FullProgress` snapshot, or decoded receipt never authorizes an effect.
- Source lifecycle generation, source revision when available, lease epoch, root registration generation, expected object identity, and live intent/capacity authority are checked immediately before the effect.
- Stale authority before an effect returns `SupersededBeforeEffect` with no mutation. A failed or ambiguous check after an effect may have started retains evidence and charges for audit/reconciliation.
- Replace, remove, restore, recursive cleanup, and cross-volume move are `Unsupported` until a qualified primitive and exact live ownership proof exist. No generic overwrite, path fallback, or copy-then-delete is permitted.
- The source owner remains the only authority for source lease/control-connection state. A source publication fence is an observation and cannot be used by a file owner as a lease.
- OPT-1564's owner-local synthetic chunk ticket is capacity/claim authority only; it does not prove filesystem progress and cannot be used as a `FileOperationOwner` write grant. OPT-1634 must produce owner-side staging identity/checkpoint evidence, and OPT-1856 can later bind exact chunk/issue/output identities.
- The missing cross-owner write-authorization adapter is the explicit [OPT-1857](https://linear.app/boostnlvp/issue/OPT-1857/capacity-transfer-bounded-live-write-authority-to-fileoperationowner) prerequisite. It owns the sealed, non-Clone, non-serializable effect grant bound to exact operation/participant/claim-or-chunk/output identity and byte bound. Until it exists, affected effects return `Unsupported` and retain staged data/operation evidence. It does not block the OPT-1629 decision or OPT-1630 queue, but it blocks real OPT-1632 effect execution. This contract does not implement or hide that adapter.

## Follow-up boundaries

The decision has no runtime dependency on a new admission gate and does not widen provider qualification, UI behavior, or physical I/O. The follow-up sequence is explicit:

| Follow-up | Boundary |
| --- | --- |
| [OPT-1630](https://linear.app/boostnlvp/issue/OPT-1630) | File-operation owner queue/registration and owner-local effect execution |
| [OPT-1857](https://linear.app/boostnlvp/issue/OPT-1857/capacity-transfer-bounded-live-write-authority-to-fileoperationowner) | Transfer the private live effect-scoped write grant from journal/capacity ownership to the FileOperationOwner; required before real effect execution in OPT-1632 |
| [OPT-1631](https://linear.app/boostnlvp/issue/OPT-1631) | Backend qualification and supported effect primitives |
| [OPT-1632](https://linear.app/boostnlvp/issue/OPT-1632) | Publication integration and coordinator barriers |
| [OPT-1633](https://linear.app/boostnlvp/issue/OPT-1633) | Cleanup/rollback policy under exact ownership proof |
| [OPT-1634](https://linear.app/boostnlvp/issue/OPT-1634) | Owner-produced durable staging/checkpoint evidence |
| [OPT-1856](https://linear.app/boostnlvp/issue/OPT-1856) | Exact chunk/issue/output binding to capability-verified staging evidence |

These follow-ups must not make a source/database owner perform file effects, make a file owner own SQLite, or let a stored observation become authority. No cycle or live-gate dependency is introduced by this decision.

## Finite review checklist

- [x] Ownership and lifetime split names the coordinator, file, source/database, and UI boundaries.
- [x] Proposed command envelope includes operation/participant/effect identity, explicit per-effect payload/root roles, source fences, root registration generation, expected identity, bounded locators, and live intent/capacity authority.
- [x] Effect vocabulary maps staging, copy/write, flush, validation, no-replace publication/move, directory creation, replace, inspect, remove, and restore.
- [x] Results echo fences and separate effect evidence; durable operation success remains coordinator-owned.
- [x] Staging, validation, no-replace, reopen, visibility/sync, and durable checkpoint barriers remain distinct with backend-specific partial ordering.
- [x] Observed journal limits and proposed file-owner command/input/receipt caps are separated; dropped-receiver behavior and no-eviction evidence retention are explicit.
- [x] Historical/reference callers are mapped while absent paths and current source-local recovery code are clearly identified.
- [x] Stale, collision, unsupported, uncertainty, drain, shutdown, exact-fingerprint duplicate/conflict behavior fail closed and retain evidence.
- [x] Missing producer adapters, the explicit OPT-1857 authority handoff, and the 1630–1634/1856 follow-up boundaries are explicit.

The runtime foundation intentionally contains no filesystem implementation or production
authority seam. Later producer and backend work must prove each barrier and outcome against
the live owner boundary above; this milestone only makes the queue, identity, lifecycle, and
retention rules executable and testable without upgrading observations into authority.
