# Original chunk-claim scalar projection (OPT-2399)

The existing single-row and paged chunk readers decode the original SQLite Row
through borrowed text. UUID/device/class/state/optional-identity parsing requires
no owned input strings. Only a valid output key is copied, after all existing
scalar checks and its existing 1024-byte bound pass. Unknown closed class/state
diagnostics show at most 64 Unicode characters instead of copying arbitrary
stored field contents. Valid records, UUID parser acceptance, optional identity
semantics, native type/UTF-8 error selection and existing row limits are preserved.

Original file-backed fixtures exercise legal states and malformed scalar shapes;
the diagnostic-only original decoder records the previous large-field behavior.
The normal-build System allocator probe compares both decoders against the same
original stored bytes, outside fixture setup, and checks byte preservation.
SQLite native allocations/RSS are outside its Rust measurements. The opaque
test-support fixture cannot issue identities, permits, effects or settlement.

This covers scalar preprojection only. Complete chunk audit page/operation and
diagnostic funding, database-attempt/full-progress/Recording readers, startup
ledger custody, all-owner admission, full Legacy adoption, native qualification
and genuine settlement remain separate open requirements. No schema, quota,
arbitrary new row limit, retry, reservation refund or authority is introduced.
