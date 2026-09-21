# Capacity authority lifetime and startup contract

This is the OPT-1554 decision artifact for the protected-capacity authority. It
defines the private owner-thread boundary needed by the later capacity issues;
the shapes below are proposed and are not implemented by this document. The
production admission gate remains unavailable.

## Current lifetime boundary

The existing implementation already makes the lifetime problem explicit:

- [`OperationJournalStore`](../crates/wavecrate-library/src/operation_journal/store.rs#L541-L554)
  owns its SQLite connection, journal-root binding, and a cloned
  `WritableProfileGuard`.
- [`RecoveryReserveContext<'a>`](../crates/wavecrate-library/src/operation_journal/store.rs#L556-L568)
  borrows the store and stores a `BoundReserve<'a>` together with a
  `VolumeOwnershipCapability`. It is a command-scoped context, not an owner
  object.
- [`BoundReserve<'a>`](../crates/wavecrate-library/src/operation_journal/capacity_reserve_namespace.rs#L271-L277)
  currently owns `ProtectedReserve` while borrowing `BoundDatabaseRoot` and
  `WritableProfileGuard`. Storing it in an object that owns either borrow would
  create a self-referential lifetime.
- [`PhysicalReserveProof`](../crates/wavecrate-library/src/operation_journal/capacity_reserve_namespace.rs#L279-L302)
  is deliberately non-serialized and can only come from a retained-handle
  revalidation.
- [`VolumeOwnershipCapability`](../crates/wavecrate-library/src/operation_journal/capacity_volume_owner.rs#L146-L202)
  contains an `Rc` marker and is therefore non-`Send`/non-`Sync`; dropping it
  releases the operating-system lock but does not prove a clean handoff.

The owner currently consumes the store and moves it to its worker in
[`OperationJournalOwner::start`](../crates/wavecrate-library/src/operation_journal/owner.rs#L234-L319).
`shutdown` closes admission, joins that worker, and lets the worker drop the
store in [`run_owner`](../crates/wavecrate-library/src/operation_journal/owner.rs#L333-L402).
The future capacity authority must therefore be installed after the store has
reached that worker. A caller must never construct a `VolumeOwnershipCapability`
and send it to the worker.

## Decision: owned authority, borrowed command views

The future private owner state is an owner-thread-only value. It owns the live
handles and never stores a borrowed reserve view:

```text
CapacityAuthorityState {
    profile_guard: WritableProfileGuard,
    journal: Option<OperationJournalStore>,
    volumes: Vec<OwnedVolumeAuthority>,
}

OwnedVolumeAuthority {
    volume_owner: VolumeOwnershipCapability,
    reserve: Option<OwnedReserveAllocation>,
}

OwnedReserveAllocation {
    root: BoundDatabaseRoot,
    reserve: ProtectedReserve,
    observation: ReserveNamespaceObservation,
}
```

`OwnedVolumeAuthority` and `OwnedReserveAllocation` are separate private
capsules. The former owns the non-`Send` volume lock; the latter owns the root
capability and retained reserve descriptor. Neither exposes a pathname or a
serialized identity as authority. Keeping the lock and reserve capsules
separate is required so reserve/root handles can be dropped before the journal
closes while the volume lock remains held. The profile guard is retained for
live validation and all profile-guard clones held by the journal or source
leases remain subject to the shutdown order below.

Each serialized command creates a temporary view from one volume authority:

```text
BoundReserveView<'a> {
    root: &'a BoundDatabaseRoot,
    profile_guard: &'a WritableProfileGuard,
    reserve: &'a mut ProtectedReserve,
    observation: &'a ReserveNamespaceObservation,
}

RecoveryReserveContext<'a> {
    store: &'a OperationJournalStore,
    volume_owner: &'a VolumeOwnershipCapability,
    reserve: BoundReserveView<'a>,
    journal_identity: String,
    profile_identity: String,
}
```

The eventual private owner method is conceptually:

```text
with_recovery_context(volume, command) -> Result<command_output, context_error>
```

It selects one required volume, revalidates the owner/root/reserve immediately
before the command, borrows the view for the duration of `command`, and drops
the view before returning. No context, view, `PhysicalReserveProof`, or
transaction may be cached, serialized, sent to another thread, or returned as
authority. A mutable reserve view permits physical spend/reconstitution while
the owner thread continues to serialize journal transitions.

This split is intentionally a future private refactor of the current
`BoundReserve` shape. It must not use `Pin`, an unsafe `Send`/`Sync` impl, a
`'static` borrow, a caller-provided identity token, or a fake fixture provider
in production.

## Startup and reopen

The owner startup sequence is:

1. Acquire the current `WritableProfileGuard` and validate its live root and
   lock identity.
2. Open the writable `OperationJournalStore` while that profile ownership is
   held, then move the store to the serialized owner worker.
3. On the owner worker, acquire `VolumeOwnershipCapability` from each required
   live descriptor only. Do not scan or lock every mounted volume. Acquire the
   per-user volume lock after profile/journal ownership is established; do not
   hold a SQLite transaction while acquiring that OS lock.
4. Reopen the fixed reserve namespace through a retained
   `BoundDatabaseRoot`, validate the durable binding against the live profile,
   volume, root, file, and allocation-unit observations, and revalidate the
   retained descriptor before installing the owned capsule. A full protected
   floor is required for ordinary qualified use; an explained partial extent
   whose allocation and length satisfy the durable active charge's expected
   remaining floor may enter `RecoveryOnly` for physical reconstitution.
   Sparse, identity-mismatched, or unexplained-short extents reject without
   authority.
5. After live identity validation, read the durable reserve state to determine
   the expected remaining extent. A durable binding or recovery hint is an
   observation; it never constructs a reserve capability. `Charged`, `Spent`,
   and `ReleaseObserved` states may enter `RecoveryOnly` when their physical
   extent is consistent, but require live physical reconstitution before
   ordinary readiness.
6. Install the owner state only when every volume required by the current
   authority is valid. A failure leaves the owner unavailable or recovery-only;
   it does not adopt, replace, delete, truncate, or silently clean an existing
   namespace entry.

The journal/profile and profile-owned reserve path follow the existing
profile-first ordering in
[`IO_ARCHITECTURE_TARGET.md`](IO_ARCHITECTURE_TARGET.md#logical-and-physical-ownership).
The future provider may report a qualified state only after the complete live
descriptor and reserve checks. Until OPT-1560 rebuilds claims, OPT-1561 wires
the provider, OPT-1562 establishes readiness/clean handoff, and OPT-1563 wires
the gate, `production_admission_gate()` continues to return
`CapacityProviderUnavailable`; that ordinary-admission result does not
invalidate a separately verified recovery context.

### External source volumes

OPT-1554 does not take ownership of `SourceLease`. The existing
[`SourceLease`](../crates/wavecrate-library/src/sample_sources/db/source_lease.rs#L263-L281)
is retained by its source owner, which keeps its control connection, source and
media root capabilities, sidecar lock, epoch, and profile guard. Its owner
must finish source mutations and retain the lease through the capacity
registration/drain handshake.

OPT-1556's private registration boundary is source-owner-mediated: the source
owner sends a `Send`-safe owned descriptor/root capsule through a typed command,
while retaining the `SourceLease` itself. The source owner holds the lease and
epoch valid until the journal owner drains/unregisters the volume and receives
an explicit release acknowledgement. The journal owner validates the retained
descriptor/root inputs and obtains fresh lease-owner confirmation immediately
before installation and use. Missing confirmation, a rebound lease, or a
descriptor mismatch keeps the volume unavailable. The current types do not yet
expose this exact handoff; OPT-1556 owns that private handoff contract. A
borrowed `&SourceLease`, caller-supplied identity string, invented `Arc`, or
serialized lease observation is never sufficient authority.

## Readiness and failure states

The private authority reports one of these states; none is a public admission
success in this milestone:

| State | Meaning and allowed result |
| --- | --- |
| `Unavailable` | Unsupported platform, missing live descriptor, or failed profile/journal/root validation. No verified recovery authority, reserve transition, operation ID, row, or permit. |
| `RecoveryOnly` | Live lock and descriptor-bound reserve evidence exist, but dirty-marker or durable-charge state permits only bounded recovery/reconstitution. Ordinary work remains closed. |
| `ReconciliationRequired` | Foreign dirty marker, malformed/missing binding, identity replacement, stale source registration, or ambiguous ownership. Preserve evidence and require an explicit recovery path; never take over by TTL or pathname. |
| `Qualified` | Future state only after OPT-1560 claim rebuild, OPT-1561's live provider, OPT-1562 readiness/clean handoff, and the common OPT-1563 pre-intent gate satisfy their contracts. It is not emitted by the current production owner. |

Provider qualification being unavailable keeps ordinary admission closed but does
not invalidate a verified `RecoveryOnly` context.

Same-profile dirty-marker restart may retain the existing recovery-only lock
and generation. A foreign dirty marker or a mismatched profile/root/volume/file
identity remains `ReconciliationRequired`. OPT-1562 owns the later clean
handoff after all charges, claims, and physical evidence are settled; dropping
the lock is never treated as that handoff.

## Startup failure and shutdown ordering

Partial startup unwinds in the reverse ownership order: discard temporary
borrowed views, retain any failed reserve/namespace evidence for recovery, drop
reserve/root capsules while retaining per-volume lock capabilities, close the
journal, then drop those lock capabilities and release the final profile guard.
No cleanup step may convert a failed physical check into a clean marker.

Normal shutdown is explicit rather than dependent on Rust field-drop order:

1. Close new admission and stop accepting owner commands.
2. Drain accepted work, or durably defer it with its recovery state, and finish
   source-participant mutations while source leases remain held.
3. Finish capacity and journal recovery transitions while every required source
   lease, volume lock, and reserve descriptor remains held.
4. End all scoped views/proofs and unregister external capacity use. No
   capacity mutation may occur after this point.
5. Ask each source owner to release its lease and wait for its explicit release
   acknowledgement. The journal owner never drops or moves a source lease it
   does not own.
6. Explicitly `take()` and drop each `OwnedVolumeAuthority::reserve` capsule
   while retaining the `VolumeOwnershipCapability` values.
7. Explicitly `take()` and drop the `CapacityAuthorityState::journal` store,
   including its SQLite connection and profile clone. Journal close is not a
   new recovery write.
8. Drop the per-volume `VolumeOwnershipCapability` values, releasing OS locks.
9. Drop all remaining profile-guard clones and release the final profile guard
   last.

This keeps final journal/recovery writes ahead of source-lease, reserve, and
volume-lock release, and keeps profile ownership ahead of every writable
participant. A failed drain leaves dirty/recovery state for the next owner; it
does not claim a clean handoff.

## Issue boundaries

The capacity work is intentionally split as follows:

| Issue | Owned contract |
| --- | --- |
| OPT-1554 | This owner-thread lifetime, startup, reopen, and handoff decision artifact. |
| OPT-1555 | Profile/journal reserve bootstrap and reopen. |
| OPT-1556 | External source-volume registration through a retained source lease and descriptor/root handoff. |
| OPT-1557 | Ordinary-claim schema, bounded reads, and migrations. |
| OPT-1558 | `IntentDurable` initial-claim settlement and provisional rollback. |
| OPT-1559 | Exact-once ordinary-claim release with failure retention. |
| OPT-1560 | Conservative restart rebuild of outstanding claims. |
| OPT-1561 | Live descriptor provider and retained-authority volume refresh. |
| OPT-1562 | Clean volume-ownership handoff after zero claims/charges and proof. |
| OPT-1563 | Common owner pre-intent gate and typed rejection with no journal row. |
| OPT-1564–1568 | Chunk claims, typed estimators, and pre/post-intent fault matrices; outside this lifetime decision. |

## Acceptance checks for implementation children

These are required future checks, not evidence that this documentation change
has run them:

- a compile-time ownership check proves the non-`Send` volume capability is
  created and retained only inside the owner worker;
- same-profile partial restart reopens a dirty marker as recovery-only;
- a foreign dirty marker, missing binding, mismatched root/file/volume/unit,
  sparse or unexplained-short reserve, and stale source registration reject
  without mutation or authority; an explained partial reserve matching the
  durable spent remainder enters recovery-only reconstitution;
- source registration revalidates the active source lease/epoch before
  install/use and keeps the source owner responsible for lease lifetime;
- startup does not acquire a volume lock while a SQLite transaction is open;
- shutdown with a pending job proves admission closure, durable defer/drain,
  final journal writes before volume-lock release, and profile guard release
  last.

No Rust test or production behavior is claimed by this document. The current
ordinary admission path remains fail-closed until the later provider, claim,
and gate issues satisfy these boundaries.
