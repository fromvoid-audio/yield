# Held source WAL owner validation (OPT-2208)

`SourceWalPreparation::revalidate_owner` checks the original cooperative deadline,
then invokes the existing retained `SourceLease::validate_current_owner` through
its read-only control connection, then checks the original deadline again.
This validates real profile/root/main-file/sidecar bindings and the exact
current durable lease row/expiry without touching the reserved ordinary SQLite
connection. No query/connection is supplied by the caller and no new connection
or path-based writer is created. No lease renewal or source mutation occurs.

Success is a current-owner observation, not a catalog lifecycle/registration
fence, semantic revision refresh, journal decision latch, capacity claim or
required-work/commit permit. The source semantic revision remains fixed by the
held empty SQLite writer reservation. Catalog mapping/lifecycle/registration
checks belong to OPT-2191; actual permits/retirement remain OPT-2136/2202.
Neither an owner error nor deadline expiry releases the reservation or proves
rollback. Only the existing explicit abort/drop paths change custody; confirmed
abort returns the exact writer for a fresh attempt.

Real retained-owner tests deny **all** ordinary SQL and observe zero authorizer
calls during repeated validation. Ordinary/control total-change counts, exact
lease row and source metadata stay unchanged. An independently opened SQLite
connection stays excluded; confirmed abort returns the original SQLite handle
and supports another genuine preparation. Actual media-root, profile and main
file disappearance/substitution reject. Moving the whole profile can make an
observer's sidecar lookup return IOERR rather than BUSY, so that case checks
retained transaction custody directly while displaced and actual external
writer exclusion after restoring the original namespace. It does not claim
that namespace displacement is a successful external SQLite probe.

Original deadline expiry remains expired across repeated calls and continues
to exclude a competing writer until explicit abort; revalidation never extends
it. Existing near-expiry/no-renew, stale context, deadline, rollback-failure,
hard/unknown WAL and connection-retry regressions remain unchanged.

```sh
cargo test -p yield-library --lib held_owner_revalidation --locked
cargo test -p yield-library --lib wal_reservation --locked
cargo test -p yield-library --lib --locked -- --test-threads=1
scripts/agent.sh preflight
```

These tests use actual roots/profile/source leases and SQLite, not fabricated
capacity or physical provider qualification. Native macOS owner bootstrap and
root/file behavior verification remains OPT-2055. Parent OPT-2191 and the
required-permit/settlement/capacity/physical acceptance scopes remain open.
