# Original admission transaction controls

The funded ordinary startup and admission readers use their original `Connection`
and original namespace/transaction ordering. `store::transaction_control::Reader`
prepays the exact outgoing `OperationJournalOwnerError` Box before BEGIN, then
prepares and steps the original one-statement BEGIN or ROLLBACK through the private
borrowed SQLite statement. Every statement is finalized by RAII on success, error,
and unwind. Unfunded compatibility readers retain `Connection::execute_batch`.

A selected native failure copies the original code and UTF8-lossy message only
after a checked debit on the original ordinary input pool. For lossy message
length n, the native envelope includes:

- Original String growth: `2n + 8`.
- Two potential Store-formatted consumers: `2 * (2 * (n + STORE_PREFIX))`.
- Shared startup diagnostic: `2 * (n + STORE_PREFIX + STARTUP_PREFIX)`.
- Three String headers.

The producer includes these named consumers conservatively. Runtime startup and
Shared diagnostic consumers also measure and reserve their own text before
formatting. A generic Display counter measures output bytes; it does not prove
that arbitrary Display implementations allocate nothing internally. This control
path formats the retained SQLite message, whose original copy is prepaid.
Namespace/native SDK and other-family formatting have separate prerequisites.

The fixed loan is held through the original read transaction. Later pressure
cannot suppress mandatory cleanup: ignored ROLLBACK always attempts the original
SQL. Its independent native loan/error is destroyed locally, error first. It
cannot replace or refund a primary selected failure. The original primary error
precedence is preserved.

Selected control loans become terminal `StartupCredit`s in a dedicated original
reply slot. They carry no OwnerShared or reply back-reference. Namespace, observer
and control slots remain separate; occupied control slots reject without replacing
credits. Slot poison invalidates the original resource gate and lifecycle. Original
reply headers are charged before allocation by the existing size-derived debit.
The private generic returned-error carrier destroys its error and Box allocation
before its reply keeper. The last reply wrapper deallocates the Arc before dropping
terminal credit. An error consumed by either Request API can therefore retain its
original debit after Request destruction and worker shutdown.

## Verification scope

The portable same-connection tests cover successful controls, native code/message
parity, exact pre-BEGIN and native-copy pressure, simultaneous primary/secondary
failures, cleanup under pressure, unwind, selected alias custody and exact refunds.
The actual queued component uses a test-only SQLite authorizer on the same original
Store to deny BEGIN. Both Request APIs, lost reply, zero ID allocation and unchanged
database bytes are checked. An existing original startup worker component exercises
the new funded reader and diagnostic handoff. These components start without
physical capacity authority and cannot qualify production physical admission.

`scripts/transaction_control_probe.rs` links a normal test-support Library with the
System allocator. Its four differential cases use the identical original database
and transaction state. Separate valid/invalid UTF8 text fixtures exercise the
production copy/formatting envelope; they are conversion fixtures, not real native
engine failures. Pool, CString and profile setup are excluded from the measurement,
as are native C allocation and RSS. No whole-process bound is claimed.

The implemented macOS production startup harness remains
`capacity_startup_native_production_rebuilds_original_descriptor_bound_image`.
OPT-2405 tracks its execution on genuine SDK/APFS resources with the original
production registry, descriptor observations and positive protected reserve floors.
Linux components do not replace that verification.

This work does not issue permits, authorize retries, refund reservations, mutate
settlement, qualify physical storage or complete the entire startup/admission
ledger. Namespace backing (OPT-2402), other-family readers and future ledger growth
retain their own acceptance and tracking parents remain open.

Collection-pressure fixtures derive their original row count from the exact
namespace working debit plus the control working debit. The latter stays live
during the nested namespace guard. The actual collection target remains one
byte below its derived debit; an earlier namespace refusal cannot stand in for
the collection refusal.
