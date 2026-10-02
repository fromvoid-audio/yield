# Source-owned Starmap layout publication

OPT-2130 resolves the portable ownership decision blocking OPT-1709. Checked
against Yield main `a9e198bac`, 2 October 2026. The commands and storage below
are requirements for implementation, not existing APIs or production readiness.
OPT-1709/1485 retain persisted rendering, navigation, selection, native input,
audition and large-map acceptance. This decision does not complete those parents.

## Existing boundaries

| Current source | Reuse and limitation |
| --- | --- |
| [Feature manifest owner](../crates/yield-library/src/sample_sources/db/write/readiness_targets.rs) | An audited manifest command already bounds 256 files and 128 KiB of identities. It deliberately leaves embedding/layout unsupported. A successful file feature job cannot supply layout readiness. |
| [Feature payload](../crates/yield-library/src/sample_sources/readiness/feature_artifact.rs) | Exactly 183 finite little-endian f32 values, 732 bytes; a private owner-issued key binds current target and digest. Lookup revalidates the target. OPT-2118/PR280 fixed both mutation-capable getters and nested lease validation; retain that strict read-only behavior. |
| [Read observation](../crates/yield-library/src/sample_sources/db/source_reference_fence.rs) | Nonserializable, writer-instance-scoped liveness and exact audited revision. Validation is read-only and point-in-time; it is not a mutation grant and belongs on a worker. |
| [Embedding](../crates/yield-analysis/src/analysis/similarity.rs), [projection](../crates/yield-analysis/src/analysis/umap/mod.rs), [clustering](../crates/yield-analysis/src/analysis/hdbscan/mod.rs) | Existing `features_v1__len183__l2`, PCA/t-SNE and HDBSCAN algorithms. Extract a bounded pure entrypoint; do not replace these engines or require external model weights. |
| [Compatibility publisher](../crates/yield-analysis/src/analysis/similarity_artifacts.rs) | Combines SQL with ambient-path ANN publication. Its supplied connection/fence and pathname-derived containers are not authority for a new production caller. Do not invoke its publication path. |
| [Read DTO](../crates/yield-library/src/sample_sources/db/starmap_layout.rs) | OPT-1860 supplies finite normalized x/y and optional cluster IDs, bounded to 20,000 requested samples/32 MiB identities. Matching revision and seeded legacy rows alone do not prove current production layout. |
| [Legacy schema](../crates/yield-library/src/sample_sources/db/schema/ddl.rs) | `layout_umap` has a sample-only primary key and references legacy `samples`; clusters also reference `samples`. These rows have no new owner receipt. Do not fabricate or overwrite compatibility sample rows to satisfy those foreign keys. |

## Owner and exact identity

Use the existing catalog worker and retained `SourceWriter`, never a second source
writer or a writable background-reader connection. Capture source ID, media-root
identity, root binding, database identity, writer incarnation, lifecycle, lease
epoch and audited source revision. A caller-supplied source ID or scalar revision
cannot substitute for this full owner context.

The owner captures the **complete** currently supported, non-missing audio
manifest, ordered by exact relative path. Read at most 257 rows; observing the
257th rejects the initial bounded producer. Validate SQL scalar types and byte
lengths before copying values. Retain relative path, file identity, full current
BLAKE3 content hash, size/mtime facts, exact AnalysisFeatures target/version, and
the validated feature key/reference for every row. Reject duplicate identities,
aliases, malformed/foreign targets and unsupported or missing feature payloads.
Do not silently omit a file or publish a partial set as a source layout.

Reuse strict current-owner feature lookup, after OPT-2118's read-only fix. Capture
does not publish a readiness matrix, renew a lease, enqueue work or repair bytes.
Return a bounded prerequisite reason when features are absent/corrupt/unsupported.
The feature adapter remains responsible for actual missing feature jobs. An empty
manifest returns Empty, not an invented point or successful layout.

The capture is a private owner-produced value containing immutable validated
payloads, exact targets and a weak issuer token/read fence. It cannot be publicly
constructed or deserialized into authority. A CPU input projection may expose
bounded feature bytes and sample identities, but carries no SQL connection, root
handle, lease renewal or mutation capability. Retain the owner capture until the
CPU result is accepted or rejected.

