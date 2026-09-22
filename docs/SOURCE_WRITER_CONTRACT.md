# Source writer and snapshot contract

Status: [OPT-1637](https://linear.app/boostnlvp/issue/OPT-1637) decision artifact for the
broader snapshot and lifecycle boundary. The bounded source-library curation slice
implemented under OPT-1638 is recorded below; the remaining sections add no runtime
owner, queue, filesystem operation, or asynchronous grant beyond that slice.

Related contracts are the [I/O architecture target](IO_ARCHITECTURE_TARGET.md), the
[file-operation owner contract](FILE_OPERATION_OWNER_CONTRACT.md), the
[capacity authority contract](CAPACITY_AUTHORITY_CONTRACT.md), and the existing
[database migration contract](DATABASE_MIGRATIONS.md).

## Implemented OPT-1638 boundary

The first source-writer slice is implemented in `wavecrate-library`. Schema version 13
adds the source-local `source_participant_curation_receipts` relation. It retains a
versioned, deny-unknown request codec and structured curation receipt, with a 256-row
and 16 MiB combined request/receipt budget and a per-record `MAX_RECORD_BYTES` limit.
The relation is additive; existing curation rows and legacy source opens remain intact.

`SourceWriter` owns one already profile-bound `SourceDatabase` and issues a private
`SourceRevision` only after live profile, database-file, bound-root, source-lease, and
authoritative-audit validation. A request carries an exact source, lifecycle, positive
lease-epoch, root-binding, database-binding, and revision fence. Live validation occurs
before duplicate lookup; an exact same-lifecycle command replays its durable receipt
without changing SQL or the revision, while changed bytes, stale fences, and superseded
lifecycle generations fail closed. The curation updates, source revision, directory/audit
revision, structured result, and receipt row commit in the existing bounded SQLite
transaction. Confirmed pre-commit rollback leaves the database unchanged; commit or
acknowledgement uncertainty retains exact command and canonical request evidence for
readback rather than blindly rerunning.

`SourceWriter::read_receipt_observation` is the bounded production readback entrypoint
for historical reconciliation. It returns an observation only and never constructs a
new live fence. SQLite source revisions are bounded by `i64::MAX`; lifecycle generations
are stored and checked across their full `u64` domain. The 256-row and 16 MiB retained
receipt budget has no eviction path, and retirement or history cleanup is deferred until
a later owner/coordinator contract can preserve duplicate and uncertainty evidence.

This slice exposes a source-library entrypoint only. It does not route catalog or family
callers, create an asynchronous source grant, change source retirement, implement
filesystem effects, or turn receipts/snapshots into operation or filesystem authority.
The broader lifecycle and snapshot proposal below remains the OPT-1637 decision
boundary; later work must add caller routing and any cross-owner lease pin explicitly.

## Boundary and authority

One physical source has one serialized writable SQL owner. The owner retains the
profile guard, the source lease, the bound source and media roots, the database-file
binding, and the control SQLite connection. The owner may be an existing serialized
catalog-worker role; this contract does not require a second source thread. Traversal,
copy, hashing, artifact work, and other-source work remain separate owners and are not
allowed to use the source SQL connection as a work queue.

The source owner is the only authority for source-local SQL mutation. A caller submits a
bounded typed, prepared-SQL command and receives a typed result. Owner startup may move
or clone the existing retained profile guard into the catalog thread as part of owner
construction, but ordinary mutation request envelopes never contain a profile guard,
`SourceLease`, control connection, or borrowed root. An owner command or SQLite
transaction never waits synchronously for another owner. A source owner may call a
separate owner only after its transaction and command scope have ended.

`SourceLease` is live ownership: the current implementation couples the lock file,
control connection, bound roots, database-file identity, profile guard, owner identity,
and lease epoch. `SourcePublicationFence` is a cloneable, non-owning observation. It can
fence delivery against the current source, but it does not retain the lease, renew it,
reopen it, or authorize a write. A serialized revision number, catalog snapshot,
operation journal row, UUID, or decoded receipt is likewise an observation until the
source owner validates it under its current lease.

The future asynchronous live grant is a separate lifecycle contract. The current
authority request does not retain the non-`Send` source lease and cannot pin it across
an effect. [OPT-1857](https://linear.app/boostnlvp/issue/OPT-1857) must define that
handoff before a source write can be granted to another owner. An atomic flag or a
cloneable fence is not a substitute for retaining the actual lease.

## Source-owner lifecycle

The state names below describe the proposed source-owner boundary. They do not claim
that the corresponding state machine is implemented by this change.

| State | Entry and allowed work | Exit and failure rule |
| --- | --- | --- |
| `Starting` | Acquire profile ownership, validate the physical source and database-file identity, acquire the source lease/epoch, then open the serialized writable connection with the required role. | Any identity, lease, or connection uncertainty fails closed; no writable command is admitted. |
| `Running` | Accept bounded SQL commands whose exact source identity, lifecycle generation, lease epoch, and expected revision match the owner. | A stale fence is rejected as `Superseded`; replacement or unavailable roots become `Unavailable`/`AuditRequired`. |
| `Draining` | Reject new mutation commands and finish only accepted commands at a safe transaction boundary. Keep the lease and evidence while results are unresolved. | A source snapshot still retained, a queue timeout, or an uncertain transaction prevents a successful retirement claim. |
| `Retiring` | Close source snapshots and transactions, retire the source lease through the source owner, and retain the exact retirement evidence until the coordinator acknowledges it. | A failed or uncertain retirement retains the registration and evidence; it never emits a false retirement receipt. |
| `Stopped` | No source authority remains and every accepted result has confirmed custody. | This is not reported after a panic, implicit drop, lost owner, or unresolved receipt. Those paths remain `Unavailable`/`RetirementUncertain`. |
| `Retargeting` | Drain the old registration, close admission for it, and supersede queued old work before using the existing source-database retarget transaction. | No old command executes after the drain boundary. The durable generation advance and old-generation retirement acknowledgement occur only after the retarget transaction commits. A failed or uncertain commit keeps the old evidence and never emits an old-generation acknowledgement. |

The session already has the required high-level ordering: `Persistence::start` takes
profile ownership, opens the operation journal, and starts the catalog; shutdown closes
the catalog before the global writer, journal, and final profile guard. Source retirement
must fit inside that ordering. Catalog shutdown and a source retirement are different
events: joining a worker or dropping an object does not prove that a source lease retired.
No panic or owner loss may be converted into a clean handoff.

Retargeting preserves source data through the existing
`SourceDatabase::retarget_catalog_root_with_source_authority` transaction. The new
generation must be registered with a fresh exact identity and fence. Admission closes and
queued old work is rejected at drain; old-generation commands are stale even when a
numeric generation appears newer. Durable generation advancement and the old-generation
retirement acknowledgement wait for confirmed retarget commit. Checked advancement
rejects overflow and wrap. A positive lease epoch is required, while the existing
lifecycle-generation domain is preserved. Exact fence equality, including maximal
representable values, is required wherever a command or receipt is consumed.

## Revisioned source writes

The implementation target is an owner-issued `SourceRevision`, not a raw integer copied
from a row or snapshot. Its conceptual fields are:

| Field | Meaning |
| --- | --- |
| `physical_source` | Stable source/database identity and the validated root/database-file binding. |
| `lifecycle_generation` | The source instance generation used to reject removed, replaced, or reopened instances. |
| `lease_epoch` | The positive owner epoch held by the source lease. |
| `revision` | The checked monotonic publication cursor for source membership, paths, identities, and structural directory truth. |

Only the source owner can issue the next cursor. A serialized `u64` is an observation;
it cannot be turned into a `SourceRevision` by a caller. A command carries the expected
full source identity, generation, positive lease epoch, and revision. The owner first
validates the live physical source, lease, and lifecycle. Within that live instance it
checks the stable duplicate key and exact stored command before checking the expected
revision for a new command. This ordering lets an exact replay return its committed
receipt even after the source revision has advanced, while an old lifecycle can never
regain authority through receipt replay. New commands are then checked immediately
before opening the write transaction and again at the commit boundary when the
implementation requires a second liveness check.

A future source write command has this shape, subject to the typed names chosen by
OPT-1638:

```text
SourceWriteCommand {
    operation_id: OperationId,
    participant: ParticipantId,
    command_key: StableSourceCommandKey,
    expected: SourceRevision,
    bounded_identity_delta: BoundedSourceIdentityDelta,
}
```

`bounded_identity_delta` contains only checked, source-local changes and their exact
identity predicates. It contains no filesystem authority, unbounded snapshot, path-only
cleanup instruction, or live lease handle. The owner checks the source identity,
revision, object predicates, and lease fences before mutation; then one SQLite
transaction commits the SQL changes, the next source revision, and the structured
identity delta. The next revision is checked before incrementing, so `u64::MAX` cannot
wrap. The receipt is a source-participant commit receipt. It is not operation-terminal
status, a filesystem write grant, or permission to publish an artifact.

The current `prepare_commit` path bumps a raw source revision, and
`commit_curation_with_profile_guard` already performs a bounded expected-revision check,
updates source metadata, and returns a curation receipt. Those paths are useful evidence
for the ordering, but their receipts do not yet carry a durable operation/participant
identity or a bounded durable codec. `SourceDatabase` transactions also do not currently
receive the catalog lifecycle generation and `OperationId`; those are implementation
inputs for OPT-1638, not facts to infer from a catalog request.

### Duplicate and uncertain writes

The stable duplicate key is the operation ID plus the stable participant and command
identity needed to distinguish two source effects. A distinct command key denotes a
distinct command; it is not inherently a conflict. For the same full key, exact replay
requires equality of the complete bounded command: identity delta, expected source
revision and fences, object predicates, and all bounded values. An exact duplicate
returns the previously committed receipt without another revision increment or SQL
mutation. The same key with a changed payload, source identity, or fence is a typed
conflict and leaves the prior state unchanged.

Receipts are stored by the source participant with the implemented bounded versioned
codec and migration rule described above. The source schema is now version 13; both
`Fast` and `Full` opens run the additive receipt relation assurance, while `Full` also
performs invalid-path cleanup. The source-local
`file_ops_journal` has recovery semantics and is not a source participant receipt table.
The volatile catalog request ID and the existing in-memory curation lineage do not supply
the durable duplicate identity; `SourceWriter` supplies that identity and readback path
for its bounded curation entrypoint.

A confirmed rollback before the transaction commits leaves the source mutation unchanged
and returns an error-specific outcome; stale, invalid, or conflicting input is not
automatically retryable. A commit or rollback whose outcome is uncertain retains the
exact operation, participant, expected fence, and bounded delta evidence and enters
reconciliation. It must not guess rollback, increment again, or report a successful
duplicate from a different request. This contract does not add a second journal or
silently route curation-family callers; those migrations belong to OPT-1782 through
OPT-1794.

## Read-only snapshots

The read contract is a bounded observation of a source revision. A snapshot request names
the physical source and expected lifecycle/fence context, and may carry a keyset
continuation. A response includes the exact source identity, lifecycle generation, source
revision, bounded rows/deltas, and an outcome. It does not include a write capability or
an inferred “latest” promise.

The source owner or a read owner uses the existing snapshot budget: two seconds, 100,000
rows, or 16 MiB of bounded result/input data, whichever is reached first. Existing
metadata queries use a keyset page of 64 rows. A continuation starts a fresh bounded
snapshot and revalidates the expected source identity, lifecycle generation, and
revision; it never concatenates pages from different epochs or silently blends a stale
projection into a newer one.

The proposed outcomes are:

| Outcome | Meaning |
| --- | --- |
| `FreshAsOf` | The complete bounded response was read from the named source revision and lifecycle generation. |
| `Stale` / `Superseded` | The source changed or the expected revision/fence no longer matches; the response is observation only. |
| `Unavailable` | The source, lease, database identity, or owner cannot be validated. |
| `BudgetExceeded` | The response exceeded the time, row, or byte bound; callers retain a continuation or request an audit. |
| `Malformed` / `AuditRequired` | Durable source evidence cannot be interpreted safely; no optimistic projection or write is allowed. |

`FreshAsOf` means fresh at the returned revision and lifecycle cut. It does not mean
perpetually current after the response returns. A `CatalogSnapshot`, copied row, old
receipt, raw revision, or `SourcePublicationFence` cannot authorize a later write.
An observation from a process that lost profile or source ownership cannot migrate,
repair, recover, or own a writable source connection.

## Connection roles and migration boundary

The current source connection profiles map to the proposed authority boundary as follows:

| Role | Current open mode | Contract treatment |
| --- | --- | --- |
| `UiRead` | `SQLITE_OPEN_READ_ONLY`, `Fast`, short busy timeout | Read-only observation. No source lease, profile guard, or database binding is created by the current read-only opener. |
| `BackgroundRead` | `SQLITE_OPEN_READ_ONLY`, `Fast` | Same read-only observation rule; bounded and stale-aware. |
| `JobWorker` | Read/write/create, `Fast` | Requires the profile/source owner and exact bound source authority before mutation. |
| `UserMetadataWrite` | Read/write/create, `Fast` | Serialized source-owner mutation; it must not become a second physical writer. |
| `PlaybackHistoryWrite` | Read/write/create, `Fast` | Serialized source-owner mutation with its existing latency policy. |
| `Maintenance` | Read/write/create, `Full` | The only role that may perform the full schema/migration maintenance contract, under source ownership. |

`open_read_only_source_database` validates the configured paths and opens a read-only
SQLite connection, but its `SourceDatabase` has no source lease, profile guard, or
database-file binding. The path policy and SQLite flags therefore establish database
content read intent, not a filesystem no-write guarantee. SQLite WAL mode can create or
update `-wal`/`-shm` sidecars when the directory is writable; see the official
[SQLite WAL documentation](https://sqlite.org/wal.html). Do not add `immutable=1` to a
live changing WAL as a staleness workaround. OPT-1639 must define the no-hidden-write
proof and fail closed when it is unavailable; existing read paths remain explicit legacy
exceptions until then.

Read-only flags skip writable schema migration and write pragmas in the current opener,
but they do not prove that the containing directory cannot receive WAL or SHM sidecars.
Path-only roots, database paths, and sidecar checks cannot authorize a source mutation;
the writable owner still needs the bound root, database identity, profile guard, and live
source lease.

Full and Fast schema opens can both create or migrate source schema state, and Full may
clean invalid paths. A read role skipping migrations is not a migration-safe immutable
reader. The target migration rule is narrower: only the source owner's `Maintenance`
startup phase may create, migrate, or repair schema while it holds source ownership.
Operational writable roles require a currently compatible schema and return a typed
`MaintenanceRequired` result instead of opportunistically repairing it. The same owned
connection may perform that maintenance before entering the operational queue; no
competing writer is introduced. Current Fast migration/repair behavior is an explicit
OPT-1639 compatibility exception, and OPT-1637 does not change it.

## Current callers and migration exceptions

This inventory separates built paths from the proposed owner boundary:

| Current path | Evidence and contract status |
| --- | --- |
| `wavecrate-session/src/lib.rs::Persistence::start` / `shutdown` | Profile ownership and journal start before catalog; shutdown closes catalog, global writer, journal, then the final writable guard. This is the lifecycle ordering the source owner must join. |
| `wavecrate-catalog/src/lib.rs::RegisteredSource`, `run_worker`, `retire_source`, and `retarget_previous_source` | Catalog currently serializes a source registration and retains `SourceDatabase` plus publication fence/authority state. Retirement and retargeting are existing handoff exceptions, not proof that every caller uses the proposed typed command envelope. |
| `wavecrate-catalog/src/lib.rs::run_worker` shutdown and panic paths | The current worker has no panic-retention boundary, and the existing shutdown/retirement paths can drop the actual source lease when they finish. OPT-1857 must extend these lifecycle boundaries with a real lease-held async grant; a counter or non-owning fence cannot provide that retention. |
| `wavecrate-catalog/src/lib.rs::CatalogScanWriter` | Direct `SourceWriteBatch` access is a current catalog scan path. It is a migration exception that must be routed through the source owner before being treated as the general contract. |
| `wavecrate-library/src/sample_sources/db/write/curation.rs` | Curation already checks an expected source revision and root/profile authority in a bounded transaction, but its receipt lacks durable operation/participant identity. |
| `wavecrate-library/src/sample_sources/db/write/transaction.rs` and source directory/index writers | Direct source transactions and raw revision bumps remain built source-local writers; they do not receive the proposed lifecycle/operation envelope yet. |
| `wavecrate-catalog/src/lib.rs::read_snapshot_from_database` | The current catalog audit path reads source identity, revision, and bounded snapshot state. Its result is an observation and must retain the exact revision/lifecycle cut for the proposed FreshAsOf contract. |
| `wavecrate-library/src/sample_sources/db/read/metadata_queries.rs::browser_metadata_snapshot`, `browser_metadata_for_paths_at_revision`, and `browser_metadata_page` | These are the current browser metadata snapshot and keyset-page callers. `browser_metadata_snapshot` has no post-read freshness check; the future snapshot backend must report that boundary rather than imply a perpetually current projection. |
| `wavecrate-library/src/sample_sources/db/open/read_only.rs` and catalog projections | These are read observations. They do not own a source lease and cannot be promoted to write authority. |
| `sample_sources/db/file_ops_journal/reconcile.rs` | This is source-local recovery evidence with its own semantics, not a general source writer or durable participant receipt store. It must not be widened as a shortcut. |

The source SQL owner is therefore a migration boundary, not a claim that current
production callers are already routed. The owner contract does not add family adapters,
filesystem writer calls, UI policy, or a second authority thread.

## Deferred implementation decisions and follow-ups

| Follow-up | Required decision or implementation |
| --- | --- |
| [OPT-1638](https://linear.app/boostnlvp/issue/OPT-1638) | Implemented source-owned curation receipts, versioned codec, schema-v13 migration, exact duplicate replay, live fence validation, and confirmed-rollback/uncertainty readback. Caller routing, source retirement pinning, and broader source-write families remain deferred. |
| [OPT-1639](https://linear.app/boostnlvp/issue/OPT-1639) | Implement bounded revisioned snapshots, migration/no-hidden-write qualification, WAL-aware freshness, and fail-closed read backends. |
| [OPT-1640](https://linear.app/boostnlvp/issue/OPT-1640) | Define the competing-writer guard and the source-owner admission/reconciliation boundary. |
| [OPT-1782](https://linear.app/boostnlvp/issue/OPT-1782)–[OPT-1794](https://linear.app/boostnlvp/issue/OPT-1794) | Migrate individual source metadata, history, playback, and family callers after the owner and receipt contracts exist. |
| [OPT-1349](https://linear.app/boostnlvp/issue/OPT-1349) | Reuse the completed physical source lease, fence, and readiness rules; a publication fence still cannot renew or transfer a lease. |
| [OPT-1857](https://linear.app/boostnlvp/issue/OPT-1857) | Define a real lifecycle-held, send-safe live grant for another owner. The current non-owning source authority request and an atomic counter are insufficient. |

## Acceptance checklist for this decision artifact

- One physical source has one stated writable SQL owner, with actual lease/control-connection
  ownership separated from cloneable fences and snapshots.
- Startup, draining, retirement, retarget, panic, owner loss, and uncertain commit behavior
  fail closed and retain actionable identity evidence.
- `SourceRevision` binds source identity, lifecycle generation, positive lease epoch, and a
  checked monotonic revision; stale commands are rejected before mutation.
- The proposed write transaction commits SQL, the next revision, and a bounded structured
  identity delta together; exact duplicates do not increment or write, and conflicts do not
  mutate prior state.
- Snapshot budgets, keyset continuation, freshness outcomes, and no-authority semantics are
  explicit, including the WAL side-effect limitation.
- Current connection roles and direct caller exceptions are named, with no claim that
  production is already fully routed.
- OPT-1638, OPT-1639, OPT-1640, OPT-1349, OPT-1857, and the caller-family follow-ups are
  linked with their concrete unresolved boundary; no runtime implementation is claimed here.
