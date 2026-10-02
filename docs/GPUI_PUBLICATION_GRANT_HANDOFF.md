# Publication grant handoff and proof gaps (OPT-2001)

Reviewed baseline `d7fa604db8014ece76cde115b48818a411623cd3` and draft
[PR #35](https://github.com/PORTALSURFER/yield/pull/35), still open/draft at
`2b2ca1b19212546fd6e692c0c6ca7844f08d26d3` against historical base
`1fd45dd532f7f9c1e97b3c8e64b8b908af0c60f1`. The draft is a document-only
blocker checkpoint, not an unmerged physical publication implementation.
It is not merged by this source contract review.

## Current seam versus the older draft

| Boundary | Current implementation | Remaining proof/interface gap |
| --- | --- | --- |
| Owner-retained roots | `FileOperationOwner::register_profile_root/register_source_root` retain profile/source capabilities and registered lifecycle fences | Registration references/role locators remain observations. Target/staging roles must resolve to the exact retained authority at effect entry |
| Staging validation | `FileOperationCommand::for_staging_validation`; real `ValidateStaging` worker path; `FileOperationResult` binds operation/participant/output/object/roles/source fence and content evidence | Validation alone is observation, not publication authority or target absence ownership |
| Durable prepublish order | `OperationJournalClient::checkpoint_and_grant_staging` forwards to serialized owner; `checkpoint_staging_result` acknowledges CopyValidated, then `reserve_staging_grant` records one issuance | No active family caller consumes that issuance into a publication command; lost result/restart requires retained custody/reconciliation, not minting another grant |
| One-shot live source lifetime | `StagingPublicationGrant` owns validation plus `SourceEffectPin`; consuming `begin_publication(self)` obtains `SourceEffectPermit` | There is no publication-scoped file-owner constructor/queue envelope; source permission alone is not fresh operation/capacity/target permission |
| Existing chunk dispatch | `for_live_chunk` / submit with `LiveChunkEffectGrant` bind WriteStagingChunk only | Reject publication under a chunk grant; do not reuse a test Mutating authorization or forge a permit from checkpoint bytes |
| Physical namespace effect | `PublishNoReplace` carries staged evidence and target absence shape | Worker returns Unsupported; qualified primitive and final target provenance/durability are absent |

The draft's missing production registration and prepublish checkpoint
foundations have since been added. Its publication constructor/backend and
qualification gaps remain. OPT-1884 owns prepublication staging checkpoint
persistence; OPT-1634 is the later verified publication checkpoint. Their
ordering must not be conflated to reintroduce the old dependency cycle.

## Required private implementation interface (not installed)

The first publication producer must request a fresh grant from the journal
owner after its exact immutable staging result is durably acknowledged. The
grant is moved once into a private file-owner publication envelope together
with immutable target request and explicit same-volume/absence requirements.
The envelope retains the original grant on confirmed queue rejection; a
generic encoded command or replayed CopyValidated row cannot reconstruct it.

Its constructor and worker must compare the grant's operation/participant,
generation and exact staging effect/output/object/fingerprint against the
target command, its source fence and both owner-local registration roles.
Retained source pin must match SourceId, root/database identities and lease
epoch, then enter its live-effect boundary. Recheck ordinary operation state,
durable capacity/retained staging charge, and cancellation immediately before
the qualified namespace primitive. A source-live permit by itself cannot
authorize a cancelled operation or a different target. Those extra sealed
operation/capacity/target checks are not supplied by today's `begin_publication`.

Keep the grant and source permit in the worker until result custody, including
queue/duplicate/cancellation handling. Before effect, wrong/stale registration,
target, capacity or cancellation is typed no-effect rejection. Exact duplicate
command retrieves the original result under the same admission identity;
changed bytes conflict. Once started, interruption/reply loss is retained
uncertainty, never a fresh grant or unconditional retry of the namespace claim.

After the primitive, a verified result must bind the created final object to
the transaction's staged object/provenance and durable synchronization proof.
OPT-1634 persists that exact effect checkpoint before source/global outcomes
or undo authority. A successful pathname reopen, matching content, UUID name
or target disappearance is insufficient. Late target collision preserves
both target and staging; unsupported/cross-volume/ambiguous outcomes retain
staging, charged capacity and journal evidence for independent resolution.

## Portable fixture/proof map

| Existing fixture | Evidence and limit |
| --- | --- |
| `staging_checkpoint_requires_exact_durable_v16_participant_and_acknowledgement` | Exact durable participant/phase, duplicate/changed evidence, checkpoint/grant persistence and restart behavior. Stored rows cannot certify an actual publication primitive |
| `chunk_staging_checkpoint_retains_charge_across_faults_replay_and_restart` | Positive staged-content checkpoint and retained charge under replay/fault/restart; a chunk result is not a publication grant |
| File-owner root/fence, wrong owner and command-conflict fixtures | Registration/role/fingerprint rejection and exact duplicate receipt behavior; synthetic authorization is not ordinary-family publication |
| `for_live_chunk` restriction and declared unsupported-effect fixture | Chunk-only constructor cannot authorize PublishNoReplace; typed PublishNoReplace remains Unsupported |

The missing private publication envelope must add wrong-grant/target/output,
stale source/operation/capacity, queue-rejection permit return, late collision,
duplicate receipt and grant-restart custody fixtures before enabling an effect.
Current portable foundations are reused; no test reports fake publication.

Original OPT-1632 owns this constructor/worker integration and its qualified
backend; OPT-1868 retains the accepted macOS unsupported policy. Windows
opened-handle candidate qualification remains independent and does not imply
Mac support. Cleanup requires its own grant/proof under OPT-2002/1633.
This completes the bounded source interface/proof-gap review, not either
physical adapter or release acceptance.
