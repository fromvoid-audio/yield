# Compound collection owner execution (OPT-2232)

This production prerequisite of [OPT-2186](https://linear.app/boostnlvp/issue/OPT-2186)
provides retained-owner capture and execution. Browser history integration and
its original acceptance remain open. A command result is committed state; it
is not a persisted undo journal or physical qualification.

`CollectionHistoryGroupRequest` captures explicit per-path Add/Remove targets
through the retained catalog worker. It rejects duplicate canonical keys,
invalid slots/paths and oversized input before retaining clones. Its original
two-second deadline includes construction and queue time; each real source
selection chunk consumes that same remaining deadline. Cancellation is checked
between chunks and before publication. No-op memberships are omitted.

`SourceCollectionHistoryGroup` privately records the actual writer issuer,
complete SourceRevision (source, root, root binding, database, lifecycle and
lease), directory generation, genuine before/after bits, and deterministic
128-update partitions. A conservative 64 KiB + sum(1024 + 8 * path bytes) charge
must fit 16 MiB; there can be at most 65536 input paths, each at most 4096 bytes.
The ordinary catalog limits remain 512 batches, 65536 updates, 16 MiB path
input and 32 MiB receipts. New commands share existing admission credits rather
than adding an unbounded queue. Output capture uses the existing bounded tag
read pool; prepared curation charges twice the complete preparation estimate.

`CatalogCollectionHistoryGroup` carries private original capture custody.
`PreparedCollectionOperation` accepts only that genuine delivery. The worker
validates the original cancellation/publication token, observation deadline,
issuer and complete owner before privately starting progression. Only then
can it retire the capture token and acquire ordinary mutation publication
custody. Thus its own write does not cancel preparation before handoff.

Each batch compares the original complete owner and the numeric revision from
the preceding genuine receipt. It rereads exact before memberships and audited
directory, then commits through the existing profile-owned curation route with
a fresh owner-issued revision. There is no caller-bumped SourceRevision,
reopened pathname as ownership, new capacity permit or journal intent. Existing
profile/root/database/lease checks and admission limits remain in force.

The terminal outcome retains the genuine preparation identity, exact typed
failed batch index/error, and actual ordered committed-prefix receipts. Failed
or unattempted suffixes never become committed state. Source replacement with
equal root/revision and writer handoff with equal lease/revision cannot reuse
old issuer custody. Ordinary independent edits retire capture.

Real profile/SQLite tests exercise first/middle/last fault injection, full and
mixed membership groups, unrelated locked fields, noops, duplicate/count
bounds, queue-time expiry/cancellation, root/main/database-child replacement,
source lifecycle replacement, exact SQL prefix/revision readback and credit
retirement. Direct writer tests cover equal numeric/lease issuer handoff,
actual lease reacquisition, original deadline/cancellation before SQL and
receipt-driven progression. The OPT-2190 fault seam remains feature gated.

```sh
cargo test -p yield-library collection_history_group -- --nocapture
cargo test -p yield-catalog --features test-support prepared_collection -- --nocapture
```

These tests use explicit profile-owned catalog fixtures. Normal protected
reserve bootstrap still needs the scoped native verification in OPT-2026;
collection native/restart/journal work and OPT-2186/2187/2114 stay open. This
leaf does not claim browser history, bounded presentation storage or active
GPUI undo/redo acceptance from catalog receipts alone.
