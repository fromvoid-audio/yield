# Recording pre-start database-claim backing

The original recording preparation read uses the original consistent transaction and
complete database-claim audit. Nonempty binding and slice rows are supported with
their existing 8,192-row ceilings, 64-KiB binary inventory, 64 databases, 256 slices
and 64 total participants. Nothing is truncated or reconstructed into live authority.

The four-MiB owner pool and its separate 64-KiB control reservation are unchanged.
The original 3,604,480-byte recording command reservation is unchanged. The
incremental database backing is 528,088 bytes, but does not overlap the initial
two-tree record decoder. The database audit peak is 3,411,672 bytes after reusing
the released 720,896-byte tree category. Compile-time guards cover actual typed
layouts and both stage peaks within the original command debit.

| Additional category | Bound |
| --- | ---: |
| Binding vector: 64 × 1,024 | 65,536 |
| Slice vector: 256 × 256 | 65,536 |
| Participant vectors: 64 × 128 | 8,192 |
| Owned strings, disjoint within the original V1 binary input | 65,536 |
| Binding IDs, physical keys, participant IDs and slice IDs: 448 × 512 | 229,376 |
| SQL key pages: (64 + 257) × (144 + 3 × 24) | 69,336 |
| Physical-key/readback string payloads: 64 × 256 | 16,384 |
| Bounded row, query and temporary context | 8,192 |

These bounds include malformed count prefixes up to the original decoder ceilings.
Each validation-set entry is charged enough for a complete B-tree node, including
the initial allocation. The audit pages operations and keeps one inventory at a time. The outer 64-key
operation page and inner 257-key claim page can overlap. Before UUID parsing,
SQLite's 36-code-point ID check can admit 144 UTF-8 bytes; vector growth is charged
three 24-byte String headers per entry. Physical-key/readback payloads have their
own allowance: the B-tree node charge does not cover cloned string bytes. V1
validation currently borrows main_file, but the allowance conservatively covers
64 complete 256-byte physical keys alongside both ID pages.

The scoped complete audit strictly decodes each operation record first, before
owning its database inventory. It then reads the original inventory and applies
the unchanged semantic helper to the already-decoded record. The initial two-tree
record decoder and binary inventory therefore cannot overlap. Subsequent command
digest calculations materialize one JSON tree at a time; their required and
optional encodings are sequential, not retained together. Reusing the released
second tree category covers all 528,088 bytes above without enlarging the debit.

During binary decoding and readback, the original 384-KiB encoding category covers
the encoded input, canonical comparison, expected readback image, digest encoding
and copied root image. The original typed-record, local-proposal, ordinary-snapshot
and auxiliary categories remain charged while their owners are live. Ordinary
unscoped database readback retains its previous audit order and acceptance.

Initial preparation peaks at 3,604,480 bytes. Revalidation keeps the original
458,752-byte producer debit throughout: its record phase is bounded by 4,063,232
bytes and database phase by 3,870,424 bytes, both below the original 4,128,768-byte
ordinary quota. A compile-time guard checks the two original live debits together;
a real original-queue test holds the producer-sized credit through a revalidation
reply and verifies temporary release while the lifetime credit stays held. That
unissued facade still refuses and creates no producer or Source authority.

Before any original inventory copy, scalar SQL checks its type, original length,
relation ceilings and magic. The existing pre-start record decoder only supports
V17 Recording. A valid matching inventory therefore has canonical V1 encoding:
V2 lease scope and V3/V4 retirement bindings require V18 retirement semantics.
The magic check excludes those unsupported nested allocations; it does not replace
the original binary decoder, exact row readback, command/participant/claim/plan
comparisons, actual journal-descriptor volume check or namespace validation.

Database attempt start/outcome and full-progress rows remain refused. Their larger
JSON and retained audit maps still need independent backing implementation. No
recording Start, required permit, retry, refund, terminal seal, physical qualification,
publication or genuine settlement is completed by this read-only audit support.
