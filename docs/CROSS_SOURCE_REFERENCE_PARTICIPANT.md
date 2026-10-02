# Cross-source Harvest reference rekey (OPT-2032)

## Required delta and finite caller evidence

The selected delta is **one Harvest file key and every existing graph endpoint that
names it → one new source-relative file key**. Existing `library.db` tables
`harvest_files` and `harvest_derivations` own this metadata. The retained
`GlobalLibraryWriter` must execute it. It neither moves bytes nor creates a derivation.

| Requirement / current code | Finding | Resulting scope |
| --- | --- | --- |
| `IO_ARCHITECTURE_TARGET.md`, cross-source ordering step 7 and origin-retirement failure matrix | After destination commit, origin absence and origin retirement, global/Harvest references must be rekeyed before successful completion. | Concrete future consumer requirement, preserved under OPT-1341/1358 and OPT-1585. No physical checkpoint is supplied by this decision. |
| `library.rs::remap_harvest_file_key`, `library/harvest.rs::remap_harvest_file_key_on_transaction` | Existing transaction merges old file state into new key, deletes old row, and changes incoming/outgoing edge endpoints. The public wrapper requires a path-only library open, rejected in ordinary builds. | Reuse this exact metadata responsibility with an explicit owner command, bounds, content fence and immutable receipt under OPT-1643. |
| Active workspace search for `remap_harvest_file_key` / `remap_harvest_file_prefix` | Definitions, wrapper-to-database calls and compatibility unit tests only; no catalog/session/UI production producer. | State the absent producer honestly. Storage implementation does not wire a move family or qualify its required origin/destination checkpoints. |
| Existing file-move remap fixture | Exercises source-a → source-b child endpoint and preserves parent, operation and inherited metadata. | Existing-API evidence of the selected delta; not production owner evidence. |
| Harvest derivation insertion and state reads | Separate legacy compatibility APIs, no current global owner route. | OPT-1645/1831 retain new derivation/checkpoint and user state work. This leaf only rekeys existing references. |
| Source-local readiness/ANN | Active source-local identity/generation storage; global legacy analysis/index tables do not imply an active global reference producer. | Do not revive canceled OPT-1830, move analysis to global storage, or manufacture unrelated reference mutations. |
| Operation-link correlations (PR #204/#207) | Link source command receipt to global work key; no Harvest mutation. | Distinct from this reference rekey; cannot substitute for it. |

The finite search does not claim every future producer exists, or erase the target's
required rekey. The old generic “global index reference” assumption is obsolete; the
concrete Harvest graph requirement is not. Folder-prefix remap is excluded because
it enumerates an unbounded set of keys and needs its own bounded manifest contract.

## One bounded transaction and immutable identity

V1 accepts a non-nil `OperationId`, the admitted plan's required `GlobalLibrary`
work key, old and new file keys, historical origin/destination source observations,
an expected bounded reference snapshot and a current profile acquisition fence.
One reference disposition per `(OperationId, WorkUnitKey)` is retained. A second
rekey under that key with different endpoints, source observations or expected
snapshot is a conflict. Multiple files need distinct frozen work obligations or a
later bounded manifest version, not locally fabricated key ordinals.

Keys use `StoredRelativePath`'s validated UTF-8 source-relative representation
(nonempty, no traversal/absolute paths), at most 4096 bytes each; source IDs are
nonempty, at most 256 bytes. V1 requires different source IDs and distinct keys.
No lossy `Path::to_string_lossy` conversion is allowed in this command. Source/root,
database, lifecycle and committed-revision observations must bind to each endpoint's
actual source owner; opaque root bytes are nonempty and at most 4096 bytes, database
and root binding strings at most 256 bytes, positive lease epochs and all counters
fit signed SQLite integers. Versioned canonical input is at most 64 KiB.

The existing compatibility helper silently merges a destination row. V1 instead
rejects an occupied destination file key or any destination endpoint reference;
this preserves independently authored state until an explicit merge contract exists.
It retains `DestinationOccupied` as the immutable disposition. A missing old file
row is allowed if existing incoming/outgoing edges name it; no old state or endpoint
means retained `ReferenceMissing`, never inferred filesystem absence.

The existing global owner supplies a bounded snapshot read command: at most one old
file row, destination occupancy and 64 distinct affected edge rows (`LIMIT 65` rejects
over-budget scope). Check text/blob lengths before materializing each field and enforce
a 64 KiB combined canonical snapshot limit. Fingerprint exact file state and complete
incoming/outgoing edge contents in stable row-ID order; an edge mentioning the old key
at both ends is counted once. Include destination occupancy. Preserve edge IDs,
content hints, operations, ranges and inherited metadata. Snapshot token is content
CAS, not a monotonic source revision or ownership grant.

Transaction order: validate current profile and source contexts, start one SQLite
transaction, read existing immutable receipt first, compare exact canonical input for
replay, enforce retained ledger budgets, read/check the bounded current snapshot,
then either retain rejection (`ReferenceChanged`, `ReferenceMissing`,
`DestinationOccupied`) or perform exactly the old-row transfer and endpoint updates.
Recheck source contexts before commit, insert and read back the exact result in that
same transaction, then commit. SQL failure rolls back both mutation and receipt.
A lost acknowledgement replays the original disposition, even if old keys are later
reused or current metadata changes. Never infer replay from “old row absent”.

Retain at most 8192 receipts / 16 MiB canonical receipt/input bytes; exhaustion rejects
new commands without evicting evidence. Exact replay remains available. Check stored
lengths, versions, row ambiguity, canonical bytes and versioned hash before returning
any receipt. Receipt sequence is ledger order, not a physical source revision.
Reclamation requires a proven journal retention contract.

## Ownership and source fencing are real implementation work

`StoredSourceFence`, `SourceRevision`, `SourceWriterReceipt` and `HarvestFileIdentity`
are observations; none alone grants live source ownership. Existing
`SourceDatabase::source_publication_fence` supplies a retained source lease/root/database
check, but its public `validate` explicitly does **not** check lifecycle or revision.
Do not pass it off as the missing complete source context.

Before production rekey, implement a source-owner-issued reference read fence. It must
bind a current `SourceWriter` instance/lifecycle, its actual source identity/root/database
and revision, and the existing retained `SourcePublicationFence`. No public constructor
from DTOs, root paths or UUID spellings. A fence from a closed/replaced writer, changed
revision, dropped/expired/replaced lease/root/database or different endpoint must fail
closed. Read-only revision checks use the retained source control connection, never a
pathname reopen or lease renewal. A writer-specific weak liveness token prevents an
old catalog lifecycle remaining valid merely because its database/lease is reused.

The global worker validates both current live fences around new mutation; source
revision races retain uncertainty and cannot advance the journal. A source/global
transaction pair is not atomic. Family ownership must serialize the admitted source
work and establish its required durable checkpoints. Immutable replay payload retains
historical source observations; restart readback uses freshly reacquired live contexts
matching source/root/database scope, without rewriting historical receipt fields.
Exact replay is an observation of a prior commit, not a new source mutation grant.
The profile acquisition equality fence is similarly live context and excluded from
immutable payload comparison, as in PR #196/#204.

The preparation API and actual global rekey are portable implementation prerequisites,
not native tests. Split them into concrete implementation leaves only to coordinate
source-writer edits with OPT-2034/2012 and global owner edits with OPT-2031/2045.
No unsupported/reject-only owner can complete OPT-1643.

## Results, tests and remaining implementation

| Outcome | Required behavior |
| --- | --- |
| `Applied(ReferenceReceipt)` | Old state/endpoints rekey and exact immutable result commit together. Correlation/metadata observation only. |
| `ReferenceChanged` | Current complete snapshot differs; only rejection receipt commits. |
| `ReferenceMissing` / `DestinationOccupied` | No silent merge or invented source absence; retain result permanently for exact replay. |
| `Replayed(ReferenceReceipt)` | Exact prior canonical command returns original result without touching later references. |
| `InvalidInput` / `StaleSource` / `StaleProfile` | No new mutation; invalid bounds fail before queue admission. |
| `ReplayConflict` / malformed or ambiguous evidence | Preserve existing row and fail closed. |
| SQL failure / capacity / post-commit ownership uncertainty | Keep global checkpoint pending; recover through current owners and exact durable identity. |

OPT-1643 implements source preparation/readback fencing plus the global bounded snapshot,
rekey command and immutable receipt. Intended source files: an isolated source-reference
fence module and tests, minimal `SourceWriter` issuance/liveness integration, retained
lease control revision validation and reexports. Intended global files: new
`library/reference_participant.rs` / isolated owner tests; coordinated `owner.rs`,
`error.rs`, `schema_defs.rs`, `library.rs`, and a transaction-local Harvest rekey helper.
Do not bypass current owner/lease/profile boundaries, copy the whole graph into a
parallel store, or change source-local readiness/ANN. No session/action-register edits
are required for this storage scope.

Required production tests: actual source-owner fence lifecycle/drop/changed revision;
exact snapshot bounds and malformed data; stale source/profile; destination collision;
missing/changed reference retained rejection; duplicate/lost response/reopen/fresh live
context; changed payload under same work key; old-key reuse after commit; self-endpoint
edge counted once; SQL rollback; row/byte capacity preserving replay. Then admitted
family consumers establish destination/origin checkpoints and private journal context
under OPT-1641; Harvest derivations, projection/readiness and physical qualification
retain their full original acceptance.

The compatibility fixture below demonstrates the current helper's lost-result and
old-key-reuse hazard. It does not implement the proposed receipt or live source fences.
Linux can build and test all the preparation/global storage work. Native owner and
filesystem qualification needs scoped macOS/Windows issues after concrete code exists.

## Source read-fence implementation (OPT-2050)

`SourceWriter::reference_read_fence` issues `SourceReferenceFence` from its retained
source/profile/database ownership and a bounded audit read through the existing source
lease control connection. A private writer-instance weak token ends the observation on
handoff/close even if another lifecycle reuses the same database/lease. Metadata row,
key and value bounds precede allocation; a single read captures current revision and
complete-audit root/revision, rejects ambiguity/malformed values, and checks the physical
root and retained ownership before and after readback. Validation neither renews the
lease nor reopens a path. Current-revision equality and owner liveness remain required
at each consuming boundary. The source ID and lifecycle are issued by the writer;
this does not invent a durable source-ID metadata row or grant file effects.

Actual owner tests cover live issuance, source-owner curation changing the revision,
writer handoff/new lifecycle with a reused lease, no renewal, oversized/ambiguous metadata,
expired lease and portable root/database replacement. Native regression remains separate.
This prepares OPT-1643; its global snapshot/rekey/immutable receipt is still required.

## Production global owner implementation (OPT-1643)

`GlobalLibraryWriter::reference_snapshot` reads the complete bounded Harvest state in
one retained-connection transaction. SQLite scalar type/byte-length predicates precede
field selection; typed streaming fingerprinting enforces the aggregate 64 KiB budget.
`ReferenceRekeyRequest` binds that snapshot and actual source-owner fences to the frozen
global work key. Callers retain `proposal_bytes()` durably before dispatch and restore
the exact canonical historical proposal with fresh current source contexts after restart.
New effects require exact historical/current observations; existing receipt replay permits
fresh lifecycle/lease/revision observations only within the same source/root/database scope.

`apply_reference_rekey` rejects destination occupancy and preserves exact file metadata
by updating only key columns, plus all incoming/outgoing endpoint columns. It retains
Applied/ReferenceChanged/ReferenceMissing/DestinationOccupied in `reference_receipts`,
with a versioned digest covering both canonical input and disposition. Rejections commit
without a rekey. Transaction failures roll back both metadata and receipt. The 8192-row /
16 MiB budget accounts for all retained command/receipt fields and scalar overhead; no
implicit eviction occurs. Exact replay precedes capacity and current snapshot checks.
Source validation surrounds execution, commit and reply; a failed post-commit check reports
uncertainty and requires exact receipt recovery. The existing profile owner boundary also
checks before execution and acknowledgement.

Ten actual source/global owner tests cover duplicate/lost acknowledgement/reopen with
fresh contexts, old-key reuse, complete incoming/outgoing/self-endpoint content, edge-only
state, missing/changed/colliding retained dispositions, bounded/corrupt/ambiguous evidence,
changed revision and ended source ownership, post-commit lifecycle uncertainty, SQL rollback,
and row/byte capacity retaining exact replay. Seeded Harvest rows are storage test inputs;
these tests do not move files or qualify production physical workflows. The source fence
regressions and existing global owner suite remain required alongside these focused tests.

The admitted family producer, durable proposal custody and private checkpoint bridge remain
consumer work under OPT-1341/1358; no UI/session move route is supplied here. Folder-prefix
manifests, new derivation/state commands, projection/readiness and native filesystem or
power-loss qualification remain separate. SQLite retains its existing NORMAL synchronous
policy; process reopen evidence is not sudden-power-loss qualification. Native retained-owner
regressions are tracked in OPT-2042 (macOS) and OPT-2043 (Windows).
