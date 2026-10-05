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

## Pre-audit lease inventory compatibility (OPT-2246)

Start V2 retains the distinct V2 source lifecycle/lease inventory and actual V18
retirement command. The lease domain correlates before/after to the original lease
epoch; it does not invent an audited revision. Legacy V1 inventories and starts
retain their exact bytes/digests, and V1 refuses the new scope/domain. Outcome V1
continues to bind the exact versioned start digest. Actual transaction and restart
readback support both without changing SQLite schema 12. See
[SOURCE_REGISTRY_LIFECYCLE_CONTRACT.md](SOURCE_REGISTRY_LIFECYCLE_CONTRACT.md) for
finite bounds, original-owner capture and the still-open admitted execution gates.

### Windows retained-handle capacity observations (OPT-2277)

`descriptor_capacity_facts` queries `FileFsFullSizeInformation` through the
original Windows disk handle. It brackets the query with handle-derived volume
serial, file index and creation-time checks, refuses opened reparse points and
non-disk handles, and validates the exact record size, counts, geometry and byte
arithmetic. Free space uses the caller-available allocation count, preserving
quota restrictions. No path lookup, reopen or filesystem mutation occurs.

The Windows volume serial is an observation identifier, not a qualified native
UUID. These facts do not qualify physical allocation policy, bind Windows WAL or
SHM objects, issue permits, authorize retries, refund reservations or complete
settlement. OPT-2194 remains open for the complete qualified policy. Portable
record tests and an isolated Windows Rust compile check cannot replace executing
the native retained-file/directory/renamed-handle and device-refusal tests on a
supported Windows filesystem.
Native Windows execution and quota verification are tracked in OPT-2278.

### Original Windows WAL object observations (OPT-2279)

The private owned VFS now checks Windows MAIN/WAL file pointers against its
original wrapper, context, role, `win32` VFS and exact bundled SQLite source ID.
It borrows the native handles without closing or duplicating them. For SHM it
locks SQLite's actual shared-node mutex, compares the original forwarder's
region-zero address with the native region array, validates the committed shared
read/write mapping, and compares both original mapping-file and locking-file
handles with the retained no-follow SHM child. Current retained-root children,
identities, sizes and deadline are checked again before returning observations.
Unknown bindings refuse without SQL or filesystem mutation. Only the reviewed
64-bit Windows layout is supported.

The readonly root clone uses the existing Windows handle-derived final path and
identity validator without acquiring a lease. Native Windows tests retain the
actual profile-child fixture and test the bundled VFS's genuine no-delete-share
rename refusal, changed mapping/foreign-role refusal and observation with all
ordinary SQL denied. They do not pretend Unix replacement semantics apply.

Reproduce the compile-only GNU Windows ABI check with the locked
`libsqlite3-sys` registry source and an installed MinGW compiler:

```sh
x86_64-w64-mingw32-gcc-posix -fsyntax-only -DSQLITE_THREADSAFE=1 \
  '-DYIELD_SQLITE_SOURCE="/absolute/locked/libsqlite3-sys/sqlite3/sqlite3.c"' \
  crates/yield-library/tests/windows_wal_layout.c
cargo check --locked -p yield-library --target x86_64-pc-windows-gnu --tests
```

These compile checks and portable layout/mapping refusal tests do not execute
Windows APIs. Native execution and supported SDK/toolchain validation are tracked
in OPT-2280; native capacity/quota execution remains OPT-2278. No physical
allocation qualification, database claim slice, permit, retry, reservation refund,
lifecycle release or genuine settlement is supplied. Full OPT-2194 remains open.
