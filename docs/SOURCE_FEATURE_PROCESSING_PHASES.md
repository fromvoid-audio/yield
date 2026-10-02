# Semantic progress for an explicit feature request (OPT-1803)

The production catalog command and browser reducer publish transient activity for
one file request under the current source/lifecycle/committed revision/request ID
and relative path. The catalog keeps its physical publication fence; the browser
also checks current activation and metadata before applying a transition.

| Phase | Actual operation | Measurement |
| --- | --- | --- |
| Inspecting audited source | Admit targets from the completely audited, current committed manifest | Indeterminate |
| Checking feature readiness | Validate the one exact current target through the retained writer | 0 / 1 readiness targets checked |
| Preparing unfinished feature work | Persist the one target deficit and prepare its claim with held input | 0 / 1 readiness targets checked |

These are activities, not cumulative row visits, completed files, percentages, or
aggregate source readiness. No extra database pass computes a denominator. The
existing scanner cancellation checkpoints and diagnostics are unchanged. Existing
library `ReadinessProgress` counted reconciliation remains a separate contract;
this request route does not invoke that whole-source reconciliation or invent its
counts. OPT-1804 owns known-count publication and OPT-1805 compact progress UI.

After preparation the original Queued/Running/Ready/Blocked states keep their
meaning. Ready is terminal for this file request only and requires validated actual
stored bytes. It never declares the whole source idle or converged (embedding and
layout producers remain unconnected). A cache hit moves from readiness comparison
directly to Ready; a retained lease/retry moves from unfinished-work preparation
to Blocked without claiming work was queued. Cancellation/stale revision can
terminate before preparation. Nothing here changes readiness classification,
persistence, queue deduplication, decoder behavior, or terminal health.

The reducer rejects duplicate/regressive phases and all updates after a terminal
result. A retry or source revision/lifecycle change starts a fresh fenced request;
old events cannot reset it. Admission reserves at most six finite events per
request; unused reservations are released, retaining bounded output/backpressure.

Production tests use a real scanner audit, WAV decoder/DSP, retained SQLite owner,
validated payload, cache hit, decoder failure/retry, cancellation/stale revision,
and retained-claim restart. Pure reducer tests cover monotonic/no-op/early-terminal
transitions; browser tests check source/lifecycle/revision/request/path/cancel
fences. These Linux tests do not qualify macOS/Windows held input or physical GPUI
presentation; see OPT-2061/2062 and OPT-2075. OPT-1243 tracking acceptance stays open.

## Exact counts (OPT-1804)

The current explicit-file request has one validated target after manifest inspection.
Preparing readiness/unfinished work therefore reports 0 / 1 readiness targets;
Queued reports 1 / 1 targets prepared, not one newly inserted job. Ready reports
1 / 1 targets validated, including a cache hit with no job. DSP and blocked states
remain indeterminate, with no elapsed-time estimate. These counts cannot be read
as source-wide file, feature, embedding or layout completion. Browser status uses
this typed measurement and plain activity text.

One already-fenced request per distinct source (at most 64) may be aggregated. Only matching
known phases can be summed; mixed phases, unknown measurements, empty input or
an exceeded bound remain indeterminate. The current browser still owns one request;
this aggregate helper does not start requests or qualify stale source observations.

The existing bulk readiness callbacks count their already-loaded target set and
distinct target/version deficits. Comparison starts at zero and advances after
classification. Queueing advances after checking each distinct target, including
already queued or no-longer-actionable targets; its count is not committed jobs.
Empty queue input reports 0 / 0. An empty file manifest still has its source-level
similarity target, so a comparison count of one must never be presented as one file.
The only extra traversal is over the already-loaded deficit slice to deduplicate
its denominator, with cancellation checked for every entry. No database recount,
readiness classification change, target publication change or job estimate is added.
Cancellation checkpoints remain on each inspected/classified/queued entry.

Real SQLite tests use 7,232 file identities / 21,697 readiness targets, a loaded
64-target queue delta and repeat/no-op, duplicate deficits, empty file manifest,
comparison cancellation and queue rollback. Actual catalog audit/DSP/cache/retry
fixtures and fenced browser reducer tests cover the explicit-file producer. Native
GPUI presentation remains OPT-2075; OPT-1805 owns the compact source-scoped layout
and OPT-1243 tracking remains open until all its original acceptance is complete.
