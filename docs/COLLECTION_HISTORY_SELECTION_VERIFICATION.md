# Retained-owner collection history preparation (OPT-2138)

`SourceWriter::capture_collection_history_selection` reads confirmed before bits
for one collection slot and explicit desired after bits through the already
retained source connection. It privately constructs a non-deserializable
`SourceCollectionHistorySelection` containing the complete `SourceRevision`,
audited directory generation and sorted changed paths. No-op paths are omitted.

The synchronous primitive belongs on an owning worker, never a render callback.
SQLite work has a two-second progress deadline; an OS identity check can still
stall while the worker retains custody. Caller/queue isolation for a future
history service is a separate consumer requirement. This API does not enqueue
or start another source writer.

Input is at most 128 distinct paths, 4,096 bytes/path, one valid collection slot and
1 MiB of conservatively counted owned input/result payload. Counting precedes
normalization and retention. The method holds the existing process extension
exclusion and verifies the compiled known schema/runtime; unsupported callbacks
or schema are rejected. Inside one read transaction, metadata strings are bounded
before the existing curation-precondition validator reads them. That validator
requires current audited root/revision, one complete directory publication, no
staged audit and supported live paths. Collection slots use integer decoding and
seven-row lookahead, rejecting malformed/duplicate/out-of-range values.

Complete revision identity is checked before and after the read snapshot. The
method rolls back its read transaction and never calls mutation-qualified lease
renewal, DDL, checkpoint, intent creation or curation commit. Later admitted
history preparation must reread/revalidate these observations in its owner
transition; they grant no capacity, undo or replay authority.

Actual profile-owned SQLite tests cover mixed membership/no-op/add/remove,
full 128-path audited capture and deterministic order, path/batch/slot/payload
limits, missing/unsupported paths, malformed integer/BLOB/audit cells, staged
directory state, unknown trigger schema, stale revision/lifecycle, near-expiry,
expired and actually reacquired leases, and an already-expired deadline before
SQL. A query-only
connection and SQL authorizer prohibit UPDATE/DDL/checkpoint, with negative
controls; database/WAL bytes and total_changes remain unchanged. The positive
cases use real retained SourceWriter/audit transactions, not a model stack.

Tests execute bounded subprocess cases because optional SQLite extension loading
taints the process even after failed initialization. The ignored child driver is
explicitly invoked by each normal parent test; its output must prove the actual
case ran. No production flag resets extension evidence.

```sh
cargo test --locked -p yield-library collection_history_selection
cargo test --locked -p yield-library --lib
cargo check --locked -p yield
bash scripts/agent.sh preflight
```

This is production read-only preparation, not history persistence or an active
GPUI restart consumer. OPT-2127/2128/2129, OPT-2115 and OPT-1476 remain open under
[METADATA_HISTORY_PERSISTENCE_CONTRACT.md](METADATA_HISTORY_PERSISTENCE_CONTRACT.md).
Native namespace, SHM, network-volume and power-loss qualification remain separate.

On Unix, literal-backslash paths cannot alias slash-separated database keys;
the full-batch actual-owner regression rejects that spelling before SQL.
