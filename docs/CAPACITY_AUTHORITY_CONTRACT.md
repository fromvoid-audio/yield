# Capacity authority lifetime and startup contract

This is the OPT-1554 decision artifact for the protected-capacity authority. It
defines the private owner-thread boundary needed by the later capacity issues;
the shapes below are proposed and are not implemented by this document. The
production admission gate remains unavailable.

## File effects remain a separate owner (OPT-1629)

Capacity proofs, ordinary claims, and chunk tickets authorize bounded capacity
accounting only. They do not authorize filesystem effects, target-root access,
publication, cleanup, or source/database transactions. The proposed
[FileOperationOwner command/result contract](FILE_OPERATION_OWNER_CONTRACT.md) keeps
those effects behind a separate retained-root owner and live fence checks; this
capacity contract does not add that runtime or create a live file-write adapter.

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

OPT-1561 adds a read-only `fresh_capacity_facts()` view method. It revalidates the
retained root, profile guard, fixed reserve child, and descriptor before deriving
the current device identity, free bytes, and allocation unit from that already-open
handle; it repeats the live validation after the observation. The owner stages one
typed, provenance-bound observation for every retained volume and publishes nothing
until all volumes pass. The result is transient owner-thread evidence, not a cached
provider fact, source-lease authority, reserve mutation, or ordinary-admission token.

This split remains private owner-thread state. It must not use `Pin`, an unsafe
`Send`/`Sync` impl, a `'static` borrow, a caller-provided identity token, or a
fake fixture provider in production.

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
OPT-1561 now performs the complete live descriptor observation after the retained
volume proof and before the owner reads or installs the restart image. A replacement,
offline/unsupported descriptor, root or marker mismatch, or allocation-unit change
fails the observation and leaves the provider ledger and rebuild readiness unchanged.
The successful result remains transient and carries profile, owner-generation, root,
file, and allocation-unit provenance. It cannot be reused for a later capacity
transition, and it does not qualify the production provider: until OPT-1562
establishes clean handoff and OPT-1563 wires the gate,
`production_admission_gate()` continues to return `CapacityProviderUnavailable`.

### External source volumes

