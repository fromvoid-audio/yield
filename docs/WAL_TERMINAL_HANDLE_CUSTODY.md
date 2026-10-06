# Original WAL terminal handle custody

The original journal actor and the sole non-Clone `WalPreparationHandle` share
cancellation and status state. Terminal status does not destroy that allocation:
a caller may retain the handle after actual journal, Source and Global joins.
The original producer regression previously observed no journal input debit while
that terminal allocation remained live, instead of the required 65,536 bytes.

Collector construction binds its already reserved original `JournalInput` before
allocating Shared handoff state. Shared retains a clone of that same input as its
last field. Actor and handle aliases use a private `SharedCustody` wrapper with
no raw Arc or Weak escape. Every alias drops with Rust 1.93 `Arc::into_inner`;
the final raw allocation frees before the moved Shared payload drops. Status,
test-only inventory/cache and hooks drop before its last input keeper. The
existing JournalInput wrapper also frees its own allocation before refunding the
input permit. A terminal caller therefore keeps the existing original 64 KiB
debit until its handle and every other original funded holder disappear.

The active fixed header gate counts the whole Collector, Shared with conservative
64-byte Arc slack, original InputPermit with Arc slack and retained budget backing
inside the unchanged 2048-byte allowance. Shared's additional input keeper is
included in its actual target layout. Snapshot and CurrentRound have separate
simultaneous credits within the unchanged combined 64 KiB budget.
The Linux test layout measured Collector 784 + Shared with Arc slack 168 +
InputPermit with Arc slack 104 + budget backing 104 = 1160 bytes. Native target
compilation checks the same unchanged allowance against each target's layout.

The actual producer test covers both explicit abort and original journal shutdown.
It requires confirmed cleanup, original Source writer reuse, all actual joins,
and removal of every report/snapshot/strong diagnostic budget before the
terminal-handle-only assertion. Exactly 65,536 bytes beyond
`InputBudget::shared_initial()` must remain; the last handle restores that baseline
exactly. Existing caller-drop/lost-reply and deadline tests cover actor-only
cleanup. The standalone allocation probe includes the actual private holder,
detects ordinary Arc's refund-before-header-free behavior as a negative control,
and checks nested payload/header deallocation before credit across concurrent
last-alias drops. This structural probe supplies no production authority.

Cancellation and status semantics, original deadlines, confirmed versus
unconfirmed cleanup, NoIntent/NoAttempt and Unsupported behavior are unchanged.
No extra input permit or larger ceiling is introduced. This fixes terminal handle
custody alongside [detached report custody](WAL_DETACHED_OBSERVATION_CUSTODY.md).
Full factory prepayment, physical allocation policy and direction claims,
permits, Start, genuine settlement and native production qualification remain
open in their tracking issues.
