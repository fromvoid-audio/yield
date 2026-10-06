# Original journal namespace backing (OPT-2402)

The diagnostic borrows the original Store, retained root, profile guard and SQLite
connection. It measures namespace validation and the actual selected error's
Display formatting through destruction. Setup, retained Store allocations,
SQLite/native allocation and RSS are excluded from the Rust System allocator
interval. Successful validation, an active original transaction, a real retained
WAL sidecar and a missing original pathname are distinct cases. The original
database bytes and sidecar evidence must remain unchanged.

This first checkpoint supplies a diagnostic only. Production namespace funding,
derived allocation bounds and last-consumer credit custody are not implemented or
verified yet. The probe does not grant a physical reserve, permit, retry, refund,
effect, cleanup confirmation or settlement. Native runtime verification and full
tracking parents remain open.
