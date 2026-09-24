# Durable-operation event vocabulary (OPT-1672)

This is the GPUI backend contract for future structured observations. It does not
install an emitter, exporter, or metric. A log line is not a journal transition,
and a recovered row is not execution authority. Producers must derive an event
from an already classified result after its owning transition, never from a
requested transition or an unverified worker callback.

## Identity and common envelope

Every event has `name`, `schema_version = 1`, `observed_unix_ms`, and `origin`
(`live` or `recovery`). `operation_id` is required only when the owner has a
validated `OperationId`; an invalid SQL key must not be copied into it. A
recovered journal-row event instead requires positive `journal_sequence`,
`journal_family` (`legacy` or `full_progress`), and `row_outcome` (`observed` or
`retained`). This sequence identifies an observation, not a mutation permit.
No event payload contains a pathname, raw journal record, raw SQL key, opaque
intent, lease token, descriptor, reserve proof, or unbounded diagnostic.

For a live event, the owner supplies its own operation ID and an owner-local
observation sequence where that sequence exists. Do not synthesize a durable
sequence for a rejected pre-journal admission. For a recovery event, the
scanner supplies the sequence. Replaying a row may re-emit an observation;
consumers deduplicate by `(journal_family, journal_sequence, name)` within a
profile and must not count replay as another durable transition. Profile is a
runtime partition, not a serialized secret or an authority token.

## Event names and owner

| Name | Owner / source | Required event fields | Meaning |
| --- | --- | --- | --- |
| `journal.row_observed` | Journal recovery scanner and status projection | Recovery identity, `row_outcome`; for observed rows: `phase`, `disposition_family`, `disposition`; for retained rows: `retention_reason` | One classified row observation. A retained row never reports success or retryability. |
| `owner.queue_observed` | Serialized coordinator owner (OPT-1673) | `queue_depth`, `queue_capacity`, `admission_outcome` | Bounded queue observation, including rejection before a journal row exists. |
| `owner.writer_wait_observed` | Owner at writer acquisition (OPT-1673) | `wait_ms`, `acquisition_outcome` | One completed acquisition attempt; no projected success while waiting. |
| `capacity.decision_observed` | Owner's capacity admission/rebuild boundary (OPT-1673) | `capacity_outcome`, `allocation_class` when applicable | Decision after live proof and durable claim checks, never proof material. |
| `operation.retry_observed` | Coordinator retry transition (OPT-1673) | `retry_outcome`, `attempt`, `error_class` when known | Persisted or rejected retry observation, not a retry lease. |
| `watcher.continuity_observed` | Source-owned watcher reconciliation (OPT-1674) | `source_id`, `continuity_outcome`, `proof_gap` | Committed acknowledgement or retained gap, not a raw watcher event. |
| `projection.state_observed` | Revisioned projection publisher (OPT-1675) | `source_id`, `source_revision`, `projection_outcome`, `proof_gap` | A revision-gated projection result. |
| `artifact.state_observed` | Artifact reconciler (OPT-1675) | `source_id`, `source_revision`, `artifact_outcome`, `proof_gap` | Artifact result distinct from source publication. |
| `completion.stale_rejected` | Owner rejecting a stale completion (OPT-1675) | `component`, `stale_reason`, `proof_gap` | Rejection with evidence retained; never terminal success. |

`source_id` and revisions are required only for a source-scoped producer after
they have been validated by that owner. A null `operation_id` is legitimate for
external watcher reconciliation and malformed retained rows. Queue, wait, and
capacity events may use an operation ID only after one has been durably
allocated. Each downstream ticket must name its exact call site and map its
typed outcomes before emitting; these rows do not assert that those call sites
or instruments exist today.

## Values and proof gaps

`disposition_family` is `legacy` for `OperationDisposition` (V1-V15) or
`full_progress` for `FullProgressDisposition` (V16). `disposition` is the exact
Rust variant name, including `None`, `RetryPending`, `PartialNeedsRetry`,
`AuditRequired`, `SucceededWithDeferredArtifacts`, `BlockedByUser`, and
`FailedDataLossRisk`. `CancelRequestedAfterObservedEffect` and
`CancelledAfterObservedEffect` are **full-progress only**. Never map either to
`CancelledAfterPublish`: an observed effect is not application publication.

`retention_reason` is one of `LegacyMissingPublicationEvidence`,
`UnsupportedCodec`, `Malformed`, `KeyMismatch`, `SizeInvalid`, or `TypeInvalid`,
from `RecoveryRetentionReason`. Only the variant is exported. Free-form reason
text and invalid key previews are excluded.

`capacity_outcome` uses `unavailable`, `recovery_only`, or
`reconciliation_required` when reporting startup capacity readiness, matching
the capacity authority contract. A later `ready` value may be used only when
the owner has actually installed qualified live capacity. An estimator's
`Known` value is a byte estimate, **not** `ready` or reservation success.

`proof_gap` is a bounded structured field with `kind` (`none`, `missing`,
`ambiguous`, `stale`, `invalid`, or `unverified`), `component`, and optional
`evidence_code` from a stable allowlist defined by the owning downstream
ticket. It has no free-form path or exception text. `none` requires an owner's
positive proof; lack of a populated field cannot imply `none`. Unknown or
unsupported producer outcomes use `unverified` and remain non-successful.
Watcher, projection, and artifact outcomes require typed owner mappings before
their event families can be emitted; this document does not assign invented
meaning to an unimplemented outcome.

## Fail-closed rules and validation

- The journal scanner's `RecoveryEntry::status_snapshot()` provides bounded
  legacy status observations. A retained `JournalStatusOutcome` yields only
  `row_outcome = retained` and a typed retention reason. Invalid keys omit
  `operation_id`. The combined scanner also sees V16 full-progress records;
  `status_snapshot()` does **not** cover that family. OPT-1673 must not silently
  emit a legacy disposition for a V16 row.
- `CapacityRebuildReadiness::{RecoveryOnly,ReconciliationRequired}` and the
  capacity authority contract close ordinary admission. A metric or event can
  observe this decision but cannot reopen it. Missing or mismatched reserve,
  ownership, and claim evidence stays retained.
- OPT-1673 owns queue, writer, capacity, and retry producer tests. OPT-1674
  owns watcher continuity and gap tests. OPT-1675 owns projection, artifact,
  and stale-completion tests. OPT-1676 maps durable disposition to user status;
  no telemetry event substitutes for that status projection.
- This contract is reviewed against the current `OperationDisposition`,
  `FullProgressDisposition`, `RecoveryRetentionReason`,
  `CapacityRebuildReadiness`, and the capacity authority contract. The
  representative journal check is the existing seeded recovery/status test:
  it must show an observed row's phase/disposition and a retained row's typed
  reason without granting authority. There is no production event-emission
  assertion in OPT-1672 because the emitter is downstream work.

Open exceptions: producer-specific enum mappings, exact live call sites,
cardinality bounds, and telemetry transport are intentionally left to the
named emission tickets. No dashboard, SLO threshold, or exporter is defined.
