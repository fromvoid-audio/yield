# Original capacity retention and release observations

`capacity_retention_telemetry_snapshot` samples fixed numeric counters for existing provisional release, audit-pending/durable binding, confirmed-rollback release and ordinary claim release. Every original returned result is preserved. A success includes idempotent confirmation; it does not prove a new state transition, durable settlement or physical capacity. Counters saturate and their atomic samples are independent.

The existing provisional Drop release is observed if it actually calls release. Durable/audit-pending Drop does not call release and produces no observation. A confirmed-rollback call on a provisional lease invokes the original provisional release; both primitive returns keep distinct categories. No retry is authorized by counting a repeated caller attempt. Panic/unwind has no returned result and is not counted.

Elapsed buckets measure original primitive entry to its selected return, excluding prior caller work and subsequent settlement/destruction. The fixed 200-byte collector does not read ledger state, SQL, files or identities and has no lock, registry, callback, reset or authority. Logical fixtures exercise original accounting code only. Full OPT-1673/1670 and genuine settlement, recovery and physical qualification remain open.
