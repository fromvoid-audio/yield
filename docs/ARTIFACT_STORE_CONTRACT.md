# Artifact identity and owner commands (OPT-1655)

Decision against current main `6a7e25282`, 2 October 2026. This is an interface
decision and caller inventory, not a live artifact backend or physical
qualification. OPT-1343 remains open. The first production implementation is
one bounded source-owned AnalysisFeatures payload; external container publication
and reverse ownership remain separate requirements.

## Current callers and evidence

| Caller | Existing implementation | Missing boundary |
| --- | --- | --- |
| Catalog readiness commands | `SourceWriter` claims, renews, completes, fails, releases and cancels readiness work on its retained database. Claims retain physical-source publication fences and exact target/claim generation. | `complete_readiness_work(..., artifact_ref)` accepts an optional string; the string does not prove durable payload publication. OPT-1922 must use a typed owner-issued publication result. |
| Readiness persistence | `ReadinessArtifact` contains source/scope/stage/version/content/source generation. `complete_with_artifact_ref` atomically updates the work/completion reference. | No capability-bound payload lookup follows from a completion marker. Keep eligibility, processing truth and optional cache residency distinct. |
| Existing feature tables | Source schema has `analysis_features`, `features` and `analysis_cache_features` payloads keyed by sample/content with version fields. | None provides this immutable owner-issued key/receipt and atomic claim completion interface. Reuse existing consumers through explicit validated adoption; do not treat a legacy row as a new publication receipt or silently add another authoritative cache. |
| Similarity helper | `yield-analysis::rebuild_exact_similarity_artifacts` computes layout/clusters and publishes exact ANN metadata and SQL rows behind a caller fence. | Supplied connection/fence is a compatibility API, not an active catalog feature-job consumer or ArtifactStore owner. |
| ANN storage | `ann_index/storage.rs` uses `PRAGMA database_list`, pathname-derived generation containers, path cleanup, and an ambient app-root fallback. | Never adopt those paths as authority for new production workers. Sanitizing/truncating generation text to 32 characters can collide; it is not an ArtifactKey encoding. |
| Cache maintenance | `WritableProfileGuard::capture_disposable_cache_namespace` captures a no-follow cache root; maintenance rechecks profile/root identities, cancellation, traversal depth and source/trash overlap. | A captured clearing namespace cannot grant artifact publication or source readiness authority. OPT-2040 removed the unused ambient clear helper. Windows qualification remains OPT-2044. |
| Waveform/preview caches | Cache paths and in-memory results are rebuildable derived observations. | No active common ArtifactStore command owner was found. Future adoption must name exact consumers; do not invent a global writer from dormant tables. |

## Identity is separate from authority

`ArtifactKeyV1` is an immutable value containing kind, format version, algorithm
version, exact source ID, file/scope identity, content generation and payload
BLAKE3 digest. Source-scoped aggregates additionally bind an exact membership
digest. Canonical serialization is versioned and length-prefixed; reject empty,
oversized or invalid fields, unsupported versions and non-finite numeric payloads.
Never sanitize identifiers into filenames or use a lossy pathname as a key.
The key carries no writable root, current lease, source revision or permission.

Only a retained owner issues `ArtifactObservation`/`ArtifactPublicationReceipt`.
They bind the exact key, owner acquisition, source lifecycle, current content
target, payload digest/length and backend identity. Fields that assert authority
are private. A serialized key, stale receipt, readiness row or operation-link
record cannot reacquire a capability. Durable observations are revalidated after
restart under the new owner before use.

File-scoped content identity remains reusable across unrelated manifest changes;
the command still requires a current source revision and current exact target.
An old source epoch, changed content/version or expired/reclaimed claim rejects
publication. Do not turn harmless source-wide revision movement into proof that
old content is still current.

## Bounded owner command interface

All commands execute behind a bounded background owner queue, outside GPUI
rendering. Reuse the catalog worker/retained SourceWriter for source-local SQL;
do not start another writable source connection. Admission reserves both input
and result bytes before queueing. Cancellation and dropped replies retain an
observable outcome until custody; queue failure is not publication success.

| Command | Required admission | Outcome and ordering |
| --- | --- | --- |
| Publish | Current revision, exact owned readiness claim, typed key and immutable validated payload; external bytes additionally need held staging/root and qualified no-replace effect evidence. | Same-key/same-payload retry returns the existing exact result; conflicting payload fails without overwrite. Completion is atomic with SQL payload insertion, or follows a durable external effect/adoption protocol. |
| Lookup | Current retained owner and exact key/current target. | Bounded validated payload/held read handle, Missing, Invalid, Stale or Unavailable. Lookup never marks work ready or rebuilds synchronously. |
| Invalidate | Exact key, expected observation and typed corruption/version/content reason. | Remove unusable availability and queue/reconcile affected derived work under the source owner; do not delete manifest, metadata, user tags or source membership. |
| Retire | Source-owned retirement/checkpoint disposition and bounded exact reverse-owner set. | Remove only this source's associations. Configured-registry removal is not physical absence or proof that no other owner references bytes. OPT-1656 owns the reverse-ownership consumer. |
| Prune | Exact captured cache namespace/backend, bounded page and current unreferenced observation. | Remove only rebuildable unreferenced bytes. Revalidate before effects, report partial/canceled/uncertain results, preserve semantic source truth. OPT-1657 owns eviction/readiness behavior. |
| Rebuild | Current target/key version and source-owned readiness scheduling admission. | Queue typed work with bounded retry/terminal diagnostics. CPU, decoding and file traversal occur outside the source transaction; no optimistic ready state. |

