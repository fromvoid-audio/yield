# Harvest GPUI filter (OPT-1898)

The browser has an independent Harvest enable control and accessible nine-choice
dropdown. Choice and enable state belong to each source session. Selecting a choice
enables it; disabling retains the choice. All contributes no exclusion and submits
no Harvest query. This is read-only filtering, not a mutation or physical operation.

Membership follows the pinned Radiant reference `c80002be` exactly:

| Choice | Membership |
| --- | --- |
| New | New or Seen |
| New and touched | New, Seen or Touched |
| Needs review | Neither Done nor Ignored, with zero immediate derivatives |
| Touched | Touched |
| Has derivatives | Positive immediate derivative count, regardless of state |
| No derivatives | Zero immediate derivative count, regardless of state |
| Done | Done |
| Ignored | Ignored |
| All | No Harvest exclusion |

`SourceHarvestFilterRequest` captures actual profile acquisition, source/lifecycle,
root identity, directory generation, revision, request ID, exact manifest length
and choice. The catalog worker derives paths and positions from the authoritative
manifest; the UI does not concatenate unrelated batches or run SQL. The compact
mask is limited to the existing 4 MiB tag-mask budget. Each owner read retains the
existing 128-path/32 KiB path/64 KiB wire limits; aggregation has a five-second
deadline and never retains an unbounded vector of whole-source facts. Larger or
uncertain sources fail closed with a visible bounded diagnostic.

All chunks run under one actual source attempt. The first genuine backend batch
retains its weak global generation until completion and while the mask is cached.
Any actual global Harvest mutation, source fence loss, attempt supersession or
explicit cancellation invalidates the mask. No strong source/global worker or
profile lock is kept alive by the result. An empty manifest has no facts or matching
positions; source/profile validation still occurs. All bypasses this lane entirely.

The GPUI consumer compares the pending request, actual current acquisition,
source/root/lifecycle/directory/revision and immutable positional mapping before
application. Cheap retained fences and projection context are checked before
rendering and while polling. Projection replacement cancels the old lane and
rehydrates the current source's mode. Pending and unavailable states exclude
ordinary rows rather than pretending the files are New; delivery waits have a
six-second timeout. Existing filters intersect with the independent mask. An
explicit reveal bypasses filters while preserving the folder boundary.

Portable regression commands:

```sh
cargo nextest run -p yield-catalog -p yield-core -E 'test(harvest_filter)' --no-fail-fast
cargo nextest run -p yield-ui -E 'test(harvest_filter)' --no-fail-fast
bash scripts/internal/agent/run_agent_preflight.sh
```

Fixtures use genuine retained catalog/source/global owners and audited WAVs.
Harvest SQL seeds are explicitly metadata fixtures; they do not qualify production
copying, derivation creation, power-loss recovery or physical ownership. Portable
GPUI interaction tests do not certify native macOS focus, keyboard or accessibility.
The exact native verification scope is [OPT-2224](https://linear.app/boostnlvp/issue/OPT-2224).
Tracking scopes OPT-1499, OPT-1867 and OPT-1907 remain open.
