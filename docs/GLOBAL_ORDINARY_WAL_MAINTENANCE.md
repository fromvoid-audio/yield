# Ordinary global WAL maintenance

OPT-2150 adds maintenance to actual registry/preferences and changed vocabulary set/delete
transactions on GlobalLibraryWriter. Immediately after each successful SQLite commit, the
LibraryDatabase records one pending bit. The retained worker consumes it and validates the
post-command profile/root before the borrowed-connection policy. Its existing reply validation
remains after maintenance, so replacement during a checkpoint cannot be confirmed. Failed
ownership validation closes the existing owner
without invoking maintenance. Maintenance failure cannot reverse a committed metadata write.

The reused retained-VFS policy attempts PASSIVE only at 32 MiB or above, outside an active
transaction, at most once every 15 seconds on that connection. It keeps before/after unknown
measurements and SQLite busy/frame evidence distinct; copied frames do not imply reclaimed WAL
allocation. There is no pathname stat/reopen, forced checkpoint, reader termination or new worker.
One bit and one timestamp retire with the LibraryDatabase. This interval is an attempt-frequency
bound, not a guarantee of operating-system latency.

Read commands, failed/stale/conflicting requests, exact snapshot acknowledgement replay,
unchanged vocabulary assignments and missing-label deletes never record an ordinary commit.
They therefore cannot trigger maintenance. Harvest/other participant commits are outside this
finite marker scope. This leaves OPT-1687 open for those families, idle/hard-pressure catch-up,
explicit shutdown/reader boundaries and scheduler coordination. OPT-1686/1685/1348 and native
physical qualification also remain open. No capacity permit or durable recovery authority is added.

The worker tests instrument actual checkpoint statements on its retained connection and use real
owned SQLite WALs above the production threshold. The initialization hook is compiled only for
unit tests. Reader, SQL/measurement failure, replay/no-op/stale, connection retirement and
post-command profile replacement cases preserve their actual data and ownership evidence.
