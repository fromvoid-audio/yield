# Collection membership undo contract

This contract was established by OPT-2108 for OPT-1617. OPT-2113 implements the
bounded active GPUI collection adapter described below. Compound operations,
restart history and native qualification remain separate acceptance; OPT-1617
and OPT-1476 remain open.

## Action and existing owners

The reference action changes a sample's membership in one of six fixed collection
slots. It is not collection display-name editing or tag-dictionary editing.

* Reference: `reference/radiant/src/native_app/sample_library/sample_collections/transaction.rs`
  registers inverse membership updates after successful state application.
  `reference/radiant/src/native_app/tests/collections.rs` contains
  `collection_assignment_transaction_undoes_and_redoes_membership`: assign, undo,
  redo, with database and visible membership assertions.
* Production GPUI: `crates/yield-ui/src/browser/curation.rs` plans collection
  actions in `CurationState::plan_action`, submits the existing catalog operation,
  and consumes `CatalogEvent::CurationOperationCompleted` in
  `Browser::apply_curation_event`. Accepted overlays are optimistic, not history.
* Session forwards the existing catalog API. `crates/yield-catalog/src/curation.rs`
  defines the operation bounds, revision fences and terminal outcome. The catalog
  owns admission and resolves a registered source's retained writer.
* `SourceWriter::commit_catalog_curation_command` in
  `crates/yield-library/src/sample_sources/db/write/source_writer.rs` validates
  source, root binding, database identity, lifecycle, lease epoch and revision.
  `SourceDatabase::commit_curation_with_profile_guard` in
  `crates/yield-library/src/sample_sources/db/write/curation.rs` commits membership,
  source revision and authoritative directory/audit continuity atomically, with
  profile validation before transaction and commit. Its opaque receipt contains
  committed rows, revision, root identity and directory generation.

History must call this route. It must not reopen a path, write SQLite on the UI
thread, manufacture a receipt, use reference closures as production capabilities,
or use playback-navigation history as edit history. Profile-owned collection
labels in `browser/collection_rename.rs` have a different owner and are outside
this action.

## Record and admission

The initial adapter is one source lifecycle, one collection slot, and one atomic
batch of at most `MAX_CURATION_UPDATES` (currently 128) changed paths. Existing
larger collection edits retain their operation behavior; supporting their partial
commits as undoable compound history remains explicit follow-up work. This initial
limit does not reduce the application-wide acceptance of OPT-1476.

A pending candidate retains:

* Exact source ID, lifecycle generation, root identity, directory generation,
  initial revision and catalog request ID.
* Deduplicated validated relative paths, and the confirmed before/desired after
  membership bit for the targeted slot only. Never restore the entire curation
  row: ratings, lock, playback history, tags and other collection slots belong to
  other operations.
* Stable selection/focus identities and the viewport anchor needed to preserve
  the original history requirement. These are presentation observations, never
  write authority.

Capture before-state only from the confirmed curation lineage while it has no
pending operations or staged latest-value edits and matches the audited active
metadata context. Admit with `ExactRevision(initial_revision)`. This binds the
observed before-state to the writer transaction: an intervening write rejects the
revision, rather than silently changing the inverse. Capturing an optimistic
overlay or using `AfterOperation` without the predecessor's acknowledged rows
cannot provide a trustworthy inverse.

Keep one history direction in flight. Ordinary edits may keep their existing
queue behavior, but any subsequently committed untracked edit invalidates the
history's continuation and redo eligibility. Accepted edits alone do not clear
redo or create history. No-op plans do neither.

Bound the initial history to 128 records and 16 MiB of owned payload, including
path bytes, source/root identities and presentation state. Check allocation and
arithmetic before retaining a candidate. Evict oldest settled records to fit;
never evict an in-flight record. A record that cannot fit must be reported as an
edit without undo coverage, rather than advertised as undoable. Pending history
payload also counts toward the bound. These are history bounds, not a change to
the catalog's existing 512-batch / 65,536-update operation limits.

## Durable acknowledgement and stack transitions

The original action enters the undo stack only after its matching terminal
outcome has all of the following:

1. The source lifecycle and request ID match the candidate exactly.
2. Exactly one committed batch, no failure, and matching initial revision.
3. Receipt root and directory generation match; revision advances exactly once.
4. The receipt contains every requested changed path exactly once and the targeted
   slot's membership equals the proposed after-state.
5. Publication is current and the active curation lineage accepts the receipt.

`is_success()` alone is insufficient; a zero-receipt outcome cannot prove a
commit. `final_revision()` on a failed operation is explicitly not database
readback. Duplicate delivery must not add a second history item.

Undo submits explicit `AddCollection` / `RemoveCollection` updates that restore
the captured before bits, with a fresh request ID and the exact last acknowledged
revision. Redo restores the after bits in the same way. They use the retained
catalog/source owner and revalidate current lifecycle/root/directory context.
Never toggle to implement an inverse. Never reuse a historical receipt revision
as a newly granted writer capability.

