# Original journal resource custody (OPT-2270)

The actual journal worker retains its original actor resources until every counted
publication resource use has quiesced. The private shared admission is scalar only;
capacity accounting, source authority, reserve, SQLite, volume lock and profile remain
on the original actor. Its Arc identity is the original runtime incarnation; it has no
serializer, public constructor, caller setter or resource-reconstruction path.

Original startup debits a fixed 4 KiB header from the existing ordinary portion of the
4 MiB input pool and retains that debit through complete runtime teardown. The header
bounds the fixed registry and at most 32 unique non-Clone use handles. Checked generations
never reuse a live entry. Read-to-effect upgrade preserves the existing nonzero count.
The complete OPT-2215 publication registry must debit this header, the charge mirror,
slot/binding backing and capsules within its shared 2 MiB publication ceiling.

Admission closes before Closing or Unavailable lifecycle publication. Unavailable,
poison, generation overflow and corrupt counters remain unknown and deny clean handoff.
The final actor close and actual runtime Drop (including unwind) wait before dropping
any original source, provider, reserve, store, volume or profile resource. The condition
wait releases admission; counted-use Drop wakes it directly, with no actor queue or
completion dependency. No SQL, native call, source validation, callback wait or queue
wait belongs inside the production admission callback. Full slot composition must use
admission then scalar slot; reverse acquisition is forbidden.

A read release proves only that its physical borrowing ended. Effect-use release is
sticky unsettled evidence and cannot grant successful shutdown/clean handoff. This leaf
has no API to settle that evidence. Physical quiescence never releases charges, refunds
reservations or completes durable settlement. Full OPT-2215 must implement exact slot
issuance, genuine source/claim/target/capacity and native capsule binding, all mutation
fences, completion reconciliation and verified settlement before any effect is enabled.

Tests use actual original journal/store/profile custody through shutdown and command
or outer panic, plus deterministic original admission versus unavailable ordering,
header retention/refund and zero-count-gap upgrade. Counted test uses are scalar custody
fixtures; they grant no source/Native/backend authority and qualify no physical effect.
Separate arithmetic tests cover the 32-use bound, failed/closed upgrade, generation
exhaustion, poison and corrupt counts. Existing command-drain tests preserve asynchronous
command closure and explicit external join.
