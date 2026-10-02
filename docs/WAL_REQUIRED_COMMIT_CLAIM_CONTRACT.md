# Exact database claims and required WAL commits (OPT-2147)

This decision completes the missing claim-binding design under OPT-2137. It extends
[retained participant preparation](WAL_PARTICIPANT_ADMISSION_CONTRACT.md); it does
not implement that handshake, issue a permit, qualify allocation, or finish
OPT-2137/1685/1688/1348. The production leaves below remain necessary.

## Design baseline and implemented persistence

The following inspection describes the OPT-2147 design baseline. OPT-2166 adds
the actual schema-11 bounded codec/atomic store boundary described in
[DATABASE_CLAIM_PERSISTENCE_VERIFICATION.md](DATABASE_CLAIM_PERSISTENCE_VERIFICATION.md).
Its rows are observations; genuine preparation, attempts and participant
consumption remain separate production work.

`full_capacity.rs` and `full_capacity_planner.rs` aggregate allocations by volume
and class. `admission_claims.rs` validates exact aggregate coverage, including
split claims, but contains no database identity. `store/ordinary_claims.rs`
persists operation, claim, volume, class and bytes. Two sources on the same volume
can therefore share an aggregate without any row identifying which database may
spend it. Neither `SourceWalShm` nor a caller's source ID resolves this ambiguity.

The design baseline had SQLite schema version 10 and operation codecs through
V17; those are separate version namespaces. `store.rs` atomically settles initial
intent/claims and retains pinned capacity on uncertainty. Its post-intent chunk
ledger already demonstrates private issuance, exact readback and durable release.
A chunk output identity is not a database allocation binding and cannot be reused
as a WAL permit.

`sqlite_wal/admission.rs` observes the retained SQLite WAL VFS object inside the
participant's immediate transaction. It rejects known hard and unknown WAL.
Source and global commit gates do not currently consume a required-commit permit.
`owner.rs::LiveAdmissionWitness` borrows genuine profile/volume/reserve authority;
an observed claim row, trait implementation or copied capacity fact cannot mint it.

`SourceLease::prepare_for_mutation` may renew `expires_at` through a separate
control connection before an autocommit mutation. Preparation must instead use
strict read-only ownership checks. Calling this renewal while the ordinary writer
holds the same database's reserved lock can deadlock the owner against itself.

## Preserve aggregate claims; partition their spending scope

Keep existing immutable ordinary rows and provider accounting. Add a private,
immutable database-binding inventory alongside them, rather than changing an
aggregate claim into a capability or charging the same bytes a second time.
The journal coordinator derives this inventory from the validated immutable
command, participant set, allocation plan and genuine live reservations.

Each distinct physical database has one binding containing:

- A non-nil binding UUID, source/global role, descriptor-derived volume, allocation
  unit and exact main-file identity; source ID alone and path spelling are insufficient.
- Profile acquisition/ownership identity, owner registration UUID and generation,
  source lifecycle/root/root-binding/database-root/lease identities where applicable,
  or the retained global writer lifecycle. Record the qualified descriptor context,
  not an unrelated media-root volume.
- The intended participant IDs, current revision, versioned immutable command digest
  and exact preparation decision nonce/inventory digest.

Aliases deduplicate only when verified physical file identity, role and owner
context agree. Conflicting registrations/roles for one physical database reject.
One binding may cover several declared participants, but each participant's legal
command, direction and revision transition remains explicit. A source and the
global database cannot accidentally share a binding merely because their volumes match.

Each positive claim slice binds a unique slice UUID, ordinary claim UUID, database
binding, participant, closed direction and allocation ceiling. Version 1 has
`Routine`, `RequiredForward`, `RequiredCompensation` and `LeaseMaintenance` directions.
Undo/Redo remain their actual immutable command families; a direction is not a
caller boolean and cannot make optional rebuilding required. Compensation needs
the declared inverse command and a currently legal journal phase/disposition.
Routine/optional slices cover inventory but never obtain a hard-WAL exception.
No slice is invented for an unknown estimator.

Use these conservation rules with checked arithmetic, before retaining input:

1. Every database-writing participant and its DB/WAL/SHM requirement is covered;
   omitted databases or unsupported requirements reject the complete preparation.
