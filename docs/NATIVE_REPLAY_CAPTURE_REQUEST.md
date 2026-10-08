# Nonblocking native replay capture (OPT-2080 work in progress)

`NativeReplayCaptureRequest::start` starts the existing original FSEvents history
producer on a separate thread. Source/physical-root context is bounded and the
positive deadline is at most two seconds, including worker scheduling. Unsupported
hosts reject before creating a worker or slot. The API performs no Source SQL,
checkpoint, audit, catalog mutation or GPUI publication and grants no continuity.

The original worker, request and opaque delivered candidate share one capture slot.
Polling is nonblocking: an empty receiver does not consume the reply, and delivery
can be consumed only once. Cancellation suppresses delivery and dropping a request
signals cancellation; neither refunds a live worker's slot. The original native
producer retains its own active-stream guard until actual native stop. A timeout
or canceled reply supplies no native-quiescence, cleanup or ownership grant.

`CapturedNativeReplay::into_observation_batch` uses the retained owner's current
lifecycle/watcher generation and keeps the slot with a nonempty opaque batch.
`observation` borrows passive input for existing catalog admission; it is not a
receipt. A controller must keep the wrapper through admission/reply, revalidate
actual owner/prior/revision and preserve genuine retained envelopes on denial.
Empty certain history has no batch; uncertain history retains a diagnostic marker.

Local tests cover empty polling, cancellation/lost reply through a genuinely
blocked worker, refusal of a second request, original result/batch custody and
final release. The candidate fixture is passive data and is not a native producer,
durable prior, continuity proof, Source revision or permission to mutate.

The catalog now retains a controller with the registered source's watcher schedule.
It starts history off the catalog thread only from actual owner-read durable prior,
root, lifecycle and revision; pathless hints supply no cursor or proof. New hints
coalesce while a request is pending. Capture errors widen to the existing audit;
empty history retains the original hint for a five-second bounded retry window,
because device history can arrive after notify/HistoryDone. Retries re-read actual
owner/prior and use the remaining deadline; expiry requires recovery audit.
uncertain consumer outcomes keep the original opaque batch for bounded retry. A
retry of that same envelope reuses its original consumer permit rather than needing
a ninth slot when all eight slots are retained. Source removal cancels delivery,
while the original worker keeps custody until it finishes.

`submit_captured_replay` accepts an `Arc` so callers keep the original on busy/closed
admission. Both actual submission and retained replay state keep that original
capture, including after a dropped reply or uncertain SQL outcome. The existing
consumer performs all durable-prior, stream, Source, revision and exact/subtree
checks. Duplicate readback fabricates no delta; a missing cached predecessor
requires a conservative recovery audit.

Confirmed transactions publish `ReplayCommitted` through the existing event budget
and publication fence. The passive delta includes at most 64 manifest rows and 128
directory additions/replacements/absences from the same bounded subtree transaction.
There is no whole-source SQL hydration. The cached catalog snapshot patches both
manifest and directory truth. GPUI checks source, lifecycle, physical root, prior
revision, persisted receipt and publication fence, then patches retained data in a
background projection. Changed WAV metadata is invalidated; unchanged entries and
curation stay intact. Delivery pauses while a projection is pending, preserving the
next delta on the catalog. A missing UI predecessor requests recovery audit instead
of manufacturing a patch from the checkpoint.

The GPUI implementation still copies its retained vectors/maps into the background
projection and rebuilds local indices/tree; this is no database hydration, but its
large-source cost must be measured and, if needed, reduced before responsiveness
acceptance. This draft is not a claim of constant-cost UI updates or completed
OPT-2080. Native cancellation/root/overflow/restart, actual callback ABI/volume
history, Finder/GPUI interaction and broader Source physical-family acceptance
remain unqualified. The new pinned native lane requires exact nonzero tests,
including an actual native hint producing a catalog delta, SDK checks and APFS
facts. A pre-step CI failure supplies no native execution evidence.
