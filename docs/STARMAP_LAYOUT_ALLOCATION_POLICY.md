# Retained layout allocation policy (OPT-2153)

`SourceWriter::capture_layout_metadata_budget` observes the existing owned source
connection without renewing its lease or changing SQL data. The private, cloneable
`SourceLayoutMetadataBudget` binds the full audited revision and writer incarnation;
it cannot be constructed or deserialized by a caller. Revalidation checks every
captured storage fact and preserves its original two-second cooperative deadline.

The scalar view exposes page size/count/cap, retained main DB and WAL bytes,
retained-root SHM bytes, existing fixed-key metadata byte length/digest, the
512 KiB generation limit and conservative disk peak. Existing text is measured,
**not validated or adopted as a canonical generation**. Its semantic codec and
exact previous-generation CAS remain OPT-2154/2155 requirements. Malformed SQL
values, invalid UTF-8/NUL, empty or oversized evidence are retained and rejected.
The separately exposed old/new encoding custody is at most 1 MiB.

The physical envelope caps the complete database at 64 MiB, including metadata,
indexes and old/new row allocation. It honors a lower existing `max_page_count`.
The disk peak is checked arithmetic over:

- `page_size * maximum_pages` for the complete main DB;
- existing retained WAL plus `32 + maximum_pages * (page_size + 24)` for one
  complete transaction's new frames and recovery custody;
- existing retained SHM plus `32768 + maximum_pages * 16` for WAL-index growth.

This deliberately conservative envelope is for **one fixed-key metadata
transaction**, with cache spilling and automatic checkpointing disabled. It is
not a bound for arbitrary SQL, repeated commits, vacuuming or other actors.
Unknown schema/runtime callbacks, optional extension initialization, unbound VFS
observations, malformed/missing SHM, overflow, excessive DB size and hard/unknown
WAL health fail closed. A cap does not promise that a transaction fits: SQLite
can return FULL under a lower cap, requiring confirmed rollback.

`begin_layout_metadata_policy` revalidates the actual budget, configures the same
connection to the measured `max_page_count`, `temp_store=MEMORY`, `cache_spill=0`
and `wal_autocheckpoint=0`, and verifies the settings. The shared entered/poison
state excludes concurrent tag/layout policies and prevents separate control-lease
renewal. `validate_entered` checks current physical ownership/lease, semantic
revision, deadline, known schema, settings, page count and fixed-key byte limits.
The eventual participant must call it before and after its bounded effect and
before commit; it must also validate the actual canonical generation, admission,
reservation, cancellation and immutable result.

`finish` or drop restores the original settings only after confirmed transaction
quiescence. Failed rollback/quiescence, schema confirmation or setting restoration
poisons this connection and sets query-only as defense in depth. Further owned
writes and budget issuance reject, including after wrapping the same connection
in a new writer. Restoration after a committed effect is not a publication receipt.
Actual close/reopen/reconciliation remains the recovery boundary.

This implementation supplies neither capacity claims nor durable intent, ordinary
layout publication or required-work exceptions above hard watermarks. The admitted
publisher must compose entered policy and the actual WAL reservation on the same
retained connection under the publication contract. OPT-2155 and all parent
production/native/physical acceptance remain open.

Validation commands:

```sh
cargo test --locked -p yield-library --lib layout_metadata -- --test-threads=1
cargo test --locked -p yield-library --lib -- --test-threads=1
bash scripts/internal/agent/run_agent_preflight.sh
```

Fifteen actual profile-owned SQLite/VFS cases run in fresh bounded subprocesses
because optional extension initialization is a process-wide safety fact. They
exercise read-only facts, generation bytes/digests/types, stale revision/handoff,
active transactions, expiry/no renewal, namespace replacement, unknown schema,
collation/extensions, missing SHM, oversized DB, real hard-WAL pressure, actual
SQLITE_FULL/oversized rollback, settings/exclusivity, expiry of the original
deadline and unconfirmed restoration/poison. The checked-arithmetic fixture tests
representation only. SQL authorizer fault injection is not physical disk or
power-loss qualification; scalar metadata test transactions are not a layout
publication or readiness result.
