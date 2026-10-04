# Active source references and registry lifecycle (OPT-2175)

Decision researched against `origin/main` c186f10b and refreshed through
25782e500 (global maintenance/preparation), 2 October 2026. This specifies the missing
production bridge under OPT-1828/1357/1670. It does not implement that bridge or
qualify physical capacity. The existing configured-registry routes remain valid.

## Current producers and custody

| Existing boundary | Actual meaning |
| --- | --- |
| `yield-session/src/lib.rs`, `SourceRegistryService` / persistence global writer; browser `source_registry.rs` registry events | Configured `SampleSource` descriptors, persisted snapshots and UI request/revision acknowledgement. `RequestApplied` removal queues catalog deactivation; it is not source-close acknowledgement. |
| `yield-ui/src/browser/catalog.rs`, `CatalogCommandIntent::Register`, `RegisterVerified`, `Remove`; session `catalog::submit` | Existing registered producer and bounded session-to-catalog consumer. Source/root/lifecycle fences and queued projection removal remain here. |
| `yield-catalog/src/lib.rs`, `register_source`, `RegisteredSource` / `CatalogSourceDatabase` | Sole retained source writer, publication fence, watcher, source authority guard and journal registration receipt. The non-Send source guard stays on this worker. |
| `SourceDatabase::prepare_source_authority`, `SourceAuthorityRegistration::journal_request`, journal `register_source_authority` | Actual private root/DB/lease registration and confirmation. Partial registration and missing capacity remain unavailable; a catalog source can be attached without a validated capacity handoff. |
| `retire_source`, `begin_source_drain`, `source_authority_close_ready`, `close_database`, `PendingSourceFinalization` | Drain admission, retained snapshot exclusion, sealed acknowledgement only after source lease release, and exact finalization retry custody. |
| Global `registry_participant.rs` | Immutable configured replacement receipt, not a live source relation. Operation ID is unique in that ledger, so it cannot encode three lifecycle stages under one operation. |

Current journal `source_registrations` and `retired_source_registrations` are
owner-process maps, cleared when the owner stops. Reserve bindings and source
lease release have durable effects; the maps and opaque registration/retirement
receipts do not themselves supply a durable restart registration ledger. Do not
infer such a ledger from comments describing a drain or from configured metadata.

There are three distinct values:

1. A configured descriptor: desired registry content, including a path and role.
2. A live active reference: private current registered source custody validated at
   both producer and consuming boundaries. No deserializer or UI path can create it.
3. A durable historical observation: exact identity, metadata transition and
   receipt persisted for restart/replay. It can explain an old acquisition but
   cannot register, pin, reopen, release or retarget a source.

## Chosen bridge

Use the existing catalog worker as the lifecycle producer and existing global
writer as the durable metadata consumer. Install a restricted clone of the
existing global client into catalog through `Persistence::start_catalog`; keep
its existing 32-entry FIFO, admission gate, dequeue metrics, generation handling,
preparation reservation and profile validation. Do not create another worker or
registry coordinator. Browser/session commands never carry source authority.

The merged `GlobalLibraryWriter::submit_preparation` currently retains an empty
transaction and offers only abort/observation. Ordinary commands remain queued
while held. A transition with a latched journal decision must use the future
admitted prepared-commit integration, matching its original command/registration/
physical DB/nonce/claim inventory. It cannot submit ordinary SQL behind its own
held reservation or reinterpret pre-intent abort/deadline as post-intent release.
OPT-2136 and required-commit permissions must implement this before2195 effects.

Add library-owned opaque candidate/validation APIs beside `source_authority.rs`.
OPT-2193 now implements that narrow authenticated handoff API and original-writer/
journal-registration custody; see [its implementation boundary](SOURCE_REGISTRY_METADATA_HANDOFF.md).
The admitted global SQL consumer and catalog lifecycle routing remain OPT-2195/2197.
Catalog requests a candidate from its retained writer and registration; library
code must match the exact registered receipt, registration state and retained
source subject. Journal validates that registration is complete, rather than
accepting a `SourceAuthorityRecoveryRequired` partial receipt as eligible.
`SourceAuthorityGuard::pin_for_effect` and `SourceEffectPin` show the existing
private liveness/effect-gate mechanism: any reused implementation must retain its
pointer-identity checks, current owner/lease validation, and drain exclusion.
The non-Send guard never crosses the queue. The new sendable envelope is private,
non-serializable and bounded; its serialized portion is observation only.