2. Each slice references an existing, unreleased claim of the same operation,
   verified volume and role-compatible allocation class. DB growth uses
   `SourceDatabaseCommit`/`GlobalDatabaseCommit`; WAL/SHM uses the corresponding
   sidecar class. Payload, journal and recovery claims cannot be relabeled.
3. For each bound claim, slice ceilings sum **exactly** to its allocated bytes.
   Partition aggregate bytes between databases/directions; do not give every
   database the aggregate ceiling. Bind explicit WAL and SHM subceilings within
   each sidecar slice. A zero component is allowed only for a proved applicable
   no-growth case, never an unknown descriptor or estimate.
4. The existing claim inventory still exactly covers the complete full-capacity
   plan. Unrelated allocation classes retain their existing settlement protocol.
   All ceilings obey the retained allocation-unit rules; per-database rounding
   must be included before aggregation, not discarded when splitting it.
5. Spent, started or unresolved attempts remain charged against their slice.
   A later attempt cannot reuse that ceiling until exact settlement proves the
   permitted remainder. Never subtract one database's spending from another's slice.

The inventory ceiling is 64 distinct databases and 256 claim slices, with the
entire owned encoding at most 64 KiB. Each opaque identity uses at most the existing
256-byte source-identity ceiling. Enforce aggregate bytes as well as element
counts: individually valid fields may still make an oversized inventory. No paths,
labels or unbounded diagnostic text are copied into it.

## Versioned encoding and atomic persistence

Choose a private canonical binary inventory encoding: ASCII `YWCB`, little-endian
u16 version 1, then the following exact field order. UUIDs use
16 bytes, digests 32 bytes, integers fixed little-endian widths, identity strings
u16 byte lengths and valid UTF-8. Role/direction/class tags have explicit versioned
values; unknown tags, duplicate IDs, unsorted entries, trailing bytes, nil UUIDs,
nonpositive generations, overflow and unsupported combinations reject. Sort
bindings by verified `(volume, main-file identity, role)` and slices by
`(claim UUID, binding UUID, participant UUID, direction, slice UUID)`.

The header contains profile identity string, acquisition identity string, positive
profile ownership generation (u64), decision nonce (UUID), binding count (u16) and
slice count (u16). It is followed by every sorted binding, then every sorted slice.
Validate SQLite storage ranges before identity allocation: persisted generations,
revisions and byte ceilings must fit positive/nonnegative i64 fields as applicable.
Volume IDs retain the existing canonical u64-text storage, not a lossy i64 cast.
A binding encodes UUID, role (u8), volume (u64), allocation unit (u64), main-file
identity string, owner registration UUID, owner generation (u64), lifecycle (u64),
database-root identity string and root-binding identity string. Source role then
encodes source ID, media-root identity and lease-owner identity strings plus
positive lease epoch (u64); global role encodes none of those source-only fields.
Next come current revision (u64), participant count (u16), then sorted participant
UUID/requirement (u8)/revision domain (u8)/command digest (32 bytes)/expected-before revision (u64)/
expected-after revision (u64) entries. The total participant inventory remains
within 64; a participant occurs once, even when several refer to the same binding.

A slice encodes slice UUID, claim UUID, binding UUID, participant UUID, direction
(u8), class (u8), total ceiling (u64), WAL ceiling (u64) and SHM ceiling (u64).
DB classes require the last two values zero; sidecar classes require their checked
sum equal the total. Tag values are Source=1/Global=2; Required=1/Rebuildable=2;
Routine=0/RequiredForward=1/RequiredCompensation=2/LeaseMaintenance=3; and
SourceDatabaseCommit=1/SourceWalShm=2/GlobalDatabaseCommit=3/GlobalWalShm=4.
Revision domains are SourceAuthoritative=1/GlobalMutationGeneration=2/
GlobalRegistry=3/GlobalPreferences=4/GlobalVocabulary=5. A source binding accepts
only domain 1; a global binding accepts only its actual applicable global domain.
The binding's current revision is the source authoritative revision or a new
private acquisition-local global mutation counter; it does not substitute for a
participant's narrower revision. Current global code has no such common u64
counter: its Harvest publication generation is a UUID and registry/preferences
proposal revisions are acquisition-local correlations. OPT-2136/2170 must add
the checked counter on the real worker, invalidate it before every potentially
mutating dispatch (including failed/replayed attempts), expose it under the held
reservation and reject exhaustion. A fresh registration may reset it; binding
that registration prevents historical counter replay. Do not cast the Harvest
UUID or a caller correlation into this field. Narrow revision domains retain
their actual persistent or acquisition-local family semantics.
These are local inventory tags, not enum discriminants of existing Rust models.
Command digests bind the actual validated family encoding and requirement, not
just a command-kind tag. Derive future revisions from that family; unchecked
caller revision arithmetic is not accepted.

