# Retained source read-only snapshots (OPT-1689)

`SourceSnapshotOwner` transfers an existing profile/source-lease-bound
`SourceWriter` to one worker. A page request never opens another database,
initializes schema, renews a lease, or runs checkpoint maintenance. Reads use
that retained connection, strict live identity/lease validation and a bounded
SQLite read transaction. `FreshAsOf` names the exact owner-issued source revision;
`Stale` returns a current observation for restarting. Neither result grants
mutation authority or freshness after delivery.

The public caller budget is two seconds, including worker admission and OS
identity checks. The worker retains custody after a timeout; its single pending
slot cannot be overtaken by a second owner. Expired requests are rejected before
payload cloning or enqueue, and before SQLite work in the worker primitive.
Pages clamp to 1–64 rows; input/output bytes are bounded to 16 MiB and SQLite
progress is interrupted at the deadline. Retirement returns the same writer;
joining belongs off the UI thread and can await an outstanding OS operation.

`OPT-1639` / PR #98 already implemented the underlying owner and freshness path.
`OPT-1687` remains open for writer checkpoint policy. Snapshot reads deliberately
never invoke that maintenance, so its former blocking link was removed without
accepting maintenance, source remap, capacity policy or the OPT-1348 parent.

## Portable verification

Run from the feature worktree using the pinned Rust toolchain:

```sh
cargo test --locked -p yield-library --lib snapshot_read_only -- --test-threads=1
cargo test --locked -p yield-library --lib source_owner_page_is_bounded_read_only
cargo test --locked -p yield-library --lib snapshot_owner_times_out
bash scripts/agent.sh preflight
```

Real profile-bound SQLite tests retain committed WAL state and put the owner's
connection into `query_only` mode with an authorizer that forbids mutation and
checkpoint SQL. Negative controls reject UPDATE, CREATE TABLE and checkpoint.
Fresh pages include the committed WAL-resident curation value; continuation and
stale revision responses preserve persisted database/WAL bytes and SQLite change
count. Expired lease reads fail without renewal. Expired requests cannot consume
the pending worker slot; subsequent reads and retirement retain the same owner.
Existing tests cover byte/row bounds, stale continuations and blocked-worker
custody. These are actual portable owner/SQLite checks, not model receipts.

This does not assert native filesystem no-write qualification. SQLite WAL readers
may update shared-memory reader marks; SHM byte identity is deliberately not an
acceptance claim. Native source identity, network-volume behavior, power-loss
qualification and release acceptance remain in their existing platform/tracking
issues. No read-only observation can establish physical effect authority.
