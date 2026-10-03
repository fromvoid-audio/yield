# Owner-refenced partial collection directions (OPT-2187)

The existing browser curation history lane now permits explicit continuation of
known, exactly validated original/direction prefixes. Unknown or lost completions
remain unavailable; a generic audit projection still clears history and cannot
reconstruct operation identity from matching bits.

A partial record is the exclusive top for both controls. Its origin membership
map contains only original committed paths; its applied set contains paths whose
last acknowledged direction left them at their after bit. Resume selects the same
endpoint; reverse selects the opposite endpoint. Older undo/redo records cannot
pass the partial top. Stack movement completes only at a fully eligible endpoint.
An original uncommitted suffix never enters either direction.

Each control allocates a fresh nonzero request at the exact last acknowledged
revision and requests every origin path, including no-op paths. The genuine
catalog capture confirms that exact frozen requested set. Its returned changed
paths plus successful no-op omissions must reconstruct every acknowledged current
bit. Full source/root binding/database/lifecycle/lease provenance must match the
original private owner observation, and root/directory/revision, original clock,
cancellation and publication memory fences must remain valid. Ordinary owner
execution independently checks the complete owner before each actual batch.

The preparation creates explicit Add/Remove commands only for changed eligible
paths, in immutable origin order, and freezes their new receipt partitions before
admission. No toggle or whole-row restoration occurs. Actual ordered receipts
settle only their acknowledged prefix. A zero-prefix failed direction retains an
exclusive partial record; zero-prefix original failure creates no new record.

Known prefix settlement advances the retained metadata projection revision only
after exact history receipt validation. This makes fresh capture possible without
a generic refresh or relaxing ordinary edit rejection. The known failed lineage
is cleared only after genuine direction refencing and mutation admission, or a
successful genuine no-change endpoint confirmation. An empty eligible set is a
local endpoint transition: it issues no mutation and manufactures no receipt.

Preparation/admission refusal keeps stack positions and acknowledged bits. Source
replacement/reacquisition, intervening revision or audit changes refuse mutation;
matching numeric values cannot establish original issuer provenance. Cancellation
before mutation admission submits no edit. Lost direction results remain unknown
even when actual SQLite readback matches an endpoint; a late genuine completion
cannot reconstruct the retired record.

The existing 128-record/16 MiB history budget includes pending/direction/subset and
presentation capacity. Requests retain ordinary 512-batch/65536-update limits.
Source captures stay asynchronous; UI/render handlers read no SQL or filesystem.
Controls show Resume/Reverse labels for partial directions and remain unavailable
while capture/mutation is pending or completion is unknown. Completed endpoints
restore surviving selection, focus and viewport. Canonical owner timestamps still
advance for actual collection decisions; historical timestamps are never replayed.

The actual profile-owned CatalogService/SQLite GPUI fixture exercises first,
middle and last Undo/Redo failures, both retry and reversal, exact eligible receipt
counts and persistent readback, all-origin capture, zero-change local endpoints,
duplicates, cancellation, real revision/audit changes, owner shutdown/reacquisition
and lost direction delivery/readback. Original-prefix fixtures now undo/redo only
their genuine origin prefix and preserve the uncommitted suffix. Models supplement
this production evidence.

```sh
cargo test --locked -p yield-ui --lib collection_history -- --test-threads=1
cargo test --locked -p yield-ui --lib
bash scripts/agent.sh preflight
python3 scripts/internal/check/check_gpui_action_register.py --check
```

These are explicit headless GPUI/profile-owned SQLite fixtures, not physical
qualification. Normal protected-reserve startup and native UI acceptance remain
OPT-2026/2143. Durable restart/journal OPT-2115/2127 and tracking parents
OPT-2114/1617/1476 remain open.
