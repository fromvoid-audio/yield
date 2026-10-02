# Changed-source feature reanalysis and restart

OPT-1718 uses the actual selected-file feature workflow implemented by OPT-1708,
OPT-1922, OPT-2072 and OPT-2082. It adds no new analysis engine or watcher and does
not claim whole-source embedding/layout readiness or native held-input support.
Those broader requirements remain with OPT-1484 and OPT-2061/2062/2075.

After the retained scanner commits changed manifest content, the previously
published desired feature matrix can still describe the old file until another
request refreshes it. For `yield-audited-feature-matrix-v1`, both immutable-key and
current-target lookup now require the target's current manifest path, stable file
identity, full content hash and nonmissing state. Readiness classification reports
an unmatched old feature target as `StaleByGeneration`, even when its payload and
completion marker remain healthy. Publication rechecks this dependency inside the
same transaction immediately before commit, so a changed manifest cannot leave a
new completion behind.

An unrelated curation/source revision does not invalidate an unchanged file's
content-bound feature payload. Rename retains the same immutable key and bytes;
once the owner refreshes the desired target's new path, those bytes are current
again. Missing or changed files retain their historical immutable rows without
reporting them as current. The existing request refreshes the complete desired
matrix from the current audit, replaces only the relevant current work generation,
computes actual held WAV bytes, and verifies the new durable result before Ready.

The production catalog restart test first computes a real WAV feature result,
drains, changes the WAV, reopens and performs the real scanner audit. Before any
new feature request, it asserts stale old readiness and unchanged SourceId/lifecycle,
and rejects a request carrying the old revision without adding artifacts. Reanalysis
then converges to the new content/source generation while retaining stable file
identity and the old immutable payload. A second restart returns the verified new
result without a second computation. Existing real job tests reject late computed
results from retired owners, reclaimed claim generations and replaced roots.

Owned SQL/vector fixtures separately cover unchanged-file preservation, rename,
missing files and transaction-local manifest-change rollback. These are storage
contract evidence. Linux WAV/DSP integration is portable implementation evidence,
not macOS/Windows physical qualification; OPT-2075 retains native verification.
The richer compact progress presentation remains OPT-1805/1728; OPT-1243 remains
an open tracking parent, not a prerequisite to this concrete source-owner route.
