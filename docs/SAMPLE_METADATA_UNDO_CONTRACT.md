# Acknowledged sample rating and lock history

Decision for [OPT-2182](https://linear.app/boostnlvp/issue/OPT-2182), preserving
[OPT-1619](https://linear.app/boostnlvp/issue/OPT-1619) and application history
tracker OPT-1476. The first action is `UnlockKeepRating`: rating 3 with lock true
becomes rating 3 with lock false. Undo restores lock true; redo restores lock
false. This document supplies no production adapter, durable history or native
qualification. Collection membership and sample metadata retain distinct payload
ownership while sharing one chronological curation history lane.

## Reference and actual route

`reference/radiant/src/native_app/sample_library/sample_ratings.rs` retains
`RatingUpdate` before/after rating and lock, and registers explicit Before/After
inverse closures in `register_rating_transaction_with_label`. Its
`persist_rating_requests` writes rating and lock and touches `last_curated_at`.
Those optimistic closures do not prove acknowledged production commits.
`reference/radiant/src/native_app/tests/file_operations.rs` contains
`context_menu_unlock_drops_locked_keep_sample_to_keep_three`: the displayed locked
keep becomes unlocked keep-three without changing numeric rating. It specifies
unlock behavior, not already-qualified sample undo or restart.

The existing production route is:

* `crates/yield-core/src/curation.rs`: `plan_curation_action` admits unlock only
  for `MAX_RATING` (3) and a true lock; other states are unchanged.
* `crates/yield-ui/src/browser/curation.rs`: `CurationState::plan_action`,
  `updates_for_action`, accepted overlays and confirmed lineage. Unlock produces
  `SourceCurationUpdate::SetLocked`, not a rating decrement. Rating adjustments
  may produce both `SetRating` and `SetLocked`; a physical trash transition is a
  different action and cannot become metadata undo.
* `crates/yield-catalog/src/curation.rs`: the worker's `execute_operation`
  validates admission and resolves the registered retained source writer. Session
  forwards this route; `Browser::apply_curation_event` consumes its terminal event.
* `crates/yield-library/src/sample_sources/db/write/source_writer.rs`:
  `SourceWriter::commit_catalog_curation_command` validates the complete retained
  `SourceRevision`, including source/root, database binding/object, lifecycle,
  lease epoch and revision. The source curation transaction in
  `crates/yield-library/src/sample_sources/db/write/curation.rs` updates metadata,
  source revision and directory/audit publication atomically with profile checks
  before transaction and commit.

Reuse these owners. A new SQL command or observation is unnecessary for this
single-batch in-process action: confirmed metadata plus `ExactRevision` already
binds its inverse to the actual writer. A path, reference closure, receipt DTO or
numeric revision supplies no owner authority. No UI SQL, pathname reopen, second
writer, new coordinator or independent sample stack is allowed.

## Capture, bounds and field ownership

Capture only from the current audited confirmed curation lineage with no pending
operations, staged latest-value edits, failure or invalidated context. Require
the current registered source, root, lifecycle, manifest revision and directory
generation. Freeze sorted unique validated relative paths, before/after numeric
rating and lock, request identity, initial revision and bounded stable selection,
focus and viewport observations. The unchanged numeric rating is a guard; unlock
writes only lock. Observed before timestamp is provenance, not an inverse value.
Exclude no-op paths. Never capture an optimistic overlay or infer the predecessor
of an `AfterOperation` chain without its acknowledged rows.

The initial undoable action is exactly one atomic batch, at most 128 `SetLocked`
updates and therefore at most 128 changed paths. Every path is at most 4,096
bytes, with checked aggregate accounting. Future two-field rating records need
up to two updates per path: 64 such paths consume the 128-update limit; never
count paths as updates or split a row across transactions to advertise atomicity.
Keep ordinary 512-batch / 65,536-update, 16-MiB path and 32-MiB estimated receipt
admission unchanged. Larger unlock edits may retain existing ordinary behavior
with explicit absent history coverage; never truncate and advertise whole-action
undo. Compound sample history requires an explicit extension of OPT-2114's
prefix contract, not implicit acceptance by this single-batch record.

Extend `browser/curation/collection_history.rs` with field-specific payload
variants and common settlement/direction ordering. Charge all variants together
against 128 logical records and 16 MiB, including original pending candidates,
direction plans, container capacities, identity/path bytes and presentation or
restore copies. Bound selection identities before cloning; use checked sums and
shared immutable payload where appropriate. A 1-MiB maximum sample record aligns
with retained restart payload bounds; existing collection coverage bounds remain
intact. Evict only oldest settled eligible records;
never in-flight, unresolved or exclusive partial compound records. If coverage
cannot fit, report that explicitly while retaining ordinary edit admission.
Do not evict redo entries merely to retain an uncommitted ordinary candidate;
if settled undo eviction cannot provide room, leave that candidate uncovered.
The ordinary edit may clear redo only after its acknowledged commit.

An accepted overlay is pending presentation, not history. Maintain one history
direction in flight and one chronological top across sample and collection
variants. An older sample record cannot overtake a newer collection record, nor
vice versa. Existing collection payloads still change only their slot; this
integration does not change collection or dictionary ownership. Preserve the
ordered immutable batch/subset/phase seams defined by OPT-2181. A partially
settled compound top blocks older directions, including sample directions.

## Acknowledgement, timestamps and direction changes

Submit the original action with `ExactRevision(initial_revision)`. Admit undo or
redo with a fresh request ID at the exact last acknowledged current revision.
Require confirmed rating and lock equal the expected endpoint before planning.
Submit explicit `SetLocked(true)` for undo and `SetLocked(false)` for redo;
never run Unlock again as an inverse or toggle the live value.

History settlement requires all of the following:

1. Exact pending source/lifecycle and request identity, current active context
   and `CatalogPublicationStatus::Current` for a committed projection.
2. Exactly one actual committed receipt, no failure, matching initial revision,
   and final/receipt revision equal initial revision plus one without overflow.
3. Receipt root identity and directory generation match the captured context.
4. Every changed path appears exactly once, with no extra/missing paths, valid
   metadata rows, rating 3 and the requested lock endpoint. Validate the actual
   retained receipt before collapsing rows into a map that could hide duplicates.
5. The confirmed curation lineage accepts that receipt and updates its projection.

The opaque `SourceCurationCommitReceipt` carries `source_revision()`,
`root_identity()`, `directory_generation()` and `files()`. Source/lifecycle/request
identity comes from the event and captured registered context, not extra invented
receipt fields. `is_success()` alone is insufficient; a zero-receipt outcome
proves no edit. `final_revision()` on failure is not database readback.

`SourceWriteBatch::set_locked` and `set_tag` in
`crates/yield-library/src/sample_sources/db/write/batch.rs` touch the curation
timestamp using owner wall-clock epoch seconds. Each acknowledged original,
undo and redo is a new curation event: retain its actual receipt timestamp. Do
not restore the pre-edit timestamp, use maintenance-only
`SetRatingAndLastCuratedAt`, or synthesize a UI timestamp. Equal seconds or a
backwards wall clock are possible; require actual receipt/readback agreement,
not strict timestamp increase. Rating, collection membership, tags, playback
history and file/manifest data remain unchanged by unlock directions. Verify
these through real SQLite and receipt rows rather than writing an entire old row.

Only a validated committed original enters history and clears redo. A direction
moves that same record between undo and redo only after its validated receipt;
it is not a new edit. The shared lane updates its continuation revision, so an
older mixed-payload record may follow a newer successfully undone record without
requiring its historical original revision. Any acknowledged untracked ordinary
edit clears redo and conservatively invalidates continuation. Accepted, no-op
or rejected commands do neither. Receipt duplication cannot move a stack twice.

| Outcome | Required state |
| --- | --- |
| No change, bound or queue rejection | No history item, timestamp claim or redo transition. |
| Known failure with no committed receipt | No original item; retain settled direction position, report failure and refuse retry until current authority is validated. |
| Actual commit with stale publication | Drain evidence, invalidate continuation and refresh; never show current success or automatically replay/invert. |
| Malformed, wrong-field or missing receipt | Do not settle history or infer rollback; invalidate unsafe continuation. |
| Terminal delivery lost, cancellation after admission or owner loss | Explicit unresolved completion, disabled directions, bounded diagnostic and released stale presentation; no assumed zero-commit retry. |
| Source/root/lifecycle/directory/revision replacement | Retire continuation; equal rating/lock or path does not restore authority. |

Retain lifecycle-scoped unresolved status after context retirement; a late old
event cannot clear the new source's history. Restore presentation only for
surviving stable identities in the current source/filter; otherwise preserve the
current view and report the result. Cached direction controls capture the exact
top record, direction, source/view and generation and revalidate on invocation;
they cannot operate on a replacement top merely because a rectangle is reused.

## Restart extension and concrete production leaves

The current curation receipt is in-process evidence, without durable history or
attempt identity. Metadata reopening does not restore history. Unknown delivery
cannot be resolved by equal values, a numeric revision, or reference closures.
Collection-only OPT-2127/2128/2129 and
[METADATA_HISTORY_PERSISTENCE_CONTRACT.md](METADATA_HISTORY_PERSISTENCE_CONTRACT.md)
do not authorize sample restart. Extend their existing owners with an explicit
versioned sample payload, rather than relabeling collection, tag or file grants.

The sample payload persists rating/lock endpoints and timestamp provenance; each
settled attempt records its actual new timestamp. Stable profile/media/database
identities survive reacquisition only after fresh owner validation; old lifecycle,
lease and acquisition identities are provenance. Reuse the combined history
budget, 1-MiB record, one pending direction, bounded codecs and owned schema
maintenance. Register the exact SetLocked/timestamp/history SQL envelope, durable
intent, attempt/digest/direction and finite capacity requirement. Consume actual
OPT-2136 all-owner admission and OPT-2137 claim-backed required/recovery permits.
No scalar WAL fixture, boolean recovery flag or caller estimate grants admission.

Commit lock, fresh timestamp, source/audit/directory continuity, settled history
direction, attempt receipt and pending consumption in one source transaction.
Exact duplicate attempts return retained historical evidence without executing
again; digest mismatch, obsolete generation and evicted ID reject. Read-only
reconciliation never renews, migrates or checkpoints. Preserve prepared,
committed, unresolved and unavailable distinctions and indexed retention that
refuses pending/unresolved eviction. Lost journal/source acknowledgements retain
the same admitted operation; no replacement ID or automatic metadata replay.

Concrete leaves preserve original acceptance and identify shared files:

* [OPT-2209](https://linear.app/boostnlvp/issue/OPT-2209): actual in-process unlock
  adapter and mixed chronology, common history/curation siblings plus narrow
  GPUI controls/tests. Coordinate OPT-2186/2187 and OPT-2160 before shared edits.
* [OPT-2210](https://linear.app/boostnlvp/issue/OPT-2210): sample variant of actual
  admitted history codec/schema/prepare, after OPT-2127; source history budget
  and journal family registration, using existing reservation/permit owners.
* [OPT-2211](https://linear.app/boostnlvp/issue/OPT-2211): sample atomic directions
  and retained restart reconciliation, after OPT-2210 and OPT-2128; source
  history/write/reconcile siblings and genuine normal-build process fixture.
* [OPT-2212](https://linear.app/boostnlvp/issue/OPT-2212): actual recovered sample
  consumption, after OPT-2211, OPT-2209 and OPT-2129; bounded catalog/session and
  common GPUI recovery/history integration.

These are implementation leaves, not qualification from this decision. Keep
OPT-1619 open until real original/undo/redo and redo-invalidation acceptance;
keep OPT-1476 open for all app-wide history, restart and physical requirements.
Native normal-bootstrap qualification requires the exact OPT-2143 profile-root,
volume/journal and GPUI resource after the portable sample route exists.

## Required evidence

Use actual registered CatalogService, profile-owned SourceWriter, GPUI controls
and SQLite assertions. Exercise unlock/undo/redo, interleaved collection/sample
top ordering, redo invalidation only after commit, no-op/rejection, duplicate or
late event, pending latest-value exclusion, bounds at 128/129 updates, timestamp
and unrelated-field preservation, stale context/cached callback, owner loss and
lost delivery. The cfg(test) registry fixture routes genuine source receipts;
it does not qualify production profile/journal bootstrap. Restart leaves require
real close/reopen and normal-build process death before/after intent and commit,
including undo/redo commit before delivery, malformed storage and rejected
capacity/WAL. Model negatives may supplement, never replace these paths.

Existing regression commands establish reused owner behavior only:

```sh
cargo test --locked -p yield-core --lib curation
cargo test --locked -p yield-catalog --lib curation -- --test-threads=1
cargo test --locked -p yield-ui --lib collection_history
scripts/agent.sh preflight
```

The adapter must add and run focused actual sample-history/GPUI tests, full
affected UI checks, current shared-owner checks and canonical preflight. Every
leaf rebases after preceding merges and verifies its entire merged tree against
the tested tree before Done. No contract fixture completes production history
or physical qualification.
