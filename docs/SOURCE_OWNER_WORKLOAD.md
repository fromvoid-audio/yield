# Bounded source-owner metadata workload (OPT-2092)

Run two independent, isolated measurements:

```sh
python3 scripts/internal/bench/run_source_owner_workload.py \
  --runs 2 --output /tmp/source-owner-workload.json
```

The runner executes the locked `yield-library` integration workload. Each run
creates a temporary profile with real `WritableProfileGuard` ownership and a
profile-bound `SourceDatabase`, retained by `SourceWriter`. No live profile is
selected. The temporary media root contains no physical WAVs: the corpus is
4096 deterministic metadata rows, with versioned path/size/timestamp facts.
Its BLAKE3 fingerprint uses UTF-8 path bytes, NUL, little-endian u64 size and
little-endian i64 timestamp for each row in path order. Content-hash strings
are explicitly synthetic fixtures, not physical content verification.

The fixed workload uses 64-row transactions through
`commit_scan_manifest_batch`, verifying exactly one revision increment, 64
touched paths and no fallback full-manifest snapshot per batch. It activates
empty staged directory truth through the existing audit-completion route.
That API returns a complete manifest as part of its acceptance behavior;
the harness adds no full-table query solely to measure performance.

After audit, `SourceSnapshotOwner` consumes the retained writer and returns
at most 64 rows per request. Each page must match the owner-issued revision;
the traversal stops at the opaque continuation's end and has a hard page
count bound. Every row's order, size, timestamp and presence is checked against
the fixed corpus. The worker retires and closes cleanly. Reopening the same
profile database at lifecycle 2 must preserve database identity, audited
revision and the entire row fingerprint, while rejecting the lifecycle-1
observation as stale. No raw SQLite connection or competing writer is opened.

The evidence includes raw nanosecond samples and nearest-rank p95, lower-median
p50 and maximum for transaction/page calls, plus open, audit, close and reopen
durations. Transaction time starts before owned batch creation and includes
row preparation, SQL, commit and any existing owner checkpoint maintenance.
Page time covers enqueue, worker execution and reply. The page queue also reports
actual monotonic try-send-to-dequeue durations through fixed owner counters and
eight non-cumulative microsecond buckets. This separates dispatch wait from the
whole page call; SQL, sync and checkpoint times remain unmeasured independently.
Audit time includes existing full-manifest materialization.
Reopen time includes database acquisition, revision validation and worker start.

Storage observations reuse `capture_tag_metadata_budget` on the current owner:
main/WAL lengths come from retained SQLite VFS descriptors, SHM length from the
retained database root, and logical pages from SQLite. If that bounded schema
and metadata observation is unavailable, the report retains the error and
marks storage unavailable. Its allocation estimate is not a capacity claim;
the harness issues no journal capacity admission or tag mutation authority.

The Python runner records the exact command, Git head/dirty paths, Rust version,
OS/kernel/architecture, logical CPUs and affinity. It checks stable fixture,
revision and restart fields across 2–5 runs and rejects a source-state change
during measurement. Timing variation is retained; no pass/fail latency target
is invented. CPU quotas and storage device qualification remain unmeasured.

Queue counters are acquisition-local observations sampled independently. Accepted
means channel admission, not a successful read. Dequeued includes expired requests.
Pending dispatch reservations include an offered enqueue before its result is
known, so this gauge/peak is not an exact physical buffer depth. Rejected admissions
refund their reservation; a caller timeout never refunds already accepted work.
The production channel remains one slot and owns the same sole writer. Fixture
control barriers are excluded. No paths, request IDs, labels or event history are
retained. A new worker starts with empty counters even when the database reopens.

This is a runnable portable metadata-owner measurement prerequisite for
OPT-1683/1852. It does not complete their full durable-operation, capacity,
crash-recovery or SLO acceptance. It does not qualify media traversal, content
hashing, physical filesystem publication, native GPUI responsiveness or
power-loss durability. OPT-1347/1348 and tracking parents remain open.
