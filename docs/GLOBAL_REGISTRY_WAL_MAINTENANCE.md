# Global registry receipt WAL maintenance

OPT-2172 extends the production retained global worker policy implemented by
OPT-2150. `LibraryDatabase::apply_source_registry_participant` marks maintenance
only after committing a **new** immutable receipt. Both `Applied` and
`RegistryChanged` write a receipt; the latter is a retained semantic rejection,
not a read-only failure. Exact replay returns the historical receipt after an
empty transaction and does not mark maintenance.

The private `note_registry_participant_commit` uses the existing connection-local
pending bit and checkpoint policy. The existing worker consumes that bit before
its final profile validation and reply. Ownership is validated before maintenance
and again afterward. No connection, worker, path-based WAL measurement, schema,
per-operation map or caller-supplied authority is added. The existing worker hook
name remains stable for preparation and dispatch integration.

The shared policy observes the retained SQLite WAL handle, attempts PASSIVE only
above 32 MiB, throttles attempts for 15 seconds per connection, and skips active
transactions. A pinned reader can retain frames. Checkpoint or observation failure
is best effort and preserves the committed receipt; profile ownership loss still
closes the worker and prevents confirmation as the current owner. Failed SQL
before commit rolls back the registry and receipt without marking maintenance.

## Verification

The private `registry_wal_` tests exercise actual SQLite and the normal global
worker, not a modeled checkpoint outcome:

- New applied and retained-rejection receipts above the real WAL threshold.
- Denied checkpoint SQL and denied post-commit WAL observation with committed
  receipt and registry readback.
- Direct pending-bit inspection for exact replay, conflicting payload, stale
  acquisition and rolled-back receipt INSERT; throttle cannot conceal a trigger.
- An external read-only connection retaining its old receipt snapshot during
  PASSIVE maintenance.
- Profile-root replacement before maintenance and during checkpoint SQL, using
  the existing worker scheduling hook and SQLite authorizer (Unix cases).
- Shutdown and genuine profile reacquisition, then exact replay above the real
  WAL threshold with a fresh policy: historical receipt and later registry edits
  are preserved with zero checkpoint attempts.

Run `cargo test --locked -p yield-library --lib registry_wal_ --
--test-threads=1`, the relevant library tests and `bash scripts/agent.sh checks`.
The first applied-receipt regression fails without the production marker (zero
checkpoint attempts instead of one).

## Limits

This trigger is production implementation, but neither a receipt nor its
maintenance marker grants capacity, journal intent, live Source lifecycle
ownership, WAL preparation, required-work permits or physical completion. The
pending bit is not a global mutation counter. Other participant families,
idle/hard-pressure/shutdown scheduling and native physical qualification remain
tracked by OPT-1687, OPT-1686, OPT-1685 and OPT-1348. Unix rename tests do not qualify
Windows handle/share-mode behavior, and these tests make no power-loss or disk
allocation recovery claim.
