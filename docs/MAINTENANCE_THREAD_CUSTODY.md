# Original maintenance worker custody (OPT-2377)

A maintenance operation can release its shared admission before its original
thread exits and drops its retained profile or disposable-cache namespace.
Public admission remains nonblocking: it may reserve a successor at that point.
The session owner must join the exact predecessor before replacing its handle
or starting that successor's filesystem work. A successful operation result or
admission release is not evidence that the original thread has been joined.

`MaintenanceService` retains one original handle. Both storage and cache starts
join that predecessor before consuming the successor reservation or capturing
its filesystem inputs. Exact reservation IDs ensure predecessor cleanup cannot
release the accepted successor. An actual predecessor join panic is retained in
one bounded sticky flag; later requests are rejected before starting effects,
and their own unstarted control reservations are released normally. Ordinary
operation errors and cancellation remain request results, separate from an
actual `JoinHandle::join` panic. No additional production worker, queue or
finished-handle inventory is introduced.

Shutdown cancels and joins the retained original worker. Session retains the
maintenance completion result, performs every original Catalog/Global/Journal
join and final profile release, then reports errors with existing Global,
Journal and Catalog precedence ahead of maintenance. Repeated private teardown
retains a maintenance panic. The public shutdown cache implementation is
unchanged; existing public healthy and Catalog repeated-failure tests cover that
cache, rather than claiming a new public maintenance fault integration.

## Acceptance and limits

Real original worker tests pause after operation admission release but before
thread exit, reject detached-predecessor shutdown, preserve the successor's exact
handle while joining its predecessor, observe its exact admission after that
join and before its own finish using fixture-only original result-channel
rendezvous backpressure, cover an original cache predecessor,
retain an actual post-operation thread panic across refused successors, and
preserve ordinary cancellation. Real Session tests release the worker only after
original shutdown cancellation, prove all other original owners and child leases
finish, exercise Global failure precedence and healthy repeated teardown.

The tiny service-local cfg(test) completion gate is absent from production builds,
adds no alternate worker or queue, waits outside original control locks, and its
controller Drop releases the original worker. It supplies no successful receipt,
capacity provider, filesystem effect authority or positive Native qualification.

Completion evidence is limited to original threads and their captured custody.
It does not prove cache cleanup completeness, every Source disposition, durable
settlement, control-state reconciliation, retry/refund/permit authority, clean
physical volume handoff, or Native crash/power-loss qualification. Full
OPT-1670/1665/1666/2197 remains open. Native maintenance/bootstrap/volume handoff
under OPT-2235 and Windows handle-bound cleanup qualification remain required;
Windows GNU compilation is compile evidence only. Original predecessor and final
maintenance joins on macOS and Windows require the runtime checks in OPT-2381.
Unfiltered native failures
must be recorded separately from exact existing portable-suite filters.
