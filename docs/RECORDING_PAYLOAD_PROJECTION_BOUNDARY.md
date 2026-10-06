# Retained recording payload projection (OPT-2372)

The original Store control and captured-chunk readers previously checked SQL type
and length in one statement, then copied `record` through a separate statement.
The second statement now independently projects a BLOB only when its length fits
its existing limit: 4,096 bytes for a control reservation or 131,072 bytes for a
captured chunk. SQLite's `CASE` protects the actual payload returned to rusqlite
before its `Vec<u8>` conversion. A preceding scalar observation is not the copy
bound. This does not make the multi-query reader a consistent snapshot.

Supported payloads still pass through the original decoders, proposal/key/slot
checks and control inventory checks. Historical noncanonical JSON remains
accepted. Empty BLOBs reach the existing decoder and retain its error. A changed
wrong SQL type retains the original `InvalidColumnType` index, name and type
without projecting its value. An oversized changed BLOB retains the decoder's
existing size error; the narrow pre-start scope preserves `ResourceBound`.
Unchanged malformed rows retain their earlier scalar-check refusal. No production
reader writes, skips malformed evidence, retries, releases a charge or changes a
schema, retention ceiling or owner pool.

A private test-only callback runs after the actual metadata query and before the
actual payload query. It changes the original retained SQLite connection, then
captures every `operation_journal*` table. The copy diagnostic counts only the
actual reader's materialized payload bytes after this boundary; the test snapshot
and later assertion reads are separate test backing. Both malformed readers must
preserve all captured rows, keys, sequences and stored claim/release evidence,
autocommit and reopening. Wrong TEXT/INTEGER/REAL types, empty BLOBs and supported
noncanonical control/chunk JSON have exact error or value comparisons.

Production owned locking excludes a second connection. These tests establish the
conditional copy bound when a payload differs from the preceding observation;
they do not qualify an external concurrent writer or a physical mutation race.
The fixtures own actual profiles and SQLite but use authority-free recording
proposals. They issue no live source, capacity, startup, terminal or physical
capability. SQL engine/native baseline memory, decoder/string/error backing,
complete retained inventory accounting and whole-process RSS are separate limits.

The genuine original run on clean commit
`9719e5a06dcbbd8143a7bbe395de1624d7994c4e` failed its zero-copy assertion after
tracing control `changed=4097 copied=4097` and chunk
`changed=131073 copied=131073`. Full retained-state/autocommit/reopen assertions
passed before that failure. Actual compilation took 58.76 seconds; the Store test
ran for 0.08 seconds. `/tmp/opt2372-before.log` records this result. Canceled waiting
jobs are not test evidence. Implementation and repository verification results
are recorded in the live issue after their actual completion.

Full OPT-2145/2058 startup, required-WAL, authentic producer/terminal capability,
Finalizing/Frozen/seal storage, physical validation/publication and retirement
acceptance remains open, as does OPT-2326/native OPT-2330 qualification.
