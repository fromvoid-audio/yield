# Global worker dispatch observations

`GlobalLibraryWriter::dispatch_metrics()` returns fixed acquisition-local numeric
diagnostics for its existing retained worker. The ordinary command transport
retains its capacity of 32 and its FIFO/nonblocking admission behavior.

A private envelope captures a monotonic instant immediately before the existing
`try_send`, after availability revalidation and acquisition of the admission
gate. At successful worker receive, it captures elapsed time before the worker's
second profile validation. Only after that validation succeeds does it increment
the saturating dequeue count and one fixed bucket: <1ms, [1,10)ms, [10,100)ms,
[100,1000)ms or >=1s. Failed validation, profile-loss drain, dropped receiver,
full/disconnected submission and undelivered commands produce no normal dequeue
sample. Ordinary accepted work drained during normal shutdown still counts if
the worker dequeues and validates it.

Elapsed time includes `try_send` transport and scheduling overhead and actual
queue residence. It excludes the admission-gate wait, subsequent profile
validation, SQL execution, SQLite reservation retries and result delivery.
Registry reads/mutations, preferences, vocabulary and Harvest use the same
private envelope. No command contents enter the collector. The instant exists
only in each bounded queued envelope and is discarded at dequeue/drop; snapshots
contain neither raw timestamps/durations nor source/profile/operation/path/SQL
or error identifiers. There is no dimension registry or extra command buffer.

The collector uses six saturating relaxed atomic counters and three fixed queue
counters under a short mutex. Successful nonblocking send and backlog increment
are serialized with dequeue/drop accounting. Each accepted envelope owns one
private ticket: normal dequeue removes it from backlog, while explicit discard
or receiver destruction/unwind removes it and increments abandoned once. Failed
full/disconnected sends contribute nothing. Current backlog excludes executing
commands; its boundary is completion of dequeue accounting, so an envelope just
removed from the channel can briefly remain counted while awaiting that lock.
Peak counts that successful envelope backlog, including this short boundary;
it is not a claim about the channel's instantaneous physical length. The queue
fields are internally consistent. Dispatch atomics in concurrent snapshots
are read independently and can differ across moments; do not require count
and bucket totals to agree while work runs. Counts are cumulative for this owner
acquisition, remain observable after shutdown, and start fresh with a new owner.
Saturation loses later samples. Observations never control queue admission,
lifecycle, retries, SQL or capacity, and cannot establish an SLO.
Dequeue is not a committed mutation or physical qualification.

Verification uses actual bounded channels for full/disconnected/discard and
measured queue-delay cases; collector tests cover bucket edges, saturation and
fixed numeric storage. A real retained `GlobalLibraryWriter` fixture uses registry,
vocabulary and Harvest routes. The Harvest request comes from a real profile-owned
source SQLite manifest audit and owner-issued observation, not a synthetic
receipt/grant. Existing owner tests retain FIFO, exact errors, shutdown and profile
replacement coverage. Run:

```sh
cargo test -p yield-library --lib dispatch_metrics --locked
cargo test -p yield-library --lib sample_sources::library::owner --locked
cargo test -p yield-library --lib ordinary_retry --locked
cargo test -p yield-library --lib global_wal --locked
scripts/agent.sh preflight
```

OPT-2161 supplies validated dispatch counts and residence buckets; OPT-2328
adds successful envelope backlog, peak and abandonment. OPT-1673 retains other
owners, reserve/retry/retained outcomes and full lifecycle acceptance; OPT-2135/2136/2137 retain their WAL
reservation/admission/required-commit contracts and production authority work.
