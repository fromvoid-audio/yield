# Historical Harvest-copy receipt lookup (OPT-2348)

`GlobalLibraryWriter::observe_harvest_copy_receipt` reads one exact operation and
Harvest work identity on the original retained global worker. The current profile
and original database ownership are checked at the existing dispatch/reply
boundaries. It does not restore a live Harvest request, source/enrollment observation,
journal owner, execution permit or private completion from stored bytes.

The separately typed `HarvestCopyReceiptObservation` is at most 192 bytes and
retains only original operation/work, sequence, historical acquisition, disposition,
edge, destination revision and proposal digest. `None` means this exact historical
row was not found in this snapshot; it proves no absence of an unacknowledged
future commit, physical effect or recovery obligation. An `Applied` observation
means an immutable global metadata result, not publication or settlement.

One read transaction covers bounded table cardinality, exact key and sequence
uniqueness, SQL type/length checks before materialization, original payload/result/
owner integrity digest, canonical payload/outcome decoding and exact historical
profile scope. At most 8192 retained rows are accepted by this reader; selected
payload/result buffers are each capped at 65536 bytes. A temporary 256KiB
SQLite value/record limit also caps native comparison allocation for a corrupted
unindexed foreign key; it preserves an existing lower limit and restores the
original connection limit before commit, on refusal and during worker unwind. Malformed, ambiguous,
oversized or foreign history rejects without changing it. Original effect/replay
capacity policy remains unchanged, including exact replay before new-receipt
capacity checks. The new read command advances neither the original mutation
fence nor the Harvest read generation and adds no commit/maintenance marker.

Portable original-queue tests execute the existing producer with the explicitly
logical `cfg(test)` journal fixture, then exercise lost reply, exact historical
read, missing/wrong keys, real global/profile reacquisition without source or
journal custody, corruption, rehashed foreign scope, duplicate and row-limit
refusal. They preserve original graph/receipt state and original exact replay.
These fixtures supply no physical authority.

`harvest_receipt_process.rs` has a normal-build Linux missing-history case using
an actual global owner, owned child process death and real profile/global
reacquisition. Its native producer branch is compiled on Linux and Windows with
`test-support` disabled. That branch uses actual source manifest/audit/content/
enrollment owners, public authority-free journal intent persistence, production
journal startup and frozen-copy observation, registry/reference prerequisites,
original Harvest commit, deliberately lost reply and actual historical lookup.
Its input source-index identities remain metadata, not held-object capabilities.
The parent kills and reaps only its own child and reads history through the new
actual queue without resurrecting the child's source or journal custody.

Actual native positive execution requires macOS protected-reserve/bootstrap
resources and remains OPT-2350. Run, on the verified source merge:

```sh
cargo test --locked -p yield-library --test harvest_receipt_process native_committed_harvest_receipt_survives_owned_process_death -- --exact --nocapture
```

Require one nonzero native parent pass; Linux zero selected tests does not count.
This is metadata process-death evidence, not power-loss, physical publication,
origin absence/retirement, retry/refund or settlement qualification. Full
OPT-1645/1666/1670 and publication/cleanup acceptance remain open.
