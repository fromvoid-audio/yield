# Historical authored Harvest recovery (OPT-2358)

The retained Global owner now reads an original authored receipt by a validated
non-nil ordinary `OperationId`, without a current source observation or restored
mutation request. `HarvestAuthoredReceiptObservation` is a fixed scalar value
(no more than 192 bytes): original operation, receipt sequence and owner,
disposition, requested workflow state, and digest of the canonical proposal.
It cannot construct a request or receipt, authorize replay, or issue a permit.
Requested `Done` means user workflow intent, never saga/journal completion.
`GraphChanged` and `IdentityConflict` retain the requested state without claiming
that state was applied.

One read-only SQLite transaction validates the current held profile root and
original canonical payload/outcome, digest, selected operation and unique
receipt sequence. The shared original authored reader preserves exact-request
replay conflict ordering and all apply/CAS/capacity behavior. No schema or
retention policy changes. Historical lookup changes no graph, receipt, mutation
revision, weak read generation, or WAL maintenance marker.

Historical reads refuse relations larger than 8,192 rows before unindexed lookup.
The shared native value guard uses the lesser of the previous SQLite length
limit and 256 KiB and restores the previous policy before COMMIT, errors, and
unwind. The existing Harvest VM progress guard checks every 1,000 instructions:
`calls > 1024` interrupts at callback 1025 (about 1,025,000 VM instructions).
The two-second deadline is observed at callbacks and does not preempt native I/O.
Only actual budget exhaustion and `SQLITE_TOOBIG` map to malformed history;
unrelated SQLite errors retain their classification. The progress handler is
cleared on return, refusal and unwind.

## Reproducible owner and process evidence

Run these on the committed repository tree with the shared build lock:

```sh
cargo test --locked -p yield-library --lib harvest_authored -- --nocapture
cargo test --locked -p yield-library --lib harvest_copy -- --nocapture
cargo test --locked -p yield-library --lib harvest_facts -- --nocapture
cargo test --locked -p yield-library --test harvest_authored_receipt_process -- --nocapture
```

The queued owner cases cover lost reply, later edits, restart with source closed,
corrupt/hash/duplicate/foreign-profile/changed-operation history, bounded native
foreign-key allocation, cardinality refusal preserving exact replay, and actual
recursive-view budget exhaustion followed by repair and healthy read/replay.
Existing retained rejection tests also read actual `GraphChanged` and
`IdentityConflict` outcomes. The existing Copy guard tests exercise shared value
policy restoration, lower prior limits, errors, and unwind.

The normal-build process harness exposes no fixture constructor or test-support
feature. Its positive producer creates a disposable actual file and observes its
opened native identity, hash, size and modification time, commits an actual
SourceWriter manifest/audit, obtains the original Global authored snapshot, and
commits a request with deliberately lost reply. It commits a later `Done` user
edit and checks original replay preserves that edit. The parent proves competing
profile ownership is refused while the child is alive, kills/reaps only that
owned child, reacquires only the profile/Global owner, and reads the original
`Seen` intent and exact historical owner/digest twice. Current workflow state,
ledger/graph counts and source bytes remain unchanged. A separate genuine
normal-build case proves missing history remains absent after process death.
The ignored child helper is explicitly invoked by each non-ignored parent;
its ignore is not a skipped acceptance case.

Linux focused evidence: 30 authored-related cases, 30 Copy cases, 19 Facts
cases, and both normal-build process parents passed with zero failures. Final
current-tree repository results are recorded in the PR and Linear after
verification. Native runtime verification is tracked by OPT-2360 with both
positive and missing-history parent commands on actual macOS and Windows.
Windows GNU compilation proves target compatibility only.

## Limits

These are ordinary metadata producer/consumer and owned process-death results.
Source indexed observations do not establish a physical copy/publication grant.
This work supplies no created-stage, absence, retry, refund, reservation-release,
private terminal checkpoint, genuine settlement, or power-loss qualification.
It does not reopen the original source or replace a source lease with historical
facts. OPT-1645, OPT-1666 and OPT-1670 remain open for their full participant and
physical acceptance. Native Copy producer verification OPT-2350 remains open;
this normal-build authored producer does not complete it.
