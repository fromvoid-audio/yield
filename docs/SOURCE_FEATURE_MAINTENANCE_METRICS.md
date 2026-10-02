# Source feature maintenance observations (OPT-2163)

`SourceWriter::prune_superseded_feature_artifacts` records a fixed process-local
`FeatureMaintenanceMetrics` snapshot once per actual call, including validation
before SQL. Instrumentation wraps the existing command and preserves its owner,
revision, cursor, audit, cancellation, WAL and exact deletion/rollback checks.
There is no change to scheduling or the database schema.

`attempted` partitions into `committed`, `stale`, `cancelled` and `failed` until
saturation. Outer stale-owner/revision/cursor rejections are stale; typed readiness
cancellation is cancelled; other errors (including invalid bounds, expired lease,
SQL, deadline and WAL admission) are failed. A successful committed report includes
unchanged or ambiguous retention; it does not imply that a payload was deleted.
Only successful reports add inspected candidates, removed rows and logical payload
bytes. Work rolled back after a DELETE adds no candidates or removal totals.

Total and maximum elapsed microseconds use a monotonic clock around the actual
call, including rejected calls. Counters saturate instead of wrapping, and the
`saturated` boolean means affected values are lower bounds. These observations
are not a latency benchmark or an SLO. All fields are numeric or boolean scalars;
there are no path/source/key/error labels, event history, extra queue, persistent
SQL telemetry or database recount.

`feature_maintenance_metrics(&SourceRevision)` returns a copy only under strict
current read-only owner/revision validation. Reading does not renew the lease or
write SQL; expired, foreign and previous-owner observations reject. Constructing
a retained writer acquisition initializes fresh zero counters. Existing feature
publication/rebuild and source snapshot metrics remain independent.

Verification extends the actual reverse-owner SQLite regressions with exact
outcome partitions for unchanged/selective pruning, private foreign/stale cursors,
cancellation, trigger rollback, ambiguous evidence, SQL deadline and pinned-reader
hard WAL rejection/retry. A real owner handoff/read-only test also checks persistent
bytes near lease expiry, expired rejection and reset. The scalar saturation/JSON
test qualifies representation only. Run:

```sh
cargo test --locked -p yield-library --lib feature_maintenance
cargo test --locked -p yield-library --lib feature_inverse
cargo test --locked -p yield-library --lib sample_sources::db
bash scripts/internal/agent/run_agent_preflight.sh
```

Logical payload bytes do not measure SQLite pages or WAL bytes physically
reclaimed, external cache cleanup, source retirement or capacity settlement.
Full artifact metrics acceptance remains in OPT-1659; native SourceSQLite
verification remains OPT-2055. Actual catalog scanner/WAV-DSP regression evidence
is separate from fixed payload vectors and observer representation.
