# Source receipt to global operation link (OPT-2033)

## Required caller and owner mapping

The first link is **source curation receipt → frozen global-library work unit**.
It records that a particular source command committed, and makes its durable identity
available to global replay and recovery. It does not copy file metadata or mutate the
source registry. The endpoints already exist:

| Endpoint / caller | Existing owner and evidence | Required integration |
| --- | --- | --- |
| Source curation command | `SourceWriter::commit_curation`, source DB `source_participant_curation_receipts`; `SourceWriterReceipt::identity()` and `source_revision()` | Retain the operation, participant and `SourceCommandId` chosen before dispatch. Recover a lost response with `read_receipt_observation` through the reacquired source owner. |
| Global operation obligation | `operation_journal::work_plan::WorkUnitKey`, role `GlobalLibrary`; frozen universal work plan | Family adapter selects the actual required global key and source key from the admitted plan, rather than constructing a sparse replacement plan. |
| Correlation persistence | Existing profile-owned `GlobalLibraryWriter`, `library.db` | OPT-1644 adds one bounded command and immutable SQLite receipt in this owner. No separate database, writer, coordinator or completion issuer. |
| Journal / Harvest / projection consumers | Existing full work progress and private owner-verified completion boundary (OPT-1641) | The OPT-1341 saga/family consumers validate journal context; dependent Harvest/projection work consumes the link as an observation and separately establishes its own completion evidence. |

The current global registry receipt already links its own operation/work key, but it
has no source command endpoint. Neither that receipt nor ordinary registry replacement
satisfies this link. At the decision snapshot no production operation-link entrypoint existed: this decision
and its synthetic fixtures do **not** close OPT-1644 or OPT-1833.

## Immutable command and scope

V1 inserts one link per `(OperationId, global WorkUnitKey, source WorkUnitKey,
SourceCommandId)`. This permits multiple source commands in one operation without
silently collapsing them into the registry participant's one-result-per-operation key.
The source key must have role `SourceAffect` and participant equal to the receipt's
source participant; the global key must have role `GlobalLibrary`. Both keys must belong
to the caller's exact admitted frozen plan. Curation for another work role requires an
explicit contract extension, not a permissive cast. No family currently lacking an
admitted operation may manufacture an operation merely to populate this table.

The immutable payload carries version 1, operation ID, both full work keys, source
command UUID, source ID, opaque root identity bytes, database identity, root-binding
identity, lifecycle generation, historical lease epoch and committed source revision.
Take all source fields from the actual `SourceWriterReceipt`; never accept a path-only
or caller-fabricated source receipt as completion. These fields describe the historical
commit. A fresh source lease may read that receipt after restart, but may not replace
its historical fields with the current revision. Current lifecycle/root/database checks
remain the source owner's responsibility before returning the observation.

Bounds: non-nil UUIDs; each serialized work key at most 256 UTF-8 bytes; source,
database and root-binding identities nonempty and at most 256 UTF-8 bytes each; root
identity nonempty and at most 4096 bytes; lease epoch positive; lifecycle/revision fit
SQLite signed integers. Canonical versioned JSON is at most 32 KiB. Reject before queue
admission; persisted data must be length-bounded before allocating/decoding. Use a
versioned BLAKE3 domain plus exact canonical payload bytes for replay equivalence;
digest equality alone is insufficient. Unknown versions and malformed retained rows
fail closed. At most 8192 rows and 16 MiB canonical payload bytes are retained; exhaustion
rejects new links without evicting replay evidence. Exact replay remains available.
Reclamation requires a separate proven journal retention contract.

The command additionally carries the **current** profile acquisition UUID as an
equality fence. The existing worker retains and validates the actual writable profile
guard before and after dispatch. UUID spelling is not ownership. Acquisition is not
part of the immutable payload: exact replay after profile reacquisition returns the
original receipt's historical acquisition and sequence. Reusing a request stamped
with the old acquisition is rejected even for an existing row.

## Transaction, typed outcomes and recovery

Source commit comes first. The family adapter obtains its exact source receipt before
submitting this link. Source and global transactions are deliberately separate; a global
failure leaves the source committed and the global work pending. Retry the identical
link, never repeat the source mutation using a newly generated command ID.

Within the global transaction: read the composite key (reject ambiguous duplicates),
validate retained bounded evidence and compare canonical payload; on an exact match
return its immutable result. Otherwise enforce capacity, insert the link with immutable
sequence and historical profile acquisition, read back the exact row, and commit. SQL
failure rolls back the insertion. Report success only after commit and the worker's
ownership recheck; a failed post-commit ownership check is uncertainty, not permission
to claim the row never committed. A lost acknowledgement is recovered by the same
command through a current owner. Do not overwrite a conflicting row.

