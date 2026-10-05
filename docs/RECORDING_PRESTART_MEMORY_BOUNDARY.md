# Recording pre-start memory boundary

OPT-2326 prepares memory plumbing only. The original journal worker retains the
actual Reson sink binding, receiver and settlements in one private slot. The weak
facade cannot start a recorder, take these parts, grant physical writes, complete
terminal SQL, settle WAL attempts or release durable reservations.

## Complete supported resource envelope

The factory establishes one original readonly SQLite transaction before reading
owned rows. Scalar queries refuse the complete inventory when it contains more
than 32 operation/ordinary-claim/release/recovery rows per relation, more than 16
recording rows, any database binding/slice/attempt/outcome, full-progress or chunk
claim row, any captured recording payload, or oversized/wrongly typed row text.
Recording rows must fit the 1 KiB empty-control envelope. Within the same original
read snapshot, the control reader compares each BLOB directly with the expected
canonical empty reservation encoded from the validated proposal. It does not
send retained control bytes to the generic JSON Value parser. Even a small BLOB
can otherwise own a large graph of singleton maps before a post-parse depth or
shape check. Trusted control encoding streams through the existing capped writer
(initial 4 KiB backing) and uses the already-budgeted proposal encoding scratch.
A different encoding returns the narrow resource refusal; it is not labeled a
corrupt historical inventory. General control reads and full rebuild retain their
existing typed acceptance, including valid noncanonical whitespace. Operation BLOBs must be
at most 64 KiB each and 128 KiB in aggregate. UTF-8 text bounds count bytes rather
than SQLite's TEXT code points.

Every operation row in that same snapshot is then strictly decoded, including
unclaimed operations. Before typed command materialization, the private decoder
requires schema 17 and the sole Recording command tag. Its initial JSON tree has
quotas of 448 nodes, 448 entries of either container kind, 32 KiB of owned decoded
key/value strings and depth 32. Unsupported complete inventories return the
factory's resource refusal; they are never validated as a truncated snapshot.
Default codec and general journal rebuild acceptance retain their existing limits.

Within the accepted snapshot, the original complete ordinary/recording/database/
attempt/recovery audits still run. Original source receipt/pin/profile, exact
source fence/proposal and current provider/physical reserve checks are required.
The scoped namespace reader preserves original identity and sidecar checks while
the transaction remains active. Every result ends the snapshot before returning
a facade or constructing producer plumbing.

## Original command and retained debits

The command reserves 3,604,480 bytes from the existing ordinary input pool before
worker execution. Revalidation reserves the same temporary debit independently
of the retained slot. The command estimate is for additional owned Rust backing;
the existing worker, SQLite connection/cache, descriptor registrations and thread
stack are separate original runtime resources. It does not bound process RSS,
SQLite's allocator or operating-system memory, and it does not change any of
their policies or introduce a new pool ceiling.

The supported build uses pinned Rust 1.93.0 and locked serde/serde_json. For this
Recording-only typed shape, variable vector elements are participant descriptors,
checkpoints and full allocation entries. Compile-time layout checks require
descriptors to fit 1,024 bytes and checkpoints/allocations to fit 512 bytes. The
stored-record fixed layout must fit 64 KiB. serde's Value sequence exposes an
exact remaining size hint; its Vec deserializer preallocates that count. The
448-entry envelope is below serde's 1 MiB preallocation cap even at 1,024 bytes
per element, so these typed vectors do not acquire geometric spare capacity.

The initial JSON visitor uses growing arrays. Compile-time guards require Value
at most 80 bytes, String at most 24 bytes and a pointer at most 8 bytes. The
pinned std growth rule is `max(2 * capacity, required, minimum_nonzero_capacity)`;
its minimum for these Values is four. Thus each array's owned backing fits
`(2 * entries + 4) * size_of::<Value>()`, including singleton arrays. An entry
requires its child value node, so their combined allowance is at most 480 bytes
per value node.

Both serde_json map backends must be supported: workspace feature unification can
activate preserve_order and make Value 72 bytes rather than the default 32.
Default BTreeMap has 11-entry leaves and 12-edge internal nodes. Their checked
field-size envelope (including padding) is below 1,536 bytes even with an 80-byte
Value; default actual node allocation layouts are 632/728 bytes. Empty maps do
not allocate; allocated node count cannot exceed owned entry count. The locked
IndexMap stores hash/key/value buckets in a Vec and usize indices in hashbrown.
Its capacity follows hash-table capacity and reserve_exact; hashbrown rounds to
powers of two with a 7/8 load factor and minimum four buckets. A conservative
1,536 bytes per owned entry covers both current/old index and bucket backing,
small-table controls and singleton capacity. Nested sparse objects are included.

