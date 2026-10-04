# Owned lifecycle SQLite workload evidence (OPT-2254)

This is repeatable Linux **logical storage-shape evidence** against the existing
production owned forwarding VFS, original empty reservations and entered settings.
It is not physical allocation qualification or an admitted lifecycle transition.
OPT-2194 remains open for qualified finite allocation policy, exact direction DB
claim slices, claim exhaustion and conflicting-identity handling. OPT-2195/2196,
2202, 2136 and the required permit/settlement consumers remain implementation gates.

## What executes

Six test entry points execute 32 bounded child cases with bundled SQLite 3.50.2
(`libsqlite3-sys` 0.35.0 from the lockfile). Each child has fresh process extension
facts and actual profile ownership. Each measured commit gets one fresh owned
connection, one original empty `BEGIN IMMEDIATE` reservation and one immutable
VFS restriction. The harness checks that the exact child selector executes one
test and emits its expected fixed numeric evidence; zero-test runs fail.

The 24 global cases cover Enrolled insertion, Enrolled-to-Draining update,
Enrolled-to-Retired update and a zero-row expected-relation-mismatch update plus
one immutable receipt insertion. Each SQL shape runs at 512, 4096 and 65536 byte
page sizes against empty fixtures and fixtures containing 8191 references,
8191 receipts and 4095 active references. Subjects and receipts occupy their full
16384-byte SQL limit; source IDs and work keys occupy their full 256-byte limit.
One enrollment reaches exactly 8192 references/receipts and 4096 active rows.
Every case installs and verifies a lower original pager maximum, with just 1 MiB
of extra logical page space. Existing receipt bytes and SQL row outcomes are
checked after commit. The blobs are deliberately zero-valued SQL workload shapes,
not authenticated command codecs, subjects or production receipts.

Four source cases use the actual pre-audit `SourceWriter` and
`prepare_lease_maintenance` path. No audit or `SourceRevision` is fabricated.
They measure the exact owner/epoch/expiry-conditioned lease-row SQL update in
an empty source and one with 96 opaque 655360-byte metadata values. This occupies
63369216 logical MAIN bytes, close to the 64 MiB source cap. A separate case
honors the lower original 15473-page maximum, only two pages above the current
15471-page image. SQL results preserve owner, epoch, expiry and unrelated data.
This private test commit **does not issue a lease-release witness or close ack**;
the changed scope cannot be returned as a reusable empty preparation. Even the
failed WAL attempt remains subject to the irreversible VFS restriction.

Fixture setup alone changes page size through DELETE/VACUUM, fills bounded rows,
and checkpoints its seed WAL before policy capture. The measured program never
checkpoints, vacuums, retries, resets a budget or kills readers to fit an effect.
Spilling, mmap and automatic checkpoints remain disabled under the entered policy.

## Measured limits

The VFS charges before forwarding native callbacks. Evidence records selected
page facts, separate MAIN/WAL/SHM ceilings, actual charged totals and retained
MAIN/WAL/SHM descriptor lengths, without paths, payloads or authority tokens.
Assertions check unit rounding, each object's ceiling, unchanged MAIN length
(no measured checkpoint), WAL/SHM length growth no larger than charged bytes,
and the original distinct native object identities. Source and global objects
on one device remain six distinct objects.

Representative successful full-limit measurements, in bytes:

| SQL shape / page size | Original MAIN | MAIN charged | WAL charged | SHM charged | Retained WAL after effect |
| --- | ---: | ---: | ---: | ---: | ---: |
| Enrolled, 512 | 289951232 | 0 | 4001792 | 4562944 | 42376 |
| Draining, 512 | 289951232 | 0 | 2240512 | 4562944 | 23616 |
| Retired, 512 | 289951232 | 0 | 3846144 | 4562944 | 40768 |
| ExpectedRelationChanged, 512 | 289951232 | 0 | 2093056 | 4562944 | 22008 |
| Enrolled, 4096 | 283463680 | 0 | 1241088 | 565248 | 86552 |
| Enrolled, 65536 | 365756416 | 0 | 1118208 | 73728 | 393392 |
| Source lease SQL, 4096, near cap | 63369216 | 0 | 94208 | 139264 | 4152 |

These are samples of the current logical charging formula, not tight worst-case
bounds or physical allocation estimates. The large small-page charge illustrates
why payload lengths cannot substitute for callback charging. Zero MAIN charges
reflect the intentionally uncheckpointed WAL commits; they do not qualify later
MAIN checkpoint, recovery, close, overwrite, CoW or filesystem metadata behavior.
SHM includes up-front charge of existing mapped extents, not measured physical
allocation events. No test resets or refunds these charges. After dropping an
attempt, a second restriction on the same connection is refused.

Three explicit global refusal cases supplement the matrix and source WAL refusal:

* A zero WAL ceiling produces actual SQLITE_FULL at COMMIT, leaves WAL length and
  all original references/receipts unchanged after rollback, and retains
  `BudgetFailure::Exhausted` with zero forwarded WAL charge.
* A real SQLite pager maximum equal to current pages refuses a full-size insertion
  before VFS exhaustion. Rollback retains the old image and no receipt appears.
* An original positive mmap setting cannot be restored after irreversible VFS
  restriction. Restoration returns failure, preserves `UnknownCoverage`, sets
  query-only and prevents a reusable lifecycle policy. This is required refusal,
  not a reason to relax the VFS or claim successful cleanup.

## Reproduction and remaining requirements

```sh
source /workspace/.yield-cloud/env.sh
export CARGO_TARGET_DIR=/workspace/yield/target CARGO_INCREMENTAL=0
cargo test --locked -p yield-library --lib lifecycle_vfs_workloads -- --nocapture --test-threads=1
cargo test --locked -p yield-library --lib
cargo test --locked -p yield-library --test lease_maintenance_preparation --test lifecycle_allocation --test source_wal_binding
cargo fmt --all -- --check
bash scripts/agent.sh preflight
```

Canonical preflight must run on a stable committed HEAD. These new files are
ordinary test modules, not `include!` inputs. Full parallel verification also exposed a shared-process hazard in the existing
invalid-extension loader test. That real loader case now executes in its own
bounded exact child, proves initialization was attempted and permanently denies
extension-free scope there, and checks that the parent flag remains unchanged.
No production flag is reset and no refusal is weakened.

Existing unknown-schema/runtime/
native-identity/deadline and cleanup regressions remain required; no ignores or
preflight exceptions were added.

[OPT-2199](https://linear.app/boostnlvp/issue/OPT-2199) and
[OPT-2200](https://linear.app/boostnlvp/issue/OPT-2200) require their actual native
original-object adapters and production admitted consumers before platform
verification. They must then exercise the equivalent SQL matrix, real positive
physical provider bounds including overwrite/CoW/metadata and all cleanup/close
callbacks, finite distinct claim slices, genuine issuance/replay/recovery and
sealed-close settlement. Missing adapters or consumers are implementation gaps,
not platform-test waivers. Contract SQL fixtures, logical caps and Linux measurements
supply no native physical grant, required permit, refund or genuine settlement.
