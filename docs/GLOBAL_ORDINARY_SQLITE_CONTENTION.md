# Ordinary global SQLite writer contention (OPT-2096)

The existing captured ordinary global registry, preferences and vocabulary set/delete
routes acquire their retained connection's `BEGIN IMMEDIATE` reservation with at most
three nonblocking attempts. Plain `SQLITE_BUSY` before a transaction begins permits
5 ms and then 10 ms worker backoff. `SQLITE_LOCKED` and extended busy results (including
snapshot/recovery) return immediately with their exact SQLite result code. Other SQL
errors retain their original error. An already active transaction is rejected.

`LibraryError::OrdinaryWriteContention` records the number of attempts and extended
result code; it acknowledges no effect. Fixed path-free tracing records each contention
attempt and its retry decision. This is bounded lock acquisition on the existing worker,
not rescheduling arbitrary commands or replaying effects. A caller may resubmit through
its original captured context; the existing acquisition, proposal revision, payload,
vocabulary CAS, WAL-admission and ownership gates still apply.

The connection's prior `busy_timeout` is restored on acquisition success and error,
before the SQL body executes. Restoration failure returns an SQL error and drops any
new transaction. Bodies and commits execute once and are never automatically retried.
Exhaustion cannot advance proposal fences, vocabulary revisions or metadata. No new
journal row, durable failure receipt, schema or filesystem authority is introduced.

Validation uses actual SQLite contention on a second connection and the retained global
worker. Tests check exact attempts, released contention with one body execution,
timeout restoration, query-only failure, active transaction rejection, effect failure,
all four worker routes, resubmission/replay, and state after worker restart. Run:

```sh
source /workspace/.yield-cloud/env.sh
cargo test --locked -p yield-library --lib ordinary_retry -- --test-threads=1
cargo test --locked -p yield-library --lib sample_sources::library -- --test-threads=1
cargo check --locked -p yield
cargo fmt --all --check
python3 scripts/internal/check/check_gpui_owner_boundary.py
python3 scripts/internal/check/check_source_db_competing_writers.py
python3 scripts/internal/check/check_filesystem_producers.py
```

Three attempts and 15 ms intentional backoff do not bound OS stalls or transaction body
latency. Existing source/saga/recovery retry families and universal pre-intent WAL gates
remain under OPT-1686/1685; OPT-1348 remains open. Restart tests establish retained SQL
state, not native crash/power-loss or physical durability qualification.