Hash the complete canonical encoding with BLAKE3 under the fixed domain
`yield.database-claim-inventory.v1`. Hash validated command bytes under its own
versioned family domain. JSON field order, abbreviated hashes and caller-produced
digests cannot replace comparison with the actual validated command/reservations.
Stored encodings and digests are observations, not live acknowledgements.

The journal store owns three new relations: `operation_journal_database_binding`,
`operation_journal_database_claim_slice` and `operation_journal_database_attempt`.
Bindings/slices are immutable, keyed by their UUIDs with operation/claim foreign
keys and unique canonical inventory membership. Attempt rows bind one unique
attempt UUID to slice, exact command/revision/direction, ceiling, current owner
context, start nonce and definite outcome/readback identity. They use bounded
closed states; history is capped at 256 attempts per operation. Each of the three
relations also has a global 8,192-row ceiling, and restart/readback uses pages of
at most 64 rows with the existing bounded owner scheduling. Include their encoded
metadata, conservative SQLite/rollback-journal peak and bounded control receipts
in the journal allocation plan before acceptance. Exhaustion retains
evidence and rejects another attempt; no eviction resets spent capacity.

Persist initial bindings, slices, full inventory digest and the sealed OPT-2136
decision nonce in the **same journal transaction** as intent and ordinary claims.
Validate shape before allocating an OperationId; validate genuine reservation
coverage/latch before provisional admission and settlement. A missing inventory
cannot be filled in after durable intent to justify an earlier acceptance.
Lost delivery reads that exact nonce/operation/inventory/claim image back; never
allocate a replacement operation or reconstruct a live latch from stored bytes.

Allocate the next journal SQLite schema version only in the implementation claim,
after coordinating recording/history/tag schema owners. This document does not
reserve version 11 or redefine V17. Extend actual schema whitelist, constraints,
migration, namespace/reopen validation, queue limits and bounded restart audits.
Unknown relations/versions or conflicting rows remain Unsupported/NeedsAudit.
Older operations without an atomic exact binding remain charged and inspectable;
they cannot obtain a hard-WAL exception by guessed backfill or a migration that
fabricates live acceptance. A separately admitted reconciliation is required.

## A ceiling must constrain the actual SQLite writes

Claim arithmetic alone cannot bound arbitrary SQL, cache spills, trigger effects
or SHM growth. Measuring after a write cannot prevent an overspend. The first
production exception uses an owner-installed **forwarding budget VFS**, selected
when opening the retained source/global/control connection. Do not patch a
borrowed `sqlite3_file.pMethods` on an already-open connection or replace SQLite's
process-wide default VFS.

The wrapper delegates locking, sync and identity controls to the approved native
VFS, preserving `HAS_MOVED`, Windows native-handle controls, main-file binding and
retained WAL size observation. Bind every participating main/WAL/SHM object to
the same private owner context; a pathname suffix or unrelated open object cannot
join the budget. Unsupported native controls or association fail closed. Preserve
the existing no-follow namespace checks and reject unplanned temporary/file roles.

While a permit is installed, check the remaining conservative allocation/write
budget **before** forwarding DB/WAL writes, size hints, enlarging truncation and
SHM mapping growth. Charge checked allocation-unit-rounded writes conservatively,
including overwrite/copy-on-write and metadata overhead required by the qualified
provider. SHM mappings need a checked region-index/size ceiling; absent exposed
identity/coverage cannot become zero. SQL effects and ordinary automatic
checkpointing cannot escape this guard. Disable implicit checkpoints for this
commit; separately owned maintenance requires its own admission.

