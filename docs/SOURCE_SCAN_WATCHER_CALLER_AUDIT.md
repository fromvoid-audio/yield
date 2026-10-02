# Scanner, watcher and directory-truth caller audit (OPT-1661)

Reviewed current GPUI source at `b3d3c8b41`, 2 October 2026. Scope is the
active scanner/watcher/directory-truth caller inventory and its source-writer
routes. This audit does not implement or accept every OPT-1352 requirement.
OPT-1352 and OPT-1344 remain open tracking parents.

## Reproducible inventory

Run `python3 scripts/internal/check/check_source_db_competing_writers.py --inventory`
and its `--self-test`. The current finite lexical tripwire reports 14 reviewed
production opens across the repository. It inventories openings, not every
method call or macro-generated connection; an allowlisted opening is not an
ownership or physical-effect qualification.

For the scanner/watcher boundary, also search active `crates` for
`scan_in_background(`, `UncoordinatedScanWriter`,
`sync_paths_with_source_writer`, `commit_watcher_replay_manifest_batch` and
`commit_bootstrap_replay_checkpoint`. Exclude inline/external cfg(test) fixtures
when judging production reachability; do not exclude arbitrary production code
because its filename sounds like a test.

| Caller/route | Existing owner and validation | Classification and limit |
| --- | --- | --- |
| `yield-catalog/src/lib.rs::register_source` | Two branches call `SourceDatabase::open_for_catalog_with_profile_guard`, either the retained registered database-root binding or its profile-owned catalog child. `CatalogSourceDatabase` retains the `SourceWriter`. | Active source-owner opens, not one connection per watcher event. Source/profile/root/database/lease validation is required. |
| Catalog `audit_source` / `CatalogScanWriter` | Full audit uses `audit_manifest_for_catalog_with_budget_and_progress_and_writer` with the retained writer. `try_lock` revalidates authority and holds the current source lifecycle/attempt admission guard for each bounded mutation. | OPT-1782/1784 full-audit route. Traversal/hashing precede the mutation guard. A safe partial commit does not make traversal complete. |
| Scanner `scan_paths.rs` exact/subtree owner entrypoints | `sync_exact_paths_with_source_writer` and `sync_paths_with_source_writer` borrow the same owner's database. `ScanWriter for SourceWriter` derives the scan fence, checks expected revision and commits through typed writer methods. | OPT-1783 entrypoints exist and are tested. A catalog-owned targeted consumer must additionally use `CatalogScanWriter` for active attempt/lifecycle admission; an owner observation alone cannot retire a catalog generation. |
| Native watcher callback and `WatcherSignal` | `yield-source-watcher` keeps one bounded merged hint, source lifecycle and root observation. No SQL connection, scanner or authoritative projection is owned by the callback. | Path/event hints are observations. Overflow, loss and destructive/root uncertainty require audit; callback paths are not mutation authority. |
| Catalog `watcher.rs::run_watcher_idle_work` | Probe/refresh/drain/claim schedules call `request_audit`; the same catalog worker's retained writer performs the audit. Event capacity and lifecycle admission are reserved before publication. | Current production watcher changes converge via authoritative full audit. Failure/incomplete/gap retains pending work and last-good projection; no competing watcher database writer was found. |
| Directory staging and terminal activation | `CatalogScanWriter` checks database pointer identity and calls `begin_scan_directory_truth_generation`, `stage_scan_directory_truth_entries`, bounded discard/cleanup, and `commit_scan_audit_completion`. | OPT-1786. Private staging is revision-neutral; complete activation, manifest revision, audit state and returned delta share the source transaction. Canceled/incomplete staging is not active truth. |
| Source watcher checkpoint/replay commands | `SourceWriter::{commit_watcher_replay_checkpoint,commit_watcher_replay_manifest_batch,retain_watcher_replay_uncertainty}` validate exact owner fences and durable proof/prior in the source transaction. | OPT-1785/1795/1796 core APIs and SQL regressions; availability is not evidence that every native producer is wired. |
| macOS initial replay bootstrap in catalog `audit_source` | cfg(macos) capture produces evidence; current scan fence and the completed audit are passed to `commit_bootstrap_replay_checkpoint`. | Live FSEvents/bootstrap is unavailable on Linux. Keep exact native verification under OPT-1981/1953; portable replay fixtures do not certify it. |
| `scan_in_background`, legacy scanner wrappers and `UncoordinatedScanWriter` | Path-only background helper calls `open_for_scan`; legacy wrappers operate on a supplied database through compatibility defaults. Active-crate caller search finds the background helper's call only in scanner tests. | Reviewed compatibility API, not the active catalog route. Do not add a new UI/catalog consumer or infer profile-owned authority from the supplied path. Retirement/enforcement remains explicit below. |
| Legacy recovery `SourceDatabaseRecoveryAccess::open` | Revalidates retained `RecoveryRoot`, captures a database-root binding, opens via `open_for_source_write_with_database_root`, then revalidates and retains that binding. | Separate legacy recovery compatibility route. Not a scanner/watcher migration or qualified general FileOperationOwner consumer. |

