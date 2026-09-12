# GPUI profile ownership

This document records the active fork's OPT-1336 ownership boundary. The
normative ordering remains in `IO_ARCHITECTURE_TARGET.md`.

The session acquires a `WritableProfileGuard` before starting the global library
writer or catalog service. A losing process receives
`ProfileOwnedByAnotherProcess`; it cannot silently start a writable participant.
The guard retains the operating-system lock and validates the live profile-root
and lock-entry identities. Replacement of either entry invalidates the owner.

Each successful acquisition has an opaque UUID recorded alongside the PID in
the version-2 diagnostic lock record. Clones retain that UUID; reacquisition
creates a fresh one. Old diagnostic contents do not prevent acquisition after
the operating-system lock is released. Neither the PID nor the UUID is proof
that takeover is safe, and neither replaces the retained lock capability.

The subprocess validation lane uses isolated profiles, verifies typed conflict,
and checks reacquisition after both orderly release and abrupt process exit
without Rust destructors. Helpers use bounded handshakes and clean up child
processes on failure. Same-process tests separately exercise root and lock-entry
replacement and cloned guard lifetime.

On 2026-09-12, the focused library ownership suite passed 10 tests (one helper
test intentionally ignored outside subprocess dispatch), and the session
subprocess regression passed. The session compilation, formatting and diff
checks passed on macOS. Windows runtime coverage was not executed locally.
These are profile-boundary checks, not source-lease or universal-journal
acceptance.

## Production database admission

Path-only source write opens and legacy global-library convenience opens return
`ProfileOwnershipRequired` in normal builds, before creating directories,
opening writable SQLite connections, or applying schema changes. This is an
admission error, not a conflict with another process. Read-only access remains
available. The catalog's explicit guard-bearing opener and the profile-owned
global writer are the production write routes.
Public file-operation recovery validates this same retained authority before
reaching filesystem finalization; a read-only journal handle cannot authorize
recovery mutations.

Authorized source databases retain a clone of the profile owner for their
entire lifetime. A snapshot write fence must also retain the profile owner and
database-root binding until its reservation is released, even if the original
database is dropped. Passing a guard only during construction is insufficient.

The macOS production admission integration suite passed six tests covering
path-only rejection without filesystem effects, retained profile and child-lease
lifetimes, root replacement, and read-only versus owned snapshot/recovery
admission. Focused suites passed: 9 snapshot, 26 journal, 27 database opening,
10 profile ownership, and 4 catalog profile tests. The full session suite passed
57 tests (one subprocess helper ignored outside dispatch); the existing
readiness process test passed with its fixture feature explicitly enabled.
The production GPUI check, formatting, diff check, and normal dependency-feature
inspection passed. Windows CI includes separate production admission and
fixture-enabled readiness lanes; execution evidence must be recorded separately.

The full UI and catalog suites also passed (394 UI tests, one existing ignored;
91 catalog tests). Tag-completion fixtures use the guarded catalog opener under
a canonical isolated profile rather than enabling fixture write access for the
UI dependency graph. Ownership tests read diagnostics through the retained lock
handle, matching Windows exclusive-lock semantics.
The complete library unit suite subsequently passed 536 tests (one subprocess
helper ignored outside dispatch). Windows validation passed in
[`34702160315`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34702160315),
against `1a8fbafa5`, including production profile admission, profile-owned
settings maintenance, audio/core/media/platform tests, GPUI compilation, and
browser state/interaction tests. The earlier run was cancelled after the test
fixes and is not acceptance evidence. This run predates the subsequent neutral
journal-model extraction.

Legacy fixture creation remains available only under library unit-test
configuration or the explicit `test-support` feature. That feature must not
appear in the normal GPUI dependency graph. The standalone `profile_admission`
integration lane exercises the library without fixture support; running only
fixture-enabled unit tests cannot establish production admission correctness.

This boundary does not implicitly acquire a profile, adopt a source, or enable
legacy standalone scanners to write. Their callers must use an owner-routed
workflow. An owned database may open a snapshot reservation through its retained
capability; a read-only database handle does not authorize that writable reopen.

## Remaining integration contracts

The active `operation_journal::OperationJournalStore` now retains a profile clone
and an exclusive child lease until its SQLite connection closes. It provides
bounded point reads and initial intent insertion with a monotonic sequence;
duplicate identities cannot overwrite existing records. Its receipt proves a
SQLite commit and post-commit identity checks, not coordinator admission or
filesystem publication authority. New intents require a consistent capacity
plan, but this store does not acquire capacity claims or queue permits.

The dedicated database uses DELETE journaling, exclusive locking, and
`synchronous=EXTRA`. Unix opening flushes the retained profile/child namespace;
Windows relies on the bundled SQLite VFS for committed file content and has no
independently verified directory-entry power-loss guarantee. SQLite's actual
open file is checked using platform file controls in addition to root and
pathname identity checks. Unknown headers, unsupported schemas, and unexpected
sidecars remain retained for audit. A pre-existing rollback journal is accepted
only when it is a regular file that is empty or has an entirely zeroed 28-byte
header. Its identity and inactive state are checked before SQLite executes SQL.
Active, truncated, replaced, and otherwise ambiguous sidecars remain retained;
this does not enable hot-journal replay.

