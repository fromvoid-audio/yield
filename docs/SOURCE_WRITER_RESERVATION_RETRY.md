# Source batch writer reservation retry (OPT-2132)

`SourceDatabase::write_batch` uses the retained source connection for at most three
`BEGIN IMMEDIATE` attempts. Only exact plain `SQLITE_BUSY`, while SQLite still
reports autocommit, permits another attempt. The owner thread waits 5 ms and then
10 ms between attempts. These limits cover this acquisition loop; they do not
promise a bound on operating-system identity checks, initial lease preparation,
transaction bodies, commits or unrelated caller retries.

The existing mutation validator prepares the lease once before acquisition.
Every attempt and the acquired transaction are revalidated through the strict
retained profile/database/lease validator, which never renews through the control
connection. Expiry or replacement during the wait rejects the batch. The
repository's explicit unowned test-support fixtures retain their existing
behavior; normal builds still require actual owned source bindings.

The helper saves the current busy timeout, sets zero for acquisition, and restores
it on success or failure before returning any transaction to its caller. A
restoration failure drops any acquired empty transaction. A caller's active
transaction is rejected before ownership preparation or timeout changes.

Exhausted plain BUSY retains `SourceDbError::Busy`. LOCKED, extended BUSY (including
SNAPSHOT and RECOVERY), noncontention failures and ownership failures preserve
existing error mapping. No transaction body or commit is retried. SQLite writer
reservation success does not mean a caller mutation or durable receipt succeeded.

One INFO event on `yield::source_writer_acquisition` contains only `attempts`,
`retries`, `sqlite_code` and a fixed `outcome` category. Attempts are actual BEGIN
calls (0..3); retries are scheduled waits (0..2). The code is the last BEGIN or timeout-restoration SQLite
failure observed by this helper, or zero when neither failed; a
successful third attempt can therefore report the preceding BUSY code. Initial
ownership/setup rejection records zero attempts and no acquisition code. Outcomes
are `rejected`, `ownership_rejected`, `acquired`, `busy_exhausted`,
`contention_not_retryable`, `sqlite_error` and `timeout_restore_failed`. No path,
profile/source/operation identity, SQL text, raw error message or owned evidence
is emitted. These observations grant no retry, capacity or admission authority.

```sh
cargo test --locked -p yield-library --lib reservation_retry -- --test-threads=1
cargo test --locked -p yield-library --lib sample_sources::db::write -- --test-threads=1
bash scripts/internal/agent/run_agent_preflight.sh
```

The focused tests use actual SQLite rival connections and retained source owners,
plus a real SourceWriter receipt/restart route. Extended-code classification
fixtures do not claim a filesystem fault or physical recovery qualification.
Full source/saga/recovery scheduling, lease/jitter/cancellation, universal
pre-intent WAL reservations and durable failure dispositions remain OPT-1686,
OPT-1685 and OPT-1348. This leaf completes only the ordinary source batch
acquisition boundary.