Use a versioned, length-prefixed input digest over source identity, ordered
path/file/content/feature-key identities, feature algorithm, embedding model,
projection version, fixed seed and cluster parameters. Ownership incarnation is
checked separately rather than making reusable content identity a capability.
The canonical sample identity remains exact `source_id::relative_path`; browser
file identity is returned separately. Never sanitize paths into identities or
infer graph links.

## Bounded computation and custody

| Boundary | Initial enforced maximum |
| --- | --- |
| Complete manifest | 256 rows, each path <=1,024 bytes, file identity <=512 bytes, full hash 64 lowercase hex bytes; aggregate manifest identities <=128 KiB |
| Captured input | 1 MiB including all owned identities, targets, references and the 732-byte payload per row; checked arithmetic before copying/queueing |
| Prepared persisted generation | 512 KiB canonical encoded bytes, including identity/point/cluster metadata; reject a conservative encoding upper bound before allocating, then check actual bytes |
| Active stored generations | One fixed metadata key per physical source database; no unbounded generation history or source-global cache |
| Catalog custody | One outstanding layout request per source; aggregate input/result credits reserved before admission, retained through reply custody |
| Owner SQL/capture | Two-second cooperative deadline including queue wait; bounded rows and SQLite progress checks, not a promise to interrupt an OS call |

Compute outside every source transaction using existing L2 embedding and
PCA/t-SNE (seed 0), then the existing deterministic HDBSCAN label mapping. Require
exactly one finite point for every captured identity, not the compatibility
helper's 95% coverage threshold. Zero/nonfinite embeddings or computation errors
return Unavailable and retain the previous generation. One/two-point projection
uses the existing engine behavior; cluster metadata is absent for fewer than five
points. For larger sets use fixed `min_cluster_size=5`, `min_samples=None`,
`allow_single_cluster=false`; bind this policy into the algorithm version/digest.
No made-up cluster ID or fabricated fallback layout is allowed.

The pure entrypoint validates every vector, identity and parameter before engine
allocation. Its implementation must document checked matrix-size bounds and
measure the maximum 256-row workload; the input cap alone is not an asserted peak
memory measurement. Check cancellation before/after each existing engine stage.
The current engines do not offer mid-call cancellation; keep CPU admission and
the capture in worker custody until that call returns. Cancellation, timeout or
dropped UI reply cannot cause detached publication or optimistic Ready. Native
input/model weights are not prerequisites for computation over stored features.

## SQL publication and admission

Choose one versioned canonical generation in the existing source `metadata`
namespace, under reserved key `source_starmap_generation_v1`. This is the sole
authority for the new layout path. It contains bounded exact membership/feature
references, raw finite points, optional cluster labels, input/point digests,
model/version/parameters and observed source revision. It is a source-local
derived artifact, not another semantic manifest or an ANN artifact. The reader
reuses OPT-1860's normalized DTO meaning. Legacy `samples`, `layout_umap`, clusters,
embeddings, ANN paths and `similarity_artifact_state_v1` remain compatibility data;
do not modify them or adopt their rows as new production evidence.

The owner validates a prepared result against its retained capture, current
complete membership and exact feature keys. Recheck the full physical owner,
issuer incarnation, revision, target content/version and cancellation at the
effect boundary. CPU output bytes, serialized keys or a historical receipt cannot
acquire that authority. Unknown/corrupt existing generation evidence is retained
and reported for audit; do not overwrite it with a nominal successful rebuild.

Before any publication, implement a layout-specific finite DB/WAL/SHM budget
and entered SQLite policy. Reuse retained-descriptor measurement and known-schema
validation patterns from [tag budget](../crates/yield-library/src/sample_sources/db/write/source_writer/tag_metadata_budget.rs)
and [policy](../crates/yield-library/src/sample_sources/db/write/source_writer/tag_metadata_policy.rs),
without borrowing a tag observation as layout permission. Account for the old
and new generation, current database pages, indexes, rollback/commit work and
resident WAL/SHM. Enforce the measured maximum pages/temp/cache-spill policy;
counting rows or bounding the JSON is not a physical allocation claim. Unknown,
overflowing, over-budget or unsupported measurements reject before effects.

