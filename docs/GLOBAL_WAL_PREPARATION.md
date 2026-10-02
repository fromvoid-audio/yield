# Empty global-worker preparation reservations

OPT-2135 implements the global local-owner boundary in
[WAL_PARTICIPANT_ADMISSION_CONTRACT.md](WAL_PARTICIPANT_ADMISSION_CONTRACT.md).
The public entry point is `GlobalLibraryWriter::submit_preparation`.

Capture `preparation_owner()` when forming a delayed request, then construct a
`GlobalPreparationRequest` with the immutable command digest, original cooperative
deadline, and complete handshake's database/encoded-byte bounds. These are bounded
correlation inputs, not proof of a valid operation inventory. The coordinator must
validate the canonical command, exact allocation claims, database coverage and
physical alias conflicts before any intent. A worker registration changes when
restarting inside the same profile acquisition; old requests cannot substitute a
new registration at dispatch. The maximum is 64 affected databases and 64 KiB across
the full handshake. Local request retention is fixed apart from the private bounded
profile acquisition identity. The deadline is at most two seconds from construction
and includes queue wait; callers must use the coordinator's earlier original deadline.

The existing 32-command channel admits preparation with ordinary commands. The
worker validates the retained profile/root and captures an existing no-follow main
file descriptor, compares path identity and the actual SQLite VFS/native identity,
and makes exactly one `BEGIN IMMEDIATE` attempt with zero busy timeout. It restores
the previous timeout on success or failure. An already active connection, unknown
binding, hard WAL (64 MiB), unknown journal/descriptor/size, cancellation, expired
request or stale registration cannot deliver readiness. WAL observations use the
actual retained VFS object while the writer lock excludes external writers; no
caller scalar, pathname size or extra writable SQLite connection supplies evidence.

The successful handle is non-cloneable and has no serialization or public
constructor. Its copyable observation carries registration, acquisition, physical
database identity, command digest, random local nonce and WAL bytes. These are
diagnostics. The nonce is not an OperationId, a capacity permission, a durable
acceptance receipt or a journal decision latch. The handle offers only abort and
observation. The 32 MiB soft-watermark flag requests future catch-up after release;
preparation itself never checkpoints or changes metadata/revisions/receipts/claims.

While held, ordinary commands remain queued with their existing FIFO/capacity. A
separate one-message abort channel, cancellation fallback and 25 ms cooperative
poll check deadline, shutdown, current profile/root/main binding. No ordinary SQL,
row query, maintenance, Harvest generation replacement or hidden lease renewal runs
on the reserved connection. Dropping pending delivery or the delivered handle
requests owner-managed abort even if the readiness or release response was lost.
After explicit abort, wait for the release response before assuming rollback was
confirmed. A copied observation or a previously delivered handle is not evidence
that the lock remains held after deadline/ownership loss.

An empty rollback must succeed and return the connection to autocommit before
ordinary dispatch resumes. Binding loss retires the connection; failed rollback or
timeout restoration closes it before releasing worker ownership and rejects queued
work. No fake action completion or metadata receipt is emitted. Shutdown first
aborts the pre-intent empty transaction, then drains accepted ordinary commands.
A logical deadline bounds queue/SQL policy and cooperative observation, not a stalled
filesystem/VFS call.

Actual retained-worker tests exercise external writer exclusion, blocked queued
updates and queue overflow, contention, stale profile/worker registrations,
protocol bounds, canceled queued and lost deliveries, expiry, shutdown, actual hard
WAL growth with a pinned reader, main-file/profile replacement, unknown journal
mode, rollback denial, exact timeout restoration and one observed acquisition.
These are Linux SQLite execution tests, not physical storage qualification.

OPT-2136 remains responsible for the full asynchronous participant inventory and
journal decision latch before identity/capacity settlement. In particular this
local pre-intent abort/deadline behavior cannot be reused after a journal decision
is latched: that integration must retain unresolved reservations until definite
outcome/readback. OPT-2137 remains responsible for actual durable required/recovery
claim permissions. OPT-1685/1348 and physical qualification remain open.

```sh
cargo test --locked -p yield-library --lib owner::preparation -- --test-threads=1
cargo test --locked -p yield-library --lib -- --test-threads=1
bash scripts/agent.sh preflight
```
