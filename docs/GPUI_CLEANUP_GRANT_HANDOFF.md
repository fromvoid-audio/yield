# Independent cleanup and rollback handoff (OPT-2002)

Baseline `95f463fb0c3e4332f90584d42e2bca287a1d86bb`. Cleanup is independent
of publication. A CopyValidated row, publication grant, matching digest,
UUID filename or source-live permission does not authorize deletion, rollback,
adoption or release of charged capacity.

## Actual caller and interface inventory

| Current boundary | Existing evidence | Missing production authority |
| --- | --- | --- |
| GPUI/source family | No admitted ordinary physical operation producer; retained/cancel UI routes remain open | No family cleanup-grant request or durable result consumer |
| Journal cancellation/recovery | Work-progress cancellation barrier, namespace/resource cleanup manifests and observation records; exact generation/claim/lineage validation | These are models/observations. No sealed live cleanup grant is issued into FileOperationOwner from them |
| `EffectRequest::RemoveOwnedObject` | Exact `OwnedRoot`, expected `OwnedObjectIdentity`, `CleanupProof` request | `CleanupProof` is an opaque marker shape, with no production proof constructor. File worker returns Unsupported |
| `EffectRequest::RestoreOwnedObject` | Expected identity and separate `RestoreProof` | No qualified restore/grant producer; a cleanup proof cannot authorize restore/replace |
| Legacy `ProductionStagedFinalizer::cleanup` | Held opened staging object and independent platform capability query; Windows uses `delete_by_handle` | Mac/Linux preserve unsupported staged data/journal. Windows candidate is not the universal owner/operation grant route |
| Test staged finalizer | Filesystem fixtures for replay/failure | Its path-based remove helper is test-only and cannot be moved into a production fallback |

## Required private grant contract (not implemented)

The coordinator chooses one immutable cleanup obligation from a durable
operation/cancellation generation. Before issuing a grant, it verifies the
closed entry/barrier, terminal or resumable operation disposition, exact
participant/resource/claim lineage, retained source lifecycle and object
custody. The grant must bind owner/registration, OperationId, participant,
generation, effect identity, expected held object, cleanup obligation and
retained charged allocation. It is consumed once by a private file-owner
envelope; stored observations and generic decoded requests cannot mint it.

The worker retains the owned object and necessary parent/root capabilities,
rechecks operation/capacity/source lifetime and grant/request fingerprint at
the entry boundary, and invokes only an independently qualified cleanup or
restore primitive. Confirmed queue rejection returns the unconsumed grant
to its original issuer. Exact duplicate retrieves its existing receipt;
same identity with altered object/obligation conflicts. A lost receipt or
restart enters durable custody reconciliation and cannot create another
deletion grant from bytes alone.

One result distinguishes proven no-effect, proven object cleanup/absence,
proven adoption by a specific surviving owner, blocked/unsupported, and
retained uncertainty after an attempted effect. Adoption must name the exact
object/claim and new durable custodian, not an object with equal content.
Absence must establish resolution of the granted owned object/obligation,
not merely that one pathname currently returns ENOENT. A replaced/missing
namespace entry cannot establish that the original object was cleaned.

Do not check identity then unlink a pathname, delete a guessed recovery
locator, infer cleanup from successful publication, or perform a compensating
pathname rename. Before any release, persist the independently verified
cleanup/absence/adoption receipt and required directory/object synchronization
checkpoint. Only that receipt transfers/releases the exact allocation class
once. Unsupported, stale, mismatched and ambiguous outcomes retain staged
data, source metadata, journal/evidence and charged capacity. Recovery-only
reserve use must preserve protected floor and reconstitution evidence;
ordinary work cannot borrow an unproven released claim.

## Portable fixture/proof map

Production-finalizer regressions exercise repeated unsupported publication
and cleanup, missing/replaced source/target roots and late unrelated collision.
The cleanup regression preserves both paths and source metadata across retries.
It proves fail-closed retention on Linux, not a qualified cleanup primitive.

Cancellation cleanup work-progress fixtures cover current generation/owner
revision, stale authority rejection, class transfer/exact claim proof, replay,
source reacquisition and terminal qualification. Snapshot/codec tests retain
cleanup lineage but do not rehydrate live authority. The file-owner declared
effect/fence fixtures preserve Unsupported and exact duplicate identities.

The missing envelope needs new exact-grant/object/obligation mismatch,
cancel/retire at effect entry, lost-result custody, duplicate/restart,
independent absence/adoption and exact-once charge-release fixtures. They must
not assert success using synthetic pathname cleanup.

Original OPT-1633 owns the physical cleanup adapter and producer boundary;
OPT-1819 owns capacity release after verified cleanup. Mac/Windows durability
qualification remains OPT-1844/1845 and accepted macOS Unsupported policy
OPT-1868. This completes the source handoff/proof-gap contract, not a native
cleanup adapter, retained-folder operation or release acceptance.
