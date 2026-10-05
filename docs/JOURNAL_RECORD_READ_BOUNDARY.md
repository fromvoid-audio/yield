# Journal point-record read boundary (OPT-2331)

The actual `OperationJournalStore::read_row` SELECT keeps original key, sequence,
storage type and length observations, but projects the record BLOB only when its
SQLite type is `blob` and its byte length is within `model::MAX_RECORD_BYTES`.
The CASE guard runs in the same SQL statement before Rust asks for the BLOB;
there is no separate size query or a window between a precheck and the copy.
Oversized or wrong-type values project NULL while their original metadata remains
available for the existing retained-record error classification.

No row, sequence, bytes, schema or transaction policy is changed. Valid bounded
records use the original strict decoder and readback. The existing 1 MiB maximum
is preserved, including retained bounded unknown bytes that fail codec checks.
The guard does not create a permit, authorize retry, refund a reservation or
settle a database attempt.

Actual SQLite tests measure callback BLOB-copy bytes through a private test-only
thread-local diagnostic. Before the change, an oversized `zeroblob(1048577)` was
copied in full before rejection; the regression failed with 1048577 instead of
zero. After the change it is refused with zero copied bytes, unchanged original
key/sequence/bytes and autocommit state, including after reopening. INTEGER,
REAL and TEXT records remain unchanged and never become BLOB projections. Valid
records still decode identically, and the full 1048576-byte boundary remains
available for retained codec failures. Fixture constraints are restored after
constructing malformed rows; no production validation is bypassed.

This is the raw point-read boundary only. The decoded JSON tree and complete
ordinary/release/provider rebuild inventory need their own checked backing and
original worker credits. OPT-2326 remains open for that factory/revalidation
accounting and genuine positive native custody acceptance; these read tests
supply no startup, terminal, physical qualification or settlement authority.
