# Session journal shutdown result (OPT-2366)

`Persistence::shutdown` preserves the existing dependency order: stop maintenance,
join catalog workers, join Global, clear pending ordinary persistence handles,
join the journal, then release the session profile guard. Errors are retained as
results rather than returning before the remaining joins.

The journal's final boolean and retained lifecycle are now consumed after its
real worker joins. A failed drain returns `SessionError::Persistence` with the
original journal unavailable diagnostic, or an explicit unconfirmed diagnostic
if no unavailable reason exists. An earlier Global drain error keeps its existing
precedence; a later journal failure cannot turn that error into success.

The public session shutdown already caches its final result. Repeated calls
retain that result; this change does not add a new journal shutdown invocation or
change public command admission. Existing healthy shutdown still returns `Ok`.

## Evidence and limits

The regression starts real profile-owned SQLite, catalog, Global and journal
workers. The existing test-support panic fixture pauses an accepted journal
command. Earlier persistence status remains healthy. Only after the original
journal enters Closing does the fixture release its command and unwind the
worker. Session shutdown must return the resulting failure after all owner joins,
close later admissions, and allow actual profile and journal child-lease reopen.
The fixture confers no physical capacity or filesystem effect authority.

This final drain result is an in-process teardown observation. It is not durable
participant disposition, publication settlement, verified cleanup, reservation
refund or retry permission, clean volume handoff, crash/power-loss qualification,
or completion of OPT-1670/1665/1666 or publication-aware shutdown.
