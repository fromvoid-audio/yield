# Source audit typed decoder boundary (OPT-2355)

`SourceRegistryReferenceProposal::decode` already checks its complete 16 KiB wire
ceiling before parsing. Its historical `SourceRegistryAuditObservation` derive
nevertheless buffered arbitrary nested values through serde's internally tagged
`Content` visitor before rejecting a wrong numeric field or an unknown field.
The shared audit type now deserializes directly through finite typed identifiers
and two inline `u64` fields. Serialization and source observation validation stay
unchanged. Metadata decoding does not recreate a capture, registration, pin,
current source revision, permit or effect authority.

## Actual original evidence

The standalone probe links the real public source-reference proposal decoder.
It validates unrelated scalar metadata before measurement, then measures the
actual refusal of a 6,000-element byte array in either audit `revision` or an
unknown audit field. Original inputs have 12,767 and 12,766 bytes, below the
existing 16 KiB ceiling. On the pinned toolchain the original decoder requested
a largest allocation of 262,144 bytes for either case, with cumulative requests
524,662 and 524,808 bytes. The exact-input 64 KiB diagnostic assertion failed.
The initial probe's incorrect transition capitalization failed before tracing
and is excluded from this evidence.

Afterfix the identical revision-array refusal requests at most 50 bytes, with
cumulative requests 286 bytes; unknown-array refusal requests at most 108 bytes,
with cumulative requests 447 bytes. The exact-input assertion passes. The
standalone allocator is not installed in the production library. Request sizes
are not resident memory, allocator overhead or physical qualification.

## Historical representations

The original serde 1.0.229 `TaggedContentVisitor` has both map and sequence
branches. The new visitor preserves status anywhere in a map and tag-first
sequences: `["unavailable"]` or `["captured", revision, directory_generation]`.
Finite variant and field identifiers retain their original serde decoding
behavior; numeric JSON tags remain refused. All known-field duplicates,
including escaped aliases, unknown fields, missing captured fields, captured
fields on unavailable values, invalid numeric types and extra sequence items
remain refused. A rejecting seed detects an extra sequence item before parsing
its nested value. Map keys reject unknown identifiers before their values.

Tests compare against the exact original derive and exercise the real public
proposal decode and canonical identity. Test scalar JSON creates metadata only.
Existing normal-build source-owner observation tests remain required; codec
fixtures alone do not qualify lifecycle or physical behavior.

## Separate allocation components

The audit visitor retains a finite status identifier and two `Option<u64>`
values on its stack, then returns the fixed audit enum. It owns no container,
key string, status string or byte vector. Plain JSON key/status text is borrowed
from the caller's input. Compiled layout guards require identifiers at most one
byte, each `Option<u64>` at most 16 bytes, and the returned audit enum at most
32 bytes; these fixed headers are not a total Rust stack bound. Escaped text
uses serde_json 1.0.151 `SliceRead`'s one
reused scratch `Vec<u8>`: `parse_str_bytes` extends source fragments, and
`parse_escape` appends decoded bytes or reserves at most four UTF-8 bytes.
Decoded scratch length cannot exceed the complete 16 KiB source-reference
input. Pinned Rust 1.93.0 u8-vector growth, including escape reserve slack,
keeps its backing at most 32 KiB. A reallocating allocator may temporarily
retain the old backing while requesting the new backing; those lifetimes must
be counted separately. These implementation bounds are pinned evidence, not
general guarantees of Rust's public `Vec` API.

Parser scratch is distinct from diagnostic allocation. `unknown_field`,
`unknown_variant` and invalid-type formatting can copy and escape a long
identifier into an owned error message. Debug escaping and formatting growth
can exceed the fixed visitor headers and the decoded identifier length; boxed
codec errors can retain or copy that text. The standalone probe therefore
measures maximum-size escaped unknown key/status inputs separately from the
malformed-array diagnostic. It does not assert that all error allocations fit
the array fixture threshold or pretend error backing is inline. Actual near-limit
inputs contain 16,379 and 16,381 wire bytes; measured largest requests are 15,672
and 15,694 bytes, with cumulative requests 48,037 and 48,021 bytes respectively.
Those measurements include real parser scratch and diagnostic formatting for
these inputs, not a general error-allocation bound.

The actual caller also owns ordinary subject strings, media-root bytes and
canonical encoding. The prior-row expectation now has its separate
[typed decoder component](SOURCE_EXPECTED_TYPED_DECODE_BOUNDARY.md). Recording proposals still retain their strict generic tree parser,
and participant/string/backing bounds remain unfinished. No complete proposal,
owner input pool, SQLite/runtime baseline, stack, RSS or native bound is claimed.

## Reproduction and remaining acceptance

Run `cargo test --locked -p yield-library --lib source_audit_typed_decode` and
the existing `source_registry_reference` unit/normal-build owner tests.
Build the actual library, then link `scripts/source_audit_decode_probe.rs` in
the same way as `scripts/recording_typed_decode_probe.rs`, selecting the current
locked serde_json library fingerprint. Source the execution environment, use
the shared target, and hold `/tmp/yield-cargo.lock` across the whole job. Touch
the current worktree's library entrypoint only after acquiring that lock.

Full OPT-2145/2058 startup, required-WAL admission, original private terminal
capability, Finalizing/Frozen/seal integration, source-registry lifecycle
OPT-1828 and genuine macOS custody OPT-2330 remain open. This component neither
adds a resource ceiling nor substitutes observations for genuine settlement.
