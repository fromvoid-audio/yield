# One-source tag-assignment participant (OPT-2034)

Decision against main `b391fe8d1`. This document and the synthetic SQLite fixture
in `crates/yield-library/tests/source_tag_participant_contract.rs` do not implement
assignment-wide rename/delete, grant capacity, or finalize a profile definition.
OPT-2012, OPT-1721/1722 and OPT-1486 remain open.

## Existing callers and owners

| Route | Existing responsibility | Exact extension |
| --- | --- | --- |
| GPUI tag dictionary/library -> session vocabulary -> profile GlobalLibraryWriter | Definition-only revision-fenced mutations. | Submit a durable assignment operation to the existing journal owner; no GUI transaction loop or definition-delete fallback. |
| Catalog worker -> SourceWriter | Owns profile guard, retained source database/lease, lifecycle and source revision. `SourceWriterRequest` already binds OperationId/ParticipantId/SourceCommandId and bounded canonical bytes. | Add one typed assignment-selection and assignment-participant command to this owner. Do not start a second source writer. |
| SourceWriteBatch/tag mutations | Owns `source_tags` and `wav_file_tags`; numeric tag IDs are source-local. Existing curation batches preserve active directory and authoritative-audit revisions. | Validate whole selection inside the transaction; apply exact tag rows, advance continuity revisions and store immutable counts/receipt atomically. |
| SourceWriter receipt reader | Same-owner retries return exact curation receipts; reopened owners can read historical observations. Direct replay currently rejects a superseded lifecycle. | Recovery validates current authority and exact stored assignment identity/counts; never reconstruct a live SourceRevision from historical receipt fields. |
| Journal participant/checkpoint owner | Owns stable operation/work-unit identity, required participant set and dispositions under OPT-1641. | Persist/read back exact source outcome before advancing that participant; preserve prefix failure and the profile definition. |

Use `source_writer.rs`, `write/curation.rs`, `tags/{identity,mutations}.rs` and
the existing `source_writer_receipts` transaction/retention boundary. Extend its
versioned request/result codec with an explicit assignment variant; V1 curation
receipts remain readable and cannot satisfy a V2 assignment receipt. A receipt
table is not a second owner. Profile vocabulary and source numeric tag IDs never
share an assumed global identity.

## Journal command prerequisite

The current `OperationKind` and `UniversalCommandKind` enumerate file operations;
there is no metadata-tag command. File Rename/Delete must never authorize tag
rename/delete. Before exposing the assignment-wide coordinator, OPT-2012 must add
an explicit versioned MetadataTagRename/MetadataTagDelete intent with bounded
profile definition identity/revision, frozen source/work-unit manifest and canonical
old/new identities. Unknown/old readers retain it as unsupported and cannot execute
it as a file command. Its admission plan includes capped journal/profile stores
and every source participant; encode/decode, retention and restart tests must cover
that real owner path. Source-local OPT-2049 supplies the participant only and cannot
claim that this journal admission or finalization adapter exists.

## First production leaf: bounded whole-source selection

Prepare exactly one source at one owner-issued SourceRevision. Resolve a **normal**
old canonical identity using the existing whitespace collapse + ASCII lowercase
policy; reject locked playback/classification labels, NUL, empty or over-1024-byte
labels. Rename resolves a distinct new identity; first leaf rejects an existing
target identity, including case/whitespace-equivalent self-renames. No implicit
merge of definitions or assignments. Delete has no target label.

Select **all** assignments to the old source-local tag, ordered by normalized
root-relative path. Query at most limit+1 rows with byte-length preflight and a
worker query deadline. First leaf accepts at most 64 rename paths (two curation
edits per path) or 128 delete paths, with at most 1024 bytes/path and 256 KiB
canonical request bytes. Empty selection produces an immutable NoAssignments
receipt, not an unexamined absent-source success. Missing/unsupported manifest
members, malformed/duplicate paths, unsafe tag identities, deadline/byte/row
overflow and an unavailable authoritative audit reject before edits. A larger
scope must use the later durable frozen-page implementation; never silently
truncate it to the first page.

The immutable payload records schema/kind, stable source/root/database identity,
initial source revision/directory generation, old source-local ID + canonical
identity, rename target identity, exact ordered affected paths and expected old
assignment membership. Capture the profile definition identity/category/revision
and frozen required source set in the journal intent. Ordinary live registry
membership is not proof that every assignment-owning source was inspected.

Key: `(OperationId, ParticipantId, SourceCommandId)`, with a durable mapping to
the frozen WorkUnitKey. Canonical request bytes include the initial selection
and logical fences. Current profile ownership, lease epoch and lifecycle are
execution admission, not a fresh replay key. A reused key with different kind,
label, path order/membership or immutable payload conflicts, even after restart.
Keep the full bounded canonical payload for equality, not only a hash.

## Transaction, admission and replay

1. Validate the current profile owner, source/root/database binding, live lease
   and lifecycle. Require a valid current source observation for replay too.
2. Look up this key before applying a new revision check. Exact stored V2 result
   returns historical counts/revision without changing rows. A different physical
   binding or conflicting payload never replays as live completion. Reacquired
   lease/lifecycle cannot rewrite the original result.
3. For an unseen key, require the exact initial revision/audit/generation and
   exact whole affected selection, including tag classification and collision
   preconditions. Check receipt count/byte retention and admitted metadata capacity.
4. One source SQLite transaction applies rename's remove-old/add-new or delete's
   remove-old assignments, preserves unrelated tags/metadata, advances source,
   active directory publication and authoritative audit revisions together, and
   inserts the V2 canonical request/result. The new result contains identities,
   exact counts and committed revisions; it must not materialize unbounded legacy
   BrowserFileMetadata/tag-label arrays merely to issue a receipt. Projection is a
   separate bounded read after commit. Record selected, changed, missing,
   already-target and no-op counts; first leaf rejects any missing/target collision
   instead of converting it into partial success. Do not delete the profile definition.
