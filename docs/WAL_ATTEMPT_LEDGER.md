# Durable database attempt observations (OPT-2206)

Journal schema 12 adds private immutable start and outcome relations. A guarded
schema-11 migration creates empty relations without changing existing intents,
bindings or claims. Older migrations likewise create empty relations. Exact table,
column and index validation rejects unexpected schema objects.
The current schema is 13; its additional publication-issuance relation preserves
these attempt relations and limits. See [durable issuance](PUBLICATION_ISSUANCE_BINDING.md).

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

### Original Registered source reservation custody (OPT-2287)

`SourceWalPreparation::retain_registered_wal_binding` retains one private,
non-cloneable counted use of the exact original Registered source registration,
request, database and lease. It checks the current CONTROL/profile and original
native MAIN/WAL/SHM binding before retention. A Prepared diagnostic, foreign or
expired binding cannot acquire this custody; repeated acquisition refuses. Its
fixed header is bounded to 512 bytes and is included in the concrete preparation
size used by transport accounting. Parked validation executes no ordinary SQL.

The count and original lease reference survive rollback or timeout-restoration
failure. Confirmed empty rollback and policy restoration release them before
returning the same writer. Drop closes ordinary SQLite before releasing the count
and its lease reference, including after registration abandonment. Draining close
validates the original guard and distinguishes precisely this one owned lease
reference from external snapshot fences. It closes ordinary SQLite, releases its
own counted use, then enters the existing lease-release/retirement-ack boundary.
Wrong guards and external snapshots cannot obtain a retirement acknowledgement.

Tests use real original source connections, the actual confirmation lifecycle and
real rollback denial. A SQLite function-userdata destructor observes the held count
at actual connection close. The timeout test injects pending restoration at the
existing private failure boundary; normal preparation already restores the timeout.
The harness is compiled for Linux, macOS and Windows; only Linux execution was
available here. Native execution remains separate verification. These fixtures
install no production capacity provider and qualify no physical allocation. This core primitive supplies no write permission, decision latch,
retry, reservation refund or genuine journal settlement. Full OPT-2136 still needs
all-owner acknowledgements and decision and post-start cleanup fences, atomic nonce/intent/claims,
readback/restart handling and qualified directions with required permits.

### Original journal source transport custody (OPT-2289)

The original private combined-handshake source requester now requires genuine
Registered custody before its producer publishes an audited, tag, layout or
pre-audit lease preparation. Missing, Prepared, foreign or expired registration
cannot publish that preparation. The producer rechecks cancellation and the
original deadline after retention; late refusal keeps native custody until
original cleanup. Standalone diagnostic requests retain their Prepared
observation contract. The private historical observation marker lets the journal
collector reject diagnostic preparations; it is never a current authority or a
post-start decision latch. Parked sweeps validate the original retained binding.

Tag/layout cleanup moves the non-cloneable count from the inner empty reservation
to the original outer policy custody before rollback. It holds the same count and
lease reference through rollback, timeout and every outer policy restoration.
Confirmed cleanup releases the count before returning the original writer;
sticky failures retain it until actual ordinary SQLite close. Draining close uses
the exact original typed guard and existing snapshot exclusion. Concrete owner,
preparation and observation headers remain charged to the original bounded pool
and combined 64-slot/64-KiB handshake budget, with no additional pool or worker.

Portable tests use real source/global/journal workers and the actual source
confirmation lifecycle without installing a capacity provider. They check all
source policy families, Prepared refusal, retained counts at publication and
confirmed reuse, original budget conservation and cancellation, caller drop,
lost reply, deadlines and shutdown. SQLite authorizer probes check the count
during rollback and all four outer restorations; actual SQLite userdata
destructors check it at sticky-failure and draining connection close. Clearing
a restoration fault does not clear policy poisoning or authorize reuse.

This supplies source transport custody only. Full OPT-2136 remains open for
qualified allocation directions, the joint all-owner post-start decision and
cleanup fences, atomic nonce/intent/claims, uncertain readback/restart, required
permits and genuine settlement. No observation, lifecycle count, fixture or
cross-compilation supplies physical qualification, retry, reservation refund or
admitted lease release. Native registered/bootstrap execution stays tracked by
OPT-2275/2262/2280.

## Explicit retirement initial-forward inventory (OPT-2337)

### Separate registration serial policy (OPT-2385)

Marker `YWCB\x04\x00` preserves all tag-3 fields and adds one positive Source
registration serial (little-endian u64, at most i64::MAX) immediately after the
header decision nonce, before the binding/slice counts. Source `owner_generation`
continues to equal the exact stored lease epoch; Source lifecycle and registration
UUID checks remain independent. Tags 1, 2 and 3 retain their exact meanings and
encodings. No old image is upgraded when read.

Tag 4 retains the exact two bindings/six initial-forward slices and uses the
separate `yield.database-claim-inventory.v4` digest domain. Every work/component
slice uses `yield.registered-retirement-forward-slice.v1`, hashing the independent
serial followed by the existing complete tag-3 slice identity. The serial therefore
binds both Source and Global work pairs to the same frozen observation. Changed
serials or mixed tag-3/tag-4 slices fail record validation. Schema 12 retains the
complete bounded canonical image and its digest; readback verifies those bytes
and the frozen record without adding a normalized authority column.

This policy stores historical comparison data only. Its scalar storage fixtures
do not issue or validate a genuine original registration. Existing attempt Start
versions refuse tag 4, as they refuse tag 3. OPT-2382 must still project the real
Source/Global/profile/decision context on the original ACK consumer, prepay its
allocations and recheck that context. Physical qualification, joint Start, permits,
admission, retries/refunds and settlement remain separate open work.

Inventory marker `YWCB\x03\x00` uses the existing bounded binary fields and schema
12 tables, with a separate digest domain. Tags 1 and 2 keep their historical
validation and encoding; a historical retirement inventory does not acquire the
new policy by being read. Tag 3 requires exactly one source lease participant and
one required global participant, with six slices: a source MAIN/WAL-SHM pair and
separate MAIN/WAL-SHM pairs for the frozen Draining and Retired work keys. The two
global transitions each retain the same full component ceilings; conservation
charges both pairs separately against the aggregate claims.

Slice identities reconstruct the complete frozen retirement-family digest, work
key, component, claim, decision nonce, original binding and registration/generation
identities. The family digest is shared with the actual original global policy
capture. Record validation rejects omissions, extras and mixed identities; durable
readback repeats that validation, including after lost commit replies and restart.
The existing 64-binding, 256-slice and 64 KiB image limits remain in force.

This is a storage comparison policy. Matching images, hashes and nonce readback
cannot issue Start, participant permits, retries, refunds or settlement. The future
live consumer must compare all six slices with the genuine original source lease
and global OPT-2334 policies and qualified physical requirements before joint
Start and atomic insertion, and must enforce the sequential transitions.
Compensation needs an explicit separate policy and reservation. OPT-2136,
OPT-2194 and OPT-2196 remain open for their complete live contracts.

Existing attempt Start V1/V2 deliberately refuse tag-3 inventories. Their existing
work/phase correlation must not silently broaden to this new per-work policy;
OPT-2168 still needs an exact issuer/attempt integration before any such attempt
can be persisted. This inventory change does not enable retirement attempt effects.
