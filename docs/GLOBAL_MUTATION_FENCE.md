# Global worker mutation fence (OPT-2180)

The existing retained `GlobalLibraryWriter` owns a private positive mutation counter.
It starts at one per actual worker registration and advances with checked arithmetic
before dispatching a potentially mutating command. Its maximum is `i64::MAX`, matching
the exact inventory's positive SQLite storage range in
[WAL_REQUIRED_COMMIT_CLAIM_CONTRACT.md](WAL_REQUIRED_COMMIT_CLAIM_CONTRACT.md).
At exhaustion new mutations return `GlobalMutationFenceExhausted` before their
validation/SQL effects. Reads remain available; preparation cannot promise another
mutation and returns `MutationFenceExhausted`. No wrap, saturation reuse or implicit
counter reset occurs on the existing worker.

An exhaustive match classifies every current worker command. Registry, preferences,
vocabulary set/delete, captured ordinary proposals, registry participants,
operation links, reference rekey, Harvest authored state and Harvest copy all advance
before execution. Rejected, no-op and replayed attempts also invalidate the old
observation. Constructor/queue rejection before actual dispatch does not advance it.
Registry/preferences caller correlations and persistent vocabulary revisions keep
their existing semantics; the counter does not replace them. The existing weak
Harvest publication UUID generation and its invalidation routes also stay separate.

Registry/preferences/vocabulary reads, reference snapshots, Harvest facts/authored
snapshots, empty preparation and abort preserve the counter. It measures the actual
worker's dispatch boundary, not arbitrary external SQL or physical file effects.
It is neither a commit count nor a durable database revision.

`GlobalPreparationObservation::mutation_revision()` captures the actual counter
with the exact profile acquisition and worker registration. Preparation validates it
before acquisition, again under its empty immediate transaction before readiness,
and during held control polling. The existing FIFO cannot dispatch ordinary commands
while held, so queued writes cannot advance it. Native main-file/profile validation,
the original cooperative deadline, cancellation, abort confirmation and uncertain
rollback connection retirement remain required as described in
[GLOBAL_WAL_PREPARATION.md](GLOBAL_WAL_PREPARATION.md).

`GlobalPreparationRequest::from_observation` forms a delayed request using the
observation's original registration and expected counter. The current worker rejects
an intervening mutating attempt with `StaleMutationFence`, or a retired/restarted
registration with `StaleOwner`, before preparation SQL. A fresh request captures the
current counter on actual dispatch. Restarting a worker inside the same profile
acquisition may restart the counter at one, but it creates a different registration;
old observations cannot substitute that new registration.

Observations remain copyable diagnostics. No constructor or serialization can create
a live decision latch, capacity claim, required commit permit or current source
authority from them. OPT-2136 must freeze/revalidate the complete live inventory;
OPT-2170 must join exact family/direction/revision domains and genuine journal-issued
permissions at actual commit consumption. Neither can infer current ownership from
historical counter bytes. Tracking OPT-1685/2137/1348 and native allocation
qualification stay open.

Actual retained-worker tests cover ordinary and operation-scoped routes, unchanged
read snapshots, rejected/replayed attempts, held FIFO work, delayed observations,
same-acquisition restart, and the checked range boundary with unchanged SQLite
metadata on rejection. Boundary seeds exist only in `cfg(test)` private startup;
they are test state, not positive physical admission. Existing real Harvest/reference
fixtures remain metadata fixtures and do not qualify physical copy/publication.

```sh
cargo test --locked -p yield-library --lib mutation_fence -- --test-threads=1
cargo test --locked -p yield-library --lib -- --test-threads=1
bash scripts/agent.sh preflight
```

Deadlines are cooperative and do not bound a stalled OS/VFS call. Default parallel
fixture isolation remains separately tracked in OPT-2178; physical allocation,
power-loss durability and native handle/lease qualification require their own runs.
