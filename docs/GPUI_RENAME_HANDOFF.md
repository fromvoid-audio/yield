# One filesystem rename handoff (OPT-1998)

Baseline `d4885e067a2203cc0cbb35d9cc75709ac3f1309b`. The reference
`BeginRenameSelected` / `RenameContextFolder` routes require a physical
namespace change. Active GPUI `collection_rename.rs::commit_collection_rename`
only changes a collection label. `context_menu.rs::open_directory_context_menu`
captures a directory target but has no admitted filesystem rename producer.
That missing source-owned service remains OPT-1700 and GPUI action OPT-1581.

The first route is one same-parent regular-file rename; folder inventory,
cross-source move and application history are separate. The typed observation
is `NamespaceRenameIntentArgs`, with `NamespaceEntryEndpoint` for the exact
existing file, `NamespaceTarget` for the retained parent plus bounded new
leaf, and fixed no-replace collision policy. Observations carry SourceId,
source revision and opaque filesystem/root identity; they are not handles.

| Phase | Contract / required authority | Current gap or rejection |
| --- | --- | --- |
| Capture | Immutable source/parent identity, source lifecycle/lease epoch, database/root fence and revision, one validated leaf | No active GPUI producer; editor cancellation/focus change discards unissued target, not files |
| Validate target | Same parent/source participant, exact root/parent identity, no-op leaf and composed path bounds | Existing typed validation rejects wrong parent/root/source or same leaf; does not authorize I/O |
| Capacity/admission | Owner-derived descriptor-bound namespace, journal, database, WAL/SHM and recovery peaks; claims and typed prepared input before identity/row | `namespace_matrix(Rename)` is `Unknown(MetadataBoundNotDefined)` for all classes; prepared builder absent; no guessed zero-byte rename exemption |
| Dispatch | Accepted OperationId/participant/generation and exact command fingerprint; source/root registrations match retained authorities; owner-issued sealed grant | `FileOperationCommand::for_live_chunk` is a boundary, not an existing rename producer; never synthesize a grant from observations |
| Effect | `MoveNoReplace { source, destination, source_object, expected_absent }`, validated source object and absent target | Worker returns `Unsupported`; native primitive/bound qualification OPT-2011 remains open |
| Commit | Verified effect result/checkpoint followed by source-owner delta/revision publication; only current target response closes editor | No production rename completion route; name-only observed target cannot become success |

## Results and retry identity

Invalid/no-op/collision/stale/unsupported/unavailable/capacity rejection before
intent has no OperationId, journal row, filesystem or source DB effect.
Unknown capacity remains a typed unsupported/unresolved result. Editor text
can remain for correction; a stale captured target must be recaptured rather
than silently renamed under a newer source/selection.

After durable admission, cancellation/retry preserves OperationId,
participants, lifecycle/generation, old/new identity expectations and exact
fingerprint. An acknowledgement loss cannot allocate a new rename. Reused ID
with changed target is a conflict. Already-started/ambiguous effects retain
evidence and capacity until independently proven resolution; no pathname
rollback, eager old/new row update or claim release is allowed. Completion
requires owner-issued effect proof and committed source revision, not an
optimistic label update.

Existing portable fixtures validate exact same-parent/root/no-op relationships
(`rename_requires_exact_parent_and_root_identity_and_rejects_same_leaf`),
source aliases/descendants and bounded leaf/path forms. File-owner fixtures
cover owner/capability fences and every typed effect returning Unsupported
without a backend. They do not execute a real rename or qualify its metadata
capacity. No duplicate fixture claiming fake physical success is added here.

OPT-2011 owns the exact primitive and bound qualification. Under the accepted
namespace-substitution boundary, an opened source followed by a name-based
`renameatx_np` call does not ensure the same source object is moved. Keep
Unsupported until a qualified binding/proof exists; do not substitute ordinary
pathname rename. OPT-1701/1707 may reuse a qualified MoveNoReplace boundary
only after their own destination/history/capacity routes are implemented.

This completes the bounded source handoff contract. Original OPT-1700 and
OPT-1581 remain open. Native GUI/key/focus evidence belongs to OPT-1953 after
a real producer exists; collection-label tests cannot close filesystem rename.
