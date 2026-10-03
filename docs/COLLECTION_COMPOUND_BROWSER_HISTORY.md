# Compound browser collection history (OPT-2186)

The browser's existing collection curation history lane now uses the production
[retained owner execution](COLLECTION_COMPOUND_OWNER_EXECUTION.md) from OPT-2232.
It captures confirmed before-state asynchronously before submitting an original
edit or a fully eligible direction. Render callbacks inspect cached identities,
clock/cancellation and memory publication liveness; they read no SQL/filesystem.

Preparation freezes one slot's exact path/target set, source/lifecycle/request,
root/directory/revision and selection/focus/viewport. The catalog supplies genuine
original issuer, root binding, database and lease provenance. The UI validates the
frozen requested set and changed bits; owner execution independently checks the
original complete owner before every batch. Replacing the owner cannot borrow
history eligibility from the ordinary mutation route's freshly qualified writer.

History accounts conservative owner/plan/subset/restore storage, identity/path
bytes and the actual selection HashSet capacity before cloning. One prepared
group is one logical record; the lane is at most 128 records and 16 MiB. A capture
that cannot fit with the retained lane refuses coverage before allocation.
Ordinary curation can still use its existing 512-batch/65536-update/16 MiB path/
32 MiB receipt limits, with explicit undo-unavailable status after acknowledgement.
Read preparation or mutation admission rejection does not move settled stacks.
Only settled records can be evicted; in-flight and partial records are protected.

The normalized UI outcome preserves the actual typed failed batch index/error
and opaque preparation identity. Settlement validates exact ordered batch
pathsets, explicit target membership, source/lifecycle, root/directory, initial
revision, each genuine increment, terminal revision and prefix/failure index.
It projects receipts only after this validation. A same-count replacement with
an original suffix path is rejected before updating confirmed lineage.

An original failure records only its genuine committed prefix. `bits` represents
origin committed paths; `applied` represents the acknowledged current after-bit
subset. Immutable owner partitions and provenance remain shared. Neither a
failed nor an unattempted original suffix becomes redo work. Status reports
confirmed/committed and remaining batch/path counts with the actual failure.
Zero-receipt failure creates no history effect. A partial record stays exclusive
and its directions remain disabled pending OPT-2187's explicit refencing/retry.

Successful full groups support the existing Undo/Redo controls with fresh
complete-owner capture and field-only Add/Remove commands. Selection, focus and
viewport restore from original presentation; ratings, lock, playback history,
tags and other collection slots are never restored as an entire metadata row.
The existing source owner advances `last_curated_at` for collection decisions;
history never replays a historical timestamp. Uncommitted paths retain their
prior timestamp and unrelated field values.

Lost or incoherent terminal results retain an unconfirmed history marker through
fresh catalog projection. Readback equality cannot reconstruct the missing
record. Delayed/duplicate results are drained without adding eligibility.
Untracked ordinary acknowledgement retires unsafe history/redo continuation.
Source changes cancel unadmitted preparation; channel/profile loss releases its
presentation, and original capture timing includes queue delay. Publication
memory liveness is checked again immediately before UI consumption.

Real headless GPUI fixtures submit active browser actions, consume actual catalog
capture/commits, click Undo/Redo (including disabled partial controls), and reopen
the actual profile-owned database. They cover 257-row full and first/middle/last
failures, exact persistent prefixes, typed outcomes, unrelated fields,
presentation, lost delivery plus genuine readback, malformed exact pathset using
an actual owner result, and oversized selection capacity with ordinary edit
fallback. Existing model tests supplement this evidence.

```sh
cargo test --locked -p yield-ui --lib collection_history -- --test-threads=1
cargo test --locked -p yield-session catalog_history -- --nocapture
```

This implements portable source/UI behavior through explicit catalog fixtures.
Normal native protected-reserve startup and physical UI acceptance remain
OPT-2026/2143. It does not complete partial direction OPT-2187, durable restart
OPT-2115, journal OPT-2127 or tracking parents OPT-2114/1617/1476. Fixture restart
readback verifies persistence, not reconstructed durable undo history.
