# Original legacy capture scratch

OPT-2356 replaces the actual original v11 row capture's owned metadata/audio text
collections with bounded scalar references and borrowed text. This is a prerequisite of
OPT-2349's retained pre-lease reserve consumer, not completed legacy adoption. It issues no
capacity permit, ordinary write permission, current lease, retry, refund or settlement.

## Historical semantics and coherent reads

The original supported bounds remain 128 metadata rows, 1,024 audio rows, 65,536 aggregate
metadata bytes and 4,194,304 aggregate audio bytes. Aggregate accounting retains the original
Rust tuple-header terms. The original SQL predicates measure encoded BLOB bytes, including
the 4,096-byte metadata-key/path gate and 65,536-byte metadata-value gate. The separate
original Rust audio-path limit remains 4,096 UTF-8 bytes. Metadata keys have no additional
UTF-8 length restriction: a supported UTF-16 key can contain 6,144 borrowed UTF-8 bytes at
the 4,096 encoded-byte boundary. Reserved/empty/duplicate keys, noncanonical revisions,
storage types, invalid UTF-8, native relative-path validation and negative file sizes retain
their original refusals. Metadata NUL bytes retain their historical text/digest semantics;
invalid native audio paths still refuse.

The existing original owner establishes one read transaction, captures the exact compiled
v11 schema, captures rows, checks data-version and the same cooperative deadline, then
revalidates its original profile/root/file/reservation and frozen digest. Complete bounded
table counts use raw table scans with one-row lookahead. The forced compiled BINARY indexes
must yield exactly those counts and unique scalar rowids. Unknown schemas, indexes,
collations and WITHOUT ROWID variants do not qualify.

SQLite BINARY order depends on the database encoding, so it is not canonical Rust UTF-8
order. Capture scans the genuine indexes but sorts only scalar references in Rust. Two
ordinary prepared rowid lookups expose simultaneously borrowed UTF-8 keys/paths. A fallible,
nonrecursive in-place heapsort compares those bytes and the original audio size/mtime tuple;
identical audio tuples retain identical digest bytes. Text is hashed with the same historical
length prefixes. No owned collection of keys, values or audio paths is constructed. A missing
or ambiguous rowid, changed scalar, comparator refusal or elapsed deadline returns refusal.
Short lookup statements may not reach SQLite's progress interval, so each comparison,
lookup/row iteration also checks the same original deadline.

All four scan/lookup plans must match exact pinned SQLite plans: the two named primary-index
scans and the two INTEGER PRIMARY KEY rowid searches. Every executed count, scan and lookup
also requires SQLITE_STMTSTATUS_SORT zero. There is no temporary sorting fallback.

## Named Rust backing and lifetime overlap

The additional `LEGACY_CAPTURE_SCRATCH_BYTES` debit derives from actual bounded components:

| Sequential stage | Owned backing |
| --- | --- |
| Schema | At most five rows of bounded type/name/table/SQL strings: `5*(16+128+128+4096)` bytes, plus capacity-eight row-vector and conservative five-row sort scratch headers |
| Metadata | Capacity 128 of scalar `i64` rowids; no owned key/value backing; canonical revision formatting uses at most 20 bytes |
| Audio | Capacity 1,024 of `(i64 rowid, i64 size, i64 modified)` triples, plus one transient original `PathBuf` validation of at most 4,096 bytes |

Schema strings and schema sort backing drop before metadata starts. Metadata scalar backing
and statements drop before audio starts. Audio sorting is in place and does not allocate
sorting scratch. The audio payload peak is 28,672 bytes on the supported 64-bit builds.
The debit additionally includes the actual `Vec<AudioReference>` header and three actual
`rusqlite::Statement` headers, conservatively charging those stack headers too. Compile-time
assertions require both preceding sequential stage inventories to fit within that derived
debit. Vector capacities are fixed at the original supported row maxima and never grow.

Pinned rusqlite 0.37 ordinary `prepare` borrows static SQL bytes, stores an empty named-
parameter BTreeMap and no statement-cache key. Positional integer bindings never populate
that map. Two lookups are prepared once and reused; they add no Rust statement-cache backing.
SQLite's native statement/cache/UTF conversion allocations remain outside this Rust debit.

The original journal custody's 64 KiB frozen-input/observer/header reservation remains,
and the derived scratch debit is added before the actual capture/effect-free preparation.
The input-pool ceiling is unchanged. This establishes the named row/schema scratch overlap,
not a whole-process or native SQLite allocator/RSS ceiling. Opening the original owner is
separate; transient native connection counts and immutable no-effect fencing remain governed
by OPT-2341. OPT-2349 must separately prove its actual retained owner/reserve/failure capsule,
checkpoint and lost-reply backing and preserve original provenance.

## Verification

The test-only historical implementation is copied from the actual preceding production
capture. Digest/refusal comparisons cover shuffled Unicode/NUL metadata, tuple and duplicate
ordering, UTF-8/UTF-16LE/UTF-16BE, unchanged original maxima, encoded-key boundaries and
storage/type/path/revision/count/byte failures. Normal-build integration tests use genuine
retained original owners, maximal images, unchanged bytes/no auxiliary sidecars, frozen-image
refusal and compiled schema rejection. Fixtures confer no physical qualification.

`scripts/legacy_capture_allocation_probe.rs` links the actual normal-build retained capture.
Its global allocator tracks successful Rust allocation requests and live requested backing,
including reallocation/deallocation. Fixture construction and original owner opening finish
before measurement. During one actual capture, it reports live peak above the existing
baseline and the largest requested layout, then verifies the original image and bytes.
It is a single-process diagnostic, not native allocation, allocator-resident overhead or RSS.

```bash
cargo build --locked -p yield-library --lib
rustc --edition=2024 scripts/legacy_capture_allocation_probe.rs \
  --extern yield_library="$CARGO_TARGET_DIR/debug/libyield_library.rlib" \
  --extern rusqlite="$CARGO_TARGET_DIR/debug/deps/librusqlite-HASH.rlib" \
  -L dependency="$CARGO_TARGET_DIR/debug/deps" -o /tmp/legacy-capture-probe
/tmp/legacy-capture-probe
```

Choose HASH from the actual current locked rusqlite fingerprint. In this shared environment,
hold `/tmp/yield-cargo.lock` for the entire job and touch the current worktree's library
entrypoint after acquiring it to force actual worktree inputs. Run nonzero selected row
tests and normal legacy-owner integrations, relevant library checks and stable committed
canonical preflight. Native Windows execution remains OPT-2282 and native macOS execution
OPT-2343 with their scoped OPT-2356 traces; GNU cross-compilation is compile evidence only.
Physical pre-lease reserve qualification remains OPT-2354. Full OPT-2117 stays open.
