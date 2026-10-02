# Retained-folder producer and identity contract (OPT-1999)

Baseline `8d2c42024c7c1c801d6b6661927bac4a5241b2e6`. There is no active GPUI
retained-folder stage/restore/purge service caller. Context-menu target capture
exists, but reference delete-confirmation messages do not have a durable GPUI
producer. Original OPT-1649 owns universal identity/claim/disposition wiring;
OPT-1650/1651/1652 own the individual physical adapters.

| Request / existing model | Identity and authority required | Current boundary / remaining owner |
| --- | --- | --- |
| Stage: `RemovalCommandArgs::StageRetainedFolder(StageRetainedFolderIntentArgs)` | Non-root child directory, exact source/root/database/revision/lease lifecycle; required filesystem participant | Model validates directory-only source and fixed no-replace destination policy. It does not choose recovery path, allocate OperationId or retain capability. Producer OPT-1649, effect OPT-1650 |
| Restore: `RetainedResolutionCommandArgs::Restore(RetainedRestoreIntentArgs)` | Exact retained selection/fingerprint, eligible Deleted stage, source participant and physical staged-directory identity | Fixed PreserveConflictsKeepBoth policy, not overwrite. Producer/replay OPT-1649; merge/restage effect OPT-1651 |
| Purge: `RetainedResolutionCommandArgs::Purge(RetainedPurgeIntentArgs)` | Same exact selected-entry observation plus UserConfirmed decision; independent cleanup authority | Unconfirmed request rejects before admission. Producer OPT-1649; physical cleanup OPT-1652 / OPT-2002 |

## Universal identity and captured lifecycle

One stage operation receives its OperationId only after accepted prepared
admission. That identity is the durable correlation key for its recovery
record; StageRetainedFolderIntentArgs deliberately creates no second delete
identity. A retained observation includes `RetainedDeleteId`, original/staged
relative paths, staged `NamespaceEntryEndpoint`, resolution stage/root kind,
and domain-separated `RetainedEntryFingerprint` over bounded captured entry
bytes. A legacy delete ID can be reused and is **not** a unique generation
identity. The fingerprint detects changed bytes; it does not read the current
record or grant ownership.

A future source-owned producer must reread/compare the selected retained
entry, source revision, root/database identities and registered lease lifecycle
before preparing admission. A changed stage/fingerprint or retired source
requires explicit stale/blocked outcome, not picking the newest entry by ID.
Retry of one admitted stage/restore/purge reuses its OperationId, participants,
generation and exact immutable arguments. An explicit new restore or purge is
a new command against a captured retained record, not a second stage identity.
Reusing an admitted ID with different bytes is conflict; losing a UI reply
cannot allocate a replacement operation.

## Capacity, cancellation and status

The `removal_matrix` and `retained_resolution_matrix` currently report
`MetadataBoundNotDefined` for every class. A full plan must include coexisting
namespace, journal, source/global database, WAL/SHM and retained recovery
peaks under descriptor-derived volume authority. Unknown is unsupported;
do not treat a directory move as zero-byte or refund an existing retained
payload merely because logical content was not copied.

Before intent: invalid/stale/unconfirmed/unsupported/capacity/queue rejection
has no OperationId, durable row, moved folder, DB update or cleanup. After
intent: durable cancellation preserves identity and evidence until independently
qualified rollback/absence/adoption proves the disposition. Charged capacity
and source projection cannot be dropped on a timeout or a pathname observation.

Typed producer results must distinguish RejectedBeforeIntent, admitted/running,
blocked retained, partial/uncertain, cancelled with proven disposition and
completed committed revision. A visible folder can disappear only after the
verified physical and source/global checkpoint boundary; one successful
model/codec write or an optimistic GUI response is not completion.

## Portable evidence and physical gaps

Existing removal fixtures reject regular files, malformed participant sets
and source/revision mismatch. Retained-resolution fixtures validate eligible
stage, exact source and fingerprint, bounded bytes, explicit purge confirmation
and fixed restore conflict policy. Fingerprint/replay tests detect reused-ID
byte changes. Existing owner V10/V11 persistence fixtures prove encoded
observation storage through the queue; they do not assign live folder authority
or execute a physical operation.

OPT-2011's first MoveNoReplace qualification, OPT-2002's independent cleanup
proof contract and the physical adapters above retain the real backend gaps.
A file rename qualification cannot by itself certify folder subtree staging,
restore merging or cleanup. Each adapter needs its own retained-directory
proof/peak before producer enablement. Source/global commit ordering remains
OPT-1641; projection publication remains OPT-1653.

This completes the bounded caller/contract inventory and portable identity
evidence. Original OPT-1649 and physical adapter issues remain open. No direct
mutation, native GUI acceptance, path-based cleanup or successful retained
folder operation is claimed.
