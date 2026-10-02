# Durability evidence vocabulary and qualification matrix (OPT-1843)

Decision scope, 2 October 2026. Source baseline: `origin/main` `18db2b075`.
This defines the evidence records required by the
[I/O target](IO_ARCHITECTURE_TARGET.md#publication-durability-contract) and
[primitive matrix](FILE_OPERATION_PRIMITIVE_MATRIX.md). It does not qualify any
platform, implement a filesystem effect, or change the existing journal codec,
publication validator, owner grants or production capability policy.

The typed schema is `yield_library::durability_matrix`. `QualificationRow`
records one platform, destination medium class, source/staging/final relationship,
actual transfer, provenance, and an opaque scope-report reference. That report
must identify the exact source/build, OS, filesystem, device/volume or remote
protocol/server, API sequence, fixtures and logs. An evidence reference is a
bounded report identifier, never a pathname, live capability or proof of
qualification by itself. There are thirteen independent evidence categories;
duplicate categories, oversized/control-containing references, missing scope and
best-effort identity/ownership observations are rejected. Absent categories are
explicitly `Missing`; `Unsupported`, `Failed` and `BestEffort` remain distinct.

## Four separate claims

| Vocabulary | Evidence required | Explicit weaker outcome |
| --- | --- | --- |
| `VisibilityVerified` | Bound root/object/transaction provenance, current owner fences, final reopened and identity/content verified. | `VisibilityUnverified`; viewing a final name alone proves nothing. |
| `AtomicNamespace` | Visibility plus durable `CopyValidated`, proven staging/final co-location and qualified object-bound atomic no-replace final claim with no collision/replacement. | `NonAtomicNamespace` observation remains nonterminal; `PublicationBlocked`, even if visibility was observed. Re-stage beside final or fail closed when co-location is unknown. |
| `WholePublicationAtomic` | Atomic namespace, source=staging=final, actual qualified atomic transfer, and evidence for every applicable step of the complete atomic sequence. | `WholePublicationNonAtomic` for bytewise, cross-volume or unqualified transfer, even on one volume with a successful atomic final claim. |
| `PowerLossSynchronized` | Publication evidence, independently qualified cleanup/rollback, WAL/SHM/restart and journal crash replay, qualified file and destination namespace sync in the required order, plus exact medium power-loss evidence. | `BestEffortSync` if some file/namespace synchronization observations exist; otherwise `SyncUnsupportedOrUnverified`. No universal media guarantee. |

The schema's visibility and atomicity booleans describe the shape of these
observations. Its synchronization enum uses the journal's existing names.
`unqualified` preserves every category lacking verified evidence, including
optional stronger whole-atomic/media claims. Shape assessment is deliberately
conservative about a complete operation-level power-loss claim. It does not
remove an existing runtime restriction: for example, the current waveform
restore validator rejects `PowerLossSynchronized` and permits only its accepted
non-atomic staging mode. No assessment result is convertible to a journal
receipt or grant, and no runtime caller consumes this schema in this change.

## Combination matrix

Every row below requires separate exact-scope observations; no evidence is
copied from one OS, volume, protocol, device or build to another.

| Platform | Local fixed | Removable | Remote |
| --- | --- | --- | --- |
| macOS | Publication/cleanup remain unsupported under the accepted macOS ownership decision. `F_FULLFSYNC`/`fsync` alone do not qualify a final claim or cleanup. | Additional exact-media sync, disconnect/reconnect, identity and recovery evidence required; unqualified. | Exact protocol/server namespace, sync, disconnect and recovery behavior required; unqualified. |
| Windows | Opened-handle no-replace/disposition API candidates exist; workflow/volume qualification remains pending. `FlushFileBuffers` alone is insufficient. | Additional device/removal/identity/recovery evidence required; unqualified. | Exact protocol/server namespace, sync and recovery evidence required; unqualified. |
| Linux | Non-Windows production staged finalization remains unsupported; POSIX availability does not qualify object-bound publication or cleanup. | Additional media/removal evidence required; unqualified. | Exact protocol/server behavior required; unqualified. |

`AllOneVolume` permits consideration of whole-publication atomicity only with
qualified actual transfer/sequence evidence. `DestinationLocalStaging` permits
consideration of atomic final-name publication, but never whole-publication
atomicity. `Unverified` co-location blocks final-publication assessment.

macOS requires evidence for the selected `F_FULLFSYNC`/fallback `fsync` sequence
and destination metadata sync. Windows requires the selected handle flush,
write-through final claim and destination metadata sequence. Linux requires its
own separately qualified sequence. Removable/remote media need their own
qualification even when a sync call reports success. Report unavailable or
rejected sync as unsupported/failed or best-effort, with its diagnostic; never
upgrade from an API name or successful flush acknowledgement.

## Independent recovery barriers and dispositions

| Evidence category | Required report content |
| --- | --- |
| `IndependentCleanup` | Identity-bound, transaction-owned cleanup qualified independently of publication; absence verification. No pathname unlink fallback. |
| `Rollback` | Effect-specific rollback or safe retention route; owned evidence, absence where removal occurs, durable capacity release and uncertainty preserved on failure. No inferred copy/delete rollback. |
| `WalRecovery` | Owner-held SQLite transactions, WAL/SHM retention, external readers, bounded busy/backoff, checkpoint and restart tests. Do not force reader cleanup. |
| `CrashRecovery` | Durable intent/checkpoint ordering, duplicate replay, participant failures and retained ambiguous objects. No whole-operation atomicity inferred from individual SQLite commits. |

`PublicationBlocked` means required publication evidence is missing, failed,
unsupported or co-location is unknown. It permits no success assertion. Before
an effect, retain/retry/reject according to the existing owner outcome; after a
possible effect, retain objects and journal evidence for audit rather than
pretend nothing happened. This schema does not select runtime retry/cleanup.

`RecoveryUnqualified` preserves any known visibility/namespace observations but
blocks completion of the qualification record until independent cleanup,
rollback, WAL and crash recovery observations are verified. It cannot authorize
cleanup or cross-source origin retirement.

With those barriers present, synchronization may remain explicitly downgraded.
The accepted target permits visibility-qualified degraded publication to
succeed only after all actual required participants reconcile, with persistent
`PowerLossSynchronizationUnverified` diagnostics. A schema-complete row does
not perform or prove that reconciliation.

`FixtureComplete` applies only to synthetic rows. `ReadyForEvidenceReview`
applies to observed rows with the necessary categories; it still requires
independent review of authenticity, scope and exact sequences. Neither value
means production-qualified. JSON decode and schema checks cannot manufacture
ownership or stronger durability guarantees.

## Verification and remaining owners

Portable Rust tests cover all nine OS/medium combinations using **synthetic**
rows, empty/unqualified rows, failed/unsupported publication barriers,
bytewise/cross-volume downgrade, visibility without no-replace qualification,
independent cleanup/rollback/WAL/replay failures, sync/media downgrade,
provenance, strict decode and bounded/duplicate evidence rejection:

```sh
cargo test --locked -p yield-library --lib durability_matrix
```

These are executable schema fixtures, not fault injection or power-loss tests.
Existing artifact keys and journal participant descriptors/checkpoints remain
untrusted stored observations; this schema supplies no parallel transport or
participant persistence. Physical operation validation and failure-family
certification remain with their existing owners.

Native evidence remains scoped to [OPT-1844](https://linear.app/boostnlvp/issue/OPT-1844)
(macOS), [OPT-1845](https://linear.app/boostnlvp/issue/OPT-1845) (Windows),
[OPT-1847](https://linear.app/boostnlvp/issue/OPT-1847) (removable), and
[OPT-1850](https://linear.app/boostnlvp/issue/OPT-1850) (remote).
[OPT-1851](https://linear.app/boostnlvp/issue/OPT-1851) owns contention/readers/WAL
qualification; [OPT-1854](https://linear.app/boostnlvp/issue/OPT-1854) owns later
runtime gating by actual qualified combinations. The tracking parent
[OPT-1360](https://linear.app/boostnlvp/issue/OPT-1360) stays open. Unsupported
production publication/cleanup and absent evidence remain real implementation
or qualification blockers, not tests waived by completing this decision.
