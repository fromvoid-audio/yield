# Existing global-owner shutdown contract

OPT-1838 covers the existing finite admitted global command queue and its sole SQLite owner.
`begin_shutdown` closes admission under the same gate as submission and drops the sender.
Accepted work retains the original profile acquisition and child lease until the worker exits.
Initialization cannot turn Closing back into Available; cancellation before opening SQLite
creates no database. Already initialized owners drain accepted commands before closing.

`try_finish_shutdown` returns `None` while that worker is running. Callers must retain the owner
and report pending when their deadline expires. It joins only once the thread reports finished;
OS scheduling and thread-local teardown mean this is cooperative bounded observation, not a
hard realtime shutdown deadline. `shutdown_with_report` is the blocking teardown-worker API.
Drop remains blocking and never silently detaches an owner that retains writable authority.

The report retains the original acquisition and one sticky outcome: Drained, Unconfirmed with
the existing initialization/ownership diagnostic, or WorkerPanicked. Drained means accepted
commands reached their terminal reply paths and the owner closed normally. Individual command
errors, rejected effects and lost replies remain their own outcomes. A panic closes cloneable
client availability even before the joining owner observes it. The old `shutdown` boolean means
only that admission closure began, and must not be interpreted as a drain acknowledgement.

Session teardown stops maintenance, catalog, global writer, then journal and its final writable
profile custody. Global failure is propagated after cleanup. A shutdown acknowledgement channel
failure still joins the session worker; its actual error/panic wins over the channel diagnostic.
The service retains the result so a repeated shutdown cannot convert failure into success.

## Actual portable evidence

The owner shutdown fixtures execute the real queue, SQLite connection, retained profile lease
and subprocess ownership lock. They cover pending initialization without premature resource
release, cancellation without a SQLite open, an accepted commit with a gated terminal reply,
late admission rejection, worker panic and replacement ownership, corrupt SQLite with unchanged
bytes, cross-process conflict, migration by the sole owner, and observational read-only opening.
The subprocess helper is intentionally ignored when discovered alone and explicitly dispatched
with a ten-second kill deadline by its parent test. Session fixtures exercise actual worker
unwind, sticky failure, replacement profile startup and reverse cleanup after global panic.
They use the explicit metadata-only portable fixture; this does not qualify reserve provisioning.

## Bounded exceptions retained by the tracking scope

The report is an in-memory teardown observation, not a durable journal checkpoint, verified
completion, file-effect grant or replacement acquisition. No new persistence route is introduced.
Durable dispositions and full coordinator/lifecycle delivery remain the corresponding OPT-1357,
OPT-1569 and OPT-1670 scopes, and unfinished Harvest consumers retain OPT-1831/1645 acceptance.
A successful existing-owner drain cannot satisfy those independent contracts. Native service
integration, reserve durability and physical qualification remain their existing platform issues.
The original audit's historical table remains evidence for its recorded commit, not current code.
