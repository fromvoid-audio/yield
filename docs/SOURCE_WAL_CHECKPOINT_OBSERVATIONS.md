# Retained source WAL maintenance observations

SourceDatabase's existing post-commit hook validates its retained profile/database ownership
and then borrows that same SQLite connection. The maintenance policy observes the retained
WAL VFS descriptor with xFileSize; it neither opens nor stats a database/sidecar pathname.
Admission still requires its separate writer-lock observation. A post-commit measurement
is diagnostic evidence only and can change as other SQLite connections run.

At 32 MiB or above the owner attempts PRAGMA main.wal_checkpoint(PASSIVE). A single timestamp
retained in SourceDatabase limits attempts to once per 15 seconds per connection. Owner
retirement drops the timestamp; there is no global pathname map to accumulate retired sources.
An active transaction, unknown measurement, or small WAL skips the attempt. The interval
limits attempt frequency, not operating-system latency. It adds no worker or durable receipt.

Before and after measurements remain distinct. An unavailable after measurement is logged as
unknown with a fixed reason, never zero. SQLite's busy flag and both frame counts are retained:
PASSIVE can report busy=0 while a reader pins frames, so completeness additionally requires
nonnegative equal log and checkpointed frame counts. Even complete frame copying does not
promise WAL allocation reclamation. A SQL error retains its SQLite code and measurements,
and consumes the same attempt interval. Maintenance failure does not reverse a committed write.

OPT-2120 covers this production source hook and its portable tests. OPT-1687 retains broader
global owner maintenance, explicit shutdown/reader boundaries and scheduler coordination;
OPT-1686/1685/1348 and native/physical qualification remain separate open acceptance.
