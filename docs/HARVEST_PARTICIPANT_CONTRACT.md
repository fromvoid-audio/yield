# Harvest derivation and source checkpoint contract (OPT-2060)

This source-backed decision prepares OPT-1645. It supplies neither a production
Harvest command nor a private journal completion. The [participant ordering
contract](PARTICIPANT_CHECKPOINT_CONTRACT.md) and [I/O target](IO_ARCHITECTURE_TARGET.md)
retain their complete acceptance. Source/global/Harvest transactions are independent.

## Existing owners and missing bridges

| Actual boundary | What exists | What remains required |
| --- | --- | --- |
| `sample_sources/library/harvest.rs::record_harvest_derivation` | One transaction upserts parent/child Harvest rows, touches parent and inserts the graph edge in `library.db`. The path-only public wrapper is compatibility-only; no production producer calls it. | Bounded retained global owner command, content/source fences and immutable operation/work-key receipt. Retrying the old helper inserts another edge. |
| `SourceWriter::commit_scan_manifest_batch` | Actual retained source writer validates its scan fence and batch ownership, commits a bounded manifest result with structured delta/revision. | Immutable operation/work-key manifest receipt committed in the same source transaction. A returned `ManifestCommitResult` lacks that durable correlation. |
| `SourceWriter::commit_curation` / `SourceWriterReceipt` | Actual operation-scoped curation transaction/receipt. | Do not recast a curation commit as destination file registration, origin retirement or physical publication. |
| `SourceWriter::reference_read_fence` | Private writer-issued live ownership/lifecycle/revision read observation, implemented in PR #215. | A bounded actual source-file identity observation for parent metadata; a read fence itself is not a committed mutation checkpoint. |
| Global registry / reference owners, PR #196/#221 | Actual immutable configured-registry and single-key Harvest reference dispositions. | Bind each applicable receipt to the exact frozen global requirement. RegistryChanged, ReferenceChanged/Missing/DestinationOccupied do not mean successful required reconciliation. |
| `work_progress/dispatch_owner.rs` | Private permit/completion machinery; the fresh dispatch-owner constructor is currently test-only. Stored progress and receipt summaries are observations. | Production journal admission/dispatch and receipt adapters. Copying stored progress or opening private constructors is not an implementation of that bridge. |
| OPT-1787 / OPT-1634 | Existing open committed-mutation reconciliation/publication checkpoint requirements. | Preserve their full qualified physical predecessor and private completion acceptance. Do not duplicate them as a nominally completed metadata leaf. |
| OPT-2059 / merged PR #223 | Actual portable scanner implementation preparing bounded exact-entry facts for one retained source transaction. | Reuse this real preparation API for source metadata enrollment; do not duplicate scanner preparation or its shared files. It does not issue a universal operation receipt. |

The finite workspace search finds Harvest wrapper/database definitions and compatibility
tests, not a current source/session/UI derivation producer. Folder prefix remaps,
user-authored Harvest state (OPT-1831), projection/readiness, tags, artifacts and
family UI routing are separate. No session/action-register change is required here.

## Select the actual family responsibility

For **one copy/rendered derivation**, retain the parent identity and create one edge
to the committed destination identity. The origin file is preserved: an origin read
observation is appropriate; no removal/absence/retirement checkpoint is invented.
The destination source metadata receipt follows the family's verified publication.
The global registry requirement uses its actual applicable owner receipt. Reference
rekey is inapplicable for this family: copying must not transfer or rewrite the
origin's existing graph. This inapplicability is derived from the exact admitted
typed command/work plan, never a caller boolean, missing receipt or unavailable API.

For a **copy-then-remove move**, destination source commit precedes durable origin
removal intent, qualified absence and origin retirement; actual global rekey follows
both source outcomes. A pure move preserves existing derivations through PR #221;
it does not create a new copied edge merely to satisfy the frozen Harvest role.
An explicit owner-backed Harvest no-new-derivation disposition records the applicable
reference receipt and source checkpoint observations. An absent bridge is not
NotApplicable. The move's full original scope remains under OPT-1645/1358.

V1 handles exactly one derivation and one required frozen Harvest work obligation.
Use the admitted plan's key, not a fabricated action/step ordinal. Multi-output
commands need a later bounded manifest/aggregate contract; repeatedly changing the
payload under the same obligation is a replay conflict.

## Bounded source metadata preparation and correlation

Implement an independently useful portable source metadata command before Harvest:
consume genuine bounded scanner-prepared exact-entry facts through the retained
SourceWriter and existing source write transaction. Traversal/hash/file reads occur
before SQL; prepared facts retain/revalidate root/file capabilities, source database
binding and observed revision. Reuse OPT-2059. The command binds non-nil OperationId,
actual SourceAffect/DestinationSourceReconcile work key, bounded command family/schema,
exact canonical input and historical source/root/database/lifecycle/lease/revision.
No unrestricted path reopen, caller-supplied file identity, second writer or callback SQL.

Commit the exact manifest/index delta, its revision and one immutable command receipt
in the **same source transaction**. A receipt is privately issued/read back by the
source owner; no public constructor/deserializer converts DTOs into a live receipt.
Retain a bounded canonical observation codec for recovery, checking exact bytes/hash,
version, ambiguity and historical identity before returning it through current owners.
A current acquisition/live source context is separate from the immutable replay key.
Duplicate/lost-ack/restart replay must not create another delta or revision advance.
Origin retirement is a separate qualified-disposition command; this enrollment leaf
cannot delete a missing pathname or claim origin absence.