## First executable backend: source-local feature payload

OPT-2052 implements the store primitive; OPT-1922 implements the real worker.
For OPT-1922's one file-scoped AnalysisFeatures job, choose an immutable bounded
SQLite payload in the existing physical source database. This avoids a second
file-publication saga for a 183-element v1 vector; it does not replace the
external-container requirements for ANN, waveform or selected-region artifacts.

The implementation adds private immutable key/receipt storage and a SourceWriter command,
not a global cache writer or an API accepting arbitrary connections. V1 payload
is exactly 183 finite IEEE f32 values encoded little-endian (732 bytes), with
explicit format/algorithm version and digest. Keys bind exact source/scope/content
identity. Strings are individually bounded to 1024 UTF-8 bytes; encoded key is
bounded to 8192 bytes. The store allows at most 4096 retained payload rows;
capacity failure leaves the claimed work retryable and does not mark ready.
These are admission limits, not permission to delete referenced artifacts.
The implementation must document its mapping to existing feature tables; an
existing payload may be adopted only after exact content/version/digest validation
and the same claim transaction. Mutable legacy cache rows cannot be its authority.

OPT-2052 adds a private writer-instance-bound feature claim; an ordinary readiness
claim alone cannot authorize this publication after a new catalog lifecycle.
Restart target lookup reissues a validated key from persisted bytes and current
owner authority, never reconstructs a capability from the reference string.
Lookup returning unavailable requires the consumer to request derived rebuild;
it does not assert processing completion or delete semantic source truth.

Within one immediate transaction: validate retained physical owner/profile/lease,
current target and exact claim/lease; validate payload; insert without replacing
an immutable existing key; persist its exact reference and ready completion;
commit only if all steps succeed. Refactor the existing readiness completion
transaction internally as needed, rather than nesting transactions or completing
in a second connection. A rollback leaves neither a new payload nor readiness.
An exact completed retry may return its original observation without redoing
work; different claim/payload/content is not an exact retry. Recheck authority
at the final existing source-owner boundary. No caller-created reference can
activate this path.

Lookup verifies length, digest, version and target before returning bounded
bytes. Restart tests reopen the actual source DB; expired/stale claims never
publish. Corrupt/missing payload yields unavailable/rebuild-needed status, never
fabricated success. A later worker uses the existing feature computation outside
SQL and publishes through this command. The store primitive alone does not
complete OPT-1922 or its production worker acceptance.

## External artifact publication remains a real implementation gate

An external store must capture a profile/source-bound root with no-follow held
objects, reject overlapping source/trash/recovery namespaces and bind staging to
that exact root. Publish requires a qualified FileOperationOwner no-replace
adapter, exact staged identity/content and a durable effect receipt. Current
mutating effects fail closed without that backend; do not substitute a raw
rename, path create-new test, cache-clear capability or fixture receipt.

A committed external effect is adopted into source readiness only after exact
receipt validation. Crash after publication but before adoption leaves explicit
recoverable evidence; crash before publication never marks ready. Cross-owner
reverse-reference updates use the accepted journal participant protocol and
actual immutable operation-link receipts, not cross-database atomicity claims.
Keep OPT-1632/1634, OPT-1656/1657 and native OPT-1844/1953 gates open.

## Targeted review and verification

Existing actual tests establish the boundaries this decision relies on:

```sh
cargo test --locked -p yield-library --lib captured_cache_ -- --test-threads=1
cargo test --locked -p yield-library --lib all_effects_are_typed_and_mutating_effects_fail_closed_without_backend
cargo test --locked -p yield-library --lib completion_persists_an_exact_artifact_reference_atomically
cargo test --locked -p yield-library --lib stale_completion_cannot_replace_a_current_artifact_reference
cargo test --locked -p yield-analysis --lib similarity_artifacts::tests -- --test-threads=1
```

Captured-cache tests exercise real Linux temporary directories, no-follow
symlinks, root replacement and cancellation. The FileOperationOwner test proves
negative admission for unimplemented effects; it does not qualify no-replace
publication. Readiness tests verify current SQLite reference/claim behavior;
they do not contain the new feature payload. Similarity tests exercise current
compatibility publication, not a new store. Exact counts and tested/merged SHAs
belong on the PR/Linear completion record. No model fixture is needed to restate
this decision; new backend tests must target actual commands and persisted bytes.
