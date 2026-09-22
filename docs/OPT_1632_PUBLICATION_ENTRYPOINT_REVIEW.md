# OPT-1632 publication entrypoint review

Status: first implementation milestone and replan evidence for [OPT-1632](https://linear.app/boostnlvp/issue/OPT-1632). Reviewed against `origin/main` at `1fd45dd532f7f9c1e97b3c8e64b8b908af0c60f1`. This review does not qualify a platform, enable a physical effect, or change the accepted macOS publication decision.

## Required end state

One `FileOperationOwner` `PublishNoReplace` command must consume an exact, live staging and destination authority, verify staged identity and content, require an already acknowledged durable `CopyValidated` checkpoint, make one qualified final no-replace claim, reopen and verify the final target, and return a typed result that retains staging and operation evidence on collision or uncertainty. The command must not infer authority from a locator, stored claim, UUID, source snapshot, or a test fixture. The target validation remains late collision, symlink, hard-link, cross-volume, cancellation, and restart-at-publish behavior. This is the OPT-1632 outcome, not a separate smaller acceptance target.

## Current entrypoint and missing authority

| Boundary | Current evidence | Missing for production publication |
| --- | --- | --- |
| Owner command and result | `file_operation_owner.rs::EffectRequest::PublishNoReplace` carries staging/target roles, `CopyValidatedEvidence`, and an absence predicate. `FileOperationOwner::submit` retains exact command fingerprints and typed outcomes. | `FileOperationCommand` has no production constructor for publication. The worker returns `EffectOutcome::Unsupported` after validation; `Applied` is reserved for a future backend. |
| Root capability | The worker checks owner-local registration references and authority fences. | `register_root_for_test` is `#[cfg(test)]`; there is no production registration that retains a real staging or destination directory handle. `RootRegistrationRef` is an observation, not a capability. |
| Live write grant | OPT-1857's merged [PR #32](https://github.com/PORTALSURFER/wavecrate-gpui/pull/32) issues and consumes `LiveChunkEffectGrant` for one `WriteStagingChunk` command. | `FileOperationCommand::for_live_chunk` rejects every other effect, and `EffectAuthorization::Mutating` has only test constructors. A chunk grant cannot authorize a final namespace claim or become one by retry/readback. |
| Durable ordering | [FileOperationOwner contract](FILE_OPERATION_OWNER_CONTRACT.md#results-and-durable-evidence) and [primitive matrix](FILE_OPERATION_PRIMITIVE_MATRIX.md#required-order-and-evidence) require `CopyValidated` acknowledgement before the final claim. | OPT-1634 owns stage/publication checkpoint persistence but is blocked by OPT-1632. A successful no-replace primitive without the prior checkpoint cannot be the production adapter's publish path. The checkpoint prerequisite and downstream ticket order need an explicit split or ordering change before enabling the effect. |
| Backend | `sample_sources/db/file_ops_journal/recovery_io.rs` has a narrower Windows opened-handle `FileRenameInfo` primitive. The [matrix](FILE_OPERATION_PRIMITIVE_MATRIX.md#effect-matrix) rates it a candidate, not an owner-qualified workflow. | Windows needs proof for the exact retained staging handle, target parent, same volume, collision result, post-claim reopen, and synchronization before a verified receipt. The accepted [macOS decision](GPUI_MACOS_PUBLICATION_DECISION.md) leaves production staged publication unsupported; a name-bound rename cannot be used as a substitute. |

The merged OPT-1857 closeout explicitly states that it added no ordinary-family producer, production root authority, or physical I/O. Its `Done` status establishes the chunk-grant seam only. It does not remove the publication-specific gaps above.

## Replan boundary

Before implementing a physical `PublishNoReplace` arm, resolve three prerequisites under explicit ownership: (1) an owner-retained production staging and destination root registration; (2) a sealed publication-scoped live intent/capacity grant, issued through the journal and source-owner lifetime gate rather than the chunk-only constructor; and (3) a durable pre-publish `CopyValidated` acknowledgement with restart custody. The first two cross the current FileOperationOwner and journal/source authority boundaries. The third conflicts with the present OPT-1632 → OPT-1634 blocker direction if OPT-1632 is expected to publish successfully before OPT-1634. This meets OPT-1632's stated stop-and-replan condition; do not fill it with synthetic authorization or an unverified pathname backend.

A safe implementation sequence is to establish those prerequisites with focused tests, then qualify one Windows handle-bound no-replace backend and its post-claim verification, while retaining `Unsupported` on macOS and unqualified filesystems. Windows-specific CI jobs previously ran zero steps because of the GitHub account spending limit; those jobs are not runtime acceptance. No successful Windows publication claim is made by this review.

## Validation of this milestone

- Inspected the exact `PublishNoReplace` shape, owner validation and worker dispatch, `for_live_chunk` restriction, test-only root and mutating-authorization constructors, OPT-1857 closeout, primitive matrix, and macOS decision at the reviewed head.
- `rg` found no production `FileOperationCommand` construction or production root registration outside the test helpers and chunk-specific private adapter. No physical publication was attempted.
- This is a dependency and ownership finding, not a completed OPT-1632 adapter. The original implementation and its late-collision, symlink, hard-link, cross-volume, cancellation, and restart tests remain required.
