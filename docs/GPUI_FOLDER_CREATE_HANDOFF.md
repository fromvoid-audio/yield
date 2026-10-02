# One-folder create handoff contract (OPT-1997)

Source baseline `ce2a3e789c660037d7a933f9bba39f2124ee2bd1`.
The active GPUI has no physical folder-create producer. Intended reference
triggers are `BeginCreateSubfolder` and `CreateFolderAtContextTarget`; the
active source target boundary is `browser/context_menu.rs::open_directory_context_menu`.
There is no create editor/service call behind that boundary yet. This contract
specifies that missing route rather than treating an encoded fixture as a
caller. Original implementation owner is OPT-1692 / GPUI action OPT-1580.

| Boundary | Required input/output | Current source / fail-closed result |
| --- | --- | --- |
| Capture one target | Stable SourceId, registered lifecycle/lease epoch, root/database identity, source revision, observed parent identity, one bounded leaf | `NamespaceDirectoryEndpoint`, `NamespaceDirectoryExpectation`, `NamespaceLeafName`; these are observations, not capabilities |
| Source-owned service request | Capture target before entering editor; submit immutable target plus accepted name; recheck lifecycle/revision before admission | Proposed catalog/source-owner route, not an existing method; no GPUI SQLite/open-directory work |
| Capacity preparation | Descriptor-bound namespace allocation plus all coexisting journal/source/global/WAL/SHM/recovery classes; positive checked rounded bounds or explicit unsupported | `estimate_capacity(Namespace::CreateFolder)` returns `Unknown(MetadataBoundNotDefined)` in all nine classes; `require_known_bytes` rejects every class |
| Prepared admission | Exact participants, command, full plan and claims; owner refresh/recovery audit before operation identity or durable row | Crate-private `PreparedAdmissionInput` has no production builder; common `admit_prepared` must be reached through an owner service, not exposed raw to GUI |
| Directory effect | Retained destination/parent capability and explicit absent leaf; sealed operation-bound dispatch after durable admission | `EffectRequest::CreateDirectoryNoReplace` exists; file worker returns `Unsupported`. `CreateStaging` is a different file-staging command |
| Completion | Verified owned directory identity/provenance, durable effect/checkpoint and committed source delta; stale UI response cannot update a replacement source | No production completion route or qualified primitive; native/physical qualification OPT-2010 must supply this proof |

## Result and retry rules

Before durable intent, invalid names/targets, stale lifecycle/root/revision,
unresolved bounds, pressure, unavailable owner or rejected queue return a
typed rejection with **no OperationId, durable row, directory or source DB
change**. A request correlation ID is only UI correlation and never becomes
operation authority. Cancelling the editor or a queued pre-intent request
releases only unissued preparation; it cannot delete a directory by name.

Unknown metadata classes remain unsupported, never zero, guessed filesystem
block size, or a positive file-staging bound. The portable regression
`folder_create_cannot_substitute_zero_or_known_staging_for_metadata_bounds`
constructs a real typed folder request and checks rejection through every
`require_known_bytes` class. It does not exercise a production create service
or prove a no-ID/no-row store transaction, because neither producer exists.

After `IntentDurable`, retry/cancellation keeps the original OperationId,
participants, generation, target identity and request fingerprint. Do not
allocate a second operation because an acknowledgement was lost. Already
present/collision, stale capability and unsupported backend are explicit
outcomes. Ambiguous creation retains charged evidence and a recoverable
blocked/uncertain disposition; it cannot report completion or perform
pathname cleanup. Successful creation requires verified owner-issued outcome
and committed source revision before inserting a visible folder row.

## Remaining qualification and implementation

OPT-2010 owns the missing descriptor-bound metadata peak and created-directory
identity/provenance qualification. A successful `mkdirat` followed by reopening
a name does not automatically prove that the reopened directory is the one
created under the accepted namespace-substitution boundary. Qualification
must establish that ownership or retain `Unsupported`.

After that decision, OPT-1692 implements one source-owned service producer and
the prepared plan/claim builder; OPT-1580 adds one GPUI editor/action using its
typed result. This source contract does **not** close either original or
pretend a reject-only route implements folder creation. Native GUI/input
verification remains in OPT-1953 after a production route exists.

Validation: targeted namespace/estimator/planner fixtures, source owner guards,
active app compilation, formatting and diff checks. No directory was created
by a production effect; native allocation or filesystem success is unverified.