5. Commit and bounded readback precede a private owner-issued result. Ack loss
   retries the original key/payload; uncertain commit returns retained uncertainty,
   not a newly generated operation. Recovery may read the original result through
   current authority; it remains historical evidence for the matching checkpoint.

**Memory/receipt caps are not metadata allocation estimates.** Admission must cover
source SQLite growth, WAL/rollback coexistence, journal intent/checkpoint and
profile definition/hold data before mutation. The existing estimator reports
MetadataBoundNotDefined for unbounded metadata; never substitute zero. The first
implementation must supply an owner-bound estimate/enforced storage budget for
its fixed schema and exact selection, not infer allocation from label/path bytes.
The first-leaf policy is deliberately conservative: WAL mode only, no SQL-writing
extensions/unknown schema/triggers, temporary storage in memory, cache spilling
and auto-checkpoint disabled while the transaction is entered, and an enforced
max_page_count. Let P be the captured page size, M the enforced maximum page count,
and B=P*M (first leaf accepts B <= 64 MiB). M must cover existing pages plus the
explicit growth budget; overflow or an observed size beyond the cap rejects.
Reserve at least B database bytes, `32 + M*(P+24)` additional WAL bytes and
`32768 + 16*M` additional WAL-index bytes over captured resident sidecars. This
conservatively treats the whole capped database as newly allocated; no-spill
single-transaction commit bounds dirty WAL frames by M. Do not count a receipt
byte cap as the page cap. Validate/revalidate these connection policies and file
facts under the source owner; restore connection settings only after quiescence.
Journal/profile store plans must separately cover their own capped intent/receipt/
hold transactions and be included in the same ordinary-admission plan before the
first source edit. The implementation must test capped growth, rollback, temporary
storage/trigger rejection and checked arithmetic; if that proof/policy cannot be
established it returns MetadataBoundNotDefined, not a successful stub. Later large
source support needs its own bounded allocation policy, not silently raised caps.
Reject unavailable claims or receipt retention exhaustion before edits. A
synthetic Known budget in the fixture grants no production capacity. Reuse existing
capacity owners and per-volume coalescing; no GUI scalar/free-space precheck.

## Definition preservation and concurrency

One-source success is not profile definition success. The profile definition
remains until every frozen required participant has its exact durable checkpoint.
Partial prefix, offline/retired source, revision race, failed persistence or unknown
admission preserves it and its intent/remaining set. Do not compensate by blindly
reversing assignments under a newer revision. A new source lease needs fresh
authority; historical receipts are observations, not write grants.

Before **assignment-wide finalization**, the journal/profile owner must hold a
durable old/new tag-definition mutation reservation. Each affected source writer
must persist a per-tag reservation before freezing selection and reject conflicting
AddTag/AssignExistingTag and rename/delete paths, including legacy tag mutation
entrypoints. Registry source-set changes and profile definition changes must
invalidate/reject finalization while that frozen set is in force. Reservations
remain through ack loss/restart and are released only after definition finalization
or an explicit definition-preserving abandoned/blocked outcome. Concurrent new
assignments cannot be waved away as outside the captured snapshot.

The first one-source leaf does **not** finalize a dictionary definition or claim
these cross-owner reservations exist. OPT-2012 implements the actual durable
reservations/coordinator and bounded source pagination before exposing assignment-wide
rename/delete. No ATTACH/WAL atomicity assumption, mutex-only reservation, source
ID deduplication shortcut or per-render-loop saga is accepted.

## Decision matrix and verification limits

The fixture uses a small synthetic SQLite schema and typed command/fence/results:
exact replay and response-loss reopen return the same historical result; conflict,
stale source revision, replaced binding, stale profile and unknown admission leave
rows/definition unchanged; a fault between row delta and receipt rolls both back;
empty/no-op scope does not advance revision; oversized scope, missing/locked source tag and target collision fail; a committed
prefix with an outstanding source or absent reservation never finalizes definition.
It reuses the real OperationId/ParticipantId/SourceCommandId types, not live grants.

Production leaf files: `db/write/tag_assignment_participant.rs`, source_writer.rs
typed dispatch/receipt codec, tags selection/mutation helpers, schema repair/migration
for the versioned result if needed, and source writer integration tests. Coordinate
only minimal reexports in db/mod.rs with OPT-2037; no global-library schema or session edits
are needed for this first source-owned delta. Later OPT-2012 owns journal/vocabulary
reservation/checkpoint and cross-source finalization adapters. Acceptance must use
real source writer/SQLite revision and lease tests, fault injection, reopen readback,
payload conflicts, exact counts and unknown/capacity rejection. Contract fixtures
do not complete this production leaf, physical qualification or original GUI scope.

## Implemented selection prerequisite (OPT-2051)

`SourceWriter::capture_tag_assignment_selection` now reads the complete bounded
scope from the real source schema through the retained owner. The public
`SourceSnapshotOwner` adapter runs it on the sole worker with a two-second caller
budget, bounded queue and custody retained after timeout. SQL progress checks bound
query work; a synchronous writer call does not bound an OS identity-check stall.

The selection has private fields and no deserialization constructor. It contains
an owner-issued revision, source-local old tag ID, canonical identities and exact
sorted paths; it is an observation, **not** a mutation reservation or capacity grant.
Tag/manifest eligibility, row/byte overflow and stale owner fences reject the entire
read. SQL byte guards run before path strings are materialized. Atomic admitted
delta, receipt codec and durable replay remain OPT-2049; full journal definition
coordination remains OPT-2012. Selection tests cannot complete those scopes.
