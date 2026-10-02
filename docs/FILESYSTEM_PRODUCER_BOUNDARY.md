# Filesystem and journal producer boundary (OPT-1636)

The active Cargo workspace is guarded by
`scripts/internal/check/check_filesystem_producers.py`. The reviewed inventory is
`filesystem-producer-inventory.json`: each row fixes one path, enclosing function,
operation and exact call count, with an owner/maintenance/compatibility/fixture
classification, issue, reason and retirement criterion. Adding, removing, moving
or duplicating a recognized producer requires explicit review. There is no
automatic approval/update command and no whole-file mutation allowance.

Run the existing agent guard entrypoint:

```sh
bash scripts/internal/check/check_source_db_open_roles.sh
python3 scripts/internal/check/check_filesystem_producers.py --inventory
```

The first command runs source-open, GPUI owner and filesystem producer checks and
their negative fixtures. The second prints candidates without approving them.
Review an intentional change against its actual caller, namespace, profile/source
authority, admission, cancellation, result and retirement policy before editing
the JSON. Never copy candidate output over the reviewed inventory.

## Current classifications

| Producer | Classification and limit | Remaining requirement |
| --- | --- | --- |
| Profile/global/source directory and lock bootstrap | Existing owner internals; runtime ownership and source leases remain necessary. | OPT-1340 tracking acceptance stays open. |
| Journal open/start in the session persistence owner | Existing owner; native test starts and production starts remain distinguished in source cfgs. | OPT-1642 receipts do not qualify physical publication/cleanup. |
| Retained-root recovery create/open/link/remove | Compatibility operations imported from cap_primitives. A qualified primitive decision is separate from this lexical classification. | OPT-1634/1635 checkpoint work and OPT-1632/1633 effect qualification remain open. |
| Scanner/reconcile/source-writer `remove_file` | Database-entry removal through existing source owner routes; these tokens do not mean physical unlink. | Committed-mutation reconciliation remains OPT-1787; caller audit OPT-1661. |
| Source snapshots | Existing held-write-fence/create-new destination owner; owned failed-artifact cleanup. | No permission to snapshot an arbitrary source without runtime leases. |
| ANN dump/container persist/retirement and analysis app directory | Compatibility pathname artifact producers, including library HNSW dumps and tempfile persistence. | OPT-1655 interface plus actual store implementation; synthetic keys are insufficient. |
| Recording File/WAV creation | Compatibility recording writer; File::create can truncate. No file-operation qualification is claimed. | Exact live caller/admission exception handed to OPT-1663; keep OPT-1339 open. |
| Edited/span WAV encoder constructors | Caller-supplied Write+Seek sinks; no output path authority is minted by the encoder. | Caller-owned file-backed sink admission remains necessary, including OPT-1610. |
| Logs and capability-relative disposable cache clear | Maintenance; log/cache namespaces only, never source media retirement. | Retire with the named maintenance owner. |
| Old ambient cache clear and legacy database layout rename | Explicit compatibility, not user media mutation parity. | Finite remaining caller audit OPT-1663 and actual retirement. |
| Portable bootstrap/test configuration helpers | Exact fixture producers are counted even when compiled under test-support features. | OPT-2025 grants no live physical capacity/source authority. |

The legacy `src/native_app`/controller paths named by the original issue are
retired from this repository. The guard derives current member roots from
Cargo.toml, instead of passing vacuously over absent legacy directories. Missing
workspace source roots fail. Vendor/reference crates are outside this finite
application-workspace inventory.

## Negative and exception evidence

Self-tests reject direct and aliased filesystem mutations, File creation,
separately configured writable OpenOptions, direct/aliased journal starts/opens,
native unlink, extra calls in an approved maintenance function, moved producers,
stale removed rows and missing retirement classifications. Read-only metadata
and File::open remain allowed. Balanced inline cfg(test) modules are masked;
production after them remains checked. A tests filename or comment is not an
exception. External cfg(test) modules and literal includes inside masked test
modules can exclude a fixture file; a production declaration/include of the
same file cancels that exemption. Test-support feature code remains inventoried.

## Limits

This is a finite lexical architecture tripwire, not a Rust parser, macro expander,
type checker, import-direction proof or security sandbox. Selected method tokens
include database and caller-sink methods; their semantics are stated above rather
than inferred from spelling. Enclosing functions are lexical names, with counts
combined for same-named methods in one file. Arbitrary FFI, reexports, macro-generated
calls, unrecognized external writers and complex module-path expressions need
source review. Inventory counts cannot detect a change to arguments or runtime
authority at an existing call. Preserve the stricter GPUI owner and source-open
guards alongside this check. OPT-1664 owns wider import-direction enforcement.

Passing these guards neither implements production effects nor completes physical,
durability, native acceptance or tracking-parent scope.
