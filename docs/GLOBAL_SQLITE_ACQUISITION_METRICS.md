# Ordinary global SQLite acquisition observations

`sample_sources::library::global_acquisition_metrics_snapshot()` returns fixed,
process-wide cumulative diagnostics for the existing ordinary metadata writer
reservation boundary. Registry, preferences and vocabulary mutations use that
boundary. Other transaction routes are excluded; rejected commands that never
reach it produce no acquisition observation.

The attempt counter increments immediately before each actual `BEGIN IMMEDIATE`.
The retry counter increments before an already permitted pre-effect backoff.
One final outcome and elapsed bucket are recorded per returning invocation,
including active-transaction rejection and noncontention failures. Elapsed time
starts at invocation and includes timeout inspection, reservation attempts,
backoffs and restoration. Restoration failure overrides an acquired reservation
with failure, after the transaction has been dropped and rolled back.

Outcomes are acquired, exhausted plain BUSY, other non-retried contention,
active-transaction rejection and other failure. Exact extended SQLite codes
remain in the original caller error, rather than creating unbounded metric
dimensions. The fixed elapsed buckets are <1ms, [1,10)ms, [10,100)ms,
[100,1000)ms and >=1s. No paths, profiles, source/operation identities, SQL,
error payloads, individual durations or timestamps are stored in these counters.

All counters use saturating relaxed atomics. Snapshots independently read fields,
so a concurrent snapshot can contain increments from different moments. Do not
require cross-field equality while work is running. Counts aggregate every
profile/owner instance in this process, are not reset by owner restart, and reset
on process restart. Saturation loses subsequent observations. No per-request or
per-owner diagnosis can be reconstructed from this snapshot.

Metrics do not control admission, retry decisions, timeout policy, ownership or
capacity. The original maximum of three attempts, retry of plain BUSY only,
exact caller errors, timeout restoration and body/commit-once behavior remain.
An acquired writer reservation is not a committed mutation. These counters and
SQLite fixtures do not establish native durability, physical qualification or
an SLO. OPT-1673/1686/1348 retain their other acceptance requirements.

Portable verification: `cargo test -p yield-library --lib ordinary_retry --locked`
uses real held/released rival writers, shared-cache LOCKED, a nested transaction,
a query-only failure and the retained GlobalLibraryWriter's actual mutation
routes. Local collectors make exact boundary assertions independent of concurrent
tests; the worker fixture checks lower bounds on process-wide increments. Fixed
numeric storage, saturation and elapsed bucket edges are tested separately.
