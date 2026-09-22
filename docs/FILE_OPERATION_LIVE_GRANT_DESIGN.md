# OPT-1857 live effect grant design checkpoint

Status: design checkpoint, not implementation or physical-effect qualification.
Source head: `28f61b8ea6714f959b9d2cc05047f8930be31f78` (2026-09-22).

## Required result

The journal owner issues one opaque, non-`Clone`, non-serializable grant for an
exact `(OperationId, ParticipantId, claim/chunk, EffectId)` and bounded byte
allowance. The file owner consumes it once with matching root and source fences.
Neither a durable row, returned permit identity, locator, nor decoded receipt
can construct the grant. This checkpoint does not add a filesystem backend or
authorize an ordinary operation family.

## Current boundary

| Component | Current evidence | Missing for the grant |
| --- | --- | --- |
| Journal/capacity | `OperationJournalOwner::issue_chunk` performs a durable `Claimed -> Issued` transition and returns a private `ChunkWritePermit`. A second issue fails, including after a dropped permit. `begin_chunk_write` validates that permit against the owner-local issue registry but only records a synthetic entry. | The permit has no live source/root authority, file-owner effect identity, or effect custody transition. Issuance alone does not prove bytes were written. |
| Source owner | `SourceAuthorityGuard` is deliberately `!Send` and retains `Arc<SourceLease>`. `SourceLease` retains the sidecar lock, control SQLite connection, root/database bindings, owner identity, and durable lease epoch. `close_with_source_authority` rejects an extra strong lease owner. | The sendable request/confirmation/use proof carries no strong lease. A private sendable pin must be minted by the live source owner, and retirement must coordinate with it. |
| File owner | `FileOperationOwner` has a bounded queue, exact effect key/fingerprint, duplicate admission record, result custody, root/fence observations, and test-only synthetic live authorizations. Production mutating commands return `Unsupported`. | Production cannot construct or consume a live authorization. `OwnerIdentity`, `SourceFence`, `RootRegistrationRef`, and `EffectId` are observations, not capabilities. |

`SourceLease::release()` invalidates its liveness tokens before changing the
durable row. Therefore a strong `Arc<SourceLease>` by itself is **not** a live
effect grant even though it retains the OS lock. Validation followed by an
effect without release serialization would also be a race.

## Proposed private protocol

1. The source-owner thread, while its `SourceAuthorityGuard` is registered and
   its retained lease validates, mints a non-`Clone` source pin. The pin holds a
   strong `Arc<SourceLease>`, the exact registration/lifecycle identity, and the
   existing root/database bindings. Its constructor is private to the source
   authority module; request/fence observations cannot call it. This step must
   establish whether `SourceLease` is `Send + Sync` on supported targets before
   wiring the queue.
2. The coordinator submits that pin and an exact bounded effect description to
   the journal owner. The journal owner revalidates the current operation,
   participant, live capacity provider, durable claim and source pin, then
   performs its one-shot issuance. Only that worker can assemble the sealed
   grant. The grant owns the issued permit, source pin, exact owner/root/source
   fences, effect identity, and byte bound. The existing issue UUID is an
   identity field inside the grant, never a constructor argument that grants
   authority.
3. The file owner admits an authority-free canonical command and the grant
   under one bounded queue reservation. The grant must match the command's
   operation, participant, claim/chunk, effect, root/source identities, and
   required bytes. A duplicate command with the same effect key must not take
   or execute a second grant; a changed command under the same key is a typed
   conflict. `FileOperationOwner` must not use its current test-only
   `LiveIntentAuthorization`/`LiveCapacityAuthorization` as production authority.
4. Before a physical effect, the worker acquires a release/effect gate owned by
   the retained source lease, revalidates the source row, binding, expiry,
   lifecycle and root capabilities, and consumes the grant exactly once. Source
   release, retirement and retarget must use the other side of this gate; an
   atomic liveness flag or strong reference alone is insufficient. The gate
   serializes lifetime but never mints authority. No gate may be held while
   waiting on the journal or file-owner queue, avoiding a cross-owner wait
   cycle. The future backend may hold the scoped gate only around a bounded
   synchronous effect; an async backend needs an equivalent explicit in-flight
   retirement protocol before qualification.
5. The producer retains custody until the file owner returns an outcome and
   the journal records its disposition. A dropped result receiver does not
   cancel accepted work. The file owner keeps one outcome by exact effect key
   until acknowledgement, as it does for its current bounded receipts. The
   journal's issued claim and charge remain retained whenever effect completion
   or disposition is uncertain.

## Failure and retirement rules

| Boundary | Required behavior |
| --- | --- |
| Before durable issue | Fail without grant or file-owner admission; any provisional capacity charge follows the existing confirmed-rollback path. |
| Issued, before file-owner admission | A confirmed unused issue may use the exact journal unused-release transition once. A lost or uncertain issue keeps its charge and identity for reconciliation; it is never reissued from readback. |
| Duplicate or dropped reply | One admitted effect key and one grant consumption. Dropped reply retains the result/custody path; a duplicate receives that disposition or a typed in-flight result, never another execution. |
| Source retirement, shutdown, or root replacement | Close new grant admission, drain accepted effects, and reject stale generations. The source owner cannot acknowledge retirement while an effect pin or actual effect is live. Explicit release and the effect gate must serialize, so a pin cannot keep using a released row. |
| Confirmed no-effect rollback | Record the no-effect outcome, release the exact unused claim once, then release the pin. |
| Post-effect or uncertain outcome | Retain exact operation/participant/chunk/effect identities and capacity charge. Retain live pin only while an effect can still execute; once quiescent, durable uncertainty is evidence, not execution authority. Never turn a historical row into a new grant. |

## Implementation order and proof obligations

1. Add the private source pin and release/effect serialization, with cross-thread
   compile evidence and tests for guard drop, explicit release, stale generation,
   root replacement, retirement/retarget, and source-owner loss. Preserve the
   existing `!Send` guard.
2. Add journal-owner issuance from a live pin plus exact command envelope. Test
   duplicate issue, dropped permit/reply, restart readback, bounded bytes, and
   confirmed versus uncertain release without granting authority from IDs.
3. Add the file-owner private submission/consumption adapter. Test exact-key
   duplicate and conflict, queued cancel, dropped reply, shutdown drain, and
   one-time consumption. The production backend remains `Unsupported` until a
   separately qualified physical adapter exists.
4. Run targeted source authority, journal chunk, file owner, and production
   profile admission tests; then workspace check, formatting, and diff check.
   A live effect has no acceptance until its authority is checked at the
   execution boundary, not merely at queue admission.

This protocol is a design direction. Two implementation decisions remain: the
smallest gate that serializes every explicit source release with a future
bounded effect without holding a lock across another owner's queue; and the
exact producer path that supplies existing live root capabilities to the file
owner, whose current registrations are test fixtures. A source pin cannot
replace a destination-root capability. Resolve both in code and tests before
claiming OPT-1857 complete.
