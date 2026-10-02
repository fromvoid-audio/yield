# Global owner admission diagnostics (OPT-2097)

`GlobalLibraryWriter::admission_metrics()` and the corresponding vocabulary-client
method return a fixed-size snapshot belonging to that owner acquisition. Client
clones share the same counters. A replacement writer starts at zero. The snapshot
contains no profile, operation, path, payload, database error or authority token.
Counters saturate at `u64::MAX`; concurrent multi-field snapshots are approximate.

The existing `ensure_available` rejection and `send_command` admission result
increment exactly one of accepted/full/initializing/unavailable/closed. Invalid
payload, route and bound rejections before queue admission are excluded. An accepted
request means `try_send` succeeded: it does not mean SQL succeeded, a reply arrived,
or a durable operation completed. Counts cannot reconstruct queue depth.

Only an actual acquisition of the existing admission/shutdown mutex records a wait.
The timer starts immediately before locking and stops immediately after acquiring;
the five non-cumulative buckets are <1ms, [1,10)ms, [10,100)ms, [100,1000)ms and >=1s.
Pre-gate lifecycle rejection records no wait. This is admission-gate contention,
not queue residence, writer acquisition, SQLite contention or command execution.

Fixed-field debug tracing on `yield::global_owner::admission` publishes the typed
outcome and wait bucket after classification. Subscribers run after the admission
mutex is released. This producer has no durable operation ID or journal sequence
and does not pretend to emit the broader `owner.queue_observed` vocabulary, whose
queue-depth evidence remains absent. No exporter or SLO threshold is installed.

Validation commands (Linux, pinned repository toolchain):

```
cargo test --locked -p yield-library --lib admission_metrics
cargo test --locked -p yield-library --lib sample_sources::library::owner
bash scripts/agent.sh preflight
git diff --check
```

Fixtures cover production bounded queue saturation, clone sharing, lifecycle
rejection, gate contention, finite bucket boundaries/counter saturation, and a
real profile-owned SQLite writer with a withheld completion reply and shutdown.
Blocked in-memory queues are explicitly test fixtures; the retained-owner test
also runs the actual worker and SQL path. No fixture qualifies physical durability
or capacity. OPT-1673 retains queue depth, writer wait, reserve decisions, retries
and retained outcomes; OPT-1670 retains its full startup/recovery/shutdown matrix.
