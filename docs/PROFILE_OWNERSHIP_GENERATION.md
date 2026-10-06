# Original profile acquisition generation (OPT-2365)

Every production `WritableProfileGuard::acquire_current` issues a positive checked
serial only after acquiring the original native profile-owner lock. The issuer is
one process-local atomic counter; values fit SQLite's positive signed integer
range. Exhaustion refuses acquisition without resetting or wrapping. Failure after
issuance can consume a serial. The failed guard construction drops the original
lock normally, and issuance failure occurs before diagnostic bytes are rewritten.

Clones retain the same serial, acquisition UUID, root and lock identities, and live
descriptor lock. A later successful acquisition in the same process receives a
larger serial. Another process can start its counter at one: numeric equality alone
has no ownership meaning. The original acquisition UUID, retained root/lock and
current native ownership must also match. This is not a durable ownership epoch.

The accessor is crate-private and observational. The existing current-ownership
boundary checks its retained range after the original root/lock checks, preserving
their refusal behavior. Lock diagnostic contents remain diagnostic only. The
additional retained state is one `u64` per guard and one static atomic counter;
there is no new filesystem record, schema, queue, thread or heap backing.

Actual guard tests exercise clone retention after dropping the initial guard,
continued native lock exclusion, reacquisition, and exhaustion after acquiring the
real lock without changing diagnostic bytes. Existing native child-process tests
exercise orderly and abrupt lock release and verify the reacquired serial range.
Existing root and lock substitution tests still exercise the original error paths.
Separate isolated atomic-counter tests cover zero, maximum, overflow and concurrent
issuance; those arithmetic fixtures establish no live ownership authority.

The production original-guard inventory-header consumer remains OPT-2136. Its
genuine joint decision, exact atomic nonce/intent/claim settlement and uncertain
readback are not supplied here. OPT-2194 physical qualification and OPT-2168 Start,
single-use permit issuance and genuine reconciliation remain open. No serial,
diagnostic or stored header authorizes effects, retry, refund or settlement.

Linux runs the genuine native guard and child-process tests. Windows GNU compilation
checks the source and test code; it does not execute the Windows kernel or qualify
native filesystem allocation. Existing platform acceptance remains separate.
