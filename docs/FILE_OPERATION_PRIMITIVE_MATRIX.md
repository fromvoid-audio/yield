# File-operation primitive support matrix

This is the OPT-1631 decision artifact for the proposed
[FileOperationOwner contract](FILE_OPERATION_OWNER_CONTRACT.md). It records the
boundary between an operating-system primitive, the narrower source-local code
that currently uses a primitive, and a qualified owner workflow. It does not
implement a backend, enable publication or cleanup, or qualify a platform for
production rollout.

The related semantic contract remains in
[IO_ARCHITECTURE_TARGET.md](IO_ARCHITECTURE_TARGET.md), especially its
[publication durability contract](IO_ARCHITECTURE_TARGET.md#publication-durability-contract).
The macOS production decision is recorded in
[GPUI_MACOS_PUBLICATION_DECISION.md](GPUI_MACOS_PUBLICATION_DECISION.md).

## Reading the matrix

The columns intentionally describe different facts:

| Axis | Meaning |
| --- | --- |
| Primitive available | The platform exposes an API that can perform a low-level operation. This is an API fact, not a Yield guarantee. |
| Source-local implementation | The current source database recovery path has code that uses the primitive for its own journal rows. This is not a general file owner. |
| Workflow/volume qualified | Yield has evidence for the exact roots, object identities, volume relationship, ownership, ordering, collision behavior, synchronization, and uncertainty retention required by the effect. |
| FileOperationOwner-ready | The owner queue, live effect grant, producer adapter, and coordinator barriers exist and have been qualified. OPT-1630, OPT-1857, OPT-1632, and OPT-1633 remain required for the corresponding effects. |

`Candidate` means that a primitive may be useful after the required evidence is
collected. `Unsupported` means the current contract must fail closed and retain
staging, claims, and journal evidence. `Pending` means the operation has a
plausible primitive or source-local precedent, but the workflow or volume proof
is still missing. None of these labels grants capacity or filesystem authority.

## Effect matrix

| Effect and claim | Primitive/API and current source evidence | Workflow and volume status | Owner status, preconditions, and retained outcome |
| --- | --- | --- | --- |
| `PublishNoReplace` / `AtomicNamespace` | macOS `renameatx_np(RENAME_EXCL)` and Windows `SetFileInformationByHandle(FileRenameInfo)` expose no-replace namespace operations. Current source-local recovery uses an opened staged handle and target-parent handle on Windows; non-Windows production recovery returns `Unsupported`. | macOS **Unsupported** under the accepted production decision: the call is name-bound and does not consume the held staged identity. Windows is a **primitive candidate**, not a qualified workflow. | Requires a live staged identity, target-root registration, absent-target predicate, matching device/volume, source and destination fences, an OPT-1857 live grant, and collision retention. A stale root, collision, unsupported filesystem, or ambiguous result retains staged data and journal evidence. |
| `WholePublicationAtomic` | No single final rename proves that source-to-staging transfer and final publication were one atomic operation. A bytewise copy is non-atomic even when source and final share a volume. | **Pending qualification** only for a fully qualified same-volume transfer whose every applicable step has evidence. Cross-volume and bytewise transfer are **Unsupported** for this claim. | Requires `CopyValidated`, qualified transfer and final-claim primitives, synchronization evidence, and reopened verification. Otherwise record `WholePublicationNonAtomic` where a qualified final claim exists, or retain the operation before publication. |
| `CreateStaging` / clone or copy | Apple `clonefileat`/`fclonefileat` and ordinary copy APIs can create staging. The current source-local recovery path consumes already-created staged entries; it is not a general staging owner. | **Pending qualification**. `fclonefileat` does not return the destination handle/provenance required by the accepted macOS contract, so it cannot qualify identity-bound staged ownership by itself. | Before creation, require a registered destination root, expected absence, bounded live capacity, and source proof for clone/copy. After creation, capture the owned staged identity and retain unvalidated staging; `CopyValidated` is required later before publication. A failed or uncertain copy retains the source/stage/journal evidence; no cleanup is inferred. |
| `CopyIntoStaging` | Bytewise copy APIs can move content into a destination staging object, but the copy itself is not an atomic publication. The current source-local recovery path replays journaled staging; it does not provide a general copy owner. | **Pending qualification** for a live source/destination owner workflow; cross-volume copy remains **Unsupported** for whole-publication atomicity. | Requires independently bound source and destination roots, bounded content/identity validation, live capacity authorization, and retention on partial or uncertain copy. No source deletion or final publication follows automatically. |
| `WriteStagingChunk` | A file handle may expose bounded writes, but no current source-local finalizer provides the proposed chunk/issue/output binding or a general file-owner write grant. | **Pending qualification** and blocked for real owner effects by OPT-1857. | Before the first write, require an exact staged identity, bounded chunk/issue/output identity, and live effect grant. Persist the resulting staging checkpoint before dependent advancement; a capacity ticket or durable UUID alone is not filesystem authority. Denied or uncertain writes retain staging and claims. |
| `FlushStaging` / synchronization | macOS `fsync`/`F_FULLFSYNC` and Windows `FlushFileBuffers` are synchronization APIs. A successful call is evidence about that handle and request, not a universal power-loss guarantee for a volume, remote medium, or namespace. Current recovery finalization does not turn these calls into a complete owner qualification. | **Pending qualification**, per backend and filesystem. | The qualified backend chooses its required position in the 1629 partial order and records exact evidence. An unavailable, failed, or incomplete flush never makes staging publish-ready; retain the staged payload and claims. |
| `ValidateStaging` | Descriptor/handle-bound inspection can compare the staged identity, length, or hash. `RecoveryRoot` and `OpenedFile` provide this narrow source-local evidence. | **Pending qualification** because the live root, object identity, source fence, and owner grant must be checked immediately before the effect. | Validation must precede `CopyValidated` and the final claim. A mismatch, replacement, or unavailable root returns a retry/audit outcome and preserves evidence; an observation never mints an effect grant. Final-target `ReopenedVerified` and `VisibilityVerified` remain separate post-claim barriers. |
| `ReplaceExpectedTarget` | `FileRenameInfo` can expose a replacement field, but the current contract requires an exact expected-target proof and a specifically qualified replacement primitive. No generic overwrite is accepted. | **Unsupported** until a backend proves expected identity, collision handling, source ownership, and rollback/retention semantics. | Requires staging proof, exact expected target identity, live roots/fences, and independent replacement qualification. Any collision or ambiguity retains both the staging and target evidence; it cannot fall back to overwrite. |
| `RemoveOwnedObject` / cleanup | Windows `SetFileInformationByHandle(FileDispositionInfo)` operates on an opened handle and is used by source-local recovery. macOS pathname unlink leaves a check-to-use substitution window under the accepted decision. | Windows is a **primitive candidate**; macOS production cleanup is **Unsupported**. No platform is workflow-qualified by API availability alone. | Requires an exact live object identity, independently qualified disposition operation, owner grant, and coordinator checkpoint. Failed or uncertain disposition retains the object and journal; publication qualification never authorizes cleanup. |
| `RestoreOwnedObject` / rollback | A reverse rename, path copy, or retained pathname is not automatically a safe rollback. Existing source-local recovery has narrow retry behavior, not a general rollback owner. | **Pending qualification** as an independent effect. | Requires a retained object identity, expected target predicate, live ownership, and a qualified backend primitive. A failed or ambiguous rollback retains evidence; it is never an automatic inverse of a failed publish. |
| `MoveNoReplace` within one qualified volume | A no-replace rename may be a namespace primitive, but source and destination ownership and source reconciliation are separate barriers. Current `reconcile.rs` performs source-root preflight before its source-local finalization. | **Pending qualification** for an owner workflow; source-local recovery remains narrower. | Requires independently bound source and destination roots/fences, exact source identity, absent target, qualified no-replace operation, and source/database acknowledgement. A late source failure retains the final/staged evidence and journal. |
| Cross-volume copy/move | No rename operation makes a cross-volume transfer atomic. Copy followed by source deletion is a multi-step workflow with an ambiguous failure window. | **Unsupported** for automatic copy+delete and `WholePublicationAtomic`. | Stage on the final live volume, validate, persist `CopyValidated`, and use a separately qualified final claim if one exists. Never delete the source as a fallback; retain both sides and the operation until independently qualified cleanup and coordinator barriers complete. |
| `CreateDirectoryNoReplace` | Directory-relative no-replace creation may be exposed by a platform, but namespace identity and expected absence remain live proofs. No current source-local finalizer qualifies this general effect. | **Pending qualification**. | Requires a bound destination root, exact absence predicate, no-replace primitive, and owner grant. Existing entries, root replacement, or ambiguity return collision/retained evidence; no recursive or alternate-path fallback. |
| `InspectOwnedObject` | Descriptor/handle inspection is an observation operation. It may use the same root and object identity checks as recovery. | **Pending qualification** as an observation path; it does not establish a mutating primitive. | It accepts observation-only root/fence evidence and never requires or creates a write-capacity grant. Results are not publication, cleanup, rollback, or write authority. |

The matrix deliberately separates `AtomicNamespace` from
`WholePublicationAtomic`. A successful final namespace claim can establish the
former after reopened identity/content verification while the transfer remains
`WholePublicationNonAtomic`. `VisibilityVerified`, synchronization evidence,
and any power-loss claim are separate fields. A flush or rename success never
proves a stronger claim on an unqualified filesystem or medium.

| Claim axis | What it can prove | What it cannot prove without a separate qualified result |
| --- | --- | --- |
| `AtomicNamespace` | A qualified final no-replace claim installed the complete staged object at an absent final name, with matching live volume identity and reopened content/identity verification. | Whole-transfer atomicity, cleanup authority, visibility beyond the verified observation, or power-loss durability. |
| `WholePublicationAtomic` | Every applicable source-to-staging transfer and final claim used the same qualified atomic sequence. | A bytewise/cross-volume transfer, even when its final rename is atomic. |
| `VisibilityVerified` | The final object was reopened through the qualified target-root capability and the selected identity/content check passed. | Namespace atomicity, synchronization, or durable media behavior. |
| `PowerLossSynchronized` | The qualified platform-specific file and namespace synchronization sequence completed with its required evidence. | A universal guarantee for faulty, remote, removable, or otherwise unverified media. |
| Cleanup/disposition result | A separately qualified handle-bound cleanup operation completed for the exact retained object. | Publication, rollback, or source deletion permission from a publication result. |

## Platform evidence

### macOS and POSIX-like platforms

Apple's [rename(2) documentation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/rename.2)
and [stdio declarations](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/stdio.h)
expose directory-relative names and the `renameatx_np`/`RENAME_EXCL`
interface. The project decision records why this is still name-bound: the call
does not take the held staged-file identity as its source operand, so a
substituted entry could be published and a post-call check could not safely
undo that exposure. This is a project qualification conclusion from the API
shape and the threat boundary, not a claim that ordinary macOS file saves are
impossible.

Apple's [clonefile(2) documentation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2)
documents clone operations including the `fclonefileat` form. The accepted
Yield decision records that the call does not return an opened destination
handle or transaction provenance. Reopening by pathname therefore cannot prove
that the inspected object is the object created by this transaction. Cloning
may be a staging primitive candidate, but it is not publication or cleanup
qualification.

The accepted [macOS publication decision](GPUI_MACOS_PUBLICATION_DECISION.md)
keeps production staged publication and staged cleanup unsupported. A pathname
unlink after validation has the same substitution window, so cleanup is an
independent unsupported capability. The test finalizer is useful for exercising
retention and journal boundaries but cannot qualify production behavior.

### Windows

The current source-local recovery backend uses an opened staged handle and an
opened target-parent handle with
[`SetFileInformationByHandle`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle)
and [`FILE_RENAME_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info),
with `ReplaceIfExists = false`. It uses
[`FILE_DISPOSITION_INFO`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/ns-fileapi-file_disposition_info)
through the same handle-bound API for source-local staged cleanup. These are
implemented primitive candidates in `recovery_io.rs`, and the independent
`Available` values in `finalization.rs` describe that low-level availability.

The Microsoft [`FILE_RENAME_INFO` documentation](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info)
states that `RootDirectory` is normally null, but can be a directory handle
when `FileName` is relative. The current source's target-parent handle plus a
relative leaf therefore matches this narrow documented API shape. That
documentation does not prove Yield's staged-identity, volume, collision,
or owner-fence contract; Windows remains a primitive candidate pending
targeted workflow and filesystem qualification.

[`FlushFileBuffers`](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-flushfilebuffers)
flushes buffered information for the supplied file or device handle. It does
not, by itself, qualify Yield's whole publication, namespace visibility,
volume, or power-loss contract. The current Windows code and policy therefore
remain **pending workflow qualification**. Windows-specific CI runs #19–#21
did not start because of the account billing gate; there is no Windows
execution or workflow acceptance in this matrix.

### Other platforms and filesystems

The non-Windows production recovery finalizer returns `Unsupported` for
publication and cleanup and retains staged data and journal evidence. The
source-local test finalizer is not production evidence. Other platforms,
filesystems, remote/removable media, and volume combinations not named above
are explicitly unqualified; no blanket support is inferred from a similarly
named API.

## Required order and evidence

The owner contract's partial order is effect-specific and does not impose a
universal flush-before-validation sequence:

1. Commit durable intent and obtain any necessary live capacity authorization
   before mutating effects.
2. Stage or write under the live-bound roots and fences.
3. Complete staged identity/content validation and record a durable
   `CopyValidated` acknowledgement before the final claim.
4. Perform backend-required staging flush/sync prerequisites at their qualified
   positions before the final claim.
5. Make the final namespace claim with the qualified no-replace primitive.
6. Reopen and verify the target, then collect any required destination-directory,
   target-sync, visibility, and media evidence before returning a verified
   publication receipt.

An incomplete flush or validation never proves publication readiness. A
successful `CopyValidated` checkpoint is staging evidence, not publication
success. The source owner, journal owner, and coordinator retain their own
barriers; the file owner never performs a cross-owner blocking wait while
holding an owner command or SQLite transaction.

For every mutating row above, a future qualification must provide all of the
following before changing its status: an exact live root/object proof, matching
source and lifecycle fences, the relationship of source/staging/final volumes,
the backend primitive and collision result, the ordering and synchronization
evidence, and an OPT-1857 effect-scoped live grant. Missing or ambiguous proof
returns `Unsupported`, `RetryableBeforeEffect`, or `RetainedUncertain` as
appropriate and preserves staging, claims, and journal evidence. A source
snapshot, FullProgress observation, claim/issue UUID, decoded receipt, or
pathname alone never supplies that authority.

## Existing source-local evidence

The current evidence is intentionally narrow:

| Evidence | What it establishes | What it does not establish |
| --- | --- | --- |
| `sample_sources/finalization.rs` capability query | Independent platform-policy values for publication and cleanup; `Available` on Windows and `Unsupported` elsewhere in the current build. | No workflow, volume, ownership, durability, or general owner qualification. |
| `sample_sources/db/file_ops_journal/recovery_io.rs` | Descriptor-bound `RecoveryRoot`/`OpenedFile`; Windows opened-handle rename and disposition; non-Windows fail-closed production finalizer. | No general operation owner, cross-owner grant, whole-publication atomicity, or universal cleanup. |
| `sample_sources/db/file_ops_journal/reconcile.rs` | Source-local preflight and retention ordering for journal replay. | No coordinator-owned publication protocol or producer adapter. |
| Existing finalization and production-finalization tests | Retention across unsupported publication/cleanup, root loss/replacement, collisions, and stable diagnostic codes. | No successful macOS or Windows workflow acceptance and no power-loss certification. |

The existing tests include
`capability_query_reports_independent_operations`,
`unsupported_reasons_have_stable_diagnostic_codes`,
`unsupported_platform_finalization_retains_staged_file_and_journal_boundary`,
`windows_finalization_uses_opened_handle_and_retains_collision`, and the
`production_replay_retains_*` regressions in
`sample_sources/db/file_ops_journal/tests/production_finalization.rs`. They
were reviewed as source evidence for this document; this docs-only change does
not claim a new runtime test run.

## Follow-ups and limits

The accepted owner contract remains the caller and ownership boundary; this
matrix does not repeat its historical producer inventory. The implementation
sequence is bounded by these follow-ups:

| Follow-up | Boundary |
| --- | --- |
| [OPT-1630](https://linear.app/boostnlvp/issue/OPT-1630) | Queue, registration, and owner-local execution. |
| [OPT-1857](https://linear.app/boostnlvp/issue/OPT-1857) | Sealed live write authority from journal/capacity ownership to the file owner. This is required before real effect execution. |
| [OPT-1632](https://linear.app/boostnlvp/issue/OPT-1632) | Publication integration and coordinator barriers. |
| [OPT-1633](https://linear.app/boostnlvp/issue/OPT-1633) | Independently qualified cleanup and rollback. |
| [OPT-1634](https://linear.app/boostnlvp/issue/OPT-1634) | Owner-produced durable staging/checkpoint evidence. |

No family writer, cross-volume copy/delete fallback, backend implementation,
provider qualification, production admission, UI policy, cleanup deletion, or
Windows workflow rollout is added by OPT-1631. The matrix is a decision and
evidence boundary for those later tickets.

## Review record

- Decision artifact: OPT-1631, 2026-09-22.
- Primary API references retrieved 2026-09-22 from the Apple XNU and Microsoft
  documentation linked above. The Apple links use the XNU `main` source tree;
  Microsoft Learn exposes stable API identifiers rather than a repository
  commit in these pages, so the retrieval date is recorded here.
- Local validation for this docs-only change: link/anchor review, matrix
  completeness review, whitespace check, and scoped diff review. No runtime
  tests are claimed.
