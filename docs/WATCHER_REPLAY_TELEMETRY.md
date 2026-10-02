# Retained replay observations (OPT-2090)

The existing `ReconciliationAdmissionSupervisor` emits
`watcher.continuity_observed` through `tracing`, target `yield::watcher_metrics`.
Its `watcher_metrics()` returns a fixed-size lifetime snapshot with saturating
counters and the current unresolved retained-marker count. It does not reset or
clear evidence. There are no per-path, stream, root or operation maps.

The producer uses the vocabulary in
[DURABLE_OPERATION_EVENT_VOCABULARY.md](DURABLE_OPERATION_EVENT_VOCABULARY.md).
Every event includes schema version 1, wall-clock observation time, `origin=live`,
component `retained_replay`, an outcome and a fixed proof-gap/evidence code.
The source ID comes only from a registered lane; unknown-source admission and
unknown-ticket rejection omit it. Paths, raw envelopes, root/stream identities,
lease tokens, SQL errors and diagnostics are excluded.

| Owning result | Outcome | Proof gap / evidence code |
| --- | --- | --- |
| Accepted continuity proof | `admitted_continuity` | `unverified` / `checkpoint_pending` |
| Accepted unproven observation | `admitted_unproven` | `missing` / `audit_required` |
| Exact duplicate suppressed | `duplicate` | `unverified` / `duplicate_is_not_acknowledgement` |
| Generation/root admission mismatch | `stale` | `stale` / `admission_identity_mismatch` |
| Other admission rejection or exhausted marker capacity | `rejected` | `unverified` / `admission_rejected` |
| Applied ticket retired with matching committed replay token | `acknowledged` | `none` / `committed_checkpoint` |
| Checkpoint authority/lifecycle/unknown-ticket rejection | `checkpoint_rejected` | `unverified` / `checkpoint_authority_rejected` |

Admission never means acknowledgement. Dispatch, application, proofless audit
handoff and duplicate suppression do not increment acknowledgement counters.
The successful checkpoint observation follows existing source/root/generation/
stream/terminal-sequence validation and ticket retirement. Repeating retirement
fails without another acknowledgement. A `none` proof gap describes that exact
checkpoint; `retained_proof_gaps` still reports other unresolved markers.

Acknowledgement lag uses a monotonic timestamp stored in the already bounded
pending ticket at admission. Only validated acknowledgements emit
`acknowledgement_lag_ms`. Seven disjoint histogram buckets cover <=1, <=10,
<=100, <=1000, <=10000, <=60000 ms and overflow. They measure local admission to
retirement, including queue wait; they do not measure native event capture age,
durable transition count, or cross-process recovery time. Counters reset when
a supervisor is replaced. Events may repeat across processes and must not be
counted as new durable transitions.

Portable validation:

```sh
cargo test --locked -p yield-library --lib sample_sources::reconciliation
cargo test --locked -p yield-library --lib replay_manifest_and_checkpoint_commit_once_before_raw_ticket_retirement
cargo fmt --all --check
cargo check --locked -p yield
python scripts/internal/check/check_gpui_owner_boundary.py
python scripts/internal/check/check_source_db_competing_writers.py
python scripts/internal/check/check_filesystem_producers.py
python scripts/internal/check/check_gpui_action_register.py --check
```

The supervisor fixture tests typed classification using explicitly synthetic
opaque checkpoint tokens. The source-writer fixture separately uses an actual
SQLite manifest/checkpoint transaction and reopen evidence. Neither establishes
physical filesystem publication or native capture qualification. OPT-1674
remains open for saga watcher echoes (OPT-1647), the combined replay matrix
(OPT-1669), and native capture/consumer integration (OPT-2080). OPT-1346 retains
its overall telemetry/SLO acceptance. No exporter or dashboard is added.