For a new enrollment mutation, the global worker must establish the matching
registered-source permit before SQL, retain the effect-gate permit through SQL commit/rollback
and quiescence, and revalidate current profile/source at the existing
reply ownership boundary. The permit authorizes this metadata relation only.
It is neither a journal-issued one-shot file effect grant nor physical capacity
qualification. Drain racing queue admission makes the queued enrollment reject;
no source revival follows from a retained candidate or receipt.

The captured subject contains source ID, catalog lifecycle generation, actual
registration ID, historical profile acquisition/source lease owner and epoch,
media-root identity, database-root identity, exact database-file identity and
root binding. A captured scan revision is an observation, not an ongoing equality
requirement for membership after subsequent legitimate scans. Audit generation
and completed revision, when present, are explicitly optional observations;
pre-audit registration must not invent an authoritative revision. Every later
file/read/write consumer still obtains its own current source fence.

## New global schema and command contract

Existing `known_sources_v1`, `sources` and registry-participant receipts do not
suffice. Add versioned `source_lifecycle_references` and
`source_lifecycle_reference_receipts` through the owned global migration path.
Do not overload a path, primary-role field, Harvest reference or registry receipt.

A reference row has immutable captured subject, registration ID primary key,
current finite state `Enrolled`, `Draining`, or `Retired`, last transition key and
row-content identity. At most one non-retired registration per source ID is
allowed. A new generation must wait until the previous relation is settled;
retarget retires the exact old tuple and separately enrolls the new tuple.
No relation is physically usable merely because its stored state is Enrolled.

Use one bounded enrollment operation, then a separate bounded retirement
operation when removal/retarget is requested. Retirement freezes distinct
GlobalLibrary work keys for Draining and Retired, plus its source-release and
journal-settlement steps. Do not retain an unfinished enrollment intent or its
capacity claims for the entire source lifetime. Each command changes one registration. Its canonical versioned payload
binds operation/work key, transition, complete captured subject and exact
expected previous row/state (or absence), and resulting state. Use a domain
separated BLAKE3 payload identity; current global profile acquisition is a live
execution fence outside that immutable semantic payload. Receipts key the
operation plus full frozen work key, not only OperationId. Their fields are the
operation/work key, canonical payload identity, immutable historical committing
profile acquisition, sequence, captured registration identity, before/after row
identities and finite disposition: `Enrolled`, `Draining`, `Retired`, or
`ExpectedRelationChanged`. Unsupported codec, stale authority, malformed storage,
NoCapacity and uncertainty are errors, never successful transition dispositions.

In one global SQL transaction, bound/decode existing evidence, check immutable
key/payload, check expected relation, mutate only the named row, insert the exact
receipt and read it back before commit. Expected mismatch records a permanent
rejection receipt without mutating the relation. SQL abort rolls back both.
Response loss retains the same key/payload; it cannot imply a second effect.
Changed payload/work under a reused identity conflicts. Duplicate receipt reads
precede retention-cap rejection and never overwrite a later state or registry edit.

Provide a separate current-global-owner historical receipt/readback command:
readback after reacquisition needs no fabricated old live envelope and grants
no source authority. An old queued mutation rejects even if a receipt exists.
New source acquisition uses a new genuine registration/epoch and new operation;
matching paths or source ID do not rebind an old source tuple. No historical
receipt changes meaning when a new current global owner reads it.

## Ordering and retained failures

Enrollment follows actual source open/root validation, completed journal
registration, then admitted global-reference transaction. Publish active-reference
eligibility only after the exact global receipt and current registered subject
are confirmed. Configured descriptor persistence or a catalog projection may
precede this; those observations must not claim durable live enrollment.

Retirement extends the existing catalog state and `PendingSourceFinalization`:

1. Close catalog admission/cancel queued work for the exact lifecycle; stop watcher
   publication and retain all unresolved registration/result custody.
2. Commit global `Draining` for the exact enrolled tuple. Ordinary descriptor
   removal may already be persisted; it supplies neither drain nor close proof.
3. Obtain journal `begin_source_drain` acknowledgement and wait for retained
   snapshots/effect permits. Reject new live-reference consumers while draining.
4. Execute admitted source release on the retained prepared main transaction,
   then consume the actual database with its private release witness only when
   close-ready. Emit the sealed source retirement acknowledgement after actual
   release commit AND connection close. Without it, retain unresolved ownership;
   no Retired transition.
