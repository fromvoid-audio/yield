# Reference-rekey commit maintenance

OPT-2357 extends the original global worker's existing postcommit maintenance
policy to new reference-rekey receipts. After the original transaction returns a
successful COMMIT, it marks the same worker-local pending bit before postcommit
source validation. Applied, ReferenceChanged, DestinationOccupied and
ReferenceMissing all retain a new immutable receipt and therefore qualify for
this maintenance trigger. Exact replay and read-only snapshots do not.

The marker preserves a confirmed commit when source validation later returns
ReferenceCommitUncertain. Failed statement execution, rejected source/owner
context, receipt retention capacity, malformed history and failed COMMIT do not
mark the bit. A failed COMMIT is not generally proof of rollback; the aborting
commit-hook test separately checks the actual retained state of its definite
abort fixture.

The worker consumes the bit in its existing post-command maintenance hook and
validates original profile ownership before checkpointing. Maintenance uses the
original connection and checkpoint policy: retained
WAL observation, soft threshold, throttle and passive checkpoint. An active
reader, denied checkpoint or unavailable WAL observation cannot reverse the
receipt or turn its acknowledgement into a SQL failure. A profile identity
replacement closes the original worker before acknowledgement through the
existing ownership path; a replacement during checkpoint is caught by the
existing post-maintenance validation.

## Verification

The tests use actual Source writers, profile guards, global queues and SQLite
transactions. The original global connection is given WAL pressure above the
soft watermark and below the hard watermark. Its actual checkpoint attempts
are observed with a SQLite authorizer, including denied observation/checkpoint
cases. A pinned SQLite reader retains the old snapshot across a committed
rekey. Direct original database tests inspect the pending bit to exclude throttle
masking on replay and refusal. A confirmed global COMMIT races an independent
source metadata commit; a worker gate exercises genuine Source writer handoff.
Unix profile replacement is exercised both before and during maintenance, with
read-only inspection of the retained original receipt and rekeyed graph.
Reacquired profile and Source owners replay the exact historical receipt while
preserving later authored edits and reuse of the original key.

## Limits

This adds a maintenance trigger, not a new WAL admission gate or a second
connection. Reference receipts and passive storage observations issue no permit,
authorize no retry, refund no reservation and complete no genuine settlement.
The logical owner/SQL fixtures do not qualify physical publication, retirement
or a native database attempt family. Full OPT-1687 and the scheduler, idle,
hard-limit, shutdown and physical qualification requirements remain open.
