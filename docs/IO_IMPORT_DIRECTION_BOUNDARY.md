# I/O import direction and fixture exceptions (OPT-1664)

Use `bash scripts/internal/check/check_source_db_open_roles.sh` from the active
workspace. It runs the source-open, GPUI owner/import-direction and reviewed
filesystem/journal producer checks, including negative self-tests. Retired
reference application paths are not the source of truth for this check.

## Direction and representative bypasses

`check_gpui_owner_boundary.py` scans the current `yield-ui/src` tree and active
Cargo workspace manifests. Library/scanner/analysis/watcher runtime dependencies
cannot point into catalog/session/UI; catalog cannot point into session/UI;
session cannot point into UI. UI uses session commands/observations and named
library data imports, without direct catalog/scanner/analysis/watcher/SQLite
runtime dependencies. Target-specific and renamed packages, workspace dependency
aliases and optional dependencies remain runtime edges. Explicit dev-dependencies
are allowed as fixture edges, including UI catalog tests.

UI source rejects raw `SourceDatabase`, `SourceWriter`, `LibraryDatabase`,
`GlobalLibraryWriter`, `FileOperationOwner`, journal store/owner/client names,
worker crate imports, raw database module imports and wildcard library imports.
Named data types such as `SourceId` and `Rating` remain allowed. The existing
`db::normalize_relative_path` import is an exact pure-helper exception: it neither
opens a database nor captures filesystem authority; retire it when the display
label projection changes its normalization seam. Other raw-module aliases and
wildcards remain denied. Renaming an owner
at a use/reexport still spells the denied name. A data-oriented new API should
expose bounded observations or commands through the existing session seam;
do not add a raw factory reexport to bypass the boundary.

The same guard rejects representative direct/aliased filesystem functions,
`File::create`/`create_new` and writable `OpenOptions`, including separately
configured builders. The workspace-wide filesystem inventory also checks native
calls and selected method producers, with exact path/function/operation/count
classifications. No automatic inventory update or broad owner-file allowance
exists. Passing a guard does not grant runtime authority.

## Explicit fixture and maintenance roles

| Role | Allowed evidence | Retirement / limit |
| --- | --- | --- |
| UI inline fixtures | Balanced `#[cfg(test)] mod ... { ... }` is masked by the owner guard. Code after the module is still checked. Comments/string literals cannot create an exemption. | Move/remove only with the relevant fixture; a tests filename alone remains checked. External fixtures remain checked by this stricter UI guard unless their source itself contains an explicit masked test module. |
| Workspace external fixtures | The existing filesystem/source-open collectors follow explicit test-only module declarations/includes and revoke exemption if the same source is reachable from production. | Never grant test status solely from filename, directory or a comment. Test-support feature producers remain explicitly inventoried, not silently masked. |
| Disposable cache and logging | Exact maintenance entries in `filesystem-producer-inventory.json`; cache clear uses retained profile namespace and worker admission. | No source media/trash/readiness authority. OPT-2040 removed the unused ambient cache-clear API and its two rows; native Windows cache checks remain OPT-2044. |
| Source/global/journal internals | Exact reviewed owner openings/producers and runtime profile/source/database fences. | Counts cannot validate arguments, leases, transaction ordering or object identity; current runtime validation remains necessary. |
| Legacy scanner/recovery/ANN/recording compatibility | Explicit finite exceptions in the two caller audits and filesystem inventory. | OPT-1787 and physical adapter/checkpoints, artifact store OPT-1655/1656/1922, recording admission OPT-2041 and Harvest migration remain required implementations. No broad compatibility permission for new active callers. |

The current audits are `SOURCE_SCAN_WATCHER_CALLER_AUDIT.md` (OPT-1661) and
`GLOBAL_JOURNAL_ARTIFACT_CALLER_AUDIT.md` (OPT-1663). They are historical source
snapshots with explicit remaining exceptions; later merged owner work supersedes
the corresponding exception only with exact production/test evidence.

## Validation and limits

Negative tests exercise direct/aliased owner and raw module imports, wildcard
reexports, worker imports, file creation, named writable builders, production
after fixtures, malformed modules, fixture names/comments, renamed runtime
packages, platform-specific runtime edges, workspace aliases and reverse optional
dependencies. Temporary workspace tests exercise the actual scan, positive
dev-only/catalog fixtures, forbidden manifests and missing source roots.

Run the guard entrypoint, GPUI action-register check, locked app/format checks and
`git diff --check`. This is a finite lexical/source and manifest tripwire, not a
Rust parser, name resolver, macro expansion proof, sandbox or physical durability
qualification. Indirect reexports/FFI/macros and runtime argument changes still
need source review. Native acceptance and tracking-parent scope remain open.
