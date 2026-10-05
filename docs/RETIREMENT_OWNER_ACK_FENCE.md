# Original retirement owner acknowledgement

The private `AdmitPrepared` retirement route matches the immutable arguments,
participants, capacity plan and ordinary claims against the original journal
collector. It requests a fresh nonce round from the original Source and Global
owners. A copied `Ready` observation cannot acknowledge that round.

Each native owner validates its original parked scope before acknowledging.
Once acknowledged, its custody excludes ordinary abort, deadline cleanup,
writer restoration, cancellation, shutdown and native close until the original
decision resolves. The journal polls acknowledgements asynchronously. The short
round mutex protects scalar state only; it never runs SQLite, native I/O or an
owner wait. Native destruction has a final custody fence during unwind.

While fenced, public Source revalidation returns `Busy`. Retaining cleanup
custody is not a successful current-validation result. Original actor polling
checks custody separately and defers ordinary validation and cleanup.

The command retains its original bounded input while waiting. Partial replies,
cancellation, expiration and missing qualification resolve definite `NoAttempt`
before reverse-prefix cleanup. The retirement route currently refuses missing
physical-family qualification before allocating an operation identity, acquiring
a provisional capacity lease or running initial journal SQL. Other prepared
admission families retain their existing route.

## Verification scope

Linux custody fixtures use genuine Source and Global actors, native SQLite
writer locks and registered lifetime custody. Separate transaction fixtures use
the actual Collector and the original journal worker's queued Store APIs to exercise committed,
confirmed-rollback and lost post-commit delivery windows. Cancellation, the
original deadline and owner shutdown must not release the parked writer locks
while the decision is unresolved. Completed transaction evidence or exact
durable readback precedes resolution.

Those transaction fixtures use historical logical storage inputs. They do not
qualify retirement capacity, and their test-only transaction-start hook does not
enable a production SQL branch or grant an operation permit. The production
consumer is verified separately through its actual queued acknowledgement and
pre-SQL refusal boundary.

## Remaining contracts

OPT-2136 remains open. Its qualified positive initial-admission consumer must
still validate the complete current registered authority, compare the exact six
tag-3 work slices with the genuine original Source/Global component policies,
and atomically persist the nonce, intent, ordinary claims and database inventory.
It must retain the original claim/nonce/native custody through unknown outcomes
and perform exact durable reconciliation. A generic journal capacity witness,
registered lifetime count, matching inventory or successful storage observation
cannot substitute for physical-family qualification or issue participant permits.

The acknowledgement fence alone does not prevent an independently requested
registration phase change, complete retirement or provide a compensation
allocation policy. Those consuming contracts remain required before positive
Start or native effects can be enabled.