This receipt proves source **metadata enrollment**, not qualified publication. The
production family adapter under OPT-1787 must additionally verify the actual file-owner
publication checkpoint, exact plan/key/claim context and predecessor ordering before
creating a private source completion. Metadata tests do not waive that adapter.

The parent observation is issued by the actual source owner from a bounded exact file
row under a live reference fence: source-relative UTF-8 path, size/mtime/content identity,
root/database and audited revision. It must not be reconstructed from a path or arbitrary
Harvest DTO. Changed identity/revision or ended ownership fails closed. A rendered output
needs the family's actual validated output identity, rather than assuming a copy hash.

## Harvest command and durable observations

The retained GlobalLibraryWriter owns the existing `harvest_files` / `harvest_derivations`
mutation and a new immutable receipt ledger. No parallel graph store is added.
The request includes operation, required frozen Harvest work key, family/schema,
one exact parent/child identity, operation/range/output duration, inherited metadata,
source checkpoint observations, applicable global owner receipts and canonical payload.
The family retains durable proposal custody before dispatch and exact historical bytes
for restart; fresh source/profile contexts never rewrite historical receipt fields.

| Field / retained scope | V1 bound / policy |
| --- | --- |
| Source IDs, database/root bindings, tool/operation spelling | Nonempty, no NUL, at most 256 UTF-8 bytes; counters fit SQLite signed integers and lease epoch is positive. |
| Relative paths / root identity | StoredRelativePath validated nonempty portable UTF-8, at most 4096 bytes; opaque root identity nonempty, at most 4096 bytes. No lossy path conversion. |
| Content identity | Owner-issued exact content observation; size fits SQLite integer. No invented hash from size/mtime or ambient filesystem read. |
| Range/duration | Finite nonnegative seconds; start <= end. Encode exact finite IEEE-754 bits canonically; reject NaN/infinity and ambiguous noncanonical representation. |
| Inherited metadata | At most 64 tags, each at most 256 bytes, total at most 16 KiB, bounded canonical playback/rating policy. User intent is not inferred from copied bytes. |
| Canonical proposal / receipts | At most 64 KiB command, 8192 retained rows / 16 MiB all retained field bytes and scalar overhead. Exact replay precedes capacity; no implicit eviction. |
| Mutation | One edge, at most two Harvest file rows; preserve independent authored Done/Ignored state and reject identity collision instead of silently merging unrelated state. Snapshot bounds precede Rust materialization. |

The receipt key is `(OperationId, frozen Harvest WorkUnitKey)`; derivation identity
binds exact endpoints, family and bounded payload under that key. In one transaction:
validate current owners; read existing receipt first; compare exact canonical input;
check finite retention budget and prerequisite owner evidence; compare complete bounded
file/edge preconditions; apply one edge/file delta or retain an explicit rejection;
recheck source contexts; insert and read back immutable disposition/edge ID/source
checkpoint observations; commit. A lost acknowledgement returns the original edge ID,
revision observations and disposition without reinserting or touching later user state.
SQL failure rolls back delta and receipt. Post-commit ownership failure reports uncertainty.

The global worker reads applicable registry/reference receipt evidence from its own
retained database and compares exact identities/dispositions; public receipt field copies
alone are not sufficient. New mutation requires actual source-issued observations and
current live fences. Existing replay retains historical source revisions while requiring
fresh contexts matching the same source/root/database scope. Validate stored lengths,
types, versions, exact bytes and result hash; malformed/ambiguous/conflicting evidence
fails closed. Rejection cannot advance the required Harvest checkpoint.

For the copy branch, the global prerequisite adapter derives no-rekey eligibility from
actual admitted copy/render command context. For the move branch, require Applied rekey
with exact endpoints and source observations. Do not reuse a rejection as no-effect
success. Global ledger sequences are ledger order, not source revisions. No global
receipt or newly stored source observation is itself VerifiedStepCompletion.

## Concrete implementation sequence and verification

1. OPT-2063 / OPT-2064, source owner: bounded parent identity observation plus operation-scoped exact-entry
   metadata enrollment/immutable receipt, reusing actual OPT-2059 scanner preparation.
   Real retained source DB/filesystem tests cover bounds, capability/path replacement,
   revision/lifecycle changes, SQL rollback, duplicate/lost ack/reopen and one delta/revision.
2. OPT-2065, global Harvest owner: one bounded derivation/file delta and immutable receipt/source
   observations, consuming those genuine source outputs and current live contexts.
   Actual global/source owner tests cover collision, stale source, prerequisite mismatch,
   exact/conflicting/restart replay, one edge ID, rollback and retention bounds.
3. Production source/global/Harvest journal adapters: bind exact admitted plan and entered
   permits to actual publication/source/global/Harvest owner results. Copy and move use
   their real predecessor chains. Preserve OPT-1787/1634 and move intent/absence/retirement
   requirements. Test receipt mismatch, persistence uncertainty, cancellation/quiescence
   and recovery without public summary -> private completion conversion.
4. Native qualified publication/disposition and physical family qualification remain in
   their exact platform issues. These are external resource gates for executing that
   physical step, not reasons to omit portable ownership/transaction/admission code.

OPT-2060 fixtures exercise only the decision matrix below; they are synthetic
contract evidence. They do not implement these leaves, prove a production commit or
qualify physical behavior. OPT-1645/1341 and other tracking parents remain open until
their complete implemented/qualified scope is fulfilled.
