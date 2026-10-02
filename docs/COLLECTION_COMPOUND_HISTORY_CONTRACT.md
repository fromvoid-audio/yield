# Compound collection membership history

Decision for [OPT-2181](https://linear.app/boostnlvp/issue/OPT-2181), under
[OPT-2114](https://linear.app/boostnlvp/issue/OPT-2114). This specifies the next
production extensions of [bounded collection history](COLLECTION_UNDO_CONTRACT.md).
It implements no new mutation, retry, durable history or platform qualification.
OPT-2114, OPT-1617 and OPT-1476 retain their original acceptance.

## Existing owners and missing consumers

`crates/yield-catalog/src/curation.rs` executes ordered atomic source batches on
the registered, retained SourceWriter. Its privately constructed outcome carries
each committed receipt, exact initial/final revision and typed zero-based failed
batch index. A failure stops the operation; `AfterOperation` cannot continue a
failed predecessor. `final_revision` without a receipt is not a readback.

`crates/yield-ui/src/browser/curation.rs` currently converts the typed failure to
a display string. `browser/curation/collection_history.rs` accepts exactly one
successful batch and retires history when an audit projection is installed.
Neither parsing that string nor enabling controls after a generic refresh is a
safe compound implementation.

Merged OPT-2138 provides `SourceCollectionHistorySelection` in
`crates/yield-library/src/sample_sources/db/write/source_writer/collection_history_selection.rs`:
an opaque read-only observation with complete SourceRevision, audited directory,
sorted changed bits and clock-limited delivery window. It accepts at most 128
unique paths, 4,096 bytes per path and 1 MiB owned payload, with a two-second SQL
deadline. OPT-2160 transports this through the existing catalog worker, using
the registered owner, caller generation, cancellation and bounded delivery
accounting. Reuse that transport; do not open a second writer or synchronously
read SQL/filesystem state on the UI thread. These observations supply neither
write admission nor a durable intent/receipt identifier.

## One bounded chronological lane

Extend the existing curation history lane, rather than adding an independent
compound stack. Future rating/lock records (OPT-2182) must share its chronology
and budget through field-specific payload variants; they cannot undo around a
newer collection record. This decision covers collection membership only.

Before original submission, freeze a deterministic ordered unique path payload
with exact before/after bits for one source and one collection slot, immutable
batch boundaries, complete owner identity and audited directory generation.
No-op paths do not enter the changed payload. Capture all bounded chunks at one
exact revision on the same retained owner; discard the entire preparation if a
chunk, identity, directory or observation window changes before admission.
The existing exact-revision mutation route still revalidates on the worker.
Sequential chunks need one explicit preparation cancellation context: retaining
the same token must not cancel itself, while a distinct replacement or mutation
must retire it. If the transport is one-shot, implement and test this bounded
group seam before admitting compound preparation; do not ignore cancellation
of an earlier chunk to assemble a group. OPT-2160 owns the transport boundary.

Retain two separate subsets: **origin committed** contains only paths covered by
validated original receipts; **currently applied** contains origin paths whose
last acknowledged direction leaves them at their after bit. Original uncommitted
suffixes never become redo work. A record also carries a direction phase,
pending request identity and stable selection/focus/viewport observations.
Membership equality cannot manufacture either subset.

Preserve ordinary command limits of 512 batches, 65,536 updates, 16 MiB path bytes
and 32 MiB estimated receipt bytes. History remains at most 128 logical records
and 16 MiB total retained payload, including pending preparation, direction
plans, subset/batch storage and presentation/restore copies. Use checked bounds
before allocation or cloning, shared immutable payload where possible, and
charge actual owned capacities conservatively. One group counts as one record.
Evict oldest settled records only; never evict an in-flight or partial top.
When coverage cannot fit, ordinary curation may proceed within its existing
limits with explicit history-unavailable status; it must invalidate unsafe
continuations after acknowledgement. Do not silently reduce ordinary limits or
report whole-selection undo coverage for a truncated history payload.

## Receipt settlement and partial ordering

For each still-live request, validate the exact source/lifecycle, request,
publication, root/directory and initial revision, then every receipt in order.
Each receipt must contain exactly the submitted batch's paths and target bits,
valid rows, the expected directory/root and the next exact revision. Successful
completion requires all batches and no failure; failed completion requires
receipt count equal to the typed failed batch index, within submitted bounds.
The final revision must equal the last receipt, or initial revision for an
empty prefix. An unresolved initial fence permits no committed receipt.
Reject malformed, repeated, out-of-order or nonmatching outcomes without moving
history. Never treat an omitted completion as zero commits.

Original partial completion creates history only for the validated committed
prefix. Report committed/remaining batch and changed-path counts and the actual
failure; do not claim aggregate success or rollback. An empty prefix creates no
history effect. Admission rejection creates no pending record and changes no
stack position. A new ordinary acknowledged edit clears redo; if it abandons a
partial direction, explicitly retire that continuation without reverting its
already committed bits.

Undo submits explicit Add/Remove inverses for currently applied origin paths;
redo submits explicit after bits for origin paths currently at before. Preserve
immutable batch ordering, filtering paths already at the requested endpoint
because of acknowledged directions. Never toggle or restore whole metadata rows.
Rating, lock, tags, play history and timestamps remain outside this payload.

A failed direction changes only its acknowledged prefix subset. Keep that record
as the exclusive partial top: neither another undo record nor an older redo
record may overtake it. Resume the same direction for remaining eligible paths,
or reverse the acknowledged subset to return to the previous endpoint. Both
require fresh owner refencing below. Move the record between stacks only when
all origin paths reach the requested endpoint through validated receipts.
Direction reversal never adds the original uncommitted suffix. An empty eligible
subset is a local endpoint transition, not a fabricated SQL receipt.

## Refencing and uncertainty

Known terminal failure and an acknowledged prefix permit preparation of a fresh
direction, not reuse of `AfterOperation` or a failed request ID. Obtain a new
nonzero request generation and owner observation at the exact last acknowledged
revision. Capture every origin path in bounded chunks, grouped by explicit
desired bit when necessary. Since successful capture omits no-ops, reconstruct
current bits only from its exact requested set plus its returned changed paths;
never interpret a missing delivery as a no-op. Require every bit to match the
record's acknowledged current subset before making a plan.

Compare complete SourceRevision identity: source, root identity, root binding,
database identity, registered lifecycle and lease epoch, plus audited directory
generation. Every chunk must name the same expected numeric revision and retain
its observation window through preparation. A new audit projection may preserve
history only through this explicit matching reconciliation transition; its
current unconditional clearing behavior must not simply be removed.
Intervening revisions, mismatched bits, replacement, lease reacquisition,
unavailable owner or expired observations retire/refuse the continuation.
Reacquisition and restart restoration remain OPT-2115 work. Do not construct a
replacement SourceRevision by copying an old identity and incrementing a number.

Lost delivery after possible commit stays **unresolved**, disables history
directions and releases stale presentation. Equal bits or numeric revision are
not durable operation identity and cannot prove receipt, rollback or safe retry.
Cancellation before admission has no effect; cancellation/retirement after
admission cannot assert rollback. Drain late outcomes with lifecycle-scoped
invalidation so an old source cannot clear a new source's history. Duplicate
outcomes cannot settle twice. UI consumption uses only in-memory token/clock
checks; later worker mutation revalidates authority independently.

## Production leaves and verification

Split OPT-2114 into [OPT-2186](https://linear.app/boostnlvp/issue/OPT-2186),
immutable compound preparation, typed receipt-prefix settlement and bounded
history storage, then [OPT-2187](https://linear.app/boostnlvp/issue/OPT-2187),
retained-owner refencing, partial direction retry/reversal and active GPUI integration. The first can
expose truthful partial status while leaving failed directions unavailable.
The second must implement the reconciliation route before enabling them.
OPT-2160 is their existing observation transport prerequisite. Neither leaf
duplicates source capture, journal admission, persistence or native bootstrap.

Required production tests use actual profile-owned SQLite, retained CatalogService
and SourceWriter, not caller-created receipts or physical grants. Exercise first,
middle and last batch failures for original apply, undo and redo, successful
prefix readback, safe same-owner retry/reversal, duplicate/late outcomes,
revision/lifecycle/root/directory/lease changes, cancellation and lost delivery,
no-op groups, boundaries, payload credits and unrelated fields. A narrowly
scoped test-only worker fault boundary may deterministically fail before a batch;
successful prefix receipts must still come from actual transactions. Simulated
failure is not crash/power-loss or physical capacity qualification.
The UI's catalog dependency does not expose catalog-local `cfg(test)` helpers.
[OPT-2190](https://linear.app/boostnlvp/issue/OPT-2190) supplies the missing bounded,
explicitly gated test-support worker seam, absent from default production builds.
It precedes the consumer fault matrix and cannot itself complete the consumer.

The GPUI fixture must click actual controls, consume actual owner outcomes, show
partial/unresolved state and verify selection, focus and viewport restoration
only for current surviving identities. Keep controls unavailable during pending
preparation/mutation and unknown commit. Model-negative tests supplement this
path. Normal qualified bootstrap remains OPT-2143; metadata reopening is not
history restart proof. OPT-2115/2127 and parent acceptance stay open.

Existing catalog serial-batch/partial-failure tests substantiate the current
receipt producer only. Their passing results do not implement this contract's
consumer, fault matrix, restart or platform acceptance.
