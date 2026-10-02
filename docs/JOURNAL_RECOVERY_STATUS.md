# Compact family-aware journal recovery status (OPT-2089)

`OperationJournalClient::scan_recovery_status_page` queues one read through the
existing profile-owned journal worker. The store method uses the existing combined
legacy/full-progress scanner, retained namespace validation and read transaction.
It performs no writable open, checkpoint, count pass or journal transition.

Each entry contains its family, positive observation sequence, optional validated
SQL OperationId and either a classified phase/disposition or a redacted retention
category. Legacy and full-progress dispositions remain distinct Rust variants;
observed-effect cancellation is never relabeled as publication. Dispositions are
persisted observations, not retry admission or physical completion guarantees.
The legacy capacity-aware `status_snapshot` remains available separately; this
page deliberately reports the classified persisted disposition without inferring
new chunk, capacity, checkpoint or release authority.

The compact value has no raw SQL key, record, intent, payload, claim, path,
diagnostic text, descriptor or execution token. Invalid keys omit OperationId.
A retained key mismatch may carry the validated SQL identity but never exports the
mismatching record identity. The page preserves the scanner's 16-row/2-MiB budgets,
keyset cursor and end-of-current-scan flag. Sequence ambiguity fails the whole scan.
Repeated observations can be deduplicated by profile/family/sequence; they are not
additional durable transitions. Consumers must retain unknown and audit states.

Validation uses isolated real SQLite records and a test-only journal-owner capacity
fixture: mixed families, corrupt records, cross-family identity ambiguity, invalid
key redaction, bounded pagination and reopen equality. Existing combined scanner
byte-budget, namespace and read-only tests cover the underlying admission route.
These fixtures do not qualify production reserve bootstrap, physical effects,
power-loss durability, a crash matrix, user status UI, telemetry or SLOs.
OPT-1671, OPT-1676 and their tracking parents retain that broader acceptance.

## Stable action categories (OPT-2107)

Every compact entry derives `JournalUserStatus` from its classified persisted
disposition or retained proof gap. UI adapters own wording and action admission.
The categories distinguish in-progress, infrastructure retry (including partial
work), audit, cancellation requested/completed at the exact recorded boundary,
success with optional artifacts still deferred, rollback, user decision, failure
with data retained, and unresolved data-loss risk. They never infer an actionable
retry, capacity readiness, full convergence or successful publication from a
callback, cancellation after an observed effect, unknown data or retained row.

The scanner copies only the bounded retry error class into a blockage hint:
contention, participant availability or capacity. A missing retry observation
leaves the hint unknown. Lease IDs, context, diagnostic text and retry timestamps
remain private. Status is deterministic across reopen. For a caller partitioned
by profile, `is_newer_observation_than` accepts only a higher sequence in the same
family for the same valid OperationId; duplicates, late, foreign or unknown-key
observations cannot replace a per-operation status. It does not treat a sequence
as execution authority or a writable-startup permit.

Actual persisted V5 retries, full-progress retries and guarded terminal records
exercise this projection. Forged observed-effect cancellation stays retained;
cross-family identity ambiguity stays audit-only. UI adapter/native evidence,
full status/matrix consumers and OPT-1676/1346 acceptance remain open.
