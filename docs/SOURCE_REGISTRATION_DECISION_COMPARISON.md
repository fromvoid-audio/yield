# Original Source registration at acknowledgement comparison

The actual Source audited and lease transport producers validate their original
CONTROL/profile/native binding, then install a private token from that exact
SourceAuthorityRegistration while its SourceEffectGate excludes drain/abandon.
The collector requires an exact original registration UUID in each Source slot.
Generic/global acknowledgements cannot fill these slots. Tokens retain the
original private lifecycle state rather than decode stored observations.

Each complete/decision comparison snapshots a fixed array of token references.
It tries the original Source gates in inventory order, then locks the round mutex,
matching producer gate-before-round ordering. The final mutex check requires
an unchanged checked token-cohort revision. A late Source acknowledgement cannot
fill a previously empty snapshot and bypass its missing gate lock. A changed
cohort is pending/refused; revoked phase, closed gate or stale liveness cannot
supply current acceptance. Checked cohort overflow invalidates the round.

Duplicate registration slots refuse. The genuine source lease has one exclusive
registration slot, preventing simultaneous registrations on one gate; token
installation also defensively rejects duplicate gate pointers. No SQLite/native
I/O or owner-queue wait runs beneath these comparison locks. Drain and guard
abandonment preserve their existing immediate revocation semantics.

Round debit includes the fixed retained arrays, one fixed snapshot array,
conservative per-original-Source state/string/gate/liveness backing up to a
checked 512 bytes and wrapper overhead. Token construction refuses backing that
exceeds this ceiling. No copied path, registry, heap-growing token list or new
physical source permit is introduced. Round references drop before their debit;
original native reservations remain separately held by the original workers.

Registration revocation before SQLite entry resolves the sole decision guard as
NoAttempt before abort polling, so native cleanup can complete. A lifetime count
that still retains the source is not a current Registered acknowledgement. An
unknown test transaction retains original cleanup custody until its genuine
outcome/readback resolution; elapsed time or phase observation never unlocks it.

## Exact limit

This is current registration at existing acknowledgement comparison points.
Short locks are released afterward and do **not** prove Registered at a later
journal SQL entry. The Linux test-only entry cutoff checks and marks entry under
the same joint locks; it grants no production Start or physical qualification.
Production retirement still refuses before ID, capacity or SQL when its physical
family is unqualified. Full OPT-2136 current joint/positive Start, genuine header
issuer, qualified directions, atomic nonce/intent/claims/inventory and uncertain
readback/restart remain open, as do OPT-2194 and OPT-2168. No token, private Boolean,
lifetime count, copied observation or test fixture issues effects, permits,
retries, refunds or settlement.

Coordinator comparisons use `try_lock` on each original Source gate. A gate busy
with existing native validation leaves completion, acknowledgement and the test
start cutoff pending without revoking the round. A gate becoming busy between
completion and Ready publication likewise preserves Preparing until a fresh
comparison succeeds. Releasing that gate permits a fresh comparison; drain or
abandonment still refuses it. Producer ACK installation keeps its existing
original ownership semantics.

The actual retirement command retains its original prepared input and input debit
when the original Ready gate is busy. Ready provenance captures the exact round
Arc and cohort at successful publication. Retry requires that same Arc and frozen
cohort, still-complete original slots, current registration and original deadline;
only gate contention is Busy. Cancellation, expiry, revocation, changed cohort or
an incomplete round refuse. Successful comparison advances the private command
phase once to a fresh decision nonce; later polls consume its genuine owner ACKs
without replacing that nonce. A separate prepaid 64-byte allowance covers the
fixed Ready/phase metadata. During overlap, old and new rounds retain their own
charged backing; checked budget growth can refuse without losing the original.

The collector retains `OwnerLost` as its refusal reason even after every original
owner has completed NoAttempt cleanup, and therefore can report
`Unavailable(OwnerLost)` on real drain. That label alone proves neither custody
release nor unknown settlement. The genuine drain regression separately requires
actual Source writer restoration with zero registered uses, zero handshake and
journal input debit, completed Global abort/reuse observation, zero durable IDs
and unchanged rows. Unknown SQLite entry still retains its original owners until
genuine outcome/readback; no refusal reason may authorize retry or settlement.
