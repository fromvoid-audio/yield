# Explicit authored Harvest state (OPT-1831)

`GlobalLibraryWriter::harvest_authored_snapshot` reads one source-backed file and
at most 64 incident derivations through the retained global connection. The
complete typed values are capped at 64 KiB; fields are inspected before Rust
allocation. The snapshot has no public constructor or decoder. Graph changes
conservatively invalidate its compare-and-swap precondition.

`set_harvest_authored_state` queues one explicit New/Seen/Touched/Done/Ignored
intent. It requires the actual source writer's current full-content observation,
its audited revision/root/database/object/file facts and the receiving profile
acquisition. Source observation validation is read-only and uses weak owner
liveness. No pathname helper, source mutation, physical hash read, lease renewal,
GUI I/O, or universal journal obligation is fabricated.

Capture a stable ordinary metadata `OperationId` and persist `proposal_bytes()`
before dispatch. Restore exact canonical bytes with fresh observations for the
same source storage and key. The retained global worker validates its actual
profile/database capability independently of UUID correlation. A new effect
requires every historical source fact, whereas an existing exact receipt may be
replayed with later source facts. Reusing an operation for different bytes fails.

The SQLite transaction changes only the selected Harvest file and inserts its
immutable outcome. Existing unverified/different content yields IdentityConflict;
changed bounded metadata yields GraphChanged. Both are retained outcomes requiring
new user intent, never a successful mutation. Notes and discovery timestamps are
preserved. Explicit New resets seen/touched/done/ignored timestamps; other states
preserve unrelated timestamps and set the selected timestamp only if absent.
Exact row and receipt readback precede commit. Any mismatch rolls back.

The immutable ledger allows 8192 rows and 16 MiB including all stored fields and
scalar overhead; no automatic evidence eviction occurs. Existing exact replay
precedes retention/WAL admission and current-state checks, preserving later edits.
New effects and retained rejection outcomes use the actual retained SQLite WAL
admission gate (PR248); unknown/unhealthy observation rejects without mutation.
Input/result canonical encodings are capped at 64 KiB. Receipt types/lengths,
key and sequence uniqueness, canonical encoding, historical owner and the
payload/result/owner digest are checked before accepting replay.

Source/global SQLite transactions are independent. Validate before consumption,
precommit and acknowledgement; there is no cross-owner atomicity claim. A lost
reply or postcommit source change leaves an original stable proposal to retry.
`HarvestAuthoredCommitUncertain` means an outcome may already be durable.
Snapshot/mutation admission and terminal shutdown replies use the existing
bounded 32-command retained worker, not a separate writer.

This API implements backend user workflow metadata. There is no active GPUI
producer for the old compatibility `set_harvest_state` route; GPUI integration
and bounded status-read publication remain separate tickets. User `Done` is not
private journal completion. These real owner/SQLite tests seed metadata hashes;
they do not qualify physical publication, source removal, native storage durability,
or the entire Harvest saga. Those tracking scopes remain open. Native owner
regressions are tracked in OPT-2042 (macOS) and OPT-2043 (Windows).