Use genuine owner-issued capacity admission for the complete layout SQL growth
plan and actual retained-source WAL reservation (OPT-2133). Choose an explicit
bounded **source-derived-layout** journal command, not a filesystem command or a
tag operation with different acceptance. This new family is implementation work:
there is currently no matching `UniversalCommandKind` or production admission
route. Its immutable prepared command contains the bounded generation bytes,
expected prior-generation digest, exact source/database/membership identities and
complete DB/WAL/SHM allocation plan. Admit durable intent only after the actual
participant preparation and journal latch, under the
[participant admission contract](WAL_PARTICIPANT_ADMISSION_CONTRACT.md). Record
one source SQL participant, its actual result and durable claim settlement. The
payload is a rebuildable layout, not a user metadata edit or undo transaction.

A local reservation or layout budget alone is not that durable intent, capacity
claim, recovery exception or permission above hard watermarks. Routine layout
publication stays subject to ordinary WAL admission. Recovery may reconcile an
already committed exact generation and settle its claims; it cannot use a
required-work exception to start a fresh rebuild. Unknown commit/readback keeps
the same operation and claims charged until the actual recovery owner resolves
them. Missing family, latch, capacity or recovery integration returns explicitly
unavailable before effects. Keep these actual outputs as production prerequisites;
read-only capture and pure computation are independently executable.

Within one bounded immediate transaction on the retained connection: validate
the admission and capture, compare the exact previous generation, install the
new canonical generation, recheck required fences/cancellation, then commit.
No CPU/DSP/file traversal runs in that transaction. Do not bump semantic source
revision or replace authoritative audit/readiness state for a derived layout.
Finish/restoration of the entered policy follows confirmed transaction quiescence;
failed rollback/restoration makes that owner unavailable until reconciliation.

Same input and byte-identical retained generation returns Duplicate after current
validation. Same input digest with conflicting point/cluster bytes is an audit
conflict, never a last-writer overwrite. A changed valid complete membership may
replace a verified old generation by exact CAS. A rollback leaves the old value
unchanged and issues no publication receipt. Commit with an uncertain reply or
policy-restoration failure remains Unconfirmed until the owner reads back and
validates the exact committed generation. Do not retry CPU/publication blindly.
An unrelated semantic revision or owner reacquisition does not rewrite immutable
generation bytes. The stored origin revision is historical; a new read observation
binds the current audited revision only after exact membership/feature validation.
Identical input/point/cluster content keeps the verified stored generation and
returns a freshly validated duplicate observation without a metadata write.

Issue a private layout-only publication observation after confirmed commit or
exact validated duplicate. Do not set EmbeddingAspects, SimilarityLayout, ANN or
whole-source readiness: their current full-bundle contract remains unsupported.
Original source-wide readiness requires its separate consumer/version transition.

## Restart, read and presentation

Persisted generation bytes and their digests are observations, not capabilities.
After restart, the newly audited retained writer must recapture current complete
membership and feature keys and validate the stored version, source identity,
row count, digests and every finite coordinate/cluster before issuing a new read
observation. Never deserialize the previous incarnation/lease into liveness.
Stale membership/content/version returns Stale; absent evidence returns Missing;
malformed/unknown evidence remains AuditRequired. Lookup is read-only and cannot
renew near-expiry or expired ownership.

Add an explicit owner-generation background read path returning the existing
normalized point/optional-cluster DTO. The legacy loader remains a compatibility
read, not a fallback that upgrades unverified rows to production Current. Read the
fixed bounded metadata value only after SQL type/length predicates; validate and
filter its exact membership in one read transaction. Compare the expected
generation and source revision inside that transaction, and the nonserializable
owner/read observation before and after it. No second writable open is allowed.
Preserve reference normalization and truthful Missing/Stale/partial reporting.
The new observation proves layout availability only. A Starmap consumer may show
those validated points while overall analysis/ANN remains incomplete; it must not
use the DTO's `Current` variant to upgrade that larger readiness contract.

