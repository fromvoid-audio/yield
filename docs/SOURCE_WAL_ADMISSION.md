# Retained source WAL admission (OPT-2077)

The existing source owner rejects new profile-bound catalog curation and unseen durable curation
commands when its actual WAL is at or above 64 MiB. The soft state begins at 32 MiB. Both checks
run inside the existing BEGIN IMMEDIATE writer transaction, after live ownership/revision checks
and before metadata delta, revision or receipt insertion. Unknown mode/VFS/size fails closed.
The owner remains available for bounded reads and recovery. No reserved-recovery bypass is added.

Observation uses SQLite's SQLITE_FCNTL_JOURNAL_POINTER and its retained VFS xFileSize method.
It does not open a pathname, trust a caller's scalar or assume a missing sidecar has zero bytes.
The borrowed SQLite pointer never leaves the synchronous owner call; the non-Sync connection
remains live and is exclusively used by its worker. WAL mode and non-null VFS methods are checked,
and failed/negative size observations are typed unavailable outcomes. The database/profile/lease
binding checks remain the existing SourceWriter/SourceWriteBatch checks.

`SourceDbError::WalAdmission` retains a bounded `WalAdmissionError`: HardWatermark includes the
observed bytes and limit, with retry-after-maintenance guidance; ObservationUnavailable retains
a fixed recovery diagnostic. The ordinary catalog response and durable source command propagate
that error without confirming edits. UI callback/worker routing remains unchanged.

An exact existing durable receipt is checked before new-write admission. It may be observed at
hard pressure under current live ownership; conflicting payloads and stale ownership still reject.
Replay does not advance revision or gain capacity/file-effect authority. This gate does not mint
an operation ID. The supplied source command identity is an observation; rejection leaves no
new receipt, source delta or revision. Universal journal pre-ID allocation remains OPT-1685 scope.

Actual fixtures pin an external read snapshot and grow SQLite WAL frames through real commits.
At the production 64 MiB threshold both catalog and receipt-backed mutations reject, unchanged
source revision/connection change count and receipt count are verified, exact historical read/replay
still works, and the external snapshot remains intact. After that reader closes and the owner
performs TRUNCATE checkpoint maintenance, the original pending command is accepted. Independent
VFS tests cover inclusive thresholds, real soft/hard decisions, observed size after truncation and
unsupported journal mode. These are SQLite/owner tests, not physical reserve qualification.

## Remaining full admission scope

This implementation does not add checkpoint scheduling, classify external readers from VFS
size alone, or truncate/kill a reader inside a write transaction. Existing passive maintenance
remains unchanged; full owner-controlled catch-up/truncation policy is OPT-1687. A retained WAL
file may remain large even when frames are checkpointed, so this conservative gate stays paused
until size-reducing owner maintenance; passive checkpoint alone does not guarantee admission.

OPT-1685/1348 remain open for live affected-source/global health in universal pre-intent admission,
reservation-backed already-admitted required/recovery commits and legal recovery dispositions.
OPT-1688/1691 retain sidecar claims and full telemetry/fault acceptance. Other source mutation
families and global/journal owners are not covered by this finite curation entrypoint. OPT-2049
still needs capped page/schema/extension/no-spill policy, genuine capacity-owner claims and its
atomic assignment receipt; an observed WAL size is not a metadata allocation estimate or grant.
