# Original Catalog thread completion (OPT-2370)

`CatalogService::shutdown_with_report` closes ordinary admission and joins the
original Catalog worker. Its bounded report records joined, worker panicked, or
missing original handle, plus whether original admission/join control was
poisoned before completion. Poisoned control is recovered so it cannot skip the
join; the report remains conservative. `Joined` with unpoisoned control confirms
thread completion only.

One original mutex protects the retained handle and completed report, including
the entire join. Concurrent callers cannot mistake a taken handle for a finished
join. Repeated callers receive the same completed report. The existing void
`shutdown` wrapper and Drop retain their synchronous join behavior.

Session retains the Catalog result, then finishes original Global and Journal
joins and releases its final profile guard before returning any error. Existing
Global error precedence is first, Journal second, and Catalog third. The public
shutdown cache implementation is unchanged; its new real Catalog failure test
checks the same failure is retained on a second call.

## Acceptance and limits

Tests exercise actual original worker healthy/panic completion, poisoned join
and admission mutexes, two concurrent callers while the worker is genuinely
paused, closed subsequent admission, and profile clone release after joining.
Session regressions release the original Catalog panic gate only after real
Closing, verify remaining owners join and original profile/journal child leases
can reopen, and exercise simultaneous Global and Journal failures separately.

The existing opt-in `test-support` feature exposes one bounded service-local
panic gate on the original worker outside its control locks. Its controller Drop
releases the gate on fixture failure. It is absent from default builds, adds no
runtime worker/channel, and supplies no ownership, successful receipt, physical
capacity or filesystem effect authority.

This report does not confirm every Source disposition, durable drain, enrollment
or retirement settlement, failure capsule cleanup, reservation refund, retry or
permit authority, clean physical volume handoff, or Native crash/power-loss
qualification. Full OPT-1670/1665/1666/2197 and the Native qualification issues
remain open. Windows GNU compilation is not Native execution evidence.

The two existing positive Catalog reserve-bootstrap regressions require qualified
macOS execution under OPT-2026; four existing Session maintenance/volume-handoff
regressions require the same under OPT-2235. Their unchanged bodies and fail-closed
production bootstrap remain required. Portable suite commands filter only those
exact named cases, with full unfiltered results recorded separately.