5. Store that acknowledgement and frozen global context in the existing pending
   capsule BEFORE queueing global `Retired`. Retain no database, root descriptor
   or live pin in this post-close capsule. Commit Retired only after matching the
   sealed acknowledgement to the exact historical registration/lease tuple.
6. Finalize journal retirement with the original receipt and sealed acknowledgement;
   remove the capsule only when both exact global and journal results are confirmed.

Each capsule holds at most one global and one journal result handle, polls an
existing pending result before resubmitting, and retains immutable inputs on
queue-full, rejection, timeout or disconnection. Before close, retain the full
RegisteredSource; after close, retain only the sealed capsule. Unknown close
result cannot become success or an invented acknowledgement. An old generation's
capsule blocks same-ID reacquisition rather than being overwritten. A configured-only source with no admitted/pending enrollment and no relation
keeps its existing ordinary detach route; absence cannot settle an uncertain
enrollment. Resolve its frozen command/receipt first. Cancellation
before effects can abandon a proposal; after committed effects it means drain
and exact settlement, not evidence deletion or rollback of an unrelated edit.

Session shutdown keeps maintenance -> catalog -> global -> journal -> final
profile guard order. Catalog must settle or report retained unresolved capsules
while global/journal clients are still available. The present catalog shutdown
return shape does not prove all such settlement: its production leaf must return
an honest bounded report and keep authority/custody until worker termination.
Existing unconfirmed drains remain unconfirmed. UI projection teardown does not
acknowledge source retirement or source volume lock release.

