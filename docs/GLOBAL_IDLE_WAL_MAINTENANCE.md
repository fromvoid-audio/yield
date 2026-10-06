# Idle Global WAL maintenance

OPT-2362 adds an independently paced idle opportunity on the existing Global
worker. Its original retained SQLite connection, profile guard and existing
checkpoint policy stay on the same thread. No new connection, timer thread,
queue, receipt or mutation generation is created.

The worker keeps one fixed monotonic timestamp (at most 32 bytes). Commands
reset it at dequeue and completion; completed preparation also resets it.
After at least 15 seconds without work, a receive timeout may admit one idle
pass. The timestamp resets after that pass, including an unavailable or
below-threshold observation, so the 25 millisecond owner poll does not become
a WAL observation loop. Missed periods produce no catch-up burst.

Admission checks Available under the existing short submission/shutdown gate.
It first checks for a command that arrived after the receive timeout, preserving
the original queue and dispatch accounting. The gate is released before profile
validation, filesystem observations or SQLite. Closing admits no subsequent
idle pass. An already admitted pass may finish with the original resources held;
begin_shutdown remains nonblocking. Disconnection adds no final idle checkpoint.
Existing SQLite close behavior is unchanged, including any native close-time
checkpoint; the tests count explicit policy PRAGMA attempts, not every internal
SQLite action.

Idle maintenance validates original profile/connection ownership before and
after the same soft-watermark/PASSIVE policy used for postcommit maintenance.
That policy's existing 15-second attempt throttle is shared across both entry
points. Active transactions exclude checkpoint work, and parked preparation
occupies the sole worker until it returns. The idle route neither consumes nor
invents the postcommit pending bit. Reads and exact receipt replay still have no
post-command commit marker; later quiescence is an independent idle opportunity.

An unavailable WAL observation, denied checkpoint or pinned reader is best
effort. It cannot reverse a confirmed metadata commit. Profile replacement before
or during maintenance closes the original worker through its existing ownership
failure path; it never opens a replacement database or confirms new ownership.

## Evidence

Actual worker fixtures use the unchanged production 15-second interval, real
SQLite WAL pressure on the original connection, a pinned reader, refused
observation/checkpoint, profile replacement and an admitted checkpoint blocked
while shutdown closes admission. Direct original-database checks cover active
transactions and pending-bit preservation. Deterministic timestamp cases cover
pacing/reset/missed periods, command preference, lifecycle refusal and a native
below-threshold observation. Those timestamp cases are timing geometry evidence;
the actual worker cases supply the real idle scheduling evidence.

## Limits

PASSIVE does not promise file truncation, hard-pressure size reduction, a bounded
native I/O duration or a durable shutdown checkpoint. Original full OPT-1687,
OPT-1686/1685/1348, Source maintenance, scheduler/retry, recovery and physical
qualification requirements remain open. Storage observations issue no permits,
authorize no retries, refund no reservations and complete no genuine settlement.
Logical metadata owner/SQL fixtures do not qualify publication or a native
database-attempt family. Windows GNU compilation is separate from native runtime
and filesystem qualification.