| Typed result / error | Trigger | Durable effect and caller action |
| --- | --- | --- |
| `Committed(LinkReceipt)` | New valid key and payload | One row, read back before commit; observation only. |
| `Replayed(LinkReceipt)` | Exact key and canonical payload | Return the original receipt unchanged, including historical acquisition and sequence. |
| `InvalidInput` | Bounds/version/role/endpoint mismatch | No queue admission, no row. |
| `StaleProfile` | Command acquisition differs from live owner | No row and no replay disclosure through the stale command. |
| `ReplayConflict` | Same composite key with changed source fence/payload | Existing row unchanged; retain recovery conflict. |
| `MalformedReceipt` | Corrupt/ambiguous retained row | Fail closed; do not select arbitrary evidence. |
| `ReceiptCapacity` | Row or byte budget full for a new key | No eviction or new row; exact replay still allowed. |
| `SqlFailure` / `OwnerUncertain` | Transaction or post-commit boundary fails | Keep global obligation pending; retry/read back through current owner. |

Public receipts are correlation observations. They do not mint
`VerifiedStepCompletion`, authorize filesystem publication/retirement, establish live
source ownership, make Harvest/projection current, or prove cross-database atomicity.
The eventual journal adapter must check exact operation/plan/key/source/execution context
and the family's required source prerequisites through OPT-1641's existing private
boundary. This decision supplies no new physical completion issuer.

## Exact remaining implementation and validation

OPT-1644 implements new `sample_sources/library/operation_link_participant.rs` and an isolated
owner test include, plus coordinated edits to `library.rs`, `library/owner.rs`,
`library/error.rs` and schema initialization/checks. Follow the actual
`registry_participant.rs` transaction/worker pattern, preserving existing routes. Expose
an immutable bounded request derived from a `SourceWriterReceipt`, typed receipt/result
and nonblocking owner command. No session or GPUI action-register changes are needed
for this storage slice. Coordinate these shared files with OPT-2031/1836 and OPT-1655.

Production tests must exercise the actual worker and SQLite: multiple source commands
under one operation; duplicate and lost acknowledgement; reopen/reacquisition retaining
receipt; changed source lifecycle/revision/payload conflict; invalid key/identity/bounds;
stale profile before duplicate; SQL abort rollback; malformed and duplicate retained
rows; row/byte capacity with exact replay; and source committed/global failed then retry.
Run relevant source receipt replay tests too. OPT-1833 validates this typed writer route
against duplicate/reordered commands and restart replay; its stated scope excludes saga
ordering and source participants. OPT-1341 saga consumers must connect an admitted family
and keep its global checkpoint pending on rejection or uncertainty. Harvest/projection
and other family acceptance remains open.

`crates/yield-library/tests/operation_link_contract.rs` exercises a typed **synthetic
contract matrix** for identity, replay and fences. It is not the production table,
owner command, journal bridge or physical qualification. Linux can implement and test
all of the storage slice. Native receipt-owner tests on macOS/Windows belong with that
implementation's scoped platform verification; neither platform run alone qualifies
physical publication, retirement or power-loss durability.

## Implemented storage boundary (OPT-1644)

`OperationLinkRequest::new` accepts an actual `SourceWriterReceipt` and both frozen
work keys. It bounds observations before cloning, then owns a validated canonical
payload. `GlobalLibraryWriter::apply_operation_link` queues it through the retained
profile worker; the worker checks acquisition before either mutation or replay.
`operation_link_receipts` stores the composite key, exact payload, versioned hash,
historical acquisition and sequence. `OperationLinkResult::{Committed, Replayed}`
returns an immutable correlation receipt. The production API supplies no public
payload/receipt constructor and no journal completion authority.

The focused actual-owner tests include a real source curation commit, failed global
transaction, retained source readback and successful global retry; lost global reply,
duplicate/multiple commands/restart/fresh acquisition; changed payload; SQL rollback;
malformed/ambiguous retained evidence; and both row and byte budgets with exact replay.
Budget fillers and isolated payload-bound fixtures are synthetic storage stress input.
They do not stand in for production source receipts or physical qualification.

OPT-1833 validates the bounded typed writer route. Admitted-family integration and full
journal context checks remain in the OPT-1341 saga consumers, outside this storage slice.
Harvest/projection and filesystem/source-retirement evidence remain independent.
The SQLite connection retains the existing `synchronous=NORMAL` policy; these tests
prove transactional replay and process reopen, not power-loss durability. Native
macOS/Windows owner and descriptor-bound SQLite regression remains separately scoped.