The required source-release primitive is missing and tracked in
[OPT-2202](https://linear.app/boostnlvp/issue/OPT-2202). Its selected protocol
executes the exact owner/epoch lease-row release on the already-reserved main
connection, using its declared LeaseMaintenance claim and required permit. A
definite commit emits a private owner-thread witness bound to operation/work,
subject and original lifecycle state. A library-only close API consumes that
witness after snapshot/effect quiescence, closes the real connection/resources
without a second control release transaction, retaining the sidecar/lease custody
until confirmed close, then emits the existing sealed
acknowledgement. A failed/uncertain commit or close emits none and retains claims
and recovery custody. The witness cannot be serialized or minted from readback
scalars; restart uses the distinct journal recovery observer below. The current
ordinary `close_database` path remains for unprepared close. Calling its separate
control write behind a prepared main lock, or aborting a latched reservation to
avoid that lock, is expressly prohibited.

## Durable recovery and admission prerequisites

The in-process bridge alone is insufficient for restart. Add an admitted durable
source-registry lifecycle intent/checkpoint family in the existing journal:
frozen subject, work keys, exact DB claims and observed global transitions,
source release and settlement. Commit the intent before its reference effects.
Missing GlobalLibrary prepared command support (OPT-2136), exact per-DB claims
(OPT-2166), journal issuance/settlement (OPT-2168), source/global required commits
(OPT-2169/2170), required permits/binding/settlement and qualified finite recovery
budgets remain actual gates. Ordinary configured edits retain their existing
route and do not impersonate this family. Unknown allocation is never zero;
logical page bytes or contract fixtures do not supply physical qualification.

Recovery treats persisted source tuples as historical and obtains new current
owners normally. It may read a recorded global receipt without a live old source,
but cannot recreate an old SourceEffectPin, registration receipt or close ack.
A source release committed without a durable checkpoint is AuditPending until
an actual source-owner recovery observer proves the exact release tuple and a
journal-owned recovery transition records it. A release row alone does not mint
an in-process ack. Conflicting/missing/replaced DB/root, live old lease, unknown
release or missing admission preserves recovery evidence and denies completion.
Recovery never bypasses production lease expiry or enrollment authentication.

Absent old relations mean configured-only sources. Unknown versions/corrupt rows
are unsupported/AuditPending, not silently imported Enrolled. New enrollment is
explicit and owner-issued. Positive native authority/capacity qualification is
separate from portable negative/SQLite/codec tests, in
[OPT-2199](https://linear.app/boostnlvp/issue/OPT-2199) (macOS) and
[OPT-2200](https://linear.app/boostnlvp/issue/OPT-2200) (Windows). Both also wait for
real production implementation; an unsupported backend is not a test waiver.

## Fixed bounds and validation

Existing descriptor policy is 4096 entries, 16 MiB aggregate, 64 KiB per descriptor,
256-byte source IDs; configured participant receipts cap at 8192. Catalog uses
32 command slots, 64 events and two-second source-authority handoff calls.
These do not automatically bound the currently unbounded registration maps.

Choose for this new family: one transition per command; canonical input and each
receipt <=16 KiB; source ID<=256 bytes; each textual root/DB identity<=1024 bytes;
opaque root identity<=4096 bytes; UUIDs nonnil and canonical; positive lifecycle
and epoch with checked SQLite integer conversion; explicit absent audit fields.
Actual encoded size is checked before queueing (not just field sums). Relations
and immutable receipt ledgers each cap at8192 rows, with <=4096 non-retired
references. New catalog/journal lifecycle carriers each cap at4096 plus bounded
32 queued transition envelopes; reserve a carrier BEFORE accepting enrollment.
New bytes/results participate in bounded queue admission, not hidden side maps.
All arithmetic is checked. Overflow/full/unknown sizes reject before effects;
exact historical replay remains possible at full retention. No automatic receipt
or retired-row eviction; journal closure/retention proof is required to reclaim.

Work attempts/replies have the existing two-second caller budget; expiry or
cancellation retains custody and does not interrupt unsafe active SQL/OS work.
Do not block GPUI or wait on global while holding the session slot mutex. Late
results match complete profile/source/registration/work/payload context before
publication. A report is not authority.

## Executable leaves

| Issue | Scope / actual prerequisite |
| --- | --- |
| [OPT-2192](https://linear.app/boostnlvp/issue/OPT-2192) | First portable owner-observation and bounded canonical metadata codec; registration unavailable stays unavailable, no effects/authority. |
| [OPT-2193](https://linear.app/boostnlvp/issue/OPT-2193) | Authenticated registered-source candidate/consumer and carrier limits; needs2192. |
| [OPT-2194](https://linear.app/boostnlvp/issue/OPT-2194) | Exact finite source/global allocation requirements; needs2166 and2167, no logical-size qualification. |
| [OPT-2195](https://linear.app/boostnlvp/issue/OPT-2195) | Owned global relation/migration/immutable transition SQL; needs2192/2193/2194,2136,2166,2168,2170. |
| [OPT-2196](https://linear.app/boostnlvp/issue/OPT-2196) | Admitted durable journal intent/claim/checkpoint family; needs2192/2193/2194/2195/2202,2136,2166,2168,2169,2170. |
| [OPT-2197](https://linear.app/boostnlvp/issue/OPT-2197) | Existing catalog/session enrollment/drain/close/capsule routing and honest shutdown; needs2193/2195/2196/2202. |
| [OPT-2202](https://linear.app/boostnlvp/issue/OPT-2202) | Admitted main-connection lease release plus sealed close evidence; needs2192/2194,2168,2169. |
| [OPT-2198](https://linear.app/boostnlvp/issue/OPT-2198) | Actual bounded restart settlement/reacquisition observers; needs2195/2196/2197/2202. |

These are unclaimed implementation scopes; shared production files require their
own claims. Only2192 is initially Linux-ready after this decision; the remaining
missing implementation/ownership/admission prerequisites are real blockers.

Production leaves must test real
owned global/source SQLite transactions and lifecycle boundaries, including
commit abort, lost replies, conflicting duplicate, stale source/profile, queue
rejection, drain racing enrollment, retained snapshot, failed close, exact
post-close retry and reacquisition. Tests using synthetic reserve/registration
providers must say so; they do not qualify native physical capacity. The contract
and any model fixtures leave OPT-1828/1357/1670 and full restart acceptance open.


## Installed lifecycle schema prerequisite (OPT-2243)

The profile-owned global `LibraryDatabase` now installs the version-one
`source_lifecycle_references` and `source_lifecycle_reference_receipts` layout
before initialization pragmas, optional extensions or unrelated migrations.
Both tables and their indexes are installed in one actual SQLite transaction.
The exact compiled `sqlite_schema` image identifies this version; a future DDL
change requires an explicit migration. Existing exact relations are inspected
without a schema transaction. Partial, altered, case-shadowed or extra-index/
trigger layouts refuse without repair or evidence deletion. Family inspection
retains at most five objects, bounds names to 128 bytes and SQL to 16 KiB before
Rust materialization, and limits SQLite VM work to 100,000 operations with a
cooperative two-second deadline. It does not inspect or qualify the entire
global schema, runtime extensions or lifecycle row contents.

The partial unique index enforces at most one Enrolled/Draining registration
per source; retired history remains retained. SQL constrains stored subject and
receipt blobs to 16 KiB, identities to their fixed sizes, full work keys to 256
bytes, states/dispositions to their finite domains, and receipt sequences to
positive integers. Receipts have a unique operation plus full-work key.
SQL fixtures demonstrate these constraints and atomic rollback; they neither
construct genuine receipts nor authorize transitions. The forthcoming owned
command must enforce canonical subjects, immutable receipts, CAS/replay,
8192-row ledger caps and the 4096 non-retired-reference cap. This schema adds
no lifecycle command, registration publication, claim, native permit or
physical allocation qualification. OPT-2194/2195 and their production and
native dependents remain open. Journal schema 12 and source schema 13 are
unchanged.

## Actual allocation capture leaf (OPT-2244)

The existing global empty preparation can request lifecycle allocation
observations with `with_lifecycle_allocation()`. Its worker validates the exact
current compiled whole-global schema and finite built-in runtime image under
its actual empty `BEGIN IMMEDIATE`, checks the 8192-per-table/4096-non-retired
limits without eviction, and captures actual MAIN/WAL/mapped-SHM objects. An
unknown/legacy-altered global schema remains refusal, even if ordinary global
migration/configured persistence continues to work. Read-only capture issues
no SQL transition, receipt, intent, claim or required-effect permission.

The original source writer separately captures and revalidates weak-owner-bound
lease-maintenance planning observations with optional completed audit metadata.
It preserves the real source/lifecycle/lease/root/database tuple and original
two-second deadline, never auto-renews a lease, and cannot manufacture an
authoritative `SourceRevision` for a pre-audit source. Reacquisition with equal
paths does not rebind the observation. Whole source schema/runtime validation
retains the existing strict metadata guard. Global and source objects on the
same device retain distinct identities and separately rounded logical ceilings.

Object capture currently supports the known retained-root Linux forwarding VFS
with libsqlite3-sys 0.35.0 bundled SQLite 3.50.2, source ID
`2025-06-28 14:00:48 2af157d77fb1304a74176eaee7fbc7c7e932d946bf25325e9c26c91db19e3079`.
The exact wrapper/context/parent role and adapter are checked before reading the
reviewed four-field native `unixFile` prefix and `fstat` of the borrowed MAIN/WAL
descriptors. `HAS_MOVED` alone is insufficient for WAL: bundled `fileHasMoved`
requires `pInode`, which WAL lacks; an actual equal-size replacement regression
proved that gap. SHM region zero is observed from the original `xShmMap` callback;
a bounded (at most 2 MiB) `/proc/self/maps` kernel read compares its shared mapping
device/inode with the no-follow current SHM descriptor. It reads no process
memory and trusts no displayed pathname. SQLite close/unmap clears the mapping
witness. Exact-size substitutions, absent/symlink sidecars and unknown/unowned
VFS/mappings refuse. Unknown versions or other hosts have no guessed ABI fallback.

The selected page bounds are at most 64 MiB for source MAIN and 512 MiB for
global MAIN, limited further by the existing pager maximum. The logical WAL
window is `32 + M*(P+24)`, and the SHM window is `32768 + 16*M`. Each object has
a unit-rounded logical refusal ceiling of four times the sum of its window
and resident bytes, plus 4 MiB of bookkeeping. These are finite I/O restrictions, not claims
that all workloads finish or that filesystem overwrite/metadata overhead is
physically reserved. The future entered policy must enforce the selected pager
maximum, known schema/runtime, no uncontrolled mmap/temp/spill/checkpoint path,
and genuine journal-issued attempt restrictions. Actual private SQL maximum-blob
and VFS refusal tests supply storage evidence only, never authentic subjects,
receipts or admission.

Full OPT-2194 remains open for qualified physical policy and exact legal
per-direction DB claim slices. Existing `SourceWalPreparation` and the stored
`SourceAuthoritative` domain require a completed audit. A distinct genuine
pre-audit lifecycle/lease binding and owned preparation must be implemented
before unaudited source lease-maintenance can be admitted; silently reusing an
authoritative audit fence is forbidden. This is a portable contract/implementation
gap, not deferred native verification. OPT-2136/2168/2169/2170/2202/2195/2196 and
tracking parents remain open; native OPT-2199/2200 must also establish their
actual descriptor/mapped-object primitive and genuine physical/backend evidence.
Contract fixtures or these observers do not complete those production routes.
