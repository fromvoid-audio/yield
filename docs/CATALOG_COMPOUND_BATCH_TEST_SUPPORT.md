# Exact catalog compound-batch fault harness (OPT-2190)

The opt-in `yield-catalog/test-support` feature exposes
`CatalogService::arm_compound_batch_failure` and
`CatalogService::cancel_compound_batch_failure`. Yield UI enables this feature
only through its catalog dev-dependency. Default catalog/app builds contain no
fault slot, API or injection branch. Existing admission and worker fences remain
unchanged.

A fixture arms one service-local pending configuration: an exact admitted active
source identity (1..256 UTF-8 bytes, nonblank/no NUL), nonzero registered lifecycle,
nonzero request and batch index below 512. Validation precedes cloning identity.
An occupied slot rejects replacement; no queue, process-global callback, timer,
SQL connection or second lock is introduced. The slot uses the existing admission
mutex and releases it before source SQL. It neither opens a path nor issues
ownership, lease, capacity, journal or publication capabilities.

The actual retained catalog worker checks its existing profile/source/snapshot,
root and revision fences before consuming a matching fixture immediately before
the selected batch's `commit_catalog_curation_command`. Prior batches execute
actual SourceWriter transactions and return genuine receipts. The selected batch
returns the existing typed `CatalogError::Database` through the normal
`SourceCurationOperationFailure` with its exact index, without running that SQL
batch. Later batches do not run. Ordinary single-batch curation and rating decay
never use the fault context. Other source/lifecycle/request tuples leave it intact.

A matching terminal request releases an unconsumed slot, including an earlier
real failure or a plan shorter than the configured index. Successful source
replacement/removal admission clears only that source's fixture; shutdown and
worker closure clear all pending state through existing invalidation. Explicit
fixture cancellation releases the one slot. This is test failure injection,
not an acknowledgement, fabricated successful receipt or crash qualification.

```rust,ignore
// Dev/test build only; the source lifecycle must already be admitted.
service.arm_compound_batch_failure(&source_id, lifecycle, request_id, 1)?;
service.submit(actual_compound_command)?;
// Consume the ordinary CurationOperationCompleted event. Its receipts are real.
// A subsequent request executes normally; explicit cancellation is also available.
```

Ten tests use profile-owned SQLite and actual CatalogService request/event paths:
first/middle/last failure with exact committed receipt/revision counts and actual
read-only SQLite prefix readback; subsequent normal success; exact request,
source, worker lifecycle and service isolation; existing stale-revision fences;
input/slot bounds and explicit cancellation; source replacement/removal; terminal
short-plan and shutdown cleanup. They check ordinary payload/update credits return.
The retained worker is detached from native watchers in catalog unit fixtures,
as in the existing suite; no qualified bootstrap or physical reserve is faked.

OPT-2186/2187 must still implement and verify active compound-history settlement
and retry using these actual partial outcomes. Tracking OPT-2114/1617, restart
OPT-2115 and native OPT-2143 remain open. The existing two positive native
journal/bootstrap catalog tests still require their separately tracked qualified
resource (OPT-2026); this harness introduces no additional exclusions.
