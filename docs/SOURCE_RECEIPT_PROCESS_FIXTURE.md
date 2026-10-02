# Source receipt process-death evidence (OPT-2176)

Run the normal-build fixture explicitly:

```sh
cargo test -p yield-library --test source_receipt_process --locked
```

The test normally takes about five minutes. It uses the production 300-second
source lease and bounds reacquisition to 360 seconds after child death. It does
not edit the lease, accelerate expiry, change production code, or enable
`test-support`.

An owned child acquires the real profile guard and source database, enrolls one
fixed manifest row through the retained scan writer, completes the real audit,
and commits a rating update. Before publishing readiness it verifies exact
same-acquisition request replay, receipt equality, and unchanged revision.
Readiness is an atomic rename of at most 8 KiB of private observations outside
the profile. The observation includes every public receipt field; the fixed
metadata row's complete Debug representation is compared within this same
executable. These values cannot reconstruct a source revision, request, or
write authority.

The parent verifies competing profile acquisition fails, kills and reaps only
its child, reacquires the real profile, and verifies early source reopening
returns `LeaseNotExpired`. It then waits for genuine production expiry. A
reopened owner must retain the exact database/root binding, committed revision,
and historical receipt, while acquiring a different lease epoch and lifecycle.
A fresh owner-issued request reusing the historical command identity must
return `ConflictingCommand` without changing revision or historical receipt.
The bounded snapshot worker must return exactly the committed manifest and
rating, with no leaked rejected changes and no continuation cursor.

Child readiness and death are bounded to 30 seconds. An unwind guard kills and
polls only the owned helper. If the parent fails before injection, the helper
exits after 30 seconds without running owner destructors. Snapshot retirement is polled with a 30-second bound before joining and closing
the returned writer. Reopening source owners and snapshot reads use existing
bounded production APIs.

This qualifies the explicit Linux process-death path after committed SQL. It
is not power-loss durability, physical file publication, file/journal capacity
admission, Harvest coordination, or full participant preparation. Cross-process
stale request replay is not claimed: canonical request bytes remain private,
and this fixture does not manufacture historical authority. Native macOS and
Windows runs must be recorded separately; broader tracking parents remain open.
