# OPT-1568 post-intent capacity fault matrix

This matrix covers the common chunk, ordinary-claim, and recovery contracts. It does not certify an operation-family writer or a production publication/cleanup route.

| Fault boundary | Focused evidence | Required retained outcome |
| --- | --- | --- |
| Claim precommit failure, claim commit uncertainty | `chunk_claim_fault_boundaries_rollback_or_retain_exact_charge` | Confirmed rollback removes the provisional claim; uncertain commit retains the exact charge and durable claim. |
| Issuance acknowledgement loss and synthetic write-entry denial | `chunk_issue_and_release_uncertainty_replay_without_reentry`, `post_intent_chunk_claim_issue_release_and_replay_are_owner_serialized` | No second permit or callback entry from an Issued row, including after owner restart. |
| Qualified staging checkpoint, next chunk exhaustion | `chunk_staging_checkpoint_retains_charge_across_faults_replay_and_restart` | A verified incomplete staging row and claim survive restart; a failed next claim projects `PartialNeedsRetry`. |
| Exhaustion without a positive checkpoint | `post_intent_capacity_exhaustion_without_positive_checkpoint_stays_retry_pending` | Durable capacity retry remains `RetryPending` after restart. An Issued identity alone cannot promote it. |
| Unused chunk release failure, retry, duplicate | `chunk_staging_checkpoint_retains_charge_across_faults_replay_and_restart`, `owner_pending_chunk_release_retry_reuses_exact_proof_without_reentry` | Failed release retains charge and exact identity; retry releases once; duplicate replay cannot release another claim. |
| Restart rebuild after partial release | `chunk_staging_checkpoint_retains_charge_across_faults_replay_and_restart`, `ordinary_claim_rebuild_reopens_and_preserves_partial_release_identity` | Rebuilt provider counts only unreleased claims and retains staged evidence. |

The focused test combines a staged chunk, a second unused chunk, injected release rollback, restart rebuild, an exact release replay, and injected post-intent capacity exhaustion. The test uses a direct store fixture for durable accounting; owner-side provider reconciliation is covered by the owner tests listed above.