The remaining four inventory classes outside this finite caller scope are reviewed
read-only opens, snapshot destinations, in-memory schema probes and the global
library connection. They are not new scanner writers. OPT-1663/1664 cover the
global/journal/artifact inventory and broader import-direction enforcement.

## Remaining exceptions and retirement criteria

### Atomic exact preparation prerequisite (OPT-2059)

`yield-scan::sample_sources::scanner::prepare_exact_scan` now accepts a retained
`SourceWriter` and up to 64 distinct exact regular entries. Private prepared state
borrows that exact owner, retains its root and files, and captures generic/index
revisions. Limits are 1024 bytes and 32 components per locator, 32 KiB aggregate
locator bytes, and 64 MiB aggregate audio hash reads. Hash reads stop at the
observed byte count; shortened or changed content fails closed. Missing, link,
directory, inaccessible, oversized or substituted targets produce no partial
mutations. Unicode remains exact; raw locators remain diagnostic index facts.

`PreparedExactScan::commit` revalidates the held content/path/root and owner before
acquiring the supplied `ScanWriter` mutation guard, then applies audio and index
facts in **one** revision-fenced `SourceWriteBatch` through the existing
`commit_bounded_manifest_batch` hook. It advances no audit/scan-completion marker.
Real temporary filesystem/profile-owned SQLite/retained-writer tests cover one
revision, rollback, foreign owner, stale generic/index revision, cancellation,
substitution, root replacement and bounded/raw inputs. These are Linux source
regressions, not live replay or native/physical qualification.

This is the actual scanner API prerequisite, not an active catalog replay caller.
OPT-2053 remains open for the production batch consumer, bounded subtree/recovery
handling, reconciliation admission and durable checkpoint/acknowledgement/retirement.
Its catalog writer can attach the separately admitted replay proof and prior using
`SourceWriter::commit_watcher_replay_manifest_batch` in this same commit hook.

* **OPT-1787**, behind OPT-1634/1632: committed file-effect reconciliation has
  no qualified publication checkpoint/owner receipt to consume. The source
  delta route must bind that exact durable effect evidence, cancellation,
  identity, lifecycle and retry. A scanner audit or model receipt cannot
  replace that implementation; retain uncertain effects and source evidence.
* **OPT-1636/1664**: explicitly enforce the finite modern-crate caller boundary.
  Compatibility scanner wrappers may remain for existing library/test use;
  retire their classification only when actual callers are migrated or the
  API is removed. A newly added active app caller requires a named owner route
  and reviewed guard change, not a broad allowlist. This audit edits no guard
  owned by another agent.
* **OPT-1981/1953**: Finder/FSEvents metadata suppression, bootstrap,
  overflow/restart and unreadable-subtree integration still require macOS.
  The catalog's incomplete-watcher integration test is cfg(macos), as are native
  initial bootstrap/cancel and restart tests. A passing Linux hint/replay test
  is not their execution.
* **Native replay integration boundary**: active catalog code uses watcher hints
  and full audits plus the macOS initial bootstrap. The general
  `ReplayObservationBatch` -> reconciliation admission ->
  `commit_watcher_replay_manifest_batch` incremental path has portable core
  APIs/tests but no active catalog caller in this source search. Any producer
  integration required by OPT-1355/1981 must be scoped and implemented before
  claiming end-to-end proof-bearing replay; do not relocate source-local state
  into another writer or report a replay candidate as continuity proof.

## Focused verification

Existing tests exercise actual temporary source/profile SQLite databases and
catalog worker commands. Scanner and watcher tests use temporary filesystem
fixtures; hint injection and replay fixtures are explicitly narrower than real
macOS events. No new fixture or production code is added by this audit.

Commands used (exact counts/results and final tested/merge SHA are recorded on
OPT-1661 and its PR after verification):

```sh
cargo test --locked -p yield-catalog --lib retained_ -- --test-threads=1
cargo test --locked -p yield-catalog --lib watcher_ -- --test-threads=1
cargo test --locked -p yield-catalog --lib directory_truth -- --test-threads=1
cargo test --locked -p yield-source-watcher --lib
cargo test --locked -p yield-scan --lib scan::tests -- --test-threads=1
cargo test --locked -p yield-library --lib source_writer -- --test-threads=1
bash scripts/gpui-development-checks.sh check
python3 scripts/internal/check/check_source_db_competing_writers.py --inventory
python3 scripts/internal/check/check_source_db_competing_writers.py --self-test
python3 scripts/internal/check/check_gpui_owner_boundary.py
python3 scripts/internal/check/check_gpui_action_register.py --check
git diff --check
```

Representative source tests include
`retained_scan_writer_commits_first_audit_then_rejects_retired_lifecycle`
(no admission guard held during traversal, wrong database/retired attempts rejected),
`retained_writer_routes_exact_and_overlapping_subtree_scans`,
`audit_publishes_complete_directory_truth_at_exact_manifest_revision`,
`detached_watcher_hint_runs_debounced_authoritative_audit`, and
`scan_discovery_does_not_hold_the_source_writer`.
The 14-opening tripwire and its negative self-tests provide finite lexical
evidence only. Physical publication/cleanup, macOS captures and full parent
acceptance remain open.
