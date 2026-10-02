# Durable collection history and restart reconciliation

OPT-2124 specifies the portable prerequisite for OPT-2115. This document is a
decision for future implementation, not evidence of implemented persistence.
OPT-2115 and application-wide history tracker OPT-1476 remain open. The active
in-process adapter in OPT-2113 and compound history in OPT-2114 retain their
acceptance in [COLLECTION_UNDO_CONTRACT.md](COLLECTION_UNDO_CONTRACT.md).

## Current production boundary

`crates/yield-catalog/src/curation.rs` resolves the registered source, validates
its lifecycle and dispatches to `SourceWriter::commit_catalog_curation_command`
in `crates/yield-library/src/sample_sources/db/write/source_writer.rs`.
`SourceDatabase::commit_curation_with_profile_guard` in
`crates/yield-library/src/sample_sources/db/write/curation.rs` atomically commits
the membership, source revision, directory publication and audit continuity.
Its receipt contains committed rows but has no durable history identity.
The writer explicitly avoids the existing physical operation-receipt table for
ordinary catalog edits. Reusing that table or treating a catalog request counter
as restart identity would change that contract without providing recovery.

The source writer owns history SQL. Catalog owns admission, bounded dispatch and
recovery presentation. GPUI owns selection/focus/viewport observations. Neither
UI candidates nor a receipt deserialized by a caller grant write authority.
All reads and writes use the registered retained connection; no pathname open,
second writer, UI-thread SQLite or read-only migration is permitted.

## Persistent record and finite bounds

Add source-local history tables through the owned schema-maintenance route.
Names and API types below describe the required semantics; they are not existing
symbols. Use a versioned codec with exact enum/bit validation and bounded SQL
projections before allocating Rust payloads. No opaque unbounded JSON field.

| Record | Required contents |
| --- | --- |
| History identity | Owner-issued random 128-bit ID, format version, stable profile-root identity, source ID, media-root identity, database-root binding and database-object identity. |
| Membership payload | One fixed collection slot; at most 128 unique validated relative paths; exact before and after bits for that slot only; bounded presentation observations. |
| Settled state | Original commit revision, latest committed continuation revision, monotonic direction generation, applied/undone position, and eligibility/invalidation reason. |
| Pending direction | Owner-issued attempt ID, history ID, expected direction generation and revision, apply/undo/redo kind, canonical payload digest and admitted requirement identity. |
| Receipt | Attempt ID/digest, direction generation, committed revision, audited directory generation, exact resulting membership rows and settled direction. |

Keep at most 128 settled records and 16 MiB of serialized owned payload per source,
with at most one pending direction. The pending payload counts toward both
bounds. One record is at most 1 MiB. Bound each path to the existing accepted
relative-path limit, presentation identities to the same path count, and encoded
identity strings to 4,096 bytes each. Validate checked sums before SQL or
allocation; reject duplicate paths, invalid collection slots, malformed digests,
unknown versions and oversized field lengths. SQL row limits alone do not bound
cell sizes.

The catalog/UI recovered projection separately retains the original combined
128-record/16-MiB limit across sources, including candidates in flight. The
per-source persistent cap does not claim a profile-wide physical storage cap:
the registered-source admission requirement must account for each source store.
Settled records outside the projected set do not become hidden undo candidates.
Retirement and profile-wide retention policies remain explicit implementation
work; do not advertise a universal bounded disk allocation from a UI memory cap.

Retain the most recent receipt for each settled record and its monotonic direction
generation. An obsolete attempt may be reported as superseded, never executed
again. An evicted history ID can never be recreated by a direction request. An
original operation has a new admitted identity; it cannot retry as a new operation
to resolve an unknown old result.

## Identity after reacquisition

Persist stable profile-root, source, media-root, database-root and database-object
identities. Persist original lifecycle, lease and owner-acquisition identities
only as provenance. `WritableProfileGuard::owner_identity` in
`crates/yield-library/src/app_dirs/profile_ownership.rs` is fresh on reacquisition;
source lease epoch and catalog lifecycle can also change at restart. Requiring
those old transient values to match would prevent genuine recovery. Ignoring all
identity fields would allow replacement databases to inherit history.

