# Catalog command admission and dispatch metrics (OPT-2106)

`CatalogService::submit` records the result of the existing validation, payload
and event-credit checks and bounded channel send. The worker records an accepted
command timestamp at its existing admission/closing check after channel receipt,
before closing discard or command handling. The hooks do not alter validation,
credits, cancellation, source ownership, dispatch or shutdown decisions.

`CatalogService::try_command_metrics` returns a fixed-size acquisition-local
snapshot or `None` if the admission mutex is busy. It performs no I/O, waits for
no lock, and returns no paths, source/sample IDs, payloads, error text, or effect
authority. Poison recovery follows the existing admission policy. Values reset
when a new service starts; they are observations, not durable recovery state.

| Observation | Actual boundary | Limit |
| --- | --- | --- |
| `accepted` | Existing channel send succeeds | Does not imply execution, commit or publication |
| `full` | Existing channel/event/payload credit rejects | Includes credit limits, not only channel occupancy |
| `closed` | Existing shutdown/closed/disconnected rejection | No timestamp or credits added |
| `invalid` | Existing input/curation validation rejects | No payload/error text retained |
| `admission_gate_wait` | Monotonic time acquiring admission mutex | Distinct from SQL writer acquisition/wait |
| `pending_dispatch` / high-water | Accepted timestamps awaiting worker receipt observation | At most channel capacity 32 plus one command received while waiting for admission: 33; not exact channel occupancy |
| `dispatched` / `dispatch_wait` | Timestamp consumed at existing worker admission check | Includes commands discarded during closing; does not mean successful work |

Both duration summaries expose count, total and maximum microseconds plus five
disjoint buckets: below 100us, 1ms, 10ms, 100ms, and the remainder. Arithmetic
saturates at `u64::MAX`. Timestamps occupy a fixed 33-slot FIFO ring. No per-event
history or dynamic labels accumulate. All submission and receipt bookkeeping
shares the existing admission serialization, so the worker cannot observe a
production command before its successful submission timestamp is recorded.

An unexpected timestamp overflow records `bookkeeping_overflow`, clears the
ring, and sets `dispatch_timing_available=false` for that owner lifetime.
Pending depth then has no completeness claim; later receipt observations are
untracked and cannot contribute fabricated latency. `untracked_dispatches`
records missing correlation. Production commands must use `submit`; internal
fixtures injecting directly into the channel do not qualify timing correlation.

Tests exercise the real owner paused before receipt: 32 accepted commands remain
pending, the next submission is Full, an absolute-path feature request is Invalid,
and a held admission mutex makes snapshot reads return immediately with None.
Shutdown drains command credits and preserves the existing Closed-event credit
until delivery; later submissions are Closed. Separate cases cover a disconnected
channel, ring wraparound/FIFO timing, saturating histograms, and explicit unknown
bookkeeping loss.

```sh
cargo test --locked -p yield-catalog --lib command_metrics
cargo test --locked -p yield-catalog --lib
bash scripts/agent.sh preflight
```

OPT-1673/1346 remain open for actual SQL writer wait, reserve/retry observations,
all required owners and startup/recovery qualification. This implements bounded
catalog metrics only, not an exporter, SLO verdict, physical effects or native
watcher qualification.
