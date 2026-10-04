# Publication lifetime gate (OPT-2183)

Decision, 2 October 2026, reviewed at `3cd9d75b2`. Install a private,
journal-issued, bounded shared slot whose entry transition competes with
cancellation and retirement. Keep capacity accounting on the journal actor.
The file worker enters this slot locally; it never asks the journal actor a
synchronous question while holding a source-effect permit.

This selects the mechanism missing from [OPT-2001's handoff map](GPUI_PUBLICATION_GRANT_HANDOFF.md).
It does not install it. [OPT-1632](https://linear.app/boostnlvp/issue/OPT-1632),
[OPT-1634](https://linear.app/boostnlvp/issue/OPT-1634) and OPT-1339 remain open.

## Existing foundations and changes required

| Current source | Reuse | Required change |
| --- | --- | --- |
| `operation_journal/owner.rs`: `checkpoint_and_grant_staging` | Fresh validation, current source registration, acknowledged `CopyValidated`, durable one-shot reservation | Bind a live slot to the exact current retained charge, operation and immutable publication target before returning authority |
| `operation_journal/store/staging_checkpoints.rs`: `reserve_staging_grant_record` | Exact checkpoint/readback and persistent `grant_reserved` | Reservation is issuance evidence only; it cannot protect later cancellation, release or worker entry |
| `StagingPublicationGrant::begin_publication` | Non-Clone grant and live `SourceEffectPin` | Replace source-only entry with slot-bound entry; no public grant deserializer |
| `CapacityProvider` | Actor-owned `Rc<RefCell<ProvisionalLedger>>`, genuine volume/reserve resources | Retain and fence exact charges through settlement; never move this ledger to the file worker |
| `SourceEffectGate` | Entry versus source drain; retirement waits for physical effects | Preserve this independent gate, without holding its permit across queue waits |
| `file_operation_owner.rs` | Production root registration, bounded admission, primary/duplicate receipts and tombstones | Private publication envelope, exact worker checks, slot entry and completion custody |

Today only staging validation executes in the general file owner. Publication
effects are `Unsupported`. Its validation result contains identity/content
observations, not a retained native staged-file handle. Current journal
`Cancel` calls the store directly; chunk release removes tickets and releases
charges without a publication-slot association. These paths must change before
the new grant can enable an effect. Current cancellation accepts eligible
`IntentDurable` operations; a staging checkpoint does not itself advance that
operation phase. Do not infer that its stored cancellation receipt excludes a
newly begun namespace effect.

## Private binding and issuer

The journal runtime owns at most 32 publication slots. Each has a fresh,
nonzero checked generation and private issuer incarnation. An opaque Arc links
the original grant, runtime entry and completion custody; IDs are comparison
data, never a way to look up or manufacture executable authority.

The immutable binding contains:

* Journal incarnation/profile identity; operation ID, current operation
  sequence/schema/phase/disposition; exact participant and durable ordinary
  claim or chunk identities, issue generation, retained byte charges and
  required publication metadata allocation policy.
* Original staging-validation effect ID/fingerprint; distinct publication
  effect ID and canonical fingerprint; output/logical object IDs; exact
  acknowledged checkpoint sequence, staged native identity, length and hash.
* Source registration identity, SourceId, media/database root and database file
  identities, lease epoch, lifecycle generation and captured source revision.
* File-owner identity; staging and destination root registration IDs and
  generations; roles, retained root identities, exact bounded relative target
  leaf, expected absence and explicit same-volume policy. Source identity is
  separate from these two effect roles.

Extend the private issuance request to bind the target before issuing the slot.
Reuse actual registered root references and the existing fresh CopyValidated
acknowledgement/reservation. The actor validates current operation/source,
exact durable unreleased charges and genuine private allocation/resources;
then reserves issuance durably and revalidates before opening the slot. Failure
or uncertain commit/readback closes it permanently and retains evidence. An
already reserved checkpoint, including after restart or lost issuance reply,
never reissues a grant. No zero estimate substitutes for unknown allocation.

The worker revalidates both references against its retained
`ProductionRootCapability`, exact profile/source/root lifetime and immutable
binding. The qualified backend must also retain or securely reacquire and bind
the exact staged native object under the accepted provenance contract. A
locator, matching bytes or a successful pathname reopen supplies no authority.
Staging replacement is rejection or retained uncertainty, never publication of
the replacement. Root retirement runs on the same file worker, so it cannot
interleave with a native call; source retirement retains its independent gate.

Retaining a charge is not a fresh physical free-space observation. The shared
admission state also carries, for each publication's bound volume, a checked
ledger generation and current aggregate incremental charge total, derived only
from the actor's actual provider. Capacity mutations mark this mirror pending
before changing the provider, then publish the exact checked total and advance
its generation; unknown outcomes keep admission closed. Do not expose a setter
to callers or copy the provider ledger to a worker. At most 32 bound-volume
entries fit the slot registry; use the existing allocation-class accounting.

Before slot entry, the worker captures that generation, obtains fresh qualified
native volume/allocation facts through a private retained descriptor capsule,
and revalidates it against the same volume/reserve incarnation. Native reads
occur outside admission/slot mutexes. Final entry requires the mirror not
pending, unchanged generation, correct allocation unit, full protected floor
and enough available bytes for the actual current incremental charge total.
Unknown/replaced/unsupported facts reject entry. The capsule retains genuine
native provenance; stored facts, a UUID or a decoded qualification result cannot
construct it. Its owner stays alive through the runtime resource guard. This
capsule/interface and mirror are required production work in OPT-2215/2218;
there is no qualified Linux positive provider asserted here. Namespace metadata
allocation must also have its actual bound/charge; unknown bounds reject entry.
Acquire a unique pre-entry resource-use permit under admission then slot before
reading that capsule. It increments the aggregate native-use count and is
limited to the primary envelope; queued grants alone perform no reads. Closing
admission prevents new reads as well as new effects. Upgrade this permit to
the entered effect without a zero-count gap, or drop it on before-effect
rejection. Resource retirement waits for these reads too. A capsule cannot
borrow actor-owned resources while uncounted, even before namespace entry.

## State, entry and cancellation

A short slot mutex protects `Issued`, `Entered`, `ClosedBeforeEntry`,
`QuiescentUnsettled` and `Settled`, plus sticky closure/uncertainty flags and a
bounded completion latch. Exactly one entered effect is possible per slot.
Poison, generation overflow, missing binding or conflicting completion closes
authority and retains charges. Mutex guards never cross SQL, native effects,
source validation, callbacks, condition waits or queue operations.

An additional shared publication-admission mutex holds the issuer incarnation,
open/closed flag and checked aggregate physical-in-flight count, including
pre-entry native-resource reads. It contains
no provider or native resource. Worker lock order is admission, then slot;
both are released before effects. Owner unavailable/shutdown closes admission
under this mutex before publishing its lifecycle transition, so iterating
individual slots cannot leave a window for another slot to enter. Per-operation
closure may lock its slot alone; it must never acquire admission afterward.
All entry requires both gates open. Dropping an entered permit uses the same
admission-then-slot order and notifies a physical-quiescence condition variable.

1. The file owner admits the canonical command and original envelope. The
   worker resolves primary/duplicate custody and validates its retained roots,
   target and binding before acquiring any source-effect permit.
2. Acquire `SourceEffectPermit` through the existing source pin. Its current
   implementation validates the source lease; do not hold the slot mutex during
   this call. No journal request/reply is needed at this point.
3. Under admission then slot mutexes, compare the exact immutable issuer/binding
   and require open admission and `Issued`. Transition to `Entered`, set slot
   physical-in-flight to one, upgrade the existing counted resource-use permit, and take
   the unique entered permit. Release both mutexes immediately.
   If closed, drop the source permit and return typed no-effect rejection.
4. Both opaque permits survive through the bounded native attempt and capture
   of its exact completion evidence. They are custody values, not held locks.
   Publish completion into the retained slot and file receipt before physical
   quiescence. Drop decrements physical-in-flight, but never settles capacity.

The actor closes affected slots under that same mutex **before** changing any
bound operation/claim/source/provider lifetime. If it wins while `Issued`, no
entry can occur; a failed actor transaction does not reopen the old issuance.
If worker entry wins, the actor records bounded pending cancellation/retirement
and rejects/defer conflicting mutations or release. It never acknowledges a
pre-effect cancellation or frees the claim while that slot remains unsettled.
Late cancellation reports pending/too-late rather than feeding the existing
pre-publication store transition blindly. After completion, cancellation is
resolved against the actual durable phase and effect outcome. A request or
closed gate alone does not establish physical absence.

Apply this fence to every actor path that can invalidate the binding:
`Cancel`, `Retry`, `PersistFull`/phase/disposition updates, ordinary claim
release, chunk release/retry/cancel, source drain/final retirement, provider or
volume retirement, owner unavailable and shutdown. Associated outputs/claims
are tracked in the bounded registry; an unassociated release must not bypass
an active slot. Read-only exact observations may continue. Source gate closure
can still win between steps 2 and 3; its already acquired permit defines the
source entry boundary, while the slot separately defines operation entry.
Drain closes future entry and waits for begun physical effects, not settlement.

## Queue, reply and completion custody

The envelope is private, non-Clone and non-serializable. Confirmed queue
rejection returns the **original** command/envelope, preserving its slot and
one-shot grant. It may be resubmitted only while the exact same issuance remains
open; it never resets `grant_reserved`. Accepted loss of a reply is not rejection.
The admitted worker/receipt retains custody, and an exact duplicate asks for
that original result. Changed canonical bytes conflict. No duplicate consumes
a second grant or executes the primitive again.

Use the existing admission/receipt/tombstone identity for canonical command
comparison. Keep authority out of serialized commands. Store the original
envelope in worker custody until an exact no-effect return or settlement;
acknowledging a result only changes receipt aliases, as in current tombstones.
Caller Drop, timeout, disconnect and worker panic cannot release stage charges.
An accepted command without a known execution outcome becomes sticky retained
uncertainty. A pre-effect `Unsupported` result retires executable authority but
retains the validated staged payload and its byte charge.

The slot has one preallocated completion latch, shared with its runtime entry,
not a second unbounded queue. The worker writes one bounded exact result and a
checked completion generation there. Exact repeats coalesce; different bytes
close the slot as uncertain. The actor scans at most 32 slots between commands
and on a bounded receive timeout (at most 50 ms). Completion does not require
worker-to-actor queue admission or a synchronous acknowledgement. Reserve
control responsiveness so ordinary input saturation cannot hide closure or
completion. Persist/read back completion on the actor before releasing any
settlement credit. Unknown SQL commit/rollback/readback leaves the latch and
charges retained and denies further authority.

## Fixed budgets

| Resource | Selected bound and accounting |
| --- | --- |
| Journal slots | 32 total, including issued, entered and unresolved quiescent slots; no eviction of uncertainty |
| Shared admission backing | At most 16 KiB, including the bounded volume mirror and synchronization state; included in the 2 MiB publication ceiling |
| Slot backing | At most 64 KiB each, including normalized binding, retained validation, completion and fixed slot overhead; checked retained capacities, not just encoded length |
| Journal retained publication backing | At most 2 MiB including shared admission and all slots, debited from existing 4 MiB queued-input credit and held through settlement; leaves 2 MiB outside publication backing |
| Journal control input | Reserve 64 KiB of the remaining input pool for bounded closure/control; ordinary admission cannot spend it |
| Publication file envelope | At most 64 KiB including canonical command, original grant backing and fixed overhead; debit existing 4 MiB file input credit |
| File admission/results/receipts | Existing 32 commands, 64 result credits, 64 receipts/tombstones; unresolved publication occupies these credits rather than creating another history |
| Effect scope | At most two root roles, two source fences; existing 1,024-byte locator and 512-byte diagnostic limits |

Count shared backing once within each owner's budget, but both owners must
reserve their own applicable credits. No arbitrary Arc clone or oversized
String capacity bypasses a limit. Result storage is reserved before admission;
completion uses that storage rather than allocating another maximum record.
Existing durable staging bounds remain 64 KiB per row and 8,192 rows. Limits
can cause typed before-effect backpressure; they never discard unknown results
or broaden the physical allocation policy. A full uncertain slot registry may
require explicit reconciliation before new publication.

## Settlement and shutdown

Physical quiescence only means native execution has stopped. `Settled` requires
the actor's exact completion/checkpoint/readback decision. No-effect settlement
may release slot/control backing, but staged bytes remain charged until a
separate genuinely authorized unused or cleanup release proves its own boundary.
Applied completion requires qualified staged-to-final object provenance and
required synchronization, then a durable verified publication checkpoint
before source/global outcomes or undo authority. Moving an object is not free
storage; charge transfer follows the exact claim ledger, never pathname absence.
Unknown results retain stage/final evidence and charges for reconciliation.

Introduce asynchronous `begin_shutdown` for publication-aware orchestration:
close command/effect admission and all slots, keep the runtime/provider/volume
resources while any physical attempt is active, and continue polling retained
completion. No journal actor waits for a file-owner reply. Source retirement
may wait for source permits only after releasing source/control locks, as the
current source effect gate requires. The worker releases its permits without
waiting for the journal. External orderly orchestration drains file effects,
then settles journal evidence, then joins; no join while holding admission,
slot, source/control or native locks. The current synchronous journal shutdown
must not be called from a file worker holding an entered permit.

If durable settlement cannot finish, return a typed deferred/uncertain shutdown,
retain durable dirty/recovery evidence and deny clean volume handoff.
Resources may be retired only after physical quiescence; failure cannot mark a
volume clean. Restart reconstructs observations/recovery charges, never a live
slot or grant. This lifecycle change is required implementation, not a property
already provided by the current shutdown/join path.

The actor's resource scope also needs an unwind guard. On unexpected exit it
closes shared publication admission and waits only for aggregate physical
quiescence before actor-local provider/volume resources can drop. Condition
waiting releases the admission mutex; no slot/source/control lock is held.
This exceptional wait cannot depend on actor commands or durable settlement:
worker permit Drop publishes uncertainty, decrements the counter and wakes it
without queueing. Catch command panics with the runtime outside the unwind
scope where possible. Never let an outer catch merely mark Unavailable after
unwinding has already dropped resources underneath an entered native call.
If a native call never quiesces, resource custody remains retained and the
shutdown is deferred; no timeout frees it. This guard is not clean handoff.

## Executable leaves and qualification

| Issue | Deliverable |
| --- | --- |
| [OPT-2215](https://linear.app/boostnlvp/issue/OPT-2215) | Actual journal slot/issuer, charge association and all cancellation/release/lifecycle fences |
| [OPT-2216](https://linear.app/boostnlvp/issue/OPT-2216) | Actual private file-owner envelope/admission/worker and retained fail-closed completion |
| [OPT-2217](https://linear.app/boostnlvp/issue/OPT-2217) | Actual durable completion/checkpoint settlement and fault/restart custody |
| [OPT-2218](https://linear.app/boostnlvp/issue/OPT-2218) | Held-object backend interface and Windows candidate behind disabled qualification policy |
| [OPT-2219](https://linear.app/boostnlvp/issue/OPT-2219) | Actual Windows runtime/SDK, controlled filesystem/volume, genuine reserve and positive provenance/collision/sync/crash qualification |

Linux implements the portable owner paths and truthful Unsupported outcomes;
contract fixtures do not qualify physical effects. Windows API availability in
the [primitive matrix](FILE_OPERATION_PRIMITIVE_MATRIX.md) is only a candidate.
OPT-2219 identifies the unavailable native runtime/volume requirement separately.
The [accepted macOS decision](GPUI_MACOS_PUBLICATION_DECISION.md) remains
Unsupported under OPT-1868; no pathname fallback or exclusive-namespace trust
assumption is introduced. Cleanup and broad durability/SLO qualification remain
independent.

## Validation required

Reuse real staging checkpoint/retained-charge tests, actual file-owner roots,
duplicate/reply-loss and source-effect retirement tests. Implementation leaves
add barrier-controlled close-versus-entry and release-versus-entry races;
queue-full return of the original grant; stale source/root/issuer/sequence;
budget saturation; no-effect versus uncertain settlement; shutdown without a
worker-to-journal wait; fault/restart after reservation and before/after result
readback. Assert backend invocation zero when any gate fails. Positive native
fixtures require genuine platform capabilities and the qualified backend.

Decision-reference commands (not new production tests):

```sh
cargo test --locked -p yield-library --lib staging_checkpoint -- --test-threads=1
cargo test --locked -p yield-library --lib v16_copy_retry_and_cancel_preserve_full_plan_through_store_cas_and_restart -- --test-threads=1
cargo test --locked -p yield-library --lib file_operation_owner::tests -- --test-threads=1
cargo test --locked -p yield-library --lib source_authority::tests -- --test-threads=1
cargo test --locked -p yield-library --lib source_effect_pin -- --test-threads=1
cargo test --locked -p yield-library --lib source_release_waits_for_effect_permit_before_releasing_durable_row -- --test-threads=1
bash scripts/agent.sh preflight
git diff --check
```

These are foundation/reference checks. Record exact head, commands, pass/skip
counts, limits and complete merged-tree verification on OPT-2183. They complete
this decision only; the production and physical leaves above remain open.
The existing `cancelled_operation_blocks_chunk_issue_and_entry_but_allows_unused_release`
case is macOS-only and its Linux filter runs zero tests. It is not a Linux
pass. The V16 store test above provides portable cancellation/restart evidence,
not the new cross-owner entry/release race acceptance required by OPT-2215.

## Command-drain lifecycle foundation (OPT-2250)

`OperationJournalOwner::begin_command_drain` now separates the existing command
admission boundary from sender disconnect and blocking join. It moves actual
Initializing/Available lifecycle to Closing under the same admission mutex used
by command submission. Repeated calls report no new transition; Closed and
Unavailable remain sticky. New requests reject before taking input/result
credits, while already accepted commands finish through the original worker.
The sender remains connected, so normal idle/queue-drained runtime resources
remain held until later external `shutdown` or owner Drop disconnects and joins.

This method closes only command admission. It does not close a publication slot,
count native resource reads/effects, poll completion, protect resource custody
through exceptional unwind, prove physical quiescence or establish settlement.
Full OPT-2215 publication-aware `begin_shutdown` must compose those actual gates
and resource-use rules; it must not alias command drain and claim completion.
Existing synchronous shutdown remains an external join and must never run while
holding an effect/source/control/admission lock needed by worker completion.

Actual worker tests hold an accepted command at a deterministic barrier, prove
the drain call returns before that barrier is released, preserve request credits,
drain/commit through actual profile-owned SQLite, and verify store acquisition
stays excluded both before and after queue drain until external join. Idle,
immediate-bootstrap and genuine worker-panic cases preserve the same lifecycle
and unavailable behavior. These portable fixtures supply no native reserve or
publication authority. All original OPT-2215/2216/2217/2218/2219 and OPT-1632/1634
acceptance remains open.

## Reserved journal control input (OPT-2255)

The actual shared journal input pool now has disjoint private ordinary and control
credit classes: 4128768 bytes (4 MiB minus 64 KiB) and 65536 bytes, respectively.
Their aggregate remains exactly 4194304 bytes. Ordinary requests, including
one-byte lookups, new source registration, payload persistence/admission and new
chunk claims/effect issuance, cannot borrow the control margin. Classification
comes from fixed private client callsites, never the caller's requested byte count
or a public flag. Cancellation, retry/exhaustion observations, chunk release/retry/
cancellation and source drain/final retirement use the control class. Existing
codec bounds and source/claim/store validation remain authoritative; a queued
cancellation still requests cancellation and does not acknowledge completed
cancellation, publication exclusion or charge release.

Each non-Clone input permit retains its exact class and debit through command
queueing, decoding, actual SQLite execution and response delivery. The worker
previously refunded input at command arrival while still retaining the payload;
it now keeps the original permit through the command scope. Rejected commands
remain intact through rejection delivery and then drop their payload before the
input field. Accepted reply loss returns the result-handle credit but cannot
refund a queued/executing command's input. Full/disconnected queues, result
backpressure, completion, rejection and unwind return that debit exactly once.
Poisoned or inconsistent input accounting closes admission; Drop neither panics,
resets the pool nor manufactures a confirmed refund.

Actual profile-owned SQLite tests hold the transaction before precommit, verify
executing input remains charged after reply loss, saturate ordinary credit, and
queue/execute genuine store-validated cancellation through the remaining control
margin. Further cases cover real retry/CAS refusal, exact aggregate/class
exhaustion, tiny ordinary requests, queue/result saturation, disconnect, command
drain, outer worker panic, inner SQLite command panic/rollback, mutex poison and
unconfirmable refund. The explicit no-capacity bootstrap fixture grants command/
storage behavior only; no native capacity or publication capability is fabricated.

This completes the input-margin prerequisite. OPT-2256 below supplies reserved
queue/result responsiveness. The 2 MiB publication-backing debit, 32 real slots,
exact grant/charge/target associations, provider mirrors, native resource-use
counts, cancellation/release fences, completion polling, resource unwind guard,
settlement and publication-aware shutdown remain mandatory OPT-2215 work.
Command drain is still not physical quiescence. Native interfaces/consumers and
qualification remain their original gates. Full parallel verification also exposed
an existing source lease fixture's missing shared runtime guard: a thread-local
app root alone does not isolate process profile/extension/CWD changes. Its actual
fixture now retains `TestRuntimeGuard` through native cleanup; Unknown refusal
and original source assertions remain unchanged. Input credit supplies no effect
permit, clean handoff, capacity refund or genuine settlement.

```sh
cargo test --locked -p yield-library --lib control_input -- --test-threads=1
cargo test --locked -p yield-library --lib operation_journal::owner::tests -- --test-threads=1
cargo test --locked -p yield-library --lib
bash scripts/agent.sh preflight
```

Run canonical preflight on a stable committed HEAD. These sibling test modules
have explicit test-only scope; no include! formatting exemption or guard waiver
is required.


## Reserved journal command/results and control scheduling (OPT-2256)

The actual worker now receives two private bounded queues: 28 ordinary entries and
four control entries, preserving the original aggregate 32 queued commands. The
64 outstanding-result credits are similarly partitioned into 60 ordinary and
four control handles. The fixed private command classification above selects
all three credit classes together. A tiny ordinary lookup cannot consume the
control margin; callers cannot set a priority or borrow unused control credits.
A dropped result handle returns only its exact result-class credit. Accepted
command backing remains charged until execution/rejection/drop, including after
lost replies. Poisoned or unconfirmable result debits/refunds make admission
unavailable without resetting unknown accounting or panicking in Drop.

The worker checks control before selecting another ordinary command and preserves
FIFO within each class. Idle control receive wakes immediately; a finite 50ms
receive poll revisits ordinary arrival and disconnection. One already-selected
ordinary command may precede newly arrived control, and running SQL is never
preempted. Queue acceptance does not acknowledge a committed intent. A control
request may overtake an ordinary intent and therefore be rejected by the real
store's unchanged missing-intent, expected-sequence, family and ownership checks.
There is no global cross-class FIFO, scheduler latency guarantee, physical
quiescence or completed-cancellation claim. Sustained control traffic may delay
ordinary work; bounded queue capacity does not bound SQL execution time.

Command drain closes admission while retaining both queues and original runtime
resources. External shutdown disconnects both senders, drains accepted work and
joins the worker. Bootstrap failure/outer panic drops both receivers; command
panic rejects both pending queues. None of this supplies publication-aware
shutdown, a native resource-use guard or clean physical settlement.

Actual profile-owned SQLite tests block inside the first transaction, fill all
28 ordinary entries, then enqueue four controls. Durable sequence values prove
control priority and each class's FIFO; overtaken missing intent and stale CAS
are refused. Both queues drain after admission closes while the original store
lease remains retained until external join. Other real worker tests retain 60
ordinary results while executing four controls, verify exact class refunds,
queue saturation with lost replies, idle wake, each half disconnected, both-queue
inner/outer panic and checked/poisoned accounting. Explicit no-capacity fixtures
exercise storage/command behavior only. Full OPT-2215 and all its physical,
publication, completion and settlement dependencies remain open.

```sh
cargo test --locked -p yield-library --lib control_
cargo test --locked -p yield-library --lib
bash scripts/agent.sh preflight
```
