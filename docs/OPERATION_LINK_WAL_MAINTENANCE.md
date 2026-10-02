# Operation-link receipt maintenance on the retained global worker

OPT-2203 extends the existing connection-local global maintenance policy from
OPT-2150 to newly committed operation-link receipts. `apply_operation_link`
marks maintenance only after its actual new receipt transaction successfully
commits. Exact `OperationLinkResult::Replayed` returns before that marker, and
stale ownership, conflicting/malformed evidence, capacity rejection, failed
INSERT and failed COMMIT cannot set it.

The existing worker consumes the private marker on its retained connection after
the command, validates current ownership before its bounded soft-pressure
PASSIVE attempt, and revalidates ownership before replying. The marker creates
no counter, capacity, journal or participant authority. No connection, worker,
SQL schema, source mutation, replay payload or receipt semantics are added.
Maintenance remains best effort: unknown measurements or checkpoint failure
cannot reverse a committed receipt. A replaced owner cannot be confirmed current.

## Actual Linux verification

The initial regression failed on the unchanged implementation: a real
`SourceWriter::commit_curation` receipt correlated through the actual global
worker under WAL pressure produced zero checkpoint attempts instead of one;
the direct connection's private pending bit was also false. The post-COMMIT
marker fixes both observations.

Six focused tests exercise actual SQLite and the existing retained worker:

- Real source-owner receipt commit/readback, global correlation and one PASSIVE
  attempt above the 32 MiB soft watermark; exact replay and reads add no attempt.
- Private pending-bit checks independent of the throttle for exact replay,
  stale acquisition, payload conflict, malformed retained evidence, failed
  INSERT, COMMIT-hook rollback, and both retained row and byte limits.
- Denied checkpoint and post-commit measurement preserve the immutable committed
  receipt and its exact replay.
- A pinned external read-only snapshot remains valid across the new receipt and
  checkpoint, then sees the committed row after ending its transaction.
- Genuine shutdown and profile reacquisition with a fresh policy under renewed
  WAL pressure return the historical real source-derived receipt without a
  checkpoint; the old acquisition rejects.
- Unix root replacement before maintenance or inside its checkpoint callback
  closes the worker, cannot confirm current success, and leaves the committed
  receipt readable under the displaced original root.

```sh
cargo test --locked -p yield-library --lib operation_link_wal_ -- --test-threads=1
cargo test --locked -p yield-library --lib sample_sources::library -- --test-threads=1
cargo test --locked -p yield-library --lib -- --test-threads=1
bash scripts/gpui-development-checks.sh preflight
git diff --check
```

Fixture pressure is actual retained SQLite WAL from bounded 33 MiB payload
inserts, not a supplied scalar health flag. Synthetic requests and ledger-fill
rows in the classification test qualify rejection/storage behavior only; the
source-derived commit and restart scenarios use real immutable source receipts.
Ownership replacement tests use the existing worker gate/SQLite authorizer and
run only on Unix. They do not qualify Windows or any physical publication.

This finite production trigger does not complete OPT-1687/1686/1685/1348.
Other participant families, idle/hard-pressure/shutdown scheduling, durable
capacity/commit permits and native or physical qualification remain open.
No universal WAL size reduction or power-loss guarantee follows from PASSIVE.
