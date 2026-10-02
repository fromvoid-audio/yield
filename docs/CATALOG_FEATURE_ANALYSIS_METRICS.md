# Catalog feature analysis observations (OPT-2188)

The actual `feature_request::request` catalog worker records fixed acquisition-local
`FeatureAnalysisMetrics` under the existing admission mutex. The collector retains
only numeric/boolean scalars, no source/path/request/key/error labels, payloads,
timestamps, per-request history, extra queue or SQL telemetry.

`entered` counts actual worker invocations, excluding submission rejection.
Closed/retired/missing sources increment `dropped_before_work`. Otherwise exactly
one terminal Ready or Blocked observation is counted. StaleSource and Cancelled
are explicit subsets of Blocked; intermediate preparation, queueing and running
states do not add terminal counts. Already-ready retry is another validated Ready
observation, not a newly committed payload.

The existing `publish_source_if_current` boolean supplies terminal enqueue versus
projection rejection counts. Both current lifecycle checks and the retained owner
publication fence remain unchanged. Enqueue is not event consumption, current UI
confirmation, full source readiness or physical success. A late Ready observation
can be rejected after source retirement while its correctly committed payload and
marker remain retained.

After requests settle, before saturation:

```text
entered = dropped_before_work + terminal_ready + terminal_blocked
terminal_enqueued + terminal_projection_rejected = terminal_ready + terminal_blocked
```

During work, entered may exceed settled outcomes. Event delivery can also precede
the final diagnostic update; callers must not use metrics as an acknowledgement.
Total/max elapsed microseconds use a monotonic clock from worker entry through
drop or terminal publication decision, including validation, I/O and DSP rather
than pending queue wait. These observations are not a benchmark or SLO. Counters
saturate rather than wrap; `saturated` means affected values are lower bounds.

`CatalogService::try_feature_analysis_metrics` copies the snapshot with a nonblocking
try-lock, returning None while admission is busy. It performs no SQL, lease renewal,
recount or scheduling. A new service acquisition starts zero counters; shutdown
does not fabricate completion.

Tests use real CatalogService, scanner, WAV/DSP, SourceWriter publication and SQLite
observations for current Ready/already-ready, failed decoding/durable retry,
cancellation, stale revision and source retirement. A cfg(test)-only pause schedules
retirement after the actual result but before the unchanged producer fence; it
injects no state, receipt or authority. Tests inspect all events through removal,
verify payload/marker retention, nonblocking read-only snapshots and acquisition
reset. A separate fixed-size/saturation representation fixture is not production
or physical qualification.

```sh
cargo test --locked -p yield-catalog --lib feature_analysis_metrics
cargo test --locked -p yield-catalog --lib feature_request
bash scripts/internal/agent/run_agent_preflight.sh
```

Full projection/artifact/replay and external-cache metrics acceptance remains in
OPT-1675/1669/1658. Native SourceSQLite, held-WAV and GPUI qualification remain
OPT-2055/2061/2062/2075. Neither a counter nor these software tests supplies a
physical capability, capacity grant or stronger durability claim.
