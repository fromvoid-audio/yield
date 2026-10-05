# Source SQLite acquisition observations (OPT-2329)

`sample_sources::source_acquisition_metrics_snapshot()` returns thirteen fixed
process-wide saturating numeric counters: actual ordinary `BEGIN IMMEDIATE`
attempts, scheduled retries, six mutually exclusive return categories, and five
elapsed buckets. The categories are acquired, plain BUSY exhausted, other
contention not retried, nested transaction, ownership rejected, and other failure.

The existing `SourceDatabase::write_batch` reservation helper updates attempts at
the actual SQLite invocation and retries when its unchanged finite policy
schedules a wait. It records one return category and elapsed bucket after busy
timeout restoration and before returning the empty transaction. Initial lease,
profile or retained-owner refusal is distinct from setup/SQLite/restoration
failure. A nested transaction still rejects before changing timeout or ownership.
Successful acquisition is measured before any transaction body or commit.

Elapsed time begins at helper entry and includes original lease preparation,
strict ownership revalidation, SQLite reservation calls, scheduled waits and
busy timeout restoration. Exclusive buckets are <1ms, [1,10)ms, [10,100)ms,
[100,1000)ms and >=1s. Trace subscriber delivery, body, commit and receipt delivery are excluded.
The result category and elapsed instant are observed before the existing trace
event, so logging callbacks cannot become reservation-wait samples. A failed
restoration remains failure and drops any empty transaction as before.

The collector retains no IDs, paths, timestamps, SQL, raw error messages or
individual durations. There are no dimensions, queues, subscriber callbacks or
reset API. Relaxed atomic fields can describe different moments in concurrent
snapshots; do not require totals to agree while calls run. All fields saturate,
and acquisition/refusal counters describe helper outcomes, not durable effects.
The existing fixed per-call trace remains unchanged. Neither observations nor
trace events authorize retries, issue permits, refund reservations or establish
settlement, filesystem qualification or an SLO.

Verification executes the same production acquisition function with an isolated
numeric collector: real rival SQLite reservation exhaustion and release, one
committed body, existing active transaction rejection, genuine shared-cache
LOCKED and query-only refusal, and actual owned-source lease expiry without
renewal. Existing reservation tests retain namespace replacement, writer receipt,
restart and extended-code coverage. Numeric tests cover finite bucket edges,
saturation and the fixed 104-byte snapshot/collector.

```sh
cargo test --locked -p yield-library --lib reservation_retry -- --test-threads=1
cargo test --locked -p yield-library --lib sample_sources::db::write -- --test-threads=1
cargo test --locked -p yield-library --lib -- --test-threads=1
cargo check --locked -p yield-library
cargo test --locked -p yield-library --doc
bash scripts/agent.sh preflight
```

OPT-1673 remains open for other owner/reserve/retry/retained and full lifecycle
acceptance. OPT-1686/1685 retain their scheduler, lease/jitter/cancellation,
all-owner pre-intent WAL and durable disposition requirements.