The newly retained owner must validate the stable identities, current profile
guard/source lease, current audited source/directory state and known schema. It
then issues a fresh observation in the new lifecycle for reconciliation. That
observation is not an old write grant. A replaced root/database or uncertain
identity yields unavailable history; matching paths or numeric revisions cannot
authorize replay. Every new undo/redo uses the complete fresh `SourceRevision`.

## Admission and durable intent

History requires a separately admitted finite metadata requirement, including
payload, SQL work, database/WAL/SHM peak, retained receipt and recovery work.
`crates/yield-library/src/sample_sources/db/write/source_writer/tag_metadata_budget.rs`
and `crates/yield-library/src/sample_sources/db/schema/metadata_budget.rs` provide
existing finite metadata mechanisms to inspect, not a transferable history grant.
Extend the compiled schema/SQL/runtime envelope for this exact command family;
do not relabel tag capacity or trust caller-supplied byte counts.

Before allocating a durable history ID/intent, validate all bounds, acquire a
fresh retained-owner observation, reject hard/unknown WAL health for new work,
and obtain the actual registered capacity admission covering both intent and
completion. OPT-2126 owns the shared affected-participant reservation and
protected required-completion handshake; history must implement that owner
contract rather than create a separate recovery exemption. No scalar fixture,
boolean recovery flag or merely retained guard
substitutes for a registered requirement/reservation. The implementation must
either provide that admission or return a typed unavailable/rejected outcome.

Prepare one durable pending attempt on the source owner before any membership
effect. Its bounded payload includes the before bits read by that owner under
the requested exact revision, after bits and presentation observations. Intent
persistence itself is admitted SQL; it does not change the content revision or
claim membership committed. Only after intent commit return the admitted attempt
identity. If delivery is lost, bounded owner readback discovers that pending
attempt; the UI need not have persisted its candidate to discover it.

While this attempt is unresolved, block conflicting source mutations at the owner
scheduler boundary, outside SQLite transactions. Do not hold a transaction across
UI delivery, hashing, filesystem work or retry sleep. Other requests receive a
bounded deferred result; do not silently run them and invalidate the captured
before-state. Cancellation may atomically settle the intent as uncommitted only
after retained readback proves the membership transaction did not commit.

Required completion above a hard watermark is permitted only through the actual
already-admitted protected recovery requirement. Otherwise retain the pending
record and report blocked recovery. This decision does not complete universal
WAL/recovery acceptance in OPT-1685/1686 or grant physical effect authority.

## Atomic commit and direction changes

The completion transaction revalidates profile/source/binding and exact revision,
then validates the pending identity/digest/generation. Read authoritative before
bits again and require equality with the pending record. Apply explicit add/remove
updates, never toggles. Commit together:

1. Only the targeted collection membership changes.
2. Source revision, active directory publication and authoritative audit revision.
3. The settled history record/direction, immutable latest attempt receipt and
   consumption of the pending intent.
4. Redo invalidation for a newly committed ordinary action, where applicable.

All are in the same source transaction. There is no global/source atomicity claim.
Existing unrelated ratings, lock, tags, other slots and playback fields remain
untouched. Profile validation is repeated before commit, as for current curation.
Commit failure/unknown result never triggers an automatic inverse or replay.

Undo and redo each prepare a fresh admitted pending attempt against a settled
history ID, exact current revision and next direction generation. Undo restores
before bits; redo restores after bits. Advance direction and receipt only in the
same membership transaction. A duplicate attempt with matching digest returns
the recorded outcome after fresh identity validation; mismatched digest rejects.
The receipt may establish historical completion while current publication is
stale; it must not be projected as current success.

New ordinary committed edits invalidate redo and stale continuation in the same
owner transaction. Accepted/queued/no-op edits do not. Untracked scanner, audit,
tag or other source revision changes invalidate continuation conservatively;
history does not merge around those changes from equality of membership bits.
All revision-changing routes need this fence or an exact readback that refuses
continuation when the current revision differs. Per-source order is authoritative;
cross-source chronological presentation provides no multi-database atomic undo.

