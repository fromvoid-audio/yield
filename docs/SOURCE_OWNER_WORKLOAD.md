# Bounded source-owner metadata workload (OPT-2092/2109/2142)

Run two independent, isolated measurements from a clean checkout:

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

After that original reopen/page verification, the same retained writer applies one
64-entry `SetRating(KEEP_3)` curation command. Revision 65 advances exactly once to
66, and the persisted receipt contains exactly the selected paths/ratings. Exact
same-acquisition replay returns the same receipt without advancing the revision.
Conflicting payload reuse and a new stale-revision command are rejected, with no
locked metadata leaked. A second close/reopen at lifecycle 3 preserves revision 66
and returns the exact historical receipt through `read_receipt_observation`.
Replaying the old request must return `SupersededLifecycle`; that historical receipt
is never fresh write authority. A 64-row page plus one 1-row continuation verifies
the persisted selected ratings and the unchanged first unselected row. This adds
no full-table measurement pass or second corpus/runner.

Curation timing fields separately cover the whole-owner commit, exact replay,
conflict/stale rejection, clean close, reopen, historical receipt read and
superseded request rejection. `persisted_selection_ns` sums the two page calls;
assertion/verification loops are outside that duration. Commit and replay include
existing ownership validation, SQLite reservation/transaction/receipt processing
and checkpoint policy; these are whole-call boundaries, not independent SQL or
sync measurements. Storage is observed both after initial audit and after curation
commit through the existing bounded producer, outside timed command boundaries.

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
and metadata observation is unavailable, the report records a fixed unavailable category and emits diagnostics to process
stderr; unavailable storage is not silently replaced with fabricated bytes. Its allocation estimate is not a capacity claim;
the harness issues no journal capacity admission or tag mutation authority.

The schema-2 runner requires a clean checkout and records the exact command, Git
head, Rust/Cargo versions, OS/kernel/architecture, logical CPUs and affinity.
SHA256 digests cover workspace/library manifests, Cargo.lock, toolchain files and
repository/Cargo-home configuration (absence is recorded explicitly). Build
semantic environment overrides such as RUSTFLAGS, Cargo profile/target/runner
settings and compiler wrappers are rejected by name without printing their values;
CARGO_TARGET_DIR remains allowed for neutral artifact caching. The runner checks
source/configuration state immediately before and after each of 2–5 independent
runs. These digests describe
the observed build configuration; they are not a hermetic build or platform grant.

Each workload process has a 300-second budget and an 8MiB combined stdout/stderr
budget on POSIX hosts. Failed, missing, duplicate or malformed marker records,
unknown fields, incomplete timing distributions, invalid quantiles, unfinished
queue counters, inconsistent receipt assertions and unsupported schema/corpus
identities fail closed. Output is replaced atomically only after the complete
bundle validates. Timing variation is retained; no pass/fail latency target is
invented. CPU quotas and storage device qualification remain unmeasured.

Run intake/process-budget negative tests separately:

```sh
python3 scripts/internal/bench/test_source_owner_workload.py
```

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
