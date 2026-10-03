# Durable database attempt observations (OPT-2206)

Journal schema 12 adds private immutable start and outcome relations. A guarded
schema-11 migration creates empty relations without changing existing intents,
bindings or claims. Older migrations likewise create empty relations. Exact table,
column and index validation rejects unexpected schema objects.

Starts bind the actual stored, codec-normalized command family and revision domain
to its complete validated database inventory, participant, direction, physical
database identity, owner fences, slices and nominal DB/WAL/SHM reservation. Images
are canonical and bounded (2 MiB per start, 16 KiB per outcome). Limits are 256
starts per operation and 8,192 globally; restart auditing uses 64-row pages. Exhaustion
fails closed without eviction or reset.

The store validates the actual durable journal allocation and database aggregate
claims before mutation. Starts and outcomes use actual SQLite transactions and
exact durable readback. Replaying the same image is observational; a changed image
or nonce correlation fails. Confirmed rollback removes only provisional new rows.
An uncertain boundary retains the full start image; an active uncertain transaction
also retains extension exclusion until the original connection is quiesced.

Every retained start conserves its entire nominal reservation and excludes another
unresolved attempt against the same physical database, including across operations
and restart. Unknown allocations are optional values, distinct from known zero.
Neither NoCommit nor CommittedObservation supplies settlement authority: neither
refunds a slice, authorizes retry, releases ordinary claims, nor permits handoff.
Outcomes are immutable first observations; later genuine settlement needs its own
authority and implementation.

## Verification

`cargo check --locked -p yield-library` passed. The full
`cargo test --locked -p yield-library --lib` suite passed with 1,947 tests,
zero failures and nine existing ignored tests. Actual profile-owned journal SQLite
fixtures exercise atomic starts, immutable outcomes, nonce readback, rollback and
lost replies, unknown-versus-zero facts, bounded exhaustion, corruption retention,
65-start paged restart auditing, and schema-11 migration preservation. Historical
migration fixtures remove the new empty tables when reconstructing old schemas;
the oversized-key fixture exceeds the updated SQLite kernel value bound.

These fixtures use logical database bindings and observation receipt identities.
They do not qualify physical capacity, construct live permits, execute participant
SQL, or manufacture SourceWriter registration. OPT-2168 remains responsible for
real private issuer integration and phase-specific command/inverse derivation;
OPT-2169, OPT-2170 and OPT-2171 retain receipt, reconciliation and settlement scope.
OPT-2136 and native physical qualification remain required. Full-progress admission
and production execution are not enabled by these storage APIs. Tracking parents
remain open until those implementations and qualifications are complete.
