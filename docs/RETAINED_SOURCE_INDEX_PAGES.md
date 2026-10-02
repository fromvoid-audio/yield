# Retained source index page publication (OPT-2023)

The catalog worker publishes bounded retained index facts through
`CatalogCommand::RequestIndexPage` and `CatalogEvent::IndexPageReady` or
`IndexPageFailed`. `yield-session` reexports these shapes. The registered
`SourceWriter` retains the database and source lease. Neither UI code nor this
query opens a database or enumerates media directories.

Requests identify a stable source, registered lifecycle and caller query.
The first page may discover the independent index revision; later requests can
require that revision. This revision is separate from the supported-sample
manifest/catalog revision. `CatalogIndexPageCursor` privately binds source,
lifecycle, query token and the library's connection/root/revision cursor. Source
replacement or reopening requires a fresh traversal. Changing the query cancels
its predecessor, including queued results. Reusing a query number does not revive
an old token. Audit admission and audit execution (including watcher-triggered
audits) invalidate index queries before changing retained facts; callers retry
after the audit completes. Removed, superseded or unavailable lifecycles may
discard these ephemeral requests without a completion event.

Reads validate the profile and retained source-publication lease before and after
the database transaction. Admission is rechecked under its mutex before enqueue.
`CatalogDelivery` retains both the source fence and query cancellation token.
Its delivery check discards stale projections. Callers using asynchronous
projection must still validate the returned source fence before/after slow work
and compare the event's query identity with their current query. Popping or
abandoning a delivery releases queue credits using the existing event boundary;
consumer retention after delivery is the consumer's responsibility.

| Bound | Enforcement |
| --- | --- |
| Rows | 1–128 returned, at most one lookahead row |
| Variable database fields | 128 KiB aggregate, including root and lookahead; SQLite text/blob values are borrowed and checked before allocating keys, paths or identities |
| Request source identifier | 4 KiB |
| Continuation input | 128 KiB plus 4 KiB for its source binding; counted in the shared input byte reservation |
| Delivery | Conservative 1 MiB reservation per admitted page, including decoded/raw keys, cursor, row metadata and error fields; shared 32 MiB payload budget |
| Concurrent work | Existing 32-command channel and 64-event credits; full admission returns `Full` |

The library field budget covers path, classification, identity and diagnostic
storage bytes, including the extra row. Keys can be duplicated and decoded paths
can expand, so the delivery credit deliberately exceeds that storage budget.
Scalar metadata and at most 128 row containers are included in the conservative
credit. Malformed rows fail visibly; oversized data is `ByteLimit`. Errors use a
bounded typed vocabulary instead of forwarding arbitrary database text.
A missing legacy index schema produces `schema_available = false`, revision zero,
and no rows/continuation without migrating the database. This is unavailable
knowledge, not evidence of an empty scan.

Every success explicitly carries `RetainedUnknown` completeness. Rows preserve
classification, diagnostics and lossless relative paths. Exhausting the retained
table does not establish a complete/current scan, live filesystem object
identity, or reveal/delete permission. There is no modal/reveal caller in this
slice. OPT-2008 remains open for the safe reveal contract and consumer; native
AX/reveal acceptance remains OPT-1953. These portable database/owner tests do not
qualify a production physical adapter or platform durability.

Validation includes real isolated-profile catalog publication, Unicode/raw paths,
multiple pages, query and revision rejection, replacement/retirement, queued
supersession and credit recovery, full delivery admission, malformed/oversized
fields, legacy unavailable/no-migration behavior, and the library's foreign and
reopened database cursor tests. Deliberate corruption of an isolated test database
is fixture setup only, never an alternative production writer route.