A subprocess test commits an intent, is forcibly terminated, and proves that a
new profile owner can reopen the acknowledged record. The restart addition passed
32 store tests (plus one ignored subprocess helper), GPUI compilation, formatting,
and diff checks on macOS. Windows storage validation passed in
[`34709638982`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34709638982)
at `87887937b`, including the subprocess restart test and participant model.

The initial storage checkpoint passed 45 focused journal tests, GPUI compilation,
formatting, and diff checks on macOS. Tests cover normal reopen persistence,
retained ownership, duplicate IDs, malformed/oversized records, and replacement
both during opening and before writes. Windows storage validation passed in
[`34707094509`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34707094509)
at store commit `3534c37b6`, including journal tests and production profile
admission. Phase updates, participant execution, and recovery execution remain
open; later checkpoints below cover persisted participants and session lifecycle.

Recovery observation now uses indexed sequence paging with at most 16 rows and
2 MiB of encoded records per page. Malformed, unsupported, oversized, or
noncanonical-key rows remain typed audit observations; unusable sequence values
stop the scan rather than being silently skipped. SQLite's value-size limit also
bounds materialization before Rust inspects a damaged key. A late-cursor test
checks database work against a 4096-row prefix, and byte-budget tests verify
that deferred rows appear on the next page. All 55 operation-journal module tests
and GPUI compilation passed for this addition. This is read-only observation,
not legacy-file import, replay, participant reconciliation, or recovery admission.
Windows storage validation for recovery paging passed in
[`34708286537`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34708286537)
at `e17684aa2`.

The pure participant model adds bounded descriptors, source identity/revision/lease
observations, and one checkpoint per participant. Only optional rebuildable
artifacts can be deferred; required work cannot be silently dropped. Malformed
decoded inventories never report required completion. Eleven focused tests and
GPUI compilation passed. Schema V3 now carries these observations in a strict,
bounded initial universal-intent envelope. V3 requires a capacity plan, object
payload, stable status key, and pending participant checkpoints; it rejects
legacy execution evidence and all later phases until their transition contracts
exist. Unknown fields and unsupported command values remain retained raw bytes
for audit rather than being accepted as execution data; metadata-only commands
remain outside this foundation until their actor/family contract is defined. The V3 codec and store
do not implement live source leases, queue admission, or authorize phase
transitions.

The V3 addition passed 82 focused journal tests and GPUI compilation on macOS.
An additional recovery regression passed after review: an unsupported V3 phase
remains byte-for-byte unchanged while a later valid intent stays observable.
Windows storage validation passed on `aa135f587` in
[`34711107753`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34711107753).
Retry/cancellation metadata, typed command arguments, full multi-class capacity
claims, execution transitions, and recovery execution remain unfinished.

The GPUI session now opens the profile-owned operation journal after acquiring
the profile guard and before starting the global writer or catalog. It scans one
bounded recovery page; an empty page permits ordinary startup, while any row or
audit error reports `RecoveryUnavailable` and leaves both writable services
uninstalled. This is a temporary fail-closed gate until recovery execution and
phase transitions exist. The session retains the journal through maintenance,
catalog, and writer shutdown, then releases it before the final profile owner.
Journal rows are never deleted, replayed, or treated as execution authority by
this startup check. Focused session lifecycle tests cover empty-journal reopen,
retained/corrupt journal blocking, external catalog-handle drop ordering, and a
losing profile process leaving the journal untouched.
All 61 session tests and GPUI compilation passed on macOS; the pending-recovery
test also passed after review of the user-facing status wording. Windows storage
validation passed on `42610b214` in
[`34711763569`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34711763569).

A profile acquisition UUID is not a monotonic source-lease epoch. The physical
lease foundation below supplies OPT-1336's independent source-conflict contract;
OPT-1349 still owns propagation through asynchronous work and publication.
An unavailable profile must never be mislabeled as a source-lease conflict.

Source write batches now revalidate retained profile/database-child ownership
before `BEGIN` and immediately before commit. Direct content/manifest audit,
directory cleanup, and source file-journal mutations use the same checks.
Replacement during a batch rejects commit and rolls back metadata and revision
changes. WAL maintenance uses the already-owned connection and skips maintenance
when post-commit ownership validation fails; the legacy path-only checkpoint
entrypoint is deprecated and performs no writes. This does not yet validate a
physical source lease or database-file identity.

The addition passed 283 source database tests, including four ownership
regressions, plus two WAL tests and GPUI compilation on macOS. Windows CI now
includes the portable ownership and WAL regressions. After gating Unix-only test
imports, Windows storage validation passed on `449082c4d` in
[`34713357213`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34713357213).

