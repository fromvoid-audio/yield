# Original journal retry return observations (OPT-2417)

The actual Store `record_retry_pending` and
`record_post_intent_chunk_exhaustion` methods observe their original entry-to-return
boundaries. Their original bodies, results, transactions, rollback/readback,
sequence checks and retained evidence are unchanged. Chunk exhaustion invokes
retry pending as before; both original primitive returns count independently.
An invalid exhaustion class returns before the nested primitive and counts only
the outer return. No new retry, SQL operation, ownership or capacity transition
is introduced.

The collector owns exactly 104 bytes: two primitives × four atomic counters ×
eight bytes plus five elapsed counters × eight bytes. Its numeric snapshot has
the same fixed size. Counters saturate at `u64::MAX` and use independent relaxed
atomics. Snapshots are process-lifetime samples, not coherent transactions, live
queue/claim gauges or resettable state. No identity, path, reason, encoded payload
or allocation enters the collector; it takes no owner/ledger locks and performs
no Store read. It borrows the original typed result and returns that same object.

Successful returns include readonly exact replay and do not prove a new commit.
`ContentionError` means only an original SQLite `DatabaseBusy` or `DatabaseLocked`
code, including their extended codes. `RetainedAuditError` means an original
`RequiresAudit`, `RetainedRecord`, `DatabaseReplaced` or
`ConnectionIdentityMismatch` variant. All other errors count as `OtherError`,
which asserts neither clean refusal nor `NoEffect`. No diagnostic is formatted,
cloned or inspected. Durations include original validation, SQLite and readback
up to return in five buckets: ≤1, ≤10, ≤100, ≤1000 and >1000 milliseconds. They
are not scheduler-attempt counts or physical/SLO evidence; panics that return no
original result produce no return sample.

Tests capture the actual calling thread's samples without a production reset.
Actual Store fixtures exercise update/exact replay, changed-sequence and invalid
sequence errors, malformed retained rows, native SQLite contention followed by
confirmed retry, and chunk-exhaustion nested returns. Stored bytes, sequences and
nonempty bounded logical claim observations remain unchanged on error/replay;
these fixtures supply no live capacity authority or physical qualification.
The contention fixture deliberately releases the Store's retained exclusive lock
using NORMAL mode and a completed read, then restores EXCLUSIVE **before** the
original retry invocation. It verifies the actual mode, uses a zero fixture
busy timeout and a real second-connection `BEGIN IMMEDIATE` reservation. It does
not claim that ordinary production EXCLUSIVE mode permits competing writers.
The blocker is rolled back before the genuine original successful retry.

Fixed backing, saturation, bucket boundaries, exact extended SQLite codes and
non-Clone result/diagnostic ownership are independently checked. Windows GNU
all-test compilation checks supported conditional bodies, not native execution.
Full OPT-1673/1670 lifecycle/other telemetry and OPT-1686 scheduler acceptance
remain open. These observations issue no permits, authorize no retry/refund and
complete no genuine settlement.
