# Linux execution readiness (OPT-2024)

## Current five-agent dispatch (OPT-2035)

The earlier review below is historical. Use the live
[Linear execution plan](https://linear.app/boostnlvp/issue/OPT-2035),
[five-agent policy](LINUX_FIVE_AGENT_EXECUTION_PLAN.md) and
[complete dependency refresh](LINUX_EXECUTION_DEPENDENCY_REFRESH.md).
Four independent tasks OPT-1661/1663/1636/1655 are now ready; portable contract
leaves OPT-2031..2034 and OPT-2036 resolve the implicit consumer gates.
OPT-1641/2025/2005/2023/1642 and the earlier contract slices have merged.
Read live claims before choosing work. A Linux: Blocked label means resolve the
first portable prerequisite, not stop; only a concrete unavailable resource
can halt that step. Tracking parents and physical acceptance remain open.

Readiness review, 2 October 2026, against `origin/main`
`0e115f62e` (`PORTALSURFER/yield`, Linear project P-OPT-63).
The review read all 177 Linux-blocked issues and their 68 distinct direct
prerequisites outside that set. This is a queue snapshot, not acceptance of
those issues or of a platform release.

## How the queue became blocked

The environment label identifies where portable work can be developed. It is
not a readiness label. Many issues inherit legitimate unfinished backend
dependencies, but some decision tasks were made to wait for the implementations
whose contracts they need to define. Other tickets waited for an entire
cross-database participant route even though an existing registry owner already
supports their narrower operation. Completed dependencies also remained linked
as blockers, and an already accepted source-reorder feature was inventoried as
missing.

Completed blocking links are removed while original requirement references
remain as traceability. Each blocked execution description names its remaining
open prerequisite issues. These edits do not complete the dependent tasks.

## Work available on Linux

Claim an issue in Linear before editing. Start an isolated worktree from current
`origin/main`; read the full issue and latest claim comments. Status and ownership
may have changed since this snapshot.

| Issue | Executable scope | Boundary preserved |
| --- | --- | --- |
| [OPT-2005](https://linear.app/boostnlvp/issue/OPT-2005) | Implement revision-fenced primary-source set/clear through the existing session/global registry writer. | No directory creation or physical import authority; protected sources cannot become primary. |
| [OPT-1641](https://linear.app/boostnlvp/issue/OPT-1641) | Define and exercise participant ordering/checkpoint/disposition contracts using existing journal and owner shapes. | Missing physical adapters remain unsupported; model receipts do not qualify a backend. |
| [OPT-1587](https://linear.app/boostnlvp/issue/OPT-1587) | Define one duplicate-plus-move batch's per-item conflict/cancel/retry/partial outcome contract and fixture shape. | No production batch effects or aggregate success inferred from individual outcomes. |
| [OPT-1843](https://linear.app/boostnlvp/issue/OPT-1843) | Define and validate the durability evidence vocabulary/matrix schema. | Synthetic qualified fixtures confer no production power-loss or platform guarantee. |
| [OPT-1996](https://linear.app/boostnlvp/issue/OPT-1996) | Specify and test one bounded current-profile OS-entry handoff through existing owners. | Legacy migration acceptance and native callback wiring remain separate. |
| [OPT-2023](https://linear.app/boostnlvp/issue/OPT-2023) | Implement bounded retained unsupported-entry page requests/publication in the catalog/session owner. | Index pages are retained facts with unknown completeness, not reveal permission or live filesystem identity. |
| [OPT-2025](https://linear.app/boostnlvp/issue/OPT-2025) | Separate portable registry test fixtures from native protected-reserve bootstrap while retaining real profile ownership and SQLite persistence. | Production startup remains fail closed; the fixture issues no physical reserve authority. Coordinate session-file edits with OPT-2005. |

`SourceRegistryService` already owns the global writer and persists bounded
registry edits through `replace_source_registry`. Its `reorder_source` route
rejects stale registry revisions and missing IDs. A primary-role setting can
reuse that owner without first implementing journal-backed cross-database
source-registry participants. The latter remain required for their own scope.

`SourceDb` already exposes bounded source-index pages from OPT-2022 / PR #187.
Catalog tag-page paths demonstrate the existing lifecycle, query, admission,
lease revalidation and queued-delivery boundaries. OPT-2023 extends those
boundaries to retained index pages before the unsupported-file modal/reveal
consumer is implemented.

## Duplicate source-reorder gap

OPT-2006 duplicates completed [OPT-1914](https://linear.app/boostnlvp/issue/OPT-1914).
[PR #97](https://github.com/PORTALSURFER/yield/pull/97) implemented the session
command, GPUI source drag/drop and persistence. Its accepted comment records
tested head `3edcdf5182ef54837824b825ff7e01e33aa7361a`, merge
`ded99721b8248467b5b8cf6c58a260512a25f9f7`, macOS local CI, user physical drag,
AX order, isolated restart and keyboard navigation. Physical insertion-marker
and Escape observation were explicitly limited. The current action register
now points to the existing handler and tests; it still does not certify the
broader native pointer inventory.

The Linux rerun of the existing session reorder test stops during protected
reserve bootstrap, before any reorder assertion: the production namespace
backend is unsupported on this target. OPT-2025 tracks an explicit test-only
fixture seam so portable registry persistence tests can run without claiming a
qualified physical reserve. This review does not report that rerun as passed.

## Real blockers that remain

* Folder creation: OPT-2010 must qualify an owned-directory effect and sound
  capacity bounds. Current creation workers are unsupported and unknown metadata
  bounds cannot become zero. OPT-1692 cannot be completed by a reject-only stub.
* Rename: OPT-2011 must qualify object-bound no-replace semantics. A directory
  descriptor plus a source pathname does not bind a rename to a held source
  object under the accepted namespace-substitution threat boundary.
* Publication/cleanup: the completed macOS decision explicitly retains
  unsupported adapters. Completing a primitive assessment is not implementing
  a supported primitive. Independent publication, cleanup, provenance and
  retained-evidence guarantees remain required by OPT-1632/1633/1868.
* Cross-database consumers: defining OPT-1641 enables implementation planning;
  it does not supply persisted participants, recovery checkpoints, physical
  effects, or Harvest state. Those consumers still need their own code/tests.
* Release qualification: OPT-1990 is the held Ubuntu release lane, not a ban on
  ordinary Linux feature development. Its macOS release prerequisite remains.

See [the I/O target](IO_ARCHITECTURE_TARGET.md),
[primitive support matrix](FILE_OPERATION_PRIMITIVE_MATRIX.md),
[macOS publication decision](GPUI_MACOS_PUBLICATION_DECISION.md), and
[reviewed action register](GPUI_REFERENCE_ACTION_REGISTER.md).

## Maintaining progress

Define a prerequisite contract before making its implementation consumers wait
on it. Split independently implementable owner/read/model work from whole-family
or physical acceptance; give each slice concrete tests and limits. Preserve
unknown, unsupported and ambiguous outcomes. A missing native test is recorded
in a native verification issue and does not block a completed portable source
scope. A missing production backend or authority contract remains open.

Do not infer readiness from completed linked documents alone. Check current
source, remove completed blocking relations, and replace stale references with
the actual open implementation/qualification gate. Keep parent/tracking issues
open until their full acceptance is complete. Coordinate shared register edits,
rebase onto other agents' merges, and rerun affected checks before merging.
