# Retained SQLite forwarding budgets (OPT-2167)

The actual profile-bound source, global and lease-control opens select a private,
uniquely named forwarding VFS through `BoundDatabaseRoot::open_owned_sqlite`.
Journal/store opens retain their existing boundary. On Linux the delegate is the
existing retained-root adapter, which preserves strict procfs directory aliases
and no-follow main/sidecar opens. Other native delegates are the bundled Unix or
Windows VFS; their physical allocation qualification remains OPT-2173/2174.

`OwnedConnection` closes its SQLite connection before unregistering/freeing the
VFS and before dropping the exact directory descriptor used in its procfs alias.
It moves that registration with the source catalog/writer/snapshot fence and
lease-control connection. A BUSY explicit close returns the complete owner.
An implicit BUSY close quarantines and retains the connection, registration and
directory descriptor rather than freeing resources still borrowed by SQLite.
That exceptional retention is not confirmed rollback or durable settlement.
The sealed opener creates the connection internally; callers cannot escape an
opened connection through a callback or replace its retained directory descriptor.
Legacy raw connection extraction rejects a registered connection. Mutable
`Connection` dereferencing is deliberately unavailable: `mem::replace` must not
separate a live connection from the VFS it borrows. Narrow transaction/readiness
methods preserve existing owner paths without exposing an escaping connection.
SQLite client-data destructors are unsuitable as the sole lifetime mechanism:
the bundled SQLite invokes them before pager rollback and file close.

The wrapper owns its own `sqlite3_file` header and an aligned native file buffer.
It installs its methods at xOpen; it never modifies a borrowed native method
table. Every delegated file/VFS call receives the original native object, including
native identity controls, locks, sync and retained DB/WAL size observation.
A single main file joins each private registration. WAL/main-journal opens must
refer to that same actual SQLite database-file object through SQLite's documented
metadata-bearing filename API. A second main/ATTACH, foreign owner's WAL, or an
unplanned temporary/journal role during a restricted attempt fails before opening
or writing the file. Filename suffixes cannot confer membership.

The crate-private `LogicalBudget` is a restriction, never an admission permit.
Installation requires a registered, already reserved WAL transaction, memory-only
temporary data, zero mmap and disabled automatic checkpoints. It disables native
preallocation chunking before applying the bound. Allocation-unit-rounded limits
are separate for DB, WAL and SHM. Each xWrite charges touched allocation units,
full file growth including holes, overwrite overhead and metadata overhead before
native I/O. Growing truncate and size hints charge the same conservative extent;
SHM mapping growth is bounded by region count/size and charged before forwarding.
Existing mapped SHM coverage is charged at installation, since later writes to an
already mapped region need not call xShmMap. Unsupported controls or allocation
coverage reject. Checkpoint writes traverse the same DB/WAL limits; no checkpoint
can bypass them. Native read/identity/lock/sync controls remain delegated.

Budget failure records a typed fixed category. Successful pre-forward charges are
retained on subsequent failure, including a partial native write. Rollback,
shrinking truncate, scope Drop and timeout never replenish charges. Scope Drop
quarantines the connection; the primitive has no scalar reset or retry/refill.
A genuine journal outcome/reconciliation consumer must settle the reported exact
attempt and retain uncertain physical charges before restoring dispatch. That
production permit/settlement protocol remains OPT-2168/2169/2170/2171; ordinary
hard/unknown WAL and profile/source admission checks are unchanged. Closing or
reacquiring an owner does not settle a durable claim or qualify an exception.

Linux tests use real bundled SQLite, retained directory descriptors, main/WAL/SHM
callbacks, pager cache spill and a real native prefix write followed by an I/O
failure. They demonstrate logical enforcement and owner lifetimes, not physical
CoW, metadata allocation, protected-reserve provenance or power-loss durability.
Native acceptance must exercise disposable supported volumes, qualified provider
bounds, native handle/identity/lock/sync controls, genuine reserve provenance and
all actual failure/retention cases in OPT-2173 (macOS) and OPT-2174 (Windows).

```sh
cargo test --locked -p yield-library --lib sqlite_budget -- --test-threads=1
cargo test --locked -p yield-library --lib -- --test-threads=1
bash scripts/agent.sh preflight
```