Budget exhaustion returns a typed pre-forwarding failure. Preserve SQLite's
transaction semantics and keep the connection private until rollback/reconciliation
is confirmed. Test actual SQLite cache spill and partial native writes, not only
a scalar classifier. The wrapper supplies a logical enforcement primitive;
qualified physical allocation bounds and reserve provenance are still necessary.
If those are unavailable, positive production permission remains unavailable.

## Private issuance, consumption and settlement

Only the journal owner can start an attempt after validating the exact durable
operation/participant/slice, legal phase/direction, remaining ceiling and genuine
current source/global registration. Refresh descriptor capacity using actual
retained profile/volume/reserve authority. Known hard WAL may proceed only with
this bounded already-admitted required scope. Unknown mode, descriptor, size,
identity or allocation coverage rejects even a nominal recovery request.

Before any participant effect, durably reserve the entire attempt ceiling and
start nonce. Exact successful readback allows issuance of one private, non-Clone,
non-serializable permit to the registered participant owner. The owner consumes
it once, under its immediate transaction, after strict profile/source/lease,
revision, command and retained WAL revalidation, and installs the matching VFS
budget. Bind both journal and participant lifecycle; observations, public trait
implementations, old receipts and matching UUIDs cannot construct this permit.
Queueing the permit must not transfer a SQLite connection or synchronously block
one owner waiting for another.

| Boundary | Required accounting and owner result |
| --- | --- |
| Before durable attempt start | No participant effect; retain/release only definite provisional state. |
| Start commit/reply is uncertain | Keep attempt ceiling pinned; no permit until exact start readback. |
| Permit is dropped or delivery lost | Retain started charge; cancellation is not proof of no effect. |
| Ownership, revision, direction, unknown WAL or budget mismatch at consumption | No new effect; exact unused proof may settle the attempt, never a caller report. |
| SQLite body/commit fails | Confirm rollback or reconcile the exact participant receipt; WAL allocation may remain even after metadata rollback. |
| Metadata commit succeeds | Read actual immutable participant command/receipt and retained budget result; journal durably records that exact outcome before replying. |
| Commit/receipt/result is unknown | Keep slice charged and participant unavailable for ordinary dispatch until exact readback; no retry with a fresh nonce. |
| Definite no-commit | Restore only proved unspent budget; retained sidecar/uncertain allocation remains charged. |
| Restart or reacquisition | No live permit survives. Rebuild claims/attempts; old owner/lifecycle/lease never resumes automatically. |

An attempt's outcome is immutable and exact replay is read-only. Reconciliation
requires genuine current owners and matching physical database plus durable
participant evidence; historical fence/receipt values alone cannot authorize
rebinding to a new lifecycle. A conflict remains NeedsAudit. Do not turn a
timeout, connection Drop or fresh lease into proof that an old commit failed.

Use the existing serialized `RecoveryReserveCharge`/spend/reconstitution protocol
only when protected reserve is actually needed, with the exact operation, purpose,
bounded budget and control-plane margin. An ordinary slice does not become reserve
authority. Unknown spend/commit retains the reserve charge. Logical slice settlement
does not release the ordinary claim: durable ordinary release still needs all
bound slices settled and genuine allocation/retirement evidence. WAL truncation,
an empty ring, a codec fixture or a sidecar pathname size is not release proof.

## Separately admitted lease maintenance

Add one closed lease-maintenance command owned by the retained source control
service. It updates only the same `source_lease` singleton's expiry, preserving
owner identity, epoch, roots, source lifecycle and revision. Freeze the exact
bounded expiry update and expected row in its admitted command; validate clock
arithmetic and actual trigger-free schema. Never use a tag/file command as a carrier.

This command has a separate preparation route: validate the genuine retained
profile, OS source lock, root/database binding and exact active owner/epoch without
pretending an expired timestamp is current. That narrow authority can renew only
its own unchanged row; it cannot admit ordinary metadata or a replacement owner.
Reserve the actual control connection with one nonblocking immediate attempt,
check retained WAL/capacity/budget, and perform the single update under the same
connection. No ordinary transaction or participant reservation may be held while
acquiring this control writer. Busy returns bounded scheduler retry.

