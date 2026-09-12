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
helper ignored outside dispatch). The current Windows validation run is
[`34702160315`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34702160315),
against `1a8fbafa5`; the earlier run was cancelled after the test fixes and is
not acceptance evidence.

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
