# Owned layout allocation preparation (OPT-2204)

`SourceWriter::prepare_layout_wal_reservation` consumes the actual writer and an
original `SourceLayoutMetadataBudget`. Its opaque, non-Clone result owns the same
SQLite connection, an empty immediate transaction, every original policy setting
and the extension-policy read guard. It stays on the owner's thread. The ordinary
reservation API and the existing borrowed tag/layout policy adapters remain
available. Both metadata families share private setting and cleanup custody;
a tag observation never supplies layout permission.

The effective deadline is the minimum of the requested deadline and the original
layout observation deadline. Queue delay, cloned observations, validation and
retry cannot extend it. Strict retained profile/root/main-file/lease and complete
source-revision validation reuse the existing owner paths without renewal. Before
entry and under the held transaction, the original issuer and all measured
DB/page/WAL/SHM, prior-generation byte/digest and allocation-peak facts are checked
directly on that connection. There is no nested BEGIN or second source writer.

Historical generation observation accepts bounded exact SQLite TEXT or BLOB bytes
at `source_starmap_generation_v1`, from 1 through 512 KiB. BLOB measurement hashes
the exact bytes, including NUL and arbitrary non-UTF8 values, without conversion
to TEXT. Existing malformed UTF8/NUL TEXT rejection remains. SQL type/length
predicates precede value materialization; empty, oversized and unknown values
fail closed. These measurements do not decode, adopt, overwrite or certify a
semantic generation. The future admitted store must separately validate canonical
bytes, membership, features and exact generation CAS under OPT-2155.

Abort and Drop confirm empty rollback before restoring the busy timeout and all
entered settings. Only confirmed cleanup returns the exact writer. Rollback
failure retains inaccessible custody for explicit retry; failed restoration is
sticky and poisons the owner until actual close/reopen. A pre-existing transaction
is never adopted or returned to ordinary dispatch; its rejected owner remains
inaccessible until close. Uncertain SQL closes before lease/extension release.
Expiry is not rollback, and this pre-intent value is not a journal settlement latch.

No preparation writes source/control metadata, semantic revision, receipt, intent,
OperationId or capacity claim. It exposes only the budget/revision, expiry,
revalidation and abort. OPT-2136 still owns the actual all-holder decision gate;
OPT-2155 owns admitted atomic publication and reconciliation; OPT-2154 owns the
durable command family; OPT-1709/1485 and native/physical qualification remain
open. This composition supplies no layout effect or required-work WAL exception.

## Linux verification

Real profile SQLite owners test same-handle restoration and subsequent actual
commands; external writer exclusion and BEGIN-denied validation; issuer/revision
and generation/schema rejection; original deadlines; hard/unknown retained WAL;
near/expired leases with unchanged source/control counters; root/main/profile
substitution; rollback and PRAGMA restoration faults; partial entry; pre-existing
transaction retention; safe drop/close/reopen. Byte-bound tests cover exact BLOB
length/digest, embedded NUL and the maximum/oversized/empty boundaries. An actual
retained-feature capture and canonical producer supplies the exact binary bytes
for a test-owned metadata row; observation/preparation changes no rows, revisions
or control counters. That setup does not exercise admitted publication.

```sh
cargo test -p yield-library --lib layout_owned_preparation -- --nocapture
cargo test -p yield-library --lib tag_owned_preparation -- --nocapture
cargo test -p yield-library --lib layout_metadata -- --nocapture
cargo test -p yield-library --lib --locked -- --test-threads=1
bash scripts/agent.sh preflight
python3 scripts/internal/check/check_gpui_action_register.py --check
```

Portable policy and allocation evidence do not qualify physical reserve, CoW,
power-loss durability or native UI behavior. Existing scoped platform issues keep
those requirements; none is waived by this leaf.
