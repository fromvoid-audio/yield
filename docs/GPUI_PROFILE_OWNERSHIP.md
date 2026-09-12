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
admission. Phase updates, universal participants, recovery execution, and
session startup/shutdown integration remain open.

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
Retry/cancellation metadata, typed command arguments, full multi-class capacity
claims, execution transitions, and recovery execution remain unfinished.

A profile acquisition UUID is not a monotonic source-lease epoch. OPT-1349 owns
the durable physical-source identity, lease, expiry, stale-worker fencing and
independent source-conflict implementation. An unavailable profile must never
be mislabeled as a source-lease conflict.

OPT-1337 owns universal journal integration. Opening that journal before the
global writer, admitting durable operations, and checkpointing before final
shutdown remain requirements for that integration. Existing legacy journal
recovery does not establish a universal coordinator. OPT-1473 must consume
the completed contracts rather than bypass these remaining requirements.

Shutdown closes admission and stops maintenance/catalog work before closing
the global writer. Retained guard clones keep profile ownership alive until
their participants finish. Future journal and source-lease participants must
preserve this ordering and release the profile owner last.
