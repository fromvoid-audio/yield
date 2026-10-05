# Recording retained-value decode boundary (OPT-2347)

Actual captured chunks, empty terminal reservations, prefix inventories and prefix seals
now deserialize directly into their closed typed representations. They retain the existing
128 KiB recording-value wire ceiling, 4 KiB control ceilings and semantic validators.
The decoder does not authorize capture, a terminal transition, physical effects or settlement.

The original path first constructed a generic `serde_json::Value` for every PCM byte, then
converted that tree into `Vec<u8>`. The actual maximum 16 KiB PCM regression contains all byte
values and produces 58,960 wire bytes. On Rust 1.93.0 / locked serde 1.0.229 and serde_json
1.0.151, the standalone probe linked to the production public chunk decoder observed:

| Original decoder | Largest allocation request | Cumulative allocation requests |
| --- | ---: | ---: |
| Generic tree followed by typed conversion | 524,288 bytes | 1,077,789 bytes |
| Direct typed decoding and original validation | 16,384 bytes | 40,994 bytes |

These are allocator request measurements for this exact input, not resident-memory or
physical qualification. The original parser-entry regression and allocator assertion both
failed before the change. The same input and assertions pass afterward. The allocator lives
only in the standalone probe executable; it is not installed in the production library.

## Closed field graph and compatibility

The private `StreamingRecordingValue` marker admits only `RecordingCapturedChunk`,
`RecordingControlReservation`, `RecordingPrefixInventory` and `RecordingPrefixSeal`.
Every nested struct rejects unknown fields, including the work key and accounting fields.
Enums are unit or externally tagged variants. There are no arbitrary JSON values, maps,
flattened fields, internally tagged enums or untagged enums in these four complete graphs.
Consequently serde does not buffer PCM bytes through its generic `Content` representation.

Their complete accepted value depth is at most four, with the root at zero. This includes
serde's historical array-shaped structs and object-shaped unit enum variants. Wrong scalar/container shapes fail
at their typed visitor, before traversing a nested graph. The earlier depth-24 guard thus
excludes no additional valid representation for these types. Direct `from_slice` checks the
entire input, rejects trailing tokens and duplicate known fields (including escaped aliases),
and preserves valid whitespace, escaped keys and array-shaped historical data. Unknown-field
refusal occurs before constructing the unknown value. Existing proposal/chunk/role/accounting
and immutable binding validations still run after decoding; decoded observations confer no
authority.

`RecordingProposal::decode` deliberately keeps the original strict generic parser and depth
check. Proposal participant descriptors can include an optional stored lease scope whose audit
observation uses an internally tagged enum. Its `Content` buffering and complete participant,
string and clone accounting require separate work. This change does not claim that proposal
decoding streams, or weaken its accepted historical schema.

## Allocation components and limits

The sole variable payload in a streamed captured chunk is its `Vec<u8>`. Fixed 32-byte hashes
and the work key stay inline. With the unchanged 131,072-byte wire ceiling, even a malformed
array of otherwise valid one-digit bytes can contain fewer than 65,536 entries: each entry
requires a digit and, except the first, a comma, before any enclosing chunk fields. JSON's
typed sequence has no advertised size hint. The pinned Rust 1.93.0 `Vec<u8>` growth policy
(minimum nonzero capacity eight, subsequent doubling) therefore reaches at most 65,536 bytes
of byte-vector backing before a semantic refusal. Valid chunks retain at most 16,384 bytes.
The vector growth peak includes both old and new backing; this component bound is not a
complete decoder or owner-input memory proof.

Reservations and prefix controls have fixed typed payloads, including fixed hash arrays,
UUIDs and numeric accounting. Escaped JSON strings still use serde_json parser scratch;
invalid keys/scalars can allocate error messages. Chunk validation also encodes the already
validated proposal to compare its exact digest. That trusted encoding, proposal ownership,
scratch/error backing, result pages, original worker/channel custody and other retained
inputs must be accounted at their actual caller. The narrow pre-start factory's separate
complete-inventory accounting remains documented in `RECORDING_PRESTART_MEMORY_BOUNDARY.md`.
This leaf neither increases a pool ceiling nor proves a whole owner pool, SQLite baseline,
allocator overhead, stack, RSS or native physical bound.

## Reproduction and actual storage evidence

Run focused behavior with `cargo test --locked -p yield-library --lib recording_typed_decode`.
Five tests cover dense maximum PCM, noncanonical and array-shaped historical chunks, all four
reservation roles, actual hound prefix inventories/seals, nested duplicates, unknown/trailing/
deep hostile data, and actual retained SQLite rows. SQLite read/rebuild/reopen preserves valid
noncanonical bytes and refuses conflicting duplicate data while retaining every original
record, key and sequence. These tests use untrusted storage observations, not source grants.

Build the actual library and compile `scripts/recording_typed_decode_probe.rs` against it:

```bash
cargo build --locked -p yield-library --lib
rustc --edition=2024 scripts/recording_typed_decode_probe.rs \
  --extern yield_library="$CARGO_TARGET_DIR/debug/libyield_library.rlib" \
  --extern serde_json="$CARGO_TARGET_DIR/debug/deps/libserde_json-HASH.rlib" \
  -L dependency="$CARGO_TARGET_DIR/debug/deps" -o /tmp/recording-typed-decode-probe
/tmp/recording-typed-decode-probe
```

Choose `HASH` from the current locked serde_json library fingerprint. In the shared execution
environment, hold `/tmp/yield-cargo.lock` over the entire job and touch the current worktree's
library entrypoint after acquiring it to force correct shared-target provenance. Proposal and
wire construction finish before measurement; only the actual public chunk decode and its
unchanged validators run while allocation tracing is enabled.

Full recording startup, required-WAL admission, original private terminal capability,
Finalizing/Frozen/seal storage and genuine physical validation remain open under OPT-2145,
OPT-2058 and their actual prerequisites. Genuine macOS custody execution remains OPT-2330.
