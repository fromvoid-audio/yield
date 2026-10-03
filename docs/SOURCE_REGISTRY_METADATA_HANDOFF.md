# Authenticated source metadata handoff (OPT-2193)

`SourceWriter::prepare_registry_metadata_handoff` captures the bounded observation
and original registration from its actual retained database. It reserves a carrier
before capture, matches the original lease/root/main-file registration, and keeps
the non-Send registration guard on the source worker. The sendable candidate has
no public constructor, Clone or deserializer. Its observation remains historical
metadata and cannot recreate its private pin or writer lifetime.

The existing journal client's `authenticate_source_registry_metadata` uses its
normal bounded FIFO, input/result accounting and shutdown rejection. The actual
journal consumer requires its original request Arc, exact receipt/lease/volume
tuple and fully `Registered` record. Prepared, partial/recovery, draining, retired
and absent records reject. First authentication binds the source ID, lifecycle
and original writer's weak lifetime in that record; a replacement writer cannot
inherit the binding even with equal source ID/generation/database scalars.

An additional weak registration token is owned only by the original journal
record. It activates once after complete installation, and permanently revokes
on partial recovery, drain or record/owner destruction. Neither a copied receipt
nor a still-live source lease can revive it. It is not stored or serialized.

The returned opaque metadata permit is custody only. It supplies no physical
capacity, file effect, required-commit permission or successful enrollment. The
future admitted global consumer (OPT-2195) must call the private
`enter_metadata_sql` immediately before its separately admitted transaction and
retain the returned borrowed custody through commit/rollback and SQL quiescence.
That boundary matches the complete immutable proposal subject and original
profile acquisition, revalidates registration/writer/root/file/lease, and enters
both the original source effect gate and a writer lifetime gate. No owner mutex
remains held during SQL. Revalidate at the existing reply ownership boundary;
loss after effects must retain the immutable receipt/uncertainty, never imply
rollback or successful current enrollment.

Writer handoff/drop closes the writer gate and waits for active SQL custody
before returning/closing its database. The gate retains quiescence even when
poisoned. Merely retaining an unopened result owns no writer SQL effect and
cannot stall handoff; handoff makes its weak writer token stale. Deadline or
registration loss does not drop active SQL custody early. The original deadline
is at most two seconds, including capture/queue wait; there is no renewal or
replacement registration. Normal source retirement still requires the existing
drain, close-ready, actual connection/lease close and sealed acknowledgement.

One process-wide carrier credit bounds all candidate/delivered tokens together
at 4096, including failures and retained results, conservatively across profiles.
Captured escaped JSON is bounded to16KiB; queued input additionally charges
private candidate/permit headers, fixed volume identities and shared-state
overhead. The existing 32-entry journal queue and input/result credits remain.
Registered, partial and retired journal records together cap at4096 before new
registration effects. Exact existing registration/retirement replay precedes
retention rejection. No record, history or unresolved custody is evicted.

## Evidence and remaining implementation

Focused command:

```sh
cargo test --locked -p yield-library --lib source_registry_handoff -- --test-threads=1
```

Tests exercise actual profile-owned source SQLite/writers/leases, real journal
queue refusal and refund, private original-request/readiness checks, writer
replacement, deadline, guard loss, drain/partial/owner revocation, poisoned gate
quiescence and both carrier/map retention limits. Positive readiness fixtures
explicitly model completed journal registration without a native reserve or
capacity provider. They do not execute admitted global reference SQL or qualify
native registration. The count-only tombstone fixture is likewise model evidence.

[OPT-2236](https://linear.app/boostnlvp/issue/OPT-2236) requires native macOS with
genuine protected namespace/reserve qualification and a real local volume:

```sh
cargo test --locked -p yield-library --lib \
  operation_journal::owner::tests::source_registry_handoff_tests::integration::native_completed_registration_authenticates_then_exactly_drains_and_retires \
  -- --exact --test-threads=1
```

Require one executed passing test, actual complete registration, original
metadata custody, and exact drain/close/ack/finalization. This macOS-gated test is
not compiled or passed on Linux. Full native lifecycle qualification remains
OPT-2199/2200, and global relation SQL/claims/intent/recovery/catalog routing remain
OPT-2194/2195/2196/2197/2198, OPT-2136 and required-commit/settlement prerequisites.
This source leaf does not complete those implementations or tracking OPT-1828.