OPT-1554 does not take ownership of `SourceLease`. The existing
[`SourceLease`](../crates/wavecrate-library/src/sample_sources/db/source_lease.rs#L263-L281)
is retained by its source owner, which keeps its control connection, source and
media root capabilities, sidecar lock, epoch, and profile guard. Its owner
must finish source mutations and retain the lease through the capacity
registration/drain handshake.

OPT-1556's private registration boundary is source-owner-mediated: the source
owner sends a `Send`-safe owned descriptor/root capsule through a typed command,
while retaining the `SourceLease` itself. The source-owner guard is the only
value that can validate the retained control connection and mint a fresh,
one-shot installation confirmation; the request never carries `SourceLease`,
its control connection, or a publication fence. The confirmation is bound to
the validated owner/epoch and expiry, and a renewed lease requires a fresh
confirmation. Missing or expired confirmation, a rebound lease, or a descriptor
mismatch keeps the volume unavailable. A borrowed `&SourceLease`,
caller-supplied identity string, invented `Arc`, or serialized lease observation
is never sufficient authority.

The private lifecycle is `Prepared` -> `Registered` -> `Draining` -> `Retired`.
Draining disables new capacity transitions while retaining the registration,
reserve, and volume ownership. The source owner then retires its lease through
the retained control connection and sends a sealed matching retirement
acknowledgement. The accepted `SourceAuthorityUseProof` becomes a long-lived
registration and drain marker for this milestone, so its installation deadline
does not block the restrictive drain; each later physical transition must use a
new source-owner confirmation. Only after that durable acknowledgement may the journal owner
remove the registration and release its volume ownership. A failed or uncertain
retirement emits no acknowledgement and leaves the registration evidence
unresolved. Milestone 1 defines and tests this handoff/retirement contract.

OPT-1556 now wires the private production path: the session starts the journal
before the catalog and passes a cloneable client into the catalog worker. The
catalog worker retains the opaque source registration and non-`Send` guard; one
bounded command carries the descriptor-bound request and confirmation to the
journal owner, which revalidates the roots and database-file identity before
installing the registration.

The journal owner deduplicates descriptor-derived volume identities, including
the profile/journal volume. It retains one owner lock and reserve per external
volume and shares that entry across source references. A failed or timed-out
registration can leave a durable reserve binding, so the owner keeps that state
recovery-only and never reports a clean registration from an uncertain reply.
Each acquired volume owner and reserve is retained before the next volume effect,
so partial multi-volume registration remains actionable evidence rather than an
implicit rollback. Finalizing the last source reference keeps a complete external
owner/reserve binding idle for same-runtime reuse; cross-runtime dirty-marker cleanup
remains deferred to the later clean-handoff contract.
Removing a source first enters `Draining`, then the source owner drops its
SQLite connection and durably retires the lease; only the matching sealed ack
permits final removal. Catalog attachment and browsing remain available when
capacity registration is unsupported or fails, while ordinary capacity
admission remains closed.

After the source lease is retired, the catalog stores the receipt and sealed
acknowledgement in a bounded pending-finalization carrier before submitting the
journal finalization command. A timeout, queue rejection, or late error retains
that exact pair for a later replacement/removal boundary retry; an in-flight
result is polled before another command is submitted. The carrier contains no
source database, guard, root descriptor, or physical authority. Shutdown makes
one bounded retry pass and leaves unresolved finalization explicitly uncertain;
it never publishes successful per-source removal for an unresolved ack.

Profile-child retargeting drains the old registration, detaches only the
source-owner guard, and advances the owner/epoch in the existing control
transaction. The old superseded-generation ack is emitted only after that
transaction commits, including when reopening the new connection fails after
commit; pre-commit and uncertain failures retain the old registration evidence.
The bounded integration adds no schema, does not route source writes through
the journal, does not qualify the provider, and does not enable ordinary
admission. Registration diagnostics are bounded worker diagnostics; a fresh
source confirmation remains required for every later physical transition.

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
| OPT-1564 | Durable post-intent chunk claims, owner-held write tickets, and logical release retry. |
| OPT-1566 | Authority-free typed requirement/estimator matrix for all command families and nine allocation classes; no admission or family wiring. |
| OPT-1567 | Deterministic pre-intent low-space, same-volume identity, multi-volume coexistence, and allocation-rounding fault coverage; no production admission expansion. |
| OPT-1565 | Post-intent retry versus partial classification, deferred until capability-bound staging checkpoints exist. |
| OPT-1568 | Post-intent chunk, release, and restart fault matrix; remains deferred. |

OPT-1566's matrix is authority-free and returns all nine allocation classes for every typed
command payload. Its stable family/class IDs and current `Known`, `Unknown`, and `Unsupported`
outcomes are specified in the typed matrix table in
[`IO_ARCHITECTURE_TARGET.md`](IO_ARCHITECTURE_TARGET.md#typed-capacity-requirement-and-estimator-matrix).
Only checked single-output byte bounds are currently `Known`; metadata, retained-data absence,
multi-output rounding, and descriptor-bound rows remain `Unknown` until their payload proves a
safe bound. `CapacityRequirementMatrix::require_known_bytes` fails closed for unresolved or
missing rows, so no future planner may filter unknown classes into implicit zero requirements.

OPT-1567 verifies the common pre-intent boundary without changing production behavior. The
owner-queue fault matrix uses deterministic seed `0x1567` and checks `free == protected_floor`
(`RecoveryReserveLow`) plus `free == protected_floor + required - 1`
(`DiskPressureRecoveryOnly`) before any `OperationId`, journal row, provider token, or claim is
created. The exact `protected_floor + required` case is a successful control. Every rejected case
compares the owner provider's claim image, dispositions, and token sequence before and after,
then reopens the journal to confirm an empty ordinary/recovery image. Provider seed cases
`0x1567` and `0x1568` reverse the bounded two-volume input order and verify same-volume staging
plus recovery charges, isolated per-volume totals, and atomic rejection when either volume is
pressured. The planner covers logical `1`, `unit - 1`, `unit`, and `unit + 1` with per-object
rounding, and uses open-descriptor identity across a Unix rename/reopen; pathname equality and
physical two-device qualification remain outside this verification leaf.

The implemented OPT-1563 boundary accepts a private prepared template only after it validates
the participant topology, derived work plan, full claim coverage, bounded arithmetic, and current
journal evidence. It refreshes retained descriptor facts for each request and evaluates the
existing owner ledger without replacing it. A valid empty recovery-only snapshot may pass; active
ordinary durable charges may coexist; audit-pending, malformed, ambiguous, or other unresolved
recovery evidence rejects with bounded typed context. Provider errors such as
`RecoveryReserveLow` and `DiskPressureRecoveryOnly` remain typed and retain their volume/byte
facts.

Before the provider witness is minted, every required retained profile/source volume is
revalidated through a fresh descriptor-bound full protected-floor proof; startup recovery may
use the separate partial-proof path, but it cannot authorize this gate. Registered source
authorities are also revalidated at the consuming boundary for root identity, lease liveness, and
active lifecycle state.

New prepared initial intents use schema V16's separate full-capacity representation, which preserves
all nine allocation classes, including `OperationRecoveryPayload`, without projecting them into the
legacy eight-class field. V1 through V15 retain their existing codecs and legacy capacity field;
they cannot encode a V16 full plan, and V16 rejects a missing or mixed legacy/full representation
before allocating an `OperationId`. Full-progress observations still do not grant execution
authority. After fresh checks pass, one owner-thread provisional lease is consumed by the existing
atomic intent-plus-ordinary-claim settlement transaction. Duplicate claim identities are checked
against both the current ledger and durable claim/release history before the ID is allocated.
Existing family entry points remain fail-closed and production qualification/admission remains
disabled pending the later integration issues.

OPT-1557's v5 slice now persists immutable ordinary-claim observations in a
dedicated table without an operation foreign key. Its crate-internal reader
uses canonical UUID keyset cursors and returns at most 256 rows, with one
bounded SQL lookahead row to produce the next cursor; malformed rows fail
closed. These rows preserve evidence only and do not grant claim, provider,
settlement, or admission authority.

OPT-1558 now adds a crate-private `IntentDurable` settlement boundary. It uses
the version-appropriate plan: legacy records are checked through
`FullCapacityPlan::from_legacy`, while V16 records validate and settle their
exact full plan. Both paths require exact claim coverage before calling the
owner-thread provisional provider, pin that lease before entering the journal's
`BEGIN EXCLUSIVE`, insert the intent and all ordinary claims in one transaction,
and require exact record and claim readback before an acknowledgement. A
confirmed rollback releases the provisional token exactly once; a committed
result changes it to a durable retained disposition, while rollback uncertainty
or post-commit readback uncertainty leaves it audit-pending. Lease `Drop` cannot
release either retained disposition.
Duplicate operations or claim identities roll back the new attempt without
changing the original charge. This slice does not upgrade full-progress
observations, expose a production write API, settle later releases, rebuild
claims, or enable the unavailable ordinary admission gate.

OPT-1559 adds the v6 `operation_journal_ordinary_claim_release` relation. It is
an immutable, claim-keyed receipt containing the durable operation identity and
one stable release-transition identity; the original ordinary-claim row remains
available as evidence. Only the crate-private owner-thread release request may
create the receipt, and it reads volume, allocation class, and allocated bytes
from the durable claim row. A snapshot or settlement observation cannot create
one. The store commits and reads the receipt back before asking the retained
provider ledger to subtract bytes.

The provider ledger retains the exact per-claim volume, class, and byte mapping
inside each aggregate token. A release subtracts one matching claim after
checking the operation, claim, release identity, and current aggregate for
underflow; all checks complete before mutation. Other claims and residual token
charges remain held. A token is removed only after every claim in that token is
released, while the private release history makes replay after removal
idempotent. Zero, conflicting, missing, or ambiguous matches fail closed.
Provider failure, rollback uncertainty, commit uncertainty, or post-commit
readback uncertainty retains the charge and stable request identity for retry.
This logical release does not delete ordinary rows, clean physical data,
replenish the recovery reserve, rebuild the provider after restart, or enable
ordinary admission.

OPT-1560 adds the private owner-thread restart rebuild without changing the
schema. After the owner acquires the profile journal volume lock and installs
the reserve capsule, it revalidates the retained descriptor and obtains a
fresh non-sparse PhysicalReserveProof. The proof must match the live volume
and meet the protected-floor minimum derived from the active durable recovery
charge; a valid explained partial reserve uses that charge-derived remainder.
Only then does the owner read claims and install the recovery-only provider
ledger. A proof failure follows the existing unavailable startup path.

The store reads both ordinary-claim relations in one consistent read
transaction. Claim and release rows use the existing canonical UUID keyset
pages, with at most 256 decoded rows plus one SQL lookahead per page. Every
claim operation is point-validated through the strict journal reader.
Only an exact durable release tuple for the claim and operation, with its
stable release identity, excludes a claim from outstanding bytes. Terminal
operation observations, settlement outcomes, snapshots, and full-progress
rows do not authorize release. Original claim and release evidence is never
deleted.

Known positive claim metadata with a missing operation or mismatched release
remains retained as AuditPending. An orphan release has no claim bytes to
attribute; its immutable evidence is retained as a bounded diagnostic and
keeps the rebuild recovery-only without fabricating a charge. These cases
produce at most 32 diagnostics of 256 characters each. A malformed claim or
release page,
unparseable identity, non-positive amount, or checked aggregate overflow yields
ReconciliationRequired; the provider is left unchanged and no optimistic
capacity is installed. RecoveryOnly can retain known charges for audit, but
ordinary qualification and admission remain unavailable.

The private provider rebuild image is sealed by the store. It carries each
claim's operation, claim identity, volume, allocation class, bytes, exact
release identity when present, and audit disposition. Staging validates all
identities, duplicate release IDs, positive bytes, checked per-operation and
aggregate sums, then installs atomically into an empty ledger. An exact replay
is accepted only when the current live ledger still has the same canonical
claims, residual charges, dispositions, and release history; a changed or
non-empty conflicting ledger is rejected without mutation. This prevents a
post-rebuild release or admission from being resurrected by a stale snapshot.

The bounded OPT-1560 checklist is:

- run owner acquisition, reserve installation, live proof, journal snapshot,
  and provider rebuild in that order on the owner thread;
- page and validate every claim and release relation, including orphan and
  malformed evidence, while preserving rows and bounded diagnostics;
- rebuild multi-operation, multi-claim, multi-volume and multi-class residual
  aggregates with checked arithmetic and exact release history;
- replay after reopen and after one or more releases, reject a changed live
  ledger, and retain the provider when a repeated rebuild conflicts;
- exercise crash/reopen fixtures across charge, pre-intent, settlement,
  durable-intent, and release commit boundaries, plus a page set larger than
  256 rows and proof failure;
- keep recovery evidence and ordinary admission fail-closed; do not qualify
  production capacity, replenish the reserve, delete physical data, or wire
  chunks, families, or new public commands.

The bounded OPT-1559 checklist is:

- migrate v5 databases to strict schema v6 and reject unknown objects, columns,
  indexes, malformed identities, and mismatched release readback;
- persist one exact receipt per claim through the owner-thread transaction and
  return only a private receipt after commit and readback;
- retry exact requests idempotently, reject conflicting request identities, and
  preserve the original ordinary row and actionable provider charge on every
  uncertain or failed reconciliation;
- subtract one claim atomically from a uniquely identified retained token,
  fail closed on ambiguity, preserve residual claims, and guard replay after
  the final token is removed;
- exercise multi-claim, duplicate/conflict, failure and uncertainty, reopen,
  concurrent serialization, retained-evidence, and v5-to-v6 migration cases.

The bounded OPT-1562 clean-volume handoff is an orderly owner-thread shutdown
boundary. It requires a structured zero proof from the bounded ordinary-claim
scan (every claim has an exact release and there is no orphan, conflict, or
malformed evidence), a fully validated recovery-reserve history with no active
charge, an empty live provider ledger, no active or incomplete source binding,
and a fresh non-sparse reserve proof at the complete 256 MiB protected floor.
Released claim history and valid fully reconstituted reserve history remain
immutable audit evidence; RecoveryOnly readiness text is neither a blocker nor
zero proof by itself.

All retained volumes are preflighted before any marker is changed. Each full
reserve revalidation refreshes the retained descriptor's identity and
allocation unit through its owned handle; no cached observation authorizes the
handoff. While the volume lock is held, the owner fences the runtime before it
attempts a Clean marker write (including all shared context clones), then
writes and syncs the marker and syncs the registry directory. A write or sync
failure is uncertain about the bytes on disk, so it returns no clean receipt,
retains conservative evidence, and prevents further capacity mutations; Drop
and panic never claim a clean handoff. On restart, only an exact same-profile
binding with matching root, file, unit, namespace, full proof, and durable-zero
audit may update its owner generation in one checked transaction. Foreign
reserve artifacts and generic generation mismatches remain recovery-only.

The finite OPT-1562 checklist is:

- refuse active provider claims, active or malformed reserve history, ordinary
  release ambiguity, active source registrations, incomplete bindings, stale
  roots, replacement descriptors, sparse/short reserves, and uncertain marker
  persistence without dropping evidence;
- complete a zero-claim handoff, reopen it with a strictly newer same-profile
  generation, rebind the existing exact reserve binding, and preserve the
  conservative result across restart and failed metadata commits;
- exercise cross-profile lock contention and foreign-artifact refusal without
  adopting or deleting another profile's reserve; leave ordinary admission,
  physical cleanup, provider qualification, chunks, and family wiring deferred.

### OPT-1564 durable chunk claims and bounded write tickets

OPT-1564 adds a post-intent chunk relation without changing the V16 operation-record codec or
the frozen initial inventory. A globally unique `ChunkId` permanently binds one operation, one
immutable bounded write identity (participant, output/artifact identity, logical offset and
length), and the exact ordinary claim IDs charged for that chunk. The chunk's ordinary rows and
its `Claimed` relation are inserted together through the owner-thread transaction after initial
settlement; they are not a deduction from an unrelated initial claim.

The chunk relation has two explicit bounded-read/write limits. The complete relation, including
released history, is capped at 256 rows globally, and each operation is capped at 256 ordinary
claim rows. The append path performs a bounded capped lookahead before inserting, so the 256th
row is allowed and a further row is rejected before any ordinary row, relation row, or provider
charge is changed. Restart and admission scans use the same bounded pages and reject
oversized, orphaned, malformed, conflicting, or overflowed evidence conservatively.

The owner validates a fresh retained-authority descriptor observation and the current provider
ledger before creating a chunk claim. The claim is charged once through the existing provisional
lease and becomes durable only after exact row readback. A `Claimed` row has no write authority.
Issuance revalidates the operation state, durable claim identity, current protected-floor facts,
and retained headroom without admitting or charging a second time. Only a confirmed durable
`Claimed -> Issued` CAS/readback creates an owner-issued opaque ticket. Repeating an `Issued`
request never creates another receipt or competing ticket; changed write metadata or reuse under
another operation is rejected.

The ticket crosses the bounded owner queue as an opaque, non-reusable value. The actual
`ChunkWriteEntry` is private to the owner thread and is created only after the owner registry,
durable row, operation disposition, fresh descriptor facts, and sealed live witness all match.
The entry borrows that witness through the synthetic callback boundary and carries no filesystem
authority. A denied identity, budget, cancellation, stale-authority, or capacity check invokes
no callback; the single-use ticket is consumed and the durable `Issued` charge remains retained.
New independent chunk IDs may coexist with already charged `Issued` chunks, including after
restart, because `Issued` is capacity accounting only and never means physical output completed.

On restart, a confirmed `Claimed` row may be issued again only after fresh retained authority,
descriptor facts, and a durable CAS. An `Issued` row or uncertain issuance remains charged and is
never reissued; a durable `Released` row is replayable by its exact release identity. An Issued
ticket can enter only once. Before logical release, the owner moves its live unused proof into an
exact `ReleasePending` record. A rollback before commit retains the `Issued` row, charge, issue
identity, and release identity. A timeout or uncertain result may already have committed
`Released` while provider acknowledgement is unknown, so the conservative provider charge and
exact pending identities remain until confirmed Released readback and provider reconciliation.
Retry uses the same owner-held proof and release ID; after confirmed reconciliation, provider
subtraction occurs once and exact replay is idempotent. Logical release does not delete rows,
clean physical data, replenish the reserve, prove physical completion/exhaustion, or enable
ordinary admission.

The chunk protocol deliberately leaves family writer wiring, physical cleanup, OPT-1565 written
checkpoint/exhaustion classification, provider backend qualification, and production rollout
deferred. Snapshots, copied issue IDs, and restart observations never mint a ticket or release
proof.

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
