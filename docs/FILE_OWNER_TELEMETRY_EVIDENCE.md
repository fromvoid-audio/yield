# File-operation owner effect queue observations (OPT-2100)

`FileOperationOwner::telemetry_snapshot()` exposes fixed aggregate observations
from the existing production submit and Execute dequeue boundaries. A snapshot
contains nine saturating admission counters, six dispatch-wait buckets, current
and peak successfully enqueued effect backlog, and abandoned effect envelopes.
It contains no operation/source identities, locators, profile names, request
bytes, diagnostics, or arbitrary metric labels.

Every effect submission attempt contributes one outcome, including duplicate
receipt replay. Accepted means queued, not effect success. Control commands
(root registration, acknowledgement, custody and shutdown) do not contribute.
Full-channel rejection refunds the existing permits and cannot increase backlog
or peak. Dequeue decreases backlog before validation/execution; an executing
request, retained receipt and outstanding reply handle are separate resources.
Worker exit clears waiting backlog and counts envelopes not dequeued as abandoned,
without inventing a dispatch sample or successful recovery disposition.

The dispatch clock starts immediately before constructing/sending an Execute
envelope and stops when its handler begins. Exclusive buckets are <=1ms, <=10ms,
<=100ms, <=1s, <=10s, and >10s. This clock includes the short sender/telemetry lock
boundary; it excludes canonicalization, admission-gate wait, execution, SQL,
physical effects and subsequent receipt custody. A short fixed-state mutex
serializes nonblocking send/backlog increment against dispatch so fast workers
cannot decrement before the increment. It runs no I/O and adds no worker thread,
scheduler retries, dynamic sample history or measurement scan.

A snapshot is internally consistent, but Accepted is incremented after the send
returns; a concurrent snapshot can see its dispatch before the Accepted counter
increment. These observations must never be used for admission, capacity claims,
authority, shutdown custody, durability qualification or SLO enforcement.

## Verification

```sh
cargo test --locked -p yield-library --lib file_operation_owner -- --test-threads=1
cargo check --locked -p yield
bash scripts/internal/agent/run_agent_preflight.sh
```

New tests use existing real owner queues and test-only authorization/barriers to
exercise 32 waiting effects, rejection/refunds, draining, duplicate replay,
prequeue budget/validation rejection, stopped/unavailable owners and worker panic.
Fixed bucket boundary and saturation tests use deterministic durations. These
fixtures confer no physical effect or protected-reserve qualification.

OPT-1673 retains writer wait, reserve/retry outcomes and complete lifecycle scope;
OPT-1670 and the tracking parents remain open. This change supplies only actual
file-owner effect admission/dequeue observations.
