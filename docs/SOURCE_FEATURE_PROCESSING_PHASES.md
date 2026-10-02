# Semantic progress for an explicit feature request (OPT-1803)

The production catalog command and browser reducer publish transient activity for
one file request under the current source/lifecycle/committed revision/request ID
and relative path. The catalog keeps its physical publication fence; the browser
also checks current activation and metadata before applying a transition.

| Phase | Actual operation | Measurement |
| --- | --- | --- |
| Inspecting audited source | Admit targets from the completely audited, current committed manifest | Indeterminate |
| Checking feature readiness | Validate exact current-target payload bytes through the retained writer | Indeterminate |
| Preparing unfinished feature work | Persist the deficit and prepare an exact claim with held input | Indeterminate |

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
