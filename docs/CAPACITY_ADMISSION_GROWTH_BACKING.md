# Original initial-admission growth backing

The queued prepared initial-admission route reserves three loans from its exact
original input pool before plan validation, claim validation, or provider graph
construction. Its production entry still consumes the original owner's genuine
live descriptor witness. The original pool and all physical qualification gates
are unchanged.

The bound uses actual allocation and claim counts, compiled key/value layouts,
and a complete pinned BTree internal node per entry. Separate scratch covers
validation identity sets, facts/volume staging, charge maps and FromIterator
sorting. Separate retained loans cover original token/claim/aggregate maps,
release history and overlapping disposition vectors, plus the exact returned
lease's charge map and claim vectors. Existing startup credit pays the fixed
ledger and publication mirror allocations.

An iterative chain of detached, non-Clone credits belongs to the original
ledger. Each chain node is prepaid. All ledger graphs drop before the chain;
each original node allocation is freed before its own credit returns. The chain
survives worker/input/provider destruction while an original ledger or lease
alias lives, without retaining the worker or reply. The lease separately owns
its own detached credit after all its backing fields. Ordinary admission errors
remove only the newly installed chain node and preserve prior ledger loans.

Ledger growth credit remains retained after a token release because aggregate
nodes and release history can survive that release. It returns at final original
ledger destruction. This conservatively retains metadata backing and never
infers physical settlement from memory destruction. Pinned/uncertain leases
still refuse ordinary release; only existing confirmed rollback rules release
their original physical charge.

Tests exercise pool pressure before token/mirror/charge changes, refused facts,
confirmed rollback and aliases surviving input/provider destruction. A System
allocator integration test checks the live original debit at every actual Rust
allocation and destruction through rollback, durable, unknown and refused
component paths, including the existing 256-row plan/claim ceiling. The public
diagnostic exposes scalar results only and installs no native qualification,
witness, lease, source mutation or effect permission.

This covers initial provider growth. Post-intent chunk admission, the complete
V19 Legacy worker/atomic settlement, other historical-family audit graphs and
native positive-floor verification remain separately required. SQLite's C
allocator, physical extent usage and process RSS are outside this Rust backing
measurement.
