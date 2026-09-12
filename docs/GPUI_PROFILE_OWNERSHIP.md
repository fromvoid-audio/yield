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
