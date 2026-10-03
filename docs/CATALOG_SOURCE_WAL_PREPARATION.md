# Catalog source WAL preparation (OPT-2191)

`CatalogService::prepare_source_wal` queues an empty, pre-intent reservation on
that service's original retained catalog worker. It accepts a privately produced
`SourceRevision`, immutable command/participant/distinct DB and WAL/SHM claim
correlations, and the original deadline. These are observations; the API creates
no operation, intent, physical capacity claim, permit, or publication authority.

Pending plus held controls share at most 64 slots and 64 KiB of complete retained
handshake context. Context accounting covers the full source/root/root-binding/
main-file identities, scalar correlations, nonce and bookkeeping, including all
three retained revision copies. Concrete type sizes and bounded owner bookkeeping
are charged as well; aggregate context can be full before 64 slots. The original
cooperative deadline is at most two
seconds and includes queue wait. It cannot be extended. Admission and acquisition
both check actual desired lifecycle and the original worker's current retained
writer, physical main-file identity and lease epoch. A duplicate source or a
physical-main alias cannot publish another prepared holder. Its empty reservation
is rolled back before rejection completes. No database is reopened.

The worker moves the complete registered entry into private custody. Its source
authority/receipt/pending handoff, snapshot, publication fence, watcher/replay,
curation lineage and cancellation metadata move with the same writer.
`RegisteredSource<ParkedDatabase>` exposes neither a SQL connection nor ordinary
writer methods. Another source can prepare while earlier sources remain locked.
Owner revalidation uses the retained read-only control connection, never queries
or renews the reserved ordinary connection, and never extends the deadline.

Ordinary commands touching a parked source retain their input/event/payload/
update credits until actual dispatch. The normal channel and deferrals share 32
credits. Per-packet enqueue timestamps preserve dispatch latency when unrelated
source work overtakes a deferred command. Bounded unrelated-source read pages
can run while another source is reserved. Whole snapshots, scans, WAV/DSP work,
replay, writes, history/Harvest handshakes and other long work first abort successful
pre-intent reservations; such normal dispatch never carries those locks across a
long SQL/IO/CPU operation or another owner's response wait. Watcher idle work is
deferred while any parked or unresolved owner exists. Register/retarget/remove
abort successful reservations before drain/finalization waits and preserve earlier
accepted source work before their lifecycle dispatch.

A non-clone pending request transfers its sole cancellation holder to a non-clone
delivered preparation. Dropping either holder, explicit abort, deadline expiry,
profile/root/lease or lifecycle loss asks the original owner to roll back. Drop and
caller clocks do not themselves unlock SQLite or refund credits. A reusable writer
is returned only after actual empty rollback and restoration of its original busy
timeout. `Aborted` means that return succeeded; `Rejected` means no preparation
resource remains. Both differ from a historical `Prepared` observation.

Rollback or restoration failure retains a private unavailable writer. Replacement
registration is rejected, accepted source commands remain queued, and accepted
durable commands receive their genuine failure shape on shutdown without a
fabricated commit or a misleading unregistered-source result. Cleanup can retry
only rollback/restoration. If a real original source authority and durable receipt
exist, lifecycle/shutdown first perform the original journal drain, check snapshot
close readiness, then consume the failed empty reservation under that exact
`SourceAuthorityRegistration`. Its original close boundary checks guard identity
and draining state, closes the main connection before actual lease release, and
produces the sealed `SourceAuthorityRetirementAck`. Only that acknowledgement
allows the existing journal finalization retry carrier. `Retired` means this close
was acknowledged; it does not mean the writer was returned, or that journal
finalization already completed. Unconfirmed close retains an unavailable source-ID
fence and credits; worker loss never substitutes a scalar or drop for a sealed ack.
A transaction predating preparation cannot use the empty-reservation close API.

`acknowledge_settlement` always returns `SettlementUnsupported`. This API is
explicitly pre-intent only. OPT-2136 must implement the actual shared private
all-owner decision gate, nonce/claim settlement, admission and lost-delivery/restart
contracts before any intent can use these owners. It must prevent timer, drop,
normal dispatch and shutdown from unlocking unresolved post-latch settlement.
These pre-intent holders cannot be promoted by copying a nonce or diagnostic.
The separate admitted lease-release witness (OPT-2202) also remains open.

## Verification

Linux actual worker/profile/SQLite tests cover simultaneous owners, external
writer exclusion, unchanged lease/rows, source deferral and unrelated-source reads,
32 normal and finite control/context saturation, original deadline and lifecycle/
root loss, pending/delivered cancellation and receiver loss, real rollback denial,
injected timeout-restoration failure at the original policy boundary, real reader
pressure and per-file retained WAL VFS size IO failure, retry, and honest shutdown
terminal outcomes. The explicit test-support faults never expose SQL, reopen an
owner, inject a positive measured size, or compile into normal production builds.
Library regressions also verify exact connection/timeout restoration and forbid
failed-resource retirement without actual drain or using another source's guard.

Run the portable catalog suite with only the two existing native bootstrap cases
filtered, rather than weakening their bodies:

```sh
cargo test --locked -p yield-catalog --lib -- --test-threads=1 \
  --skip tests::journal_client_registration_reopens_and_removes_source_authority \
  --skip tests::source_finalization_queue_rejection_retains_ack_for_exact_retry
cargo test --locked -p yield-library --lib -- --test-threads=1
cargo test --locked -p yield-session --lib -- --test-threads=1
bash scripts/agent.sh preflight
```

Positive native source-authority qualification is pending in OPT-2026. Linux
protected-reserve namespace bootstrap is genuinely `Unsupported`; portable/model
registries must not stand in for actual native registration or capacity. The new
positive case is compiled here but explicitly ignored. On the qualified macOS
fixture lane, execute it explicitly and require one executed passing test:

```sh
cargo test --locked -p yield-catalog --lib \
  source_wal_preparation::tests::integration::native_failed_reservation_uses_original_drain_close_ack_and_finalization \
  -- --ignored --exact --test-threads=1
```

It requires the genuine Available journal, original source registration/receipt,
actual rollback denial, original drain, close/lease acknowledgement and retries to
real removal after finalization. Run the two existing positive native bootstrap
regressions and full native catalog suite as well. Existing session bootstrap failures have been
inventoried separately in Linux fixture work OPT-2234; portable fixture gaps are
implementation work, while protected reserve/volume qualification stays native.
Record commit, OS/filesystem and
commands/results. This source implementation and its portable tests do not finish
tracking parents, physical qualification, admitted generation publication or the
post-latch coordinator.
