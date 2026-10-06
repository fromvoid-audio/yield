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

Connection observations now borrow the same original SQLite connection and return
a fixed refusal before constructing its owned path or reason. The materializer
calls the funding callback before each copy. Windows compares the original
volume/index/creation-time triplet using a 60-byte stack buffer. This callback is
not yet connected to the Store's original pool or a last-consumer keeper.

The Windows BOOL-error materializer captures the original full native message
once, measures its exact UTF-8 representation without allocating a Rust String,
and calls the callback before copying. It preserves the pinned SDK's trim and
HRESULT diagnostic grammar. Native message storage remains outside the Rust
heap envelope. On checkpoint e94ccdaa, two Linux text/trim contracts and locked
Windows GNU library/catalog test-support compilation passed. That compilation
does not execute the Windows native diagnostic parity harness.

Funding the enclosing namespace still requires more than counting Display output.
Pinned Rust 1.93's Unix OS-error formatter uses a 128-byte strerror buffer and
may allocate a lossy UTF-8 String. Its Windows counterpart uses a 2,048-unit
UTF-16 buffer and builds a Rust String before the enclosing formatter sees any
output. These are implementation bounds on formatter scratch, not new limits on
accepted paths or native errors. Root/profile validation, capability-library
scratch, error-conversion overlap and actual consumer lifetimes remain part of
the unfinished envelope.
