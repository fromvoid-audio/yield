# Bounded retained-owner feature rebuild diagnostics

OPT-2101 instruments the production `SourceWriter::admit_feature_artifact_rebuild`
entrypoint from OPT-2082/1718. Publication metrics are independently owned by
OPT-2087. Original OPT-1659/1343 prune, external-cache and retirement acceptance
remains open; these counters do not substitute for missing adapters.

One fixed-size per-retained-writer scalar snapshot records attempted calls and
exact ready/admitted/unchanged/deferred/ambiguous/stale/cancelled/failed outcomes.
It records total/max monotonic elapsed microseconds for owner validation and SQL,
not WAV decoding/DSP or physical durability. The observer validates the caller's
current audited source/revision/root/database/profile/lease/lifecycle through the
existing owner validator. It copies scalar diagnostics, supplies no mutation
capability and does no payload/path history retention or database recount.

A committed corrupt-payload repair records one removed payload and 732 logical
payload bytes. Missing payloads, healthy missing-marker recovery, deferred claims,
ambiguous controls and failed/cancelled/stale attempts record no removed bytes.
SQLite row deletion does not prove physical disk reclamation: pages/WAL/history
may remain allocated. There is no physical reclaimed-byte or retirement-success
counter here.

To make the existing removal outcome trustworthy, its corrupt DELETE now requires
one affected row and exactly one total SQL change, and rechecks row absence before
commit. Ignored deletion, trigger reinsertion or unrelated trigger mutation rolls
back with retained evidence. Metrics record failure after that rollback and never
count the attempted removal as committed logical bytes.

Counters saturate rather than wrap. `saturated` means affected totals are lower
bounds. Reacquiring the source writer resets this process-local observation; it is
not retained replay/retirement evidence. JSON has fourteen fixed numeric/boolean
fields, no source/path/reference/error labels and remains below 768 bytes even
with maximum scalar values. No event queue, persistent telemetry, scheduling or
physical filesystem effect is added.

Actual profile/source-owned SQLite tests cover every outcome, exact counts,
healthy retention, source/revision/handoff rejection, trigger rollback and observer
fences. Scalar saturation/serialization tests qualify the observer representation
only. The real catalog/scanner/WAV-DSP repair regressions also run. Native
OPT-2055/2061/2062/2075 and original family/retirement parents stay open; no physical
platform qualification or latency benchmark follows from these Linux checks.
