# Participant ordering and checkpoint/disposition decision (OPT-1641)

This decision supplies the contract for OPT-1341 consumers, including OPT-1642–1648,
OPT-1357 and later OPT-1352/1353 routes. It does not implement filesystem effects,
cross-database atomicity or UI policy. The normative phase/disposition vocabulary remains
[IO_ARCHITECTURE_TARGET.md](IO_ARCHITECTURE_TARGET.md#cross-source-move-participant-ordering).
OPT-1341 remains a tracking parent until its consumers are implemented and verified.

## Ownership and current interface inventory

Paths below are relative to `crates/yield-library/src/`. A shape or stored observation
is not a live receipt. Queue acceptance is not a committed checkpoint.

| Boundary / actual interface | Owner and caller contract | Current limit / fail-closed rule |
| --- | --- | --- |
| `operation_journal/participants.rs`: `ParticipantSet`, `ParticipantDescriptor`, `ParticipantCheckpoint`, `StoredSourceFence` | Coordinator freezes participant IDs and required/optional status with durable intent; each participant reports its own committed revision. | Bounded untrusted data; `required_complete()` does not prove ordering, ownership or physical effects. Only artifacts may be `OptionalRebuildable` / `Deferred`. |
| `operation_journal/work_plan.rs`: `derive_work_plan`, `WorkUnitKey`, `WorkUnitRole` | Coordinator derives exact per-action edges from typed command endpoints and frozen participants. | A plan is not execution authority. Same-source copy-then-remove still needs separate destination/origin work units, even when the participant ID is shared. |
| `operation_journal/work_progress.rs`: `WorkProgress`, `VerifiedStepCompletion`, `CompletionContext` | Verified private completion must match operation, plan, key, source scope, execution epoch and receipt kind; prerequisites must already be complete. | Public stored summaries cannot construct production verified completions. Replay is exact context + evidence; conflicting replay fails closed without changing progress. |
| `operation_journal/work_progress/dispatch_owner.rs`: `LiveDispatchOwner`, `DispatchPermit`, `EnteredDispatchPermit` | An owner reserves and enters one effect; returned, entered, late and uncertain outcomes remain distinct. | This is internal dispatch/receipt machinery, not a complete production source/global/Harvest adapter. Cancellation must drain entered work before closure. |
| `file_operation_owner.rs`: `FileOperationCommand`, `EffectRequest`, `FileOperationResult`, `EffectOutcome` | File owner retains verified registered roots and object identities; journal authorization and source authority precede physical effects. | `Unsupported` remains unsupported. A request or retained `FilesystemReceiptSummary` cannot mint a live absence, publication or compensation proof. Qualification for one effect does not qualify disposition/cleanup. |
| `operation_journal/owner.rs`: `OperationJournalOwner`; `store/staging_checkpoints.rs`: staging checkpoint/acknowledgement path | Profile-owned worker owns SQLite; exact live validation result is checkpointed and read back before reserving a publication grant. | Stored staging evidence alone cannot authorize publication after restart; live roots, authority and identity must be reacquired. Persistence uncertainty retains identities and holds. |
| `sample_sources/db/write/source_writer.rs`: `SourceWriterRequest`, `SourceWriterReceipt`, `SourceWriterCommandIdentity` | Source writer owns its database transaction and revision fence. A consumer must bind its receipt to the operation/work unit and exact source/root/database/lifecycle/lease. | Source writer availability is not a completed universal-operation destination/origin receipt bridge. Legacy source-local journal reconciliation is a compatibility participant, not a global coordinator. |
| `operation_journal/work_progress.rs`: coordinator receipt summaries and source revision observations | Global-library and Harvest owners each commit an idempotent operation/work-unit delta before reporting success. | Full production command-to-owner-to-durable-checkpoint bridges for every consumer remain implementation gates; no SQLite transaction is promised across independent owners. |
| `operation_journal/readiness_scheduling.rs`: durable scheduling/deferral observations; `dispatch_owner.rs`: readiness completion routes | Source publication precedes exact desired-deficit persistence and wake observation. Projection reports the committed revision, not a UI request acknowledgement. | Typed scheduling routes exist; arbitrary coordinator summaries do not substitute for source-owned durable rows. A projection/revision gap is auditable, not success. |
| `sample_sources/db/replay_authority.rs`; external observation provenance | Watcher owner proves continuity/generation and identifies the committed operation effect. | No universal watcher-echo acknowledgement bridge is established by the participant model. Raw events/path equality never authorize retirement or mark an operation successful. OPT-1357 and external reconciliation consumers must supply the bridge. |

Renderer/GPUI callers submit commands and consume bounded status/projections. They do not
open databases, mint receipts, run participant transactions or advance phases. This decision
does not add another coordinator, change `yield-session/src/lib.rs`, or alter the action register.

### Durable participant command/result identity

The consumer envelope carries a caller-stable `OperationId`, frozen participant ID and
`WorkUnitKey`, command family/schema and bounded canonical payload identity, plus the **current**
profile owner epoch and applicable live source fence. The stable receipt key is
`(OperationId, participant ID, WorkUnitKey)` within the owned profile database; the epoch is
an admission fence, not part of that replay key. A new owner epoch must not create a second
application of the same logical command after restart. A command-kind/schema/payload mismatch
under the same key is a conflict, never a fresh command.

The database owner checks the live profile epoch before effects, then atomically commits its
local delta and immutable result receipt in its own transaction. The receipt retains the key,
command/payload identity, applied/not-applicable outcome, owner revision and historical fences.
On exact retry through a valid current owner, it returns the original committed result; it
does not reapply the delta or advance the revision. A stale epoch is rejected even for a
duplicate request. Persistence/readback uncertainty retains the stable key and does not emit
a success receipt. Reordered results can complete only their matching operation/work unit;
they cannot clear a newer revision's deficit or satisfy another operation's checkpoint.

Recovery reads these receipts as observations, reacquires current authority and verifies the
command/owner binding before producing a private coordinator completion. It preserves the
historical committed revision and context; it does not rewrite old receipts with a fresh
lease. These are requirements for OPT-1836 and the OPT-1642/1644/1645 adapters, **not an assertion
that the current global writer already persists this envelope or receipt relation**. A bounded
consumer must define its concrete payload codec and migration with restart/conflict tests;
an in-memory UUID cache cannot meet the contract.

## Ordered checkpoints and preconditions

All receipts bind `OperationId`, action/work-unit key, participant ID, current execution epoch,
stable source/root/database identity, lifecycle generation and live lease as applicable. A
source revision may advance only with owner evidence; reacquisition must not resurrect a
historical lease. Persisted observations are useful for recovery but grant no authority.

| Checkpoint / current work role | Required predecessor and receipt | Phase after checkpoint |
| --- | --- | --- |
| Prepared / `Prepare` | Durable intent, bounded capacity/admission claims, source read, live verified capabilities and collision plan. Planning-time absence is advisory. | `Prepared` |
| Staging / `Stage` | App-owned destination-local staging; incomplete bytes are allowed here. | `FilesystemStaged` |
| `CopyValidated` | Exact complete staged object, validated content/identity, durable staging acknowledgement. No publication/adoption continuation before this gate. | `FilesystemStaged` |
| Reopened publication / `Publish` | Qualified handle/descriptor-bound no-replace final claim, reopened/content-verified final object; retain namespace, whole-publication atomicity and synchronization evidence separately. | `FilesystemPublished` |
| `DestinationSourceReconciled` / `DestinationSourceReconcile` | Verified publication followed by destination source manifest/identity/metadata transaction and committed revision receipt. | `FilesystemPublished` |
| `OriginRemovalStarted` / `OriginRemovalStart` | Destination checkpoint durable; exact origin source, expected object/content identity, locator and verified no-follow capability retained before mutation. | `FilesystemPublished` |
| `OriginAbsenceVerified` / `OriginAbsence` | Expected-identity handle-bound origin disposition, followed by capability-bound absence proof. Missing pathname or watcher event is insufficient. | `FilesystemPublished` |
| Origin source retirement / `OriginSourceReconcile` | Durable removal intent and verified absence; origin writer commits retirement. Both destination and origin source outcomes must be complete. | `SourceReconciled` |
| `GlobalLibrary` + `Harvest` | All affected source commits; each required owner delta is applied or explicitly not applicable with evidence. | `GlobalReconciled` |
| `Projection` | Required source/global/Harvest outcomes; bounded projection of the authoritative committed revision. | `ProjectionPublished` |
| `Readiness` | Source publication and projection; exact deficits persisted plus coordinator wake observed. | `ReadinessScheduled` |
| Terminal success | Every required step complete, no unresolved audit, exact admitted claim settlement; only optional rebuildable artifacts can remain deferred. | `Terminal` |

For copy/create/rename and other same-source operations, use the command's actual work plan:
source reconciliation follows the verified filesystem effect, then global/Harvest, projection
and readiness. Do not invent origin removal for a copy. Copy-then-remove moves obey the entire
destination-first chain above, whether the endpoints share one source or use independent source
owners. Qualified relocation is a separate plan with separately qualified effect evidence;
the copy plan cannot be silently upgraded to atomic relocation. There is one operation across
independent source transactions, never a fabricated cross-database commit.

`NotApplicable` means a frozen plan proves the work is irrelevant. It is not a waiver for an
unimplemented adapter, unavailable platform or failed required participant. Filesystem sync
may retain a documented power-loss downgrade while visibility is verified; absence of a
qualified no-replace effect cannot be treated as such a downgrade.

## Retry, restart, cancellation and guarded dispositions

| Situation | Required behavior / disposition |
| --- | --- |
| Transient failure before an effect | Retain the exact retry lease, attempt/context and identities as `RetryPending`; bounded retry requires fresh owner authority. |
| Committed effect with required work outstanding | Retain completed checkpoints and pending required work as `PartialNeedsRetry`; a global/Harvest failure after source commit stays at `SourceReconciled`, never globally reconciled/successful. |
| Exact duplicate receipt | Return replay without a second effect, revision advance or phase change. Conflicting operation/key/context/evidence is rejected; retain the first receipt. |
| Restart after origin-removal intent | Reacquire both verified capabilities and inspect both objects by handle. One verified destination plus proven origin absence can continue; unchanged expected origin may retry qualified disposition. Ambiguous copies/replacement/identity mismatch preserve candidates and enter `AuditRequired` or guarded `FailedDataLossRisk`. Never delete to resolve ambiguity. |
| Projection revision gap / watcher continuity gap | Retain authoritative receipts and phase; `AuditRequired` blocks completion until owner-bound audit resolution. Echo acknowledges the same operation + object + revision + generation, not a new mutation or generic duplicate-path event. Missing echo bridge remains a consumer implementation gate. |
| Prepublication cancellation | Quiesce entered work; independently qualified cleanup with verified staging absence and durable capacity release precedes `CancelledBeforePublish`. Unvalidated staging is not publication-ready. |
| Cancellation after publish / observed effect | Preserve the reached phase and exact entered effects; complete cancellation-scoped forward reconciliation or verified compensation. Origin intent prevents any claim that the origin was untouched. Retained uncertainty stays resumable. The full-progress after-observed-effect disposition must not be projected into legacy after-publish vocabulary. |
| Rollback after origin mutation | Require verified destination compensation and verified origin presence/source restoration, downstream reconciliation and claim settlement before `RolledBack`. No proof means no rollback success. |
| Required participant unavailable | Remain retryable/auditable or use an explicitly guarded `BlockedByUser` / `FailedPreservingData` outcome. Preserve receipts and data; terminal failure requires its own safe-state proofs. |
| Optional artifact deficit only | `SucceededWithDeferredArtifacts` after all required work and settlement; never use optional deferral for source/global/Harvest/projection/readiness work. |

## Evidence and remaining consumer gates

`operation_journal/participant_contract_tests.rs` exercises the existing journal work plan and
progress guards, with same-source and cross-source table rows. It rejects each skipped immediate
predecessor, holds `FilesystemPublished` through origin absence, requires retirement before
`SourceReconciled`, checks exact/conflicting replay, round-trips removal intent through the
full-progress codec, and retains checkpoints under a watcher-continuity audit. Existing
`partial_retry_*`, cancellation and guarded terminal fixtures cover disposition-specific guards.
All these are **portable contract/model fixtures**, using private test receipt constructors;
they are not production physical qualification or evidence of actual cross-owner commits.

Review checklist for consumers: name the real caller/adapter; bind receipt identity and owner
fences; commit/read back before downstream dispatch; preserve replay and restart evidence;
exercise persistence uncertainty, replacement and stale ownership; prove quiescence and claim
settlement at terminal boundaries. A model test alone does not satisfy that checklist.

Open implementation/qualification gates remain OPT-1692 (owned creation), OPT-1700 (rename admission),
OPT-1634 (verified publication checkpoints), and the OPT-1341 participant/coordinator
children. macOS/Windows handle-bound effects, synchronization, removable/remote filesystem
classification and crash qualification belong to the platform matrix (OPT-1843) and its scoped
native issues. This decision requires no native execution; consumers must record unavailable
native checks explicitly and must not mark missing receipt bridges complete.
