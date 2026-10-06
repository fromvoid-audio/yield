# Source prior-row typed decoder (OPT-2367)

The actual `SourceRegistryReferenceProposal::decode` checks its complete 16 KiB
wire ceiling before parsing. Its internally tagged prior-row expectation still
buffered arbitrary serde `Content` before validating a state and 32-byte row
identity. The shared type now uses direct typed decoding. Serialization,
canonical identity, public wire ceiling and downstream validation remain intact.
This is historical metadata; decoding restores no current owner, permission,
registration, lease or physical outcome.

The actual retained retirement consumer calls the same public decoder from
`SourceRegistryRetirementIntentArgs::proposals`. Its exact canonical base64
comparison and complete family validation remain unchanged. Actual profile-owned
SQLite tests inject malformed embedded expectations into a retained operation:
operation and typed claim readers refuse, while exact key/sequence/BLOB,
ordinary charges/releases, binding inventories, slices, autocommit and reopen
remain unchanged. These storage fixtures issue no current registration or grant.

## Evidence and compatibility

`scripts/source_expected_decode_probe.rs` links the exported production decoder.
Original malformed 6,000-byte arrays in `row_identity` and an unknown field have
12,769 and 12,764 wire bytes. Both requested a largest allocation of 262,144
bytes, cumulative 524,745 and 524,757 bytes. The exact-input 64 KiB diagnostic
failed. An earlier fixture accidentally changed an unrelated audit status and
failed before tracing; it is excluded. After implementation the same arrays
request largest 40 and 108 bytes, cumulative 120 and 396 bytes.

Thirty cases compare the exact original derive with the new decoder: arbitrary
map status order, escaped identifiers, tag-first struct sequences, absent and
present forms, all three states, historical singleton unit-state objects,
32-byte identity, duplicate and unknown fields, invalid shapes, extra sequence
items, missing tags/fields and trailing data. Actual public decode preserves
canonical identity for accepted map/array representations. Malformed data remains
refused; the order and text of malformed diagnostics can differ. Generic record
codec behavior and quotas are unchanged.

## Separate allocation components

The visitor owns no key, status string, `Value`, `Content`, container or vector.
Compiled guards require status/field identifiers and optional state at most one
byte each, optional identity at most 33 bytes, and the result at most 34 bytes.
The fixed locals and returned result can overlap; these individual layout guards
are not a whole stack bound. Array decoding writes into the inline 32-byte array;
state uses the existing externally tagged finite unit enum, without an internally
tagged buffering visitor. Unknown keys reject before reading their values; a
rejecting seed detects extra sequence items before traversing them.

At the actual public boundary plain keys/status are borrowed from caller input.
Pinned serde_json 1.0.151 `SliceRead` reuses one escaped-string scratch vector.
Its decoded length is bounded by the complete 16 KiB input. Pinned Rust 1.93.0
u8 growth plus escape reserve slack bounds backing at 32 KiB; during reallocation
an old backing can overlap with the new one (conservative 48 KiB combined).
These are pinned implementation observations, not public `Vec` guarantees.
The standalone shared type has no independent wire cap; the 16 KiB argument
applies to the actual public proposal caller and retirement's call to it.

Diagnostic allocation is separate. serde_core 1.0.229 unknown-field/variant
formatting copies the identifier plus a fixed finite expected-name list;
serde_json `Error::custom` creates a String, then `make_error` boxes its text.
Invalid string-type formatting can also escape input. Formatting growth,
reallocation and boxed conversion may overlap with parser scratch; they are not
covered by the inline payload or the malformed-array fixture's 64 KiB assertion.
Near-limit escaped unknown key/status inputs measure this separate actual path:
16,379/16,380 wire bytes, largest requests 15,672/15,688, cumulative requests
47,986/47,963. These exact observations do not establish a general formatter,
whole proposal, owner pool, SQLite baseline, allocator overhead or RSS bound.
Other owned subject/registration strings, media-root data, canonical output,
retirement base64 and record graph remain separate components.

## Verification and remaining acceptance

Run `cargo test --locked -p yield-library --lib source_expected_typed_decode`,
the existing source-reference unit and normal-build owner observation tests,
and `actual_versioned_binding_storage_child_suite` (its marked child executes
the retained-row preservation cases). Build the real library and link the probe
against the locked current serde_json fingerprint. Hold `/tmp/yield-cargo.lock`
for each whole build/check job, using the configured environment/shared target.
Full library/privacy/Linux/Windows/fmt and stable canonical preflight are required.

OPT-1828/2136/2194 and recording startup/terminal/native qualification remain
open. This component changes parsing allocation, not lifecycle admission,
physical settlement, required-WAL Start or shutdown authority.
