# Protected-reserve namespace observations (OPT-2111)

`operation_journal::reserve_telemetry::reserve_telemetry_snapshot()` reads a fixed
24-counter matrix. It aggregates actual return boundaries for namespace create,
complete reopen and partial reopen across owners in this process. The outcomes
are Bound and the seven existing typed failure stages. Counters saturate, survive
owner replacement within the process and reset on process exit. Each field is
sampled independently; the snapshot does not claim a coherent multi-owner state.

The same observer emits an INFO event on `yield::reserve_namespace` with exactly
two fixed fields: `reserve_attempt` and `reserve_decision`. Values are finite static
categories. No identity, file path, profile name, volume, descriptor facts, SQL,
error text, dynamic registry or retained per-request sample crosses this boundary.

The original operation completes before its result is observed; the observer
returns that exact result, including the owned failure descriptor. Unsupported
still returns before namespace inspection/creation. Observation performs no
filesystem or database work, activates no platform backend, schedules no retries,
and cannot grant capacity, recovery authority or physical qualification. Bound
reports the existing namespace function's result, not an ordinary operation's
admission, durable ledger transition, publication or successful recovery.

```sh
cargo test --locked -p yield-library --lib reserve_telemetry -- --test-threads=1
cargo test --locked -p yield-library --lib capacity_reserve_namespace -- --test-threads=1
bash scripts/internal/agent/run_agent_preflight.sh
```

Linux exercises all three real entrypoints returning Unsupported with no namespace
effects, failure files, descriptor observations or allocation observations. A
thread-local tracing collector checks their exact fields. Independent fixed-stage
fixtures check every counter/category and saturation. Those fixtures supply no
successful reserve backend or reserve ledger evidence. Existing macOS namespace
allocation/binding/reopen tests remain the physical verification lane; this Linux
run cannot execute that platform's native tests. Parent OPT-1673 retains reserve
ledger/retry and full owner lifecycle requirements; physical tracking parents stay
open.
