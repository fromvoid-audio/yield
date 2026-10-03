# Harvest authored receipt WAL maintenance (OPT-2223)

The retained Global library writer now marks its existing connection-local
maintenance policy immediately after a successful **new** Harvest authored
receipt transaction commits. Applied, GraphChanged and IdentityConflict are all
immutable committed receipt dispositions. The marker precedes the final Source
validation: loss there still returns HarvestAuthoredCommitUncertain, with the
actual durable receipt eligible for maintenance. No uncertain result becomes a
confirmation.

Snapshots and exact replay commit read transactions and never mark this policy.
Admission, ownership, Source validation, ledger capacity and transaction failures
before commit also leave it unmarked. The existing worker consumes the bit,
validates Global ownership, measures the retained connection WAL, and attempts
PASSIVE above its 32 MiB soft watermark with its existing 15-second throttle.
It revalidates ownership before delivering the reply. Checkpoint or measurement
failure cannot undo a committed receipt or convert it to a SQL failure.

The additive `wal_maintenance/tests/harvest_authored.rs` fixtures obtain genuine
content observations from retained SourceWriters. They exercise real SQLite and
the normal Global worker, including:

- Applied and both retained-rejection receipts, and direct pending-bit checks
  that cannot be masked by throttling.
- Snapshot/replay, stale owner or Source observation, receipt count/byte limits,
  unknown WAL admission, aborting INSERT and failing COMMIT exclusions.
- WAL pressure above 32 MiB and below the 64 MiB admission ceiling, pinned external
  readers, denied checkpoint and unavailable postcommit measurement.
- Actual Source metadata loss at SQL commit (uncertain receipt still marked),
  loss after worker execution, and profile replacement before/during maintenance
  (closed worker, retained original receipt, no confirmed replacement write).
- Fresh profile acquisition replay under fresh maintenance policy and WAL
  pressure, preserving the historical receipt and later authored state/note.

Run `cargo test --locked -p yield-library harvest_authored_wal --lib`, the existing
`harvest_authored` and `wal_maintenance` tests, the full Library suite, and
`bash scripts/agent.sh preflight`. Synthetic capacity rows exercise only ledger
limits; no request or Source authority is synthesized.

This is one production metadata commit trigger. It introduces no connection,
worker, counter, schema, lease, journal permit or capacity authority. Harvest copy
and reference triggers, idle/hard/shutdown scheduling, parent OPT-1687/1686/1685/1348
acceptance, and native or physical SQLite qualification remain open. Linux tests
do not establish power-loss, device, or native release qualification.
