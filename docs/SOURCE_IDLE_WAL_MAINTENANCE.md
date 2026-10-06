# Source idle WAL maintenance (OPT-2369)

The original Catalog worker gives its retained Source writers an idle PASSIVE
checkpoint opportunity. This complements postcommit Source maintenance (OPT-2120)
and original Global idle maintenance (OPT-2362); it does not complete OPT-1687.

One fixed worker timestamp and index (at most 48 bytes, no heap cursor) pace at most
one original Source per opportunity after 15 seconds of ordinary-command quiescence.
Every completed opportunity, including unavailable or below-threshold observations,
starts a fresh interval. Commands already queued under the original submission gate
take precedence. Stable idle membership rotates through the existing map; membership
changes can change order. There is no per-source latency guarantee independent of
the number of configured sources, no burst catch-up, new connection, worker or queue.

Closing, closed, inactive and mismatched Source generations cannot admit a new pass.
Any parked or settling original WAL preparation excludes maintenance and resets idle
pacing. An already admitted pass retains its original resources while shutdown joins
the same worker. The Catalog admission mutex covers only lifecycle/queue selection;
Native ownership checks, SQLite and owner waits occur after releasing it.

The SourceWriter entrypoint requires genuine retained profile, root, MAIN and Source
lease custody even in test-support builds. It skips active SQLite transactions and
refuses an entered or poisoned metadata policy. The original lease effect gate counts
the admitted maintenance use until ownership validation after the checkpoint finishes.
This existing in-flight pin is not a journal-issued effect grant. Original root/MAIN
identity and the unexpired durable lease are checked before and after through readonly
validation. The idle path never calls `prepare_for_mutation` or writes/renews through
the separate CONTROL writer. Lease expiry, replacement or a closed gate skips work.

The retained connection's unchanged CheckpointPolicy observes its original WAL and
uses the same 15-second attempt throttle shared with postcommit maintenance. A denied,
partial or unavailable observation cannot authorize retry, refund reservations or
complete genuine settlement. Ownership loss after an admitted checkpoint can follow
real checkpoint writes; refusal is not rollback or a clean NoEffect disposition.

Actual tests use the unchanged production interval, real original workers/profile
locks/leases/SQLite WAL, pinned readers, Native checkpoint shutdown barriers and a
genuinely parked other-source preparation with its original two-second deadline.
Direct owner cases cover readonly near-expiry/expired lease preservation, active
transactions and policy exclusion, shared attempt throttle, denied/unknown observations,
closed original gate and root/MAIN replacement before or during Native work. Fixed
pacing geometry supplements these owner tests. External SQLite writes are fixture
pressure setup, never production source authority or physical qualification.

PASSIVE does not promise WAL truncation, hard-watermark size reduction, bounded Native
I/O, a durable shutdown checkpoint or physical power-loss qualification. Missing
broader admission, scheduler, hard-pressure, shutdown and qualification requirements
remain open in OPT-1687/1686/1685/1348. Windows GNU compilation is not Native MSVC/NTFS
execution. No idle observation issues permits, authorizes retries, refunds credit or
confirms genuine settlement.