Below hard, separately admit this finite maintenance command before its effect,
then retry ordinary strict preparation only after exact renewal readback. At hard,
new maintenance intent is also forbidden: renewal requires an **existing admitted**
required operation's explicit lease-control slice and remaining budget. Without
that slice, fail closed for owner maintenance/reconciliation; do not relabel a
payload or consume the recovery reserve through a boolean. Include bounded lease
control space in new required-operation plans before admission; old plans lacking
it remain unsupported for the exception. Unknown WAL has no renewal exception.

Use the same attempt/budget/receipt protocol. Ambiguous renewal pauses admission
until the exact row/attempt is reconciled; it is not repeated with a different
expiry. Preparation remains read-only, including near-expiry rejection before
SQL. Audit/replace implicit `prepare_for_mutation` renewal on affected routes so
ordinary validation cannot bypass this control policy. This is portable API and
implementation work, not a native-test waiver.

## Production leaves and validation

The concrete leaves are:

| Leaf | Acceptance boundary |
| --- | --- |
| [OPT-2166](https://linear.app/boostnlvp/issue/OPT-2166) | Atomic exact binding/codec/store, with real SQLite conservation/readback; no live permit. |
| [OPT-2167](https://linear.app/boostnlvp/issue/OPT-2167) | Forwarding VFS pre-write budget enforcement; no physical qualification or independent exception. |
| [OPT-2168](https://linear.app/boostnlvp/issue/OPT-2168) | Genuine journal issuance/settlement after binding, budget and OPT-2136 latch. |
| [OPT-2169](https://linear.app/boostnlvp/issue/OPT-2169) | Actual retained source transaction consumption and uncertain receipt recovery. |
| [OPT-2170](https://linear.app/boostnlvp/issue/OPT-2170) | Actual global worker transaction consumption with correct revision domains. |
| [OPT-2171](https://linear.app/boostnlvp/issue/OPT-2171) | Separately admitted lease-control renewal without hidden writes or self-deadlock. |

Connect OPT-2166's binding persistence to OPT-2136's genuine latch and preserve
OPT-2133/2135's reservations. The first two leaves are independently implementable
after this design; later issuance/consumers retain their real prerequisites.
Keep OPT-2137 and the original WAL parents open until every route is implemented.

Required tests use real retained source/global SQLite owners: two databases on
one volume, physical aliases/conflicts, omitted inventory, conservation/rounding,
malformed bounded encoding, atomic intent rollback/readback/reopen and legacy
unsupported rows; then actual hard-WAL commits, unknown VFS rejection, stale
registration/revision/direction, double consumption, overspend/cache spill/SHM,
failed/uncertain commit, dropped delivery/restart and retained charges. Lease
tests hold an external writer and exercise near/expired exact rows, no hidden
renewal, no self-deadlock, one bounded update, hard/unknown rejection and lost reply.
Synthetic capacity ledgers test logical settlement only. Actual native
provider/reserve and forwarding-VFS allocation bounds require
[OPT-2173](https://linear.app/boostnlvp/issue/OPT-2173) on macOS and
[OPT-2174](https://linear.app/boostnlvp/issue/OPT-2174) on Windows: genuine retained
native controls, disposable filesystem volumes, physical allocation/CoW/metadata
peaks and real reserve provenance. Linux cannot execute those platform runs.
These scoped resource steps do not block the portable codec/store/VFS work;
stronger power-loss claims remain the existing durability qualification scope.

Portable baseline commands:

```sh
cargo test --locked -p yield-library --lib operation_journal::admission_claims
cargo test --locked -p yield-library --lib chunk_issue_and_release_uncertainty_replay_without_reentry -- --test-threads=1
cargo test --locked -p yield-library --lib sqlite_wal
bash scripts/agent.sh preflight
```

These passed 7, 1 and 9 tests on the design source baseline; they demonstrate current
aggregate validation, actual SQLite uncertainty accounting and retained WAL, not this missing
production permit. Each implementation leaf must add and run its own actual cases,
verify merged/tested code and record limitations before Done.