Catalog results bind source/root/database/lifecycle/lease/revision, generation,
browser request signature and exact file/sample set. Perform SQL/lease/read-fence
validation on a worker. GPUI performs cheap captured-context comparisons before
installing a result; render cannot synchronously query the owner. A source/context
change invalidates old points and selection. New selection/actions still require
their own current identity/capability checks under OPT-1729. A successful read is
FreshAsOf the worker observation, never continuous physical liveness.

## Required implementation leaves and validation

1. **[OPT-2151: Actual retained-owner capture](https://linear.app/boostnlvp/issue/OPT-2151):** complete bounded manifest, strict current
   feature lookup, private capture/digests and cancellation. Depend on verified
   OPT-2118 read-only getter correction. Test real owner/SQLite feature publication
   and lookup, empty/257th/byte limits, alias/malformed/missing/corrupt rows, changed
   source/content/version, owner handoff, near-expiry and expired lease, no SQL or
   control-lease writes, and restart under a newly issued capture.
2. **[OPT-2152: Existing-engine computation](https://linear.app/boostnlvp/issue/OPT-2152):** bounded pure helper reusing L2/PCA/t-SNE and
   HDBSCAN with fixed version/parameters; exact finite coverage and cancellation
   custody. Test 0/1/2 and representative multi-point data, bad vectors, ordering,
   determinism, bounds and the measured maximum-size workload. This helper alone
   cannot complete production publication or OPT-1709.
3. **[OPT-2153: Layout allocation observation/policy](https://linear.app/boostnlvp/issue/OPT-2153):** actual retained DB/WAL/SHM planning
   and enforceable layout-specific SQLite limits on the same connection. This
   issues no claim or write grant. Test known/unknown schema and descriptors,
   maximum bytes/pages, sidecars, arithmetic, lease/owner changes and restoration.
4. **[OPT-2154: Derived-layout command](https://linear.app/boostnlvp/issue/OPT-2154):** explicit bounded immutable family/codec
   and durable validation/scanner foundation. Depend on the actual capture and
   preserve unknown/stale/corrupt command evidence. Model/codec completion alone
   does not supply admission, an operation ID or source publication.
5. **[OPT-2155: Admitted atomic store](https://linear.app/boostnlvp/issue/OPT-2155):** genuine capacity/WAL preparation/latch,
   generation CAS/readback, private receipt and durable settlement. Depend on
   capture/computation/policy/command
   and actual reservation/admission outputs, not just their contract issues. Test
   real SQLite rollback, duplicate/conflict, stale membership/owner, every
   allocation failure, cancellation, unconfirmed commit/policy restoration and
   restart under the same durable operation. Keep required recovery/physical
   qualification open until its exact prerequisites.
6. **[OPT-2156: Owner-generation read](https://linear.app/boostnlvp/issue/OPT-2156):** restart validation and actual background
   persisted DTO read; no lease renewal or legacy fixture substitution. Test real
   stored generations, missing/stale/corrupt evidence and normalized points/clusters.
7. **[OPT-2157: Catalog consumer](https://linear.app/boostnlvp/issue/OPT-2157):** actual bounded production dispatch/custody,
   public facade, queue/result limits, cancellation, late/duplicate replies and
   changed context using the real producer/read outputs.
8. **OPT-1709 renderer and existing siblings:** representative actual persisted
   generation through the production read path and GPUI render state. Preserve
   pan/zoom, stale selection, overlay/audition, native input and large-map tests.
   The initial 256-row producer does not satisfy OPT-1730 large-map acceptance;
   its scalable producer prerequisite must remain tracked before that workload.

Run focused actual-owner/engine/read/catalog/UI tests as each leaf lands, locked
application compile, formatting and canonical architecture guards. Rebase and
rerun affected checks, compare exact tested/merged contents, then refresh only
fulfilled blockers. Native/manual acceptance remains in its scoped platform
issues. Missing portable admission, ownership or producer code is work to resolve,
not a Linux platform blocker. This document and contract fixtures supply none of
those production or physical passes.
