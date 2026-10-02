# Bounded catalog/session Harvest facts (OPT-1900)

The session's `Persistence::start_catalog` supplies the actual global writer's
read-only `HarvestFactsClient` to `CatalogService::start_with_profile_owners`.
The catalog rejects a foreign acquisition at startup. Existing catalog starts
without that client remain supported, but Harvest queries return OwnerUnavailable;
they never reopen the global database or fabricate a read capability.

`SourceHarvestFactsRequest::new` captures the actual session acquisition (available
through `SourceRegistryService::vocabulary_acquisition_id`), source ID, lifecycle,
root identity, directory generation, committed revision, nonzero query ID and an
ordered exact path set. Input is 1–128 unique canonical UTF-8 paths with at most
32 KiB aggregate path bytes; aliases, traversal, duplicates and oversized context
fail before admission. `CatalogCommand::RequestHarvestFacts` carries the validated
request and a one-way `HarvestFactsCancellation`. Session `submit_catalog` forwards
it through the existing bounded catalog queue.

The catalog worker checks current admission, profile, actual source-root identity,
snapshot context, retained SourceWriter revision and live manifest membership.
It prepares genuine SourceWriter file observations and submits the backend's
bounded coherent global read. A wait checks cancellation every ten milliseconds
and expires after two seconds. Backend input/output, full graph shape, incident
edge and actual SQLite VM/deadline limits remain those in
[HARVEST_FACTS_OWNER.md](HARVEST_FACTS_OWNER.md). Excess work rejects; no truncation
or whole-source Harvest vectors are used. The wait deadline bounds waiting for
the global owner; source proof preparation uses existing retained read operations.

Internal pending-work cancellation is distinct from caller cancellation. Timeout
or a failed post-read check cancels the backend request while preserving a bounded
QueryBudget/failure completion for the caller. Explicit caller cancellation and
supersession may discard ephemeral results. Source and snapshot context are checked
again after the read, and retained source ownership is validated off the UI thread
before queue publication. Existing input/result credits charge a conservative
bounded complete request/result footprint; delivery releases those credits.

`CatalogEvent::HarvestFactsCompleted` returns the exact request and either privately
produced `CatalogHarvestFacts` or a bounded `HarvestFactsFailure`. Facts expose exact
recognized state, tracked/untracked status and immediate derivative count in request
order. An absent row is explicitly untracked/New; uncertain/malformed identities
remain failures rather than guessed states. Failure completions do not carry a
successful source publication fence.

Facts retain the actual source publication fence, source-attempt cancellation,
explicit caller cancellation and the backend's weak real-worker Harvest generation.
`CatalogDelivery::into_parts_if_live` discards invalidated successful results.
Cached facts' `is_live` and `matches_projection` checks perform only in-memory
comparisons/atomic liveness checks, with no SQL, filesystem, mutex or owner renewal.
Consumers must compare the exact current acquisition/source/root/lifecycle/directory/
revision, pending request ID and selected path set before every application/render;
they must cancel replacement requests and discard invalidated caches. These checks
are point-in-time fences, not an atomic source/global/UI transaction. Retaining facts
keeps no profile lock, source/global worker or writable connection alive.

Actual catalog/session fixtures cover every stored state, untracked/New, zero/two
derivatives, read-only source bytes/global data version, bounded input, stale context,
missing members/closed owner, cancellation, supersession, actual Harvest mutation,
source retirement, foreign profile, shutdown and reacquisition. Graph/secondary-source
seeds are metadata fixtures, not physical copy or qualification. The timeout transport
control fixture produces no facts or owner capability. Focused verification:

```sh
cargo nextest run -p yield-catalog -p yield-session \
  -E 'test(harvest_catalog) | test(harvest_session)' --no-fail-fast
bash scripts/internal/agent/run_agent_preflight.sh
```

Harvest UI filtering remains OPT-1898. No authored-state mutation, private journal
completion, physical publication/removal, reserve qualification or power-loss result
is inferred. Full Harvest/UI tracking parents stay open; native owner/platform checks
remain separate from portable metadata fixture evidence.
