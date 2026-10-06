# Detached original WAL observation custody

`WalPreparationObservation::databases()` exposes clonable original Source,
pre-audit lease Source and Global historical reports. A report may outlive its
Collector, Snapshot, retirement inventory and the actual worker cleanup/joins.
Local Source queue and Global preparation counters are not the original journal
input backing debit.

The private `HandshakeBudget` shares one original journal `JournalInput` after
the journal reserves its existing 64 KiB input backing. Collector construction
binds that custody exactly once before allocation of Collector handoff state or
acquisition of an owner. Poison/duplicate binding refuses without replacing
existing custody. Every real budget/credit alias retains this same original debit;
no second permit, larger ceiling, physical reservation or authority is created.

No raw Arc or Weak escapes the budget, audited Source observation or nested
Source scope holders. Their private Option<Arc> wrappers use Rust 1.93
`Arc::into_inner` on every Drop. With no explicit Weak, the last wrapper frees the
raw Arc allocation before dropping the moved payload. Source payload/fence
backing drops before its credit; the moved budget backing drops its input custody
after its own allocation is freed. Existing lease, Global credit, Snapshot and
JournalInput wrappers preserve the same ordering.

The factory already constructs a local accounting budget before journal input
reservation. It is not an original Shared debit or proof of complete factory
prepayment. `FIXED_FACTORY_HEADERS` accounts the budget backing with conservative
64-byte Arc/header slack, HashWriter, WalPreparationInput and original Global
request within the unchanged 4096-byte factory allowance. A compile-time check
also runs in native target/test configurations; variable source vectors/fields
and validation/topology scratch keep their existing separate checked charges.
The original journal reserve precedes new post-reservation allocations and reply
growth. This change does not claim original prepayment before factory construction.

The actual-worker regressions detach reports, complete original cleanup and
Source/Global/journal joins, drop every snapshot/handle and the strong diagnostic
budget, then require the exact original 65536-byte journal debit while only report
aliases remain. Final concurrent aliases must restore the exact input baseline.
Diagnostic budget handles are real funded aliases and cannot be retained during
a detached-only custody proof or final refund assertion.

Before-fix original Source+Global and genuine pre-audit Lease cases each retained
historical backing but observed zero journal debit instead of 65536. Their failing
logs are retained in OPT-2408. Interrupted compilation is recorded separately.

Reports remain historical observations. They cannot acknowledge current custody,
qualify a physical allocation policy, issue Start/attempt/permit authority, authorize
retry or reservation refund, or complete genuine release/settlement. Full OPT-2194,
OPT-2136 and native verification remain open. Terminal handle/header custody and
complete factory prepayment require their own actual ownership analysis; this
leaf supplies detached report backing custody.
