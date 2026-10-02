# Global, journal and artifact caller audit (OPT-1663)

Reviewed current main `b60881e6f` on 2 October 2026. This finite inventory
records existing callers and required exceptions. It does not migrate three
subsystems or complete OPT-1344, OPT-1357, OPT-1339 or OPT-1343.
Historical `GLOBAL_LIBRARY_WRITER_EVIDENCE_AUDIT.md` describes its named earlier
head; the current registry participant below supersedes its missing-registry
receipt observation, not every other missing participant.

## Caller inventory

| Caller | Existing route and evidence | Remaining acceptance boundary |
| --- | --- | --- |
| Session startup / shutdown | `yield-session/src/lib.rs::Persistence::start` acquires `WritableProfileGuard`, opens the profile-bound journal, starts `OperationJournalOwner`, then `GlobalLibraryWriter`, and supplies the journal client to catalog. Shutdown closes catalog and drains maintenance before releasing global writer, journal and profile authority. | The portable registry fixture uses the real profile/global/journal SQLite owners but deliberately excludes native protected reserve setup. It does not qualify production filesystem effects or physical startup capacity. |
| Configured source registry and primary role | Session `queue_save` sends immutable snapshots to `replace_source_registry`; retained-root lookup/load use the same owner queue. Registry revisions and request completion checks prevent stale session publication. | Ordinary whole-snapshot calls still lack caller-stable durable command identity/result custody. OPT-2031 -> OPT-1836 defines and implements that common boundary. Configured membership is not physical source retirement. |
| Operation-scoped registry participant | `GlobalLibraryWriter::apply_source_registry_participant` executes under current acquisition identity. `registry_participant.rs` atomically persists registry mutation/rejection and immutable OperationId/work-key receipt in the same SQLite transaction; capacity is 8192 receipts, exhausted admission fails closed. | OPT-1642/2030 evidence applies to this named participant. It is not a common ordinary-command envelope, cross-database atomicity, operation-link receipt or Harvest checkpoint. |
| UI preferences | Session queues bounded preference payloads through `replace_ui_preferences_payload`; owner validates profile binding before and after commands. Database replacement is transactional. | Session request/deadline/revision inputs are local observation fences. Dropped reply/restart replay contract remains OPT-2031/1836. |
| Profile tag vocabulary | `yield-session/src/vocabulary.rs` uses the retained `ProfileTagVocabularyClient` for lookup/page/set/delete; bounded inputs and revision CAS execute on the global writer. | Vocabulary transaction/revision evidence is not source tag-assignment evidence or a stable ordinary OperationId receipt. OPT-2034/2012 owns the actual source assignment participant; OPT-2031/1836 owns ordinary global replay policy. |
| Catalog source-capacity registration / drain | `yield-catalog/src/lib.rs` derives source authority from the retained source writer, then calls journal `register_source_authority` and `begin_source_drain`. Pending/failed acknowledgements remain explicit and lifecycle-bound; no catalog journal SQL writer is opened. | A registration receipt is capacity/source authority handoff, not a file-publication or cleanup checkpoint. OPT-1634/1635/1787 remain required actual consumers. |
| Journal queued commands | `operation_journal/owner.rs` retains the store and bounded client/results; typed prepared admission and encoded family intents execute on its worker. Existing v6-v15 tests persist/read actual temporary journal SQLite records. | Encoded intent persistence and synthetic capacity fixtures do not execute the corresponding FileOperationOwner physical adapter. Do not claim publication, cleanup or rollback from them. |
| GENERAL clear-cache UI | `yield-ui/src/browser/general_settings.rs` -> `SourceRegistryGlobal` -> session `clear_rebuildable_cache` -> maintenance `start_clear` -> captured `DisposableCacheNamespace::clear`. Admission rejects overlapping configured source/trash namespaces. Worker checks cancellation, profile/root identity, no-follow traversal and depth bounds. | This is disposable profile-cache maintenance, not source readiness deletion, global artifact references or source retirement. Windows handle/no-follow behavior needs the scoped native run; Linux tests exercise the non-Windows capability implementation. |
| Source-local readiness / ANN helpers | `yield-library/src/sample_sources/readiness` stores source-local readiness. `yield-analysis/src/analysis/similarity_artifacts.rs::rebuild_exact_similarity_artifacts` accepts a supplied source connection, exact manifest and caller publication fence; ANN/layout/cluster state is transactionally published for that generation. | This helper is not currently consumed by the active catalog/GPUI feature-job route. A supplied SQLite connection and callback are not retained ArtifactKey/store authority. OPT-1655/1656/1657/1922 own the missing capability store, reverse ownership, eviction distinction and actual job consumer. |
| ANN filesystem storage | `analysis/ann_index/storage.rs` derives the artifact directory from `PRAGMA database_list`; publication creates a path-based temporary dump/container and cleanup removes superseded/retired paths. | Explicit compatibility helper exception, not a qualified capability-relative no-replace publish/prune/retire route. Future active consumers must first use the concrete store contract and actual owner, not allowlist the path helpers as production authority. |
| Global cache/reference and legacy Harvest helpers | No active session/GPUI global cache/artifact-reference mutation caller was found. Legacy `sample_sources/library.rs` Harvest wrappers remain separate compatibility APIs; dormant schema tables alone do not identify a producer. | Preserve OPT-1834's cancellation for the nonexistent global cache route. Actual Harvest owner/derivation migration remains OPT-1831/1645; use a concrete caller/delta rather than inventing a global reference writer. |
| Ambient cache-clear compatibility API | `app_dirs/paths.rs::clear_rebuildable_cache_payloads` creates/removes paths without a retained namespace. Active-workspace search finds only its definition/reexport; `reference/radiant` has historical callers outside the active workspace. | OPT-2040 retires this unused active API; retain historical reference evidence and the captured maintenance route. |
| Recording output compatibility API | `reson/recording/mod.rs` -> `capture::start_recording_runtime` -> `RecorderWriter::spawn` -> `WavSampleWriter::new` uses pathname `File::create`, which can truncate. | OPT-2041 owns concrete recording sink/admission/cancellation/result contract and actual producer inventory. No recording physical-effect qualification is inferred from WAV serialization tests. |
| Legacy source database filename migration | `db/open/paths.rs::prepare_writable_db_path_with_policy` validates database/sidecar path policy before renaming the legacy database and existing sidecars. Read-only observation keeps the legacy name. | Existing source-open compatibility, not user media rename or object-bound no-replace publication. Retire only after the layout compatibility window and actual caller migration; preserve source identity/lease checks. |

