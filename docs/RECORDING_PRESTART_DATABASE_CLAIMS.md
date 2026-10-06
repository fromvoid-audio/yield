# Recording pre-start database-claim backing

The original recording preparation read uses the original consistent transaction and
complete database-claim audit. Nonempty binding and slice rows are supported with
their existing 8,192-row ceilings, 64-KiB binary inventory, 64 databases, 256 slices
and 64 total participants. Nothing is truncated or reconstructed into live authority.

The four-MiB owner pool and its separate 64-KiB control reservation are unchanged.
The original 3,604,480-byte recording command reservation now includes 499,712 bytes
for the incremental database audit, giving 4,104,192 bytes below the original
4,128,768-byte ordinary quota. Compile-time guards cover the actual typed layouts.

| Additional category | Bound |
| --- | ---: |
| Binding vector: 64 × 1,024 | 65,536 |
| Slice vector: 256 × 256 | 65,536 |
| Participant vectors: 64 × 128 | 8,192 |
| Owned strings, disjoint within the original V1 binary input | 65,536 |
| Binding IDs, physical keys, participant IDs and slice IDs: 448 × 512 | 229,376 |
| SQL pages, readback columns, temporary keys and query context | 65,536 |

These bounds include malformed count prefixes up to the original decoder ceilings.
Each validation-set entry is charged enough for a complete B-tree node, including
the initial allocation. The audit pages operations and keeps one inventory at a time.

During binary decoding and readback, the original 384-KiB encoding category covers
the encoded input, canonical comparison, expected readback image, digest encoding
and copied root image. `database_claims::read` releases these buffers before its
caller decodes the operation record. The decoded inventory remains live and funded
by the additional category while the original record/tree/validation-clone/proposal
categories cover record comparison and command digest calculation. The retained
ordinary snapshot and local proposal keep their existing categories throughout.

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
