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

The retained catalog lifecycle controller, automatic native hint-to-history
integration, bounded committed GPUI delta handoff, restart/retained retry and real
Finder/FSEvents acceptance remain required by OPT-2080. This request seam alone
does not complete it. Darwin callback execution still requires genuine native
SDK/runtime; pre-step CI failure supplies no native acceptance evidence.