## Explicit exceptions and retirement criteria

* **OPT-2031/1836:** ordinary registry/preferences/vocabulary durable identity,
  bounded result retention and restart replay are missing production work.
  Retire this exception after current producers use the accepted common boundary
  and its stale-owner, lost-reply and restart matrix passes.
* **OPT-2033/1644/1833:** PR #199 defines operation-link data/receipt ownership;
  its model fixture is not the global owner/SQLite consumer. Retire after the
  actual journal-backed link participant and durable replay tests are merged.
* **OPT-1831/1645:** retain the named Harvest compatibility boundary until the
  actual owner/checkpoint producer replaces it; a direct-library reopen fixture
  does not qualify the global writer route.
* **OPT-1632/1633/1634/1635/1787:** actual qualified filesystem adapters and
  durable effect/checkpoint/reconciliation consumers remain missing. Generic
  journal persistence, scanner convergence or a synthetic receipt cannot
  substitute for them. Native qualification is an additional scoped gate.
* **OPT-1655/1656/1657/1922:** implement the concrete capability store and actual
  source-owned job consumer before accepting path-based ANN helpers as routed
  artifact authority. Rebuildable bytes and source truth remain distinct.
* **OPT-1636/1664:** PR #202 adds a finite filesystem/journal producer inventory
  and negative tripwire; broader import-direction work remains OPT-1664.
  This inventory edits no shared guard. Reviewed maintenance and
  fixture exceptions require exact scope and retirement criteria; no broad
  owner-name or directory allowlist makes a bypass acceptable.

## Reproducible verification

Search current `crates/yield-session`, `yield-ui`, `yield-catalog` and library
owner modules for `GlobalLibraryWriter`, `OperationJournalStore::open`,
`register_source_authority`, `begin_source_drain`,
`capture_disposable_cache_namespace` and `rebuild_exact_similarity_artifacts`.
Classify cfg(test) fixtures separately; do not infer production reachability from
an exported API or from a historical audit. Search ANN storage/publication for
`PRAGMA database_list`, `create_dir_all` and `remove_file` to reproduce its
remaining path-based boundary.

Run the source-open inventory/self-test and GPUI owner-boundary/action guards.
Their finite lexical coverage cannot prove every Rust/macro caller. Focused
global-owner, journal persistence, captured-cache, portable session-registry and
source-local similarity tests exercise the listed current implementations;
record exact filters/counts with the PR and Linear completion evidence. No new
fixture or production code is added here. Parent acceptance and every named
implementation/native exception remain open.