## Reconciliation and retention

On owner registration, read bounded settled and pending records through the
retained source. Recovery reads never renew leases, checkpoint or migrate. A
writable recovery transition separately obtains fresh owner authority.

| Retained evidence | Result |
| --- | --- |
| Matching committed receipt and history state | Report the completed direction once; recover projection only if current authority/revision still matches. Never rerun the membership mutation. |
| Pending intent, no receipt, unchanged expected revision/bits | Report prepared but uncommitted. Offer explicit admitted completion/cancellation; do not auto-execute an edit after restart. |
| Pending intent with conflicting or malformed evidence | Keep unresolved; block conflicting mutation and expose a fixed recovery reason. Absence of a UI event is not rollback proof. |
| Missing pending intent before any admitted ID was returned | No history mutation was admitted. Treat lost UI candidate as unavailable, not a durable edit. |
| Stale/replaced/uncertain source or profile | No continuation or replay; retain bounded diagnostic evidence with no authority-bearing paths in metrics. |
| Receipt read unavailable, schema missing or size overflow | Typed unavailable/unresolved state; never convert to no history or uncommitted success. |

Evict oldest settled records to fit before admitting a new record, using finite
indexed SQL and checked byte accounting. Never evict a pending/unresolved attempt
or its required recovery evidence. If those prevent fitting, reject new history
admission explicitly; an ordinary edit without history coverage must use its
separate existing admission and must be presented as non-undoable. Cancellation,
settlement and eviction release capacity only after their owner-backed durable
disposition is proven. Deleting SQLite rows does not prove physical WAL/database
space reclaimed. No VACUUM, forced checkpoint or physical release is implied.

Restore selection/focus/viewport only from bounded stable observations that still
resolve in the active source; otherwise preserve current presentation and report
the history result. Never focus a stale path or mutate metadata to make an old
viewport valid. Duplicate recovery events cannot move stacks twice.

## Schema rollout and concrete implementation

Follow [DATABASE_MIGRATIONS.md](DATABASE_MIGRATIONS.md): base DDL, idempotent
current-stamp additive repair, known-schema metadata budget, preservation fixtures
and read-only compatibility must change together. Current source schema is v13;
do not assume a matching stamp installs these proposed tables. History reads on
old/missing tables return explicitly unsupported without DDL. Maintenance and
legacy adoption must remain coordinated with OPT-2112/2116/2117; this document
does not authorize pre-lease schema writes.

OPT-2138 first supplies actual retained-owner bounded confirmed membership
observations without intent, renewal or a write grant. Later intent preparation
must still reread/revalidate the before bits inside its admitted owner transition.
Implement OPT-2127 for admitted finite history preparation and schema,
OPT-2128 for atomic original/direction persistence and retained reconciliation,
and OPT-2129 for catalog/session/GPUI restart consumption. None completes OPT-2115 alone. Required
actual-owner cases include restart before intent, after intent, before commit,
after commit/before terminal delivery, undo/redo commit-before-delivery, duplicate
attempt/digest mismatch, obsolete direction, replaced DB/profile/lifecycle,
untracked intervening revision, malformed/oversized storage, low capacity,
hard/unknown WAL, retained unresolved record preventing eviction, and legacy
read-only opens. Verify exact membership plus unrelated fields and all revision/
audit/directory/history rows, not a model-only stack.

Compound partial directions must use OPT-2114's committed-prefix behavior; do not
promote a multi-batch prefix into whole-selection success. Physical filesystem,
network-volume and power-loss qualification remain separate requirements.

Useful existing baseline commands (not acceptance for missing implementation):

```sh
cargo test --locked -p yield-library source_db_migration_tests
cargo test --locked -p yield-library snapshot_
cargo test --locked -p yield-catalog --lib curation
cargo test --locked -p yield-ui --lib curation
bash scripts/agent.sh preflight
```