Move the item between undo and redo stacks only after the same receipt validation
passes for that direction. Update its continuation revision to the new committed
revision. A successfully committed new ordinary action clears redo. A successful
undo/redo is a direction transition, not a new ordinary history record. Admission
failure leaves the settled stack position unchanged.

| Outcome | Required behavior |
| --- | --- |
| No change or queue rejection | No history or redo transition; no fake success. |
| Failure before any committed receipt | Retain settled history; report failure; refresh authority before retry if stale. |
| Committed receipt with stale publication | Drain the event; invalidate history continuation and refresh; do not project success or replay the mutation. |
| Missing, duplicate or malformed receipt rows | Reject history acknowledgement and refresh; do not infer rollback. |
| Owner/profile loss or terminal channel closure | Stop admission and release pending presentation state; retain an explicit unresolved status if commit cannot be excluded. |
| Source replacement, lifecycle change or intervening revision | Invalidate continuation; matching paths or membership bits do not restore authority. |
| Compound operation with a committed prefix and later failure | Preserve committed-prefix projection/status; do not record the entire selection as successful or silently roll it back. |

The catalog curation route currently supplies an in-process commit receipt, not a
durable history identity. `commit_catalog_curation_command` deliberately does not
consume the source operation-receipt table. Therefore restart must not reconstruct
undo or retry an unknown commit from UI candidates. Durable restart/recovery
history needs a separate owner-backed record and recovery contract under
OPT-1476; adding an in-memory adapter cannot satisfy that acceptance.

## Concrete implementation and validation

The following leaves preserve the original acceptance:

1. OPT-2113, production collection history integration: add the bounded candidate/settled
   record to the browser curation lane, capture confirmed before bits, validate
   actual catalog outcomes, and submit undo/redo through the same owner. Include
   real registered-source SQLite commit/undo/redo assertions, selection/viewport
   preservation, no-op, rejection, redo invalidation, duplicate event and stale
   lifecycle/revision cases. The implementation is in
   `browser/curation/collection_history.rs`, integrated with actual admission and
   receipt consumption in `browser/curation.rs`. Dedicated collection controls
   submit fresh catalog commands; this does not claim global undo shortcuts or
   waveform, file, folder, profile-label or dictionary history.
2. OPT-2114, compound history: define and implement committed-prefix records and retry
   behavior across existing multi-batch operations; test failure at every batch
   boundary with actual owners. No whole-selection success after partial failure.
3. OPT-2115, restart/recovery history: specify and implement bounded retained history
   identity, atomic persistence with metadata commit, owner-fenced readback,
   unresolved completion reconciliation and retention/eviction. Test actual
   restart after commit and before delivery, preserving OPT-1476's recovery scope.

Useful existing portable regression commands are:

```sh
cargo test --locked -p yield-library --lib sample_sources::db::write::tests -- --test-threads=1
cargo test --locked -p yield-catalog --lib curation -- --test-threads=1
cargo test --locked -p yield-ui --lib curation -- --test-threads=1
bash scripts/internal/agent/run_agent_preflight.sh
```

The focused integration command is:

```sh
cargo test --locked -p yield-ui --lib collection_history -- --test-threads=1
```

Its GPUI fixture clicks the actual collection controls and uses an actual
registered-source CatalogService and retained SourceWriter, then checks SQLite
membership, unrelated rating/play history, selection/focus/scroll restoration,
redo invalidation and duplicate delivery. Reopening the actual catalog verifies
persisted metadata; it does not restore history. Separate negative cases cover
malformed/stale receipts, failure/no-op, bounded pending/settled presentation and
retired source contexts. The cfg(test)-only SourceRegistry harness routes these
commands/events directly to the actual CatalogService without replacing its
commit receipts or adding a production bootstrap bypass.
Late retired completions invalidate only their matching source lifecycle and
preserve the new active source's history. Unconfirmed pending completion remains
visible even after the active catalog source is cleared.

Normal Linux session bootstrap rejects this fixture with:
`shared volume ownership is unsupported at .yield: no qualified identity-safe
per-user application root`. OPT-2143 retains normal qualified native
profile/journal bootstrap and active UI qualification. The reference fixture is
the behavioral specification; portable owner tests do not complete physical
recovery, native execution, compound history (OPT-2114), or retained restart
history (OPT-2115).

## Dependency correction

OPT-1611's waveform bytes/trim adapter and OPT-1592's physical restore-command
adapter are not prerequisites for source-local SQL collection membership undo.
Retain them as related application-history work, remove their blocking links from
OPT-1617, and gate that issue on the concrete collection implementation leaves.
Physical qualification stays with the relevant native/recovery issues. The
missing collection design and production adapter are portable work.
