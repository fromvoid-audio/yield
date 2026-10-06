# Funded historical ordinary-claim observation

The queued, generic `AdmitPrepared` route now observes a referenced V1/V2
operation without owning its opaque payload graph. This is a BOOL validity
lookup inside the original admission read transaction. It supplies neither
capacity nor filesystem authority, and it does not complete fresh durable/live
reconciliation or the Legacy initial-admission consumer.

The original SQLite key/type/positive-sequence/unique-sequence/BLOB/length gates
precede the borrowed projection. The original one-MiB wire cap remains. The
complete JSON is checked using serde's existing string, UTF-8, finite-number
and nesting rules. Exact scalar offsets identify every object key. In-place
unstable sorting compares the decoded UTF-8 bytes, including escapes and
surrogate pairs, within each distinct object. Duplicates remain invalid even
inside nested opaque payload objects.

Only the root V1/V2 payload is replaced with `null`. The remaining bounded
envelope goes through the unchanged unique-value parser, strict nested schema
validators, typed conversion and pure consistency classifier. V1/V2 validity
does not inspect the opaque payload. Replacing a one-byte scalar may expand
the synthetic envelope by three bytes; that is not a new original-wire limit.
The public decoder and `get` still return the complete original payload.

Temporary backing is debited from the actual queued command's original
ordinary input pool. Ordinary callers cannot borrow control or publication
ceilings. Parser/key scratch is reserved before those allocations. After the
key vector is dropped, any supplemental envelope/classifier debit is reserved
before copying the projected bytes or constructing its tree. Pool refusal is
explicit and cannot fall through to an uncharged V1/V2 lookup.

For raw row length R, the first phase reserves 2R plus compiled fixed headers,
covering geometrically grown escaped-string parser scratch. The key phase
uses the exact pre-counted number of 12-byte scalar offsets; sorting allocates
no key strings or auxiliary vector. The envelope inventory excludes opaque
payload nodes and backing. It includes a full pinned Rust internal BTree node
per remaining object entry/root, compiled Value headers, geometrically grown
array backing, raw upper bounds for decoded strings, projected bytes/parser
scratch, and simultaneous typed capacity-array/string/record backing. Wider
GPUI builds enable serde_json IndexMap; its dense buckets, hash indices and
control growth have a separate conservative bound. The maximum object-node
bound covers both layouts. The classifier's distinct-volume BTreeSet and
retained typed backing are named separately. The credit funds the maximum of
these sequential phases rather than relying on unnamed spare capacity.

The row/statement locals are declared after the scratch holder, so early
return and unwind drop the Native borrow and statement before the per-row
debits. Projected record/backing is dropped before its debit. Normal paths
explicitly end both SQLite lifetimes before releasing scratch or entering the
unchanged other-family reader.

Startup, retirement admission, other record families, Recording/database/chunk
audits and the existing whole-ledger collections/provider clones are outside
this first component. Valid wire data can still encounter explicit pool
pressure; no supported wire cap or pool ceiling is increased or silently
reduced. This is a Rust temporary-allocation account, not a SQLite-native
allocator, process RSS, stack, I/O preemption or physical qualification proof.

The normal allocator probe reads the same real SQLite fixture through the
unchanged public lookup and the funded test-support component. It includes
isolated original-pool fixture setup; it grants no physical capability. The
separate actual queued-owner test proves production command funding and
pre-effect pressure refusal. Native SDK/physical-floor verification and full
Legacy adoption remain in their existing open issues.