Readiness writes now borrow the retained ownership context through
`SourceDatabase::readiness_store()`. Publication, work claims and completion,
artifact invalidation, and cleanup validate ownership before transactions and
before commit. In production builds, a raw `ReadinessStore::new` connection can
serve queries but returns `OwnershipRequired` for mutations. Fixture-only
unowned writes remain gated behind tests or `test-support`.

Validation passed 52 readiness tests in normal and test-support builds, the
production admission regression, and GPUI compilation. The ownership regression
invalidates the retained binding from a SQLite update hook after a real row
write and verifies rollback. Windows storage validation, including that portable
regression, passed on `9aa0a74e1` in
[`34714685207`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34714685207).
These checks establish profile
ownership, not physical source lease or main database-file identity authority.

Profile-owned source databases now retain a no-follow main-file identity proof.
Their opener validates the bound directory before file creation, opens SQLite
without `CREATE`, and checks the retained pathname and native connection identity
before pragmas, schema setup, and later mutations. Readiness borrows this proof;
snapshot writer connections must match the original identity and cannot recreate
a missing database. Replacement, unavailable checks, and native-handle mismatch
have typed errors. Legacy layouts requiring pathname migration are retained with
a migration-required error. WAL/SHM symlinks and nonregular entries are rejected;
regular WAL handling and the existing source SQLite configuration are unchanged.

The source and journal share the platform identity primitives while keeping their
own opening and recovery policies. Seven focused identity regressions and seven
production admission tests passed, including actual-write readiness rollback and
snapshot rejection with no output. The earlier local pass covered a 51-test source
database subset, 47 readiness tests, and 35 journal store tests (one subprocess
helper ignored). GPUI compilation and formatting passed. Windows storage
validation passed on `10bd70527` in
[`34717029864`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34717029864);
local Windows cross-compilation lacked the MSVC tools and headers. This
milestone does not implement physical source leases or qualify WAL durability.

## Physical source ownership

Profile-authorized source opens acquire a separate operating-system lock in the
database namespace before opening writable SQLite. An additive singleton lease
table binds the media root, database directory, main file, lock sidecar, profile
root and profile lock identities to an acquisition UUID and monotonic epoch.
The media root and database directory are distinct capabilities: profile-local
catalog databases keep their existing placement. An explicit guard-bearing
external-source opener allows independent profiles to contend for the same
physical database and receive a typed source conflict.

Snapshot fences share the retained lease. Ordinary connections close before
the final lease release, and its control connection closes before the OS lock
and profile owner. Consuming `SourceDatabase::close` reports release failures;
outstanding snapshots keep ownership until they drop. Implicit cleanup logs a
release failure and retains the durable record for conservative recovery.

Mutation admission validates identities and the durable owner/epoch. Healthy
leases avoid renewal writes; near-expiry or idle leases renew before beginning
the caller transaction. Commit checks cannot renew inside that transaction.
Renewal uses a retained control connection with a bounded busy timeout and
`synchronous=EXTRA`; macOS also requests and verifies `fullfsync`.
This is not a power-loss qualification of source WAL storage.

A released row can advance to a new owner. An expired active row can be reclaimed
only with matching live source identities and the same retained profile-root
and profile-lock identities. The OS locks must also be available. Expiry alone
cannot establish that a foreign profile owner is no longer live. Unknown schema,
missing prior lease state, identity replacement and malformed rows remain audit
errors; reopening cannot reset an established epoch or recreate its missing DB.
Lease schema and row reads are bounded and validated before control writes.

Explicit catalog source swaps consume the previous database owner. For a
profile-child database with no snapshot holders, the handoff closes the ordinary
connection and atomically advances the lease epoch, acquisition UUID and media
root binding through the retained control connection. The OS lock remains held;
the old and new root capabilities and unchanged database identity are validated
before commit. External databases cannot use this catalog-only transition.
Ordinary opens still reject a different root. Catalog audits check root identity
before maintenance so accidental replacement reports a stale-root outcome.

Full asynchronous scanner, watcher and catalog publication fencing remains
OPT-1349 work. A database transaction fence alone does not reject every late
result already queued outside the database.

The lease and handoff milestone passed 33 source identity tests, seven production
admission tests, all 91 catalog tests and 61 session tests (one subprocess helper
ignored outside dispatch). GPUI compilation, formatting and diff checks passed
on macOS. Tests include independent-profile subprocess conflicts, abrupt-owner
expiry recovery, unchanged malformed-state retention, actual control-connection
write counts for healthy renewal, and implicit drop with an outstanding SQLite
transaction. Windows storage validation remains pending for this milestone.

OPT-1337 owns universal journal integration. The session now opens that journal
before the global writer and closes it before final profile release, but durable
operation admission, checkpointing, and recovery execution remain requirements
for the completed integration. Existing legacy journal recovery does not
establish a universal coordinator. OPT-1473 must consume the completed
contracts rather than bypass these remaining requirements.

Shutdown closes admission and stops maintenance/catalog work before closing
the global writer. Retained guard clones keep profile ownership alive until
their participants finish. Future journal and source-lease participants must
preserve this ordering and release the profile owner last.