Object entries and array entries together have at most one child per JSON value
node. The joint envelope therefore uses 1,536 bytes per node plus the separate
32 KiB string quota, rather than adding independent maximal map/array allowances.
Parser scratch is conservatively twice the 64 KiB wire ceiling. The two-tree
category includes original parse and validation-clone lifetimes; the two typed
representations separately include conversion/validation while the current
validated proposal remains live.

Source anchors:
- Rust 1.93.0 `alloc/src/collections/btree/node.rs`, B/leaf/internal declarations:
  https://github.com/rust-lang/rust/blob/1.93.0/library/alloc/src/collections/btree/node.rs
- Rust 1.93.0 `alloc/src/raw_vec/mod.rs`, min_non_zero_cap/grow_amortized:
  https://github.com/rust-lang/rust/blob/1.93.0/library/alloc/src/raw_vec/mod.rs
- Locked serde_core 1.0.229 `de/impls.rs` VecVisitor and
  `private/size_hint.rs` cautious; serde_json 1.0.151 `value/de.rs` SeqDeserializer.
- Locked indexmap 2.14.2 `lib.rs` Bucket and `inner.rs` reserve_entries;
  hashbrown 0.17.1 `raw.rs` capacity_to_buckets.

`scripts/recording_prestart_layout_probe.rs` is a standalone diagnostic using the
actual std/serde containers and allocation Layout requests. Link it against a
built locked serde_json rlib with `rustc --edition 2024 PROBE --extern
serde_json=RLIB -L dependency=TARGET/debug/deps -o PROBE_BINARY`, then execute the
binary. Verify the chosen rlib's fingerprint features and dependency source path;
the shared target contains both backend builds. The probe checks every growing
array length 0..448 and nested 32-level sparse objects plus a simultaneous clone.
Actual default results: Value32/String24, 448-entry map cumulative requested
44,568 bytes (69 node allocations, max layout728), nested32+clone40,448 bytes.
Actual preserve_order results: Value72/String24, map cumulative102,076 bytes
(16 allocations, max contiguous layout46,592), nested32+clone23,296 bytes.
The cumulative figures conservatively include obsolete growth backing. These
checks substantiate the pinned implementation assumptions; they do not confer
journal admission or native physical qualification.

| Additional backing category | Bytes |
| --- | ---: |
| Two initial/validation-clone JSON trees | 1,441,792 |
| Two typed record/validation representations | 1,114,112 |
| Independent validated current proposal | 196,608 |
| Raw record plus parser scratch | 196,608 |
| Three capped canonical proposal encodings | 393,216 |
| Complete small claim/release maps, sets, pages and diagnostics | 131,072 |
| Auxiliary bounded keys, errors and original context | 65,536 |
| Conservative total | 3,538,944 |
| Original command debit | 3,604,480 |

These categories overlap conservatively, including JSON-to-typed conversion and
Recording validation clones. A current validated proposal has at most 64
descriptors and 64 checkpoints; their checked layouts, 32 KiB string quota and
64 KiB fixed allowance fit the 192 KiB proposal category. Complete rebuild maps
have at most 32 entries per relation; their fixed records, BTree node backing,
bounded pages/keys and existing capped diagnostics fit 128 KiB. No database
attempt command trees or arbitrary global inventories are hidden in that estimate.

The retained slot keeps a separate 458,752-byte debit. Its validated proposal
fits 192 KiB; the three original fixed-capacity-one Reson channels, Arc instance,
boxed sink and route state fit a further 64 KiB envelope. Binding does not allocate
a callback ring, start serialization or enqueue captured bytes. The remaining
slot allowance covers its original pin/receipt/context backing.

Only after the original read snapshot, decoded outer records, rebuild maps and
canonical scratch are destroyed does the factory shrink its command permit to
the retained debit. This returns memory credit only. Lost replies preserve the
occupied original slot and its debit. Failed revalidation keeps the slot fenced
until original shutdown; it cannot replace the route or refund durable charges.

## Verification limits

Actual Linux tests cover consistent readonly snapshots, complete count and
known-shape refusal, raw BLOB refusal before copying, JSON quota refusal, oversized
recovery text, byte preservation, generic acceptance and original negative source
custody. The genuine production bootstrap/admission positive harness is compiled
on Linux and has two macOS-only invocations tracked by OPT-2330. It must execute
on the real supported runtime before claiming original-slot/lost-reply positive
execution. It grants no physical qualification or recording startup/terminal
completion. OPT-2145 and OPT-2058 remain open for those implementations.
