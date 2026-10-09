# Original journal writer acquisition observations (OPT-2446)

`operation_journal::writer_acquisition_telemetry::journal_writer_telemetry_snapshot()`
exposes fixed process-lifetime saturating counts for four original Store call
sites: intent insertion, cancellation request, retry-pending persistence and
ordinary-claim release. Each selected exclusive acquisition returns Acquired,
Busy, Locked or OtherError. Extended SQLite BUSY and LOCKED codes retain their
base category. Counters retain no diagnostic, identity, SQL or payload.

Timing surrounds only the original `BEGIN EXCLUSIVE` call. Intent insertion
keeps its existing funded control implementation and error loans. That call
can refuse during preparation before SQLite starts a transaction; such a
refusal is OtherError, never Acquired. Validation, encoding, transaction body,
commit, readback and telemetry bookkeeping are outside the measured interval.
Five inclusive buckets are <=1, <=10, <=100, <=1000 and >1000 milliseconds.
Elapsed buckets aggregate all four selected sites.

The 168-byte static collector uses relaxed atomic counters, without a queue,
heap allocation, lock, reset or callback in production. Concurrent snapshots
are approximate across fields. Original SQL, timeout, retry policy, result
ownership and transaction lifetime are preserved. Acquired means the original
acquisition returned successfully; a later duplicate, invalid body or failed
commit still contributes Acquired. Pre-call validation refusals contribute
nothing. This supplies observations, never durability or effect authority.

Actual Store fixtures verify insert/duplicate/reopen, cancellation persistence,
claim-release replay/conflict, pre-BEGIN refusal and post-BEGIN rollback.
The existing genuine competing SQLite reservation fixture verifies Busy,
unchanged journal/claim bytes and successful acquisition after the blocker
releases. Fixed-storage tests cover all counters, saturation, exact bucket
edges, extended contention codes, unchanged owned diagnostics and a real nested
SQLite transaction refusal that preserves the original transaction.

This is a bounded prerequisite of OPT-1673. Other journal transaction families,
all-owner writer/reserve/retry observations and full startup/lifecycle acceptance
remain open. Existing queue residence and whole retry-return measurements have
separate boundaries and must not be treated as writer-acquisition duration.
