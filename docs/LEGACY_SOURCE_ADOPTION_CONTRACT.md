# Pre-lease legacy source adoption (OPT-2112)

This decision resolves the allocation and recovery ownership prerequisite for
OPT-2098. It installs no migration command, allocation observation, capacity
grant, or receipt. Normal catalog opens continue to reject version 11 before
source lease bootstrap. OPT-2098 and OPT-1745 remain open for implementation and
their original acceptance.

## Existing entrypoints and the gap

The normal-build profile fixture in
`crates/yield-library/tests/versioned_profile_migration.rs` opens a known version
11 source using `SourceDatabase::open_for_catalog_with_profile_guard`. It receives
`SourceLeaseError::MigrationRequired`, preserves the source database bytes and
rows, and creates no `source_lease` relation. Independent global migration succeeds
through `GlobalLibraryWriter`; that result does not adopt the source.

`sample_sources/db/source_lease.rs` already owns `SourceLeaseReservation::acquire`,
`validate_before_open`, `bootstrap`, and `bootstrap_lease_row`. The reservation
retains the locked sidecar and profile-authorized media/database roots before the
durable lease exists. `DatabaseFileBinding` binds the actual retained database.
`sample_sources/db/open.rs` opens through those retained roots and bindings.
`schema/mod.rs` currently stamps version 13; its schema-assurance helpers are not
an admitted migration transaction and may perform cleanup. Calling `apply_schema`
early or accepting a version stamp alone would bypass ownership and allocation.

`SourceTagMetadataBudget` requires a current `SourceWriter`, complete audit and
live source lease. Neither it nor a fabricated `SourceRevision` can qualify this
pre-lease effect. The pre-lease reservation is the owner; it must retain its
resources until the migration outcome and subsequent lease bootstrap are known.

## First admitted shape and observation

The first supported input is exactly the two-table version 11 shape compiled from
`tests/fixtures/profile-v11/source.sql`: `metadata(key TEXT PRIMARY KEY, value
TEXT NOT NULL)` and `wav_files(path TEXT PRIMARY KEY, file_size INTEGER NOT NULL,
modified_ns INTEGER NOT NULL)`, with only their expected implicit primary-key
indexes. Match the finite DDL/column/constraint image, not just these table names.
Additional tables, views, indexes, triggers, virtual tables, generated columns,
attached databases, unknown functions/collations/modules and unsupported schema
versions reject without cleanup. This deliberately does not claim every historical
version 11 profile is supported.

An explicit adoption request must capture a fresh `SourceLeaseReservation`, the
original profile acquisition, media/database/sidecar/database-file identities,
one immutable command identity, and the exact input observation. The observation
is privately constructed, non-deserializable and not write authority. It requires
the existing process extension-loading exclusion boundary; an already tainted
SQLite runtime makes allocation unknown. It does not open a path-only writer.

Only a fresh sidecar with no prior lease relation/row and no pre-existing WAL,
SHM or rollback journal is admitted initially. Existing sidecar evidence, including
an empty sidecar left by a rejected ordinary open, is not reset or deleted. Such a
source needs an explicit evidence-based recovery/adoption route, outside this
first shape. A later retry may accept an existing sidecar only when a durable
adoption receipt and retained intent positively identify its original object.
An unlocked file, expired timestamp, empty database or missing row proves nothing.

Bound the original main file and logical pages to 16 MiB, metadata to 128 rows/
64 KiB, audio rows to 1024/4 MiB, and individual path text to 4096 bytes. Reject
malformed paths, types, duplicate/ambiguous metadata, negative sizes and arithmetic
overflow. The original canonical revision must parse within the owner's numeric
bound. A pre-existing `source.legacy_adoption.v1` metadata key rejects this initial
shape; only the distinct recovery route can interpret it against a genuine retained
intent. Read with bounded lookahead and SQL progress interruption (two seconds);
sort bounded owned observations in memory, without SQL sorting/temp-file effects.
Hash the exact bounded preserved rows and schema together with the physical
bindings. Do not scan the filesystem or manufacture an authoritative source audit.

## Allocation and effect admission

Observation must use the retained SQLite VFS/main descriptor and retained roots,
including no-follow sidecar inspection. An input file-size estimate alone is
insufficient. In-memory policy validation must precede effect admission. The
entered migration policy caps main pages to the smaller of the existing
`max_page_count` and 64 MiB/page_size; it never raises a smaller existing cap.
Disable cache spill, auto-checkpoint and disk temporary storage, verify readback,
and hold the extension exclusion guard. Do not change journal mode, checkpoint,
VACUUM, rename or replace the database while observing or migrating it.

The first input must report DELETE journal mode. WAL/TRUNCATE/PERSIST/OFF/MEMORY
inputs remain unsupported by this first adoption path; do not silently change
them. For page size P and entered page limit M, the conservative database allowance
through migration and the separately admitted later WAL bootstrap is

```
J = (M+1)*65536 + M*(P+8)
B = P*M + J + (32 + M*(P+24)) + (32768 + 16*M)
```

The bundled `libsqlite3-sys` SQLite pager defines `MAX_SECTOR_SIZE = 0x10000` and
`JOURNAL_HDR_SZ` from its clamped sector size. Each rollback page record carries
eight bytes beyond its page; J conservatively permits a padded header per page
plus one, for one non-spilling transaction with no savepoints/attached databases.
The implementation must review and enforce those premises against the exact
compiled SQLite/VFS, including no repeated spill/savepoint journaling. Unsupported
VFS/header behavior makes the bound unknown. J is intentionally conservative and
can reject low-capacity volumes; no observed-small-journal discount is allowed.
All arithmetic is checked. The plan covers additional worst-case sidecar
allocation even when the observed sidecars are absent. A later journal-mode
transition is a separately admitted effect, not an observation. Include retained
main bytes, profile/journal intent/claim/receipt overhead
and all volume aliases exactly once in the common capacity owner's requirement.
An existing page cap limits database growth, not all physical allocation.

The production consumer must extend the existing prepared metadata admission
boundary with an explicit source-adoption input/work kind. The present
`UniversalCommandKind` has no source migration family; do not relabel migration as
CreateFile, ExternalReconciliation or a tag edit. Reuse the existing journal and
capacity owners, with a versioned frozen proposal and genuine descriptor-bound
claim for every affected volume, before migration writes. The observer, fixture,
decision, schema version and UUID cannot mint that claim. No new parallel
coordinator or unjournaled metadata reservation is introduced.

## Atomic source receipt and lease ordering

Freeze `(OperationId, participant, command, codec_version)` with the original
physical bindings, legacy version/DDL/row digest, selected policy and admitted
requirement. Reusing that identity with different bytes rejects. Persist the
prepared intent/claims before source mutation. Revalidate retained profile, roots,
sidecar and database before and inside the existing source-owned transaction.
No current source lease or complete-audit fence is required or fabricated here.

Run exactly one explicitly bounded SQLite transaction under the reservation. Its
DDL adds only the reviewed current schema. The immutable source-local adoption
receipt uses one reserved `metadata` key, `source.legacy_adoption.v1`, with its exact
bounded request/receipt encoding in the value. Preserve every original
metadata/audio row and its original values;
nullable/default additions are recorded separately. Do not invoke generic cleanup
helpers or reset source readiness, authored tags, collections or lease evidence.
Before commit, verify the exact post-schema image and unchanged preserved-row
digest (excluding the one newly inserted reserved key); atomically insert the
receipt and version stamp. The receipt contains the
frozen command key, pre/post schema and row digests, physical binding identifiers,
before/after version, and bounded entered-policy/allocation facts. Bound the
combined encoded request/receipt value to 64 KiB and permit at most one adoption
record per original database identity. The metadata primary key enforces its
single slot. Unknown/corrupt/conflicting records retain
evidence and reject. No caller-authored receipt establishes completion.

The existing metadata relation avoids introducing a second schema change just for
receipt storage. The receipt has its own explicit codec version; it does not
override the compiled source schema version or accept arbitrary metadata key
prefixes. Revalidate the complete expected current schema before lease bootstrap.
The owned recovery reader must explicitly bound and parse only this reserved key,
and it may not treat its presence as permission to repair an unknown schema.

After the committed receipt is read back through the retained connection and
bindings, the existing `bootstrap_lease_row` path may install the first current
lease. Bootstrap remains separately admitted/accounted; failure retains the
adoption intent/claim/receipt and does not replay migration. Current lease creation
does not certify an authoritative audit: the existing scanner must establish it
before a `SourceRevision` or readiness publication can be issued. Catalog/session
publication occurs only after their original ownership/freshness checks.

## Crash and uncertainty dispositions

| Observed state | Required disposition |
| --- | --- |
| Rejected observation/admission | No migration intent, schema/lease write or data change; an acquired sidecar remains evidence. |
| Durable intent, transaction never started | Retain claims; reacquire real ownership and exact input before a bounded retry. |
| SQL failure with confirmed rollback | Preserve original rows/version; retain command identity and admission evidence for explicit retry. |
| Commit error or lost reply, outcome unknown | Retain claim/intent and all sidecars; no success or automatic body replay. Inspect the existing receipt under recovered ownership. |
| Exact receipt and exact post-image, lease absent | Migration is committed; resume only admitted lease bootstrap, not schema mutation. |
| Current lease plus exact migration receipt | Continue ordinary current-schema ownership checks; replay returns the same migration result, not a new lease or audit grant. |
| Changed roots/database, unknown schema, conflicting receipt/claim, unexplained sidecar | Audit/recovery required; retain evidence and prohibit ordinary writes. |

A receipt alone is durable metadata evidence, not proof of qualified power-loss
semantics. Restore exact SQLite policy only after transaction quiescence; failed
restoration poisons the retained connection until real close/reopen. On recovery,
inspect existing bounded records using retained ownership and the versioned codec.
Never delete a receipt/claim to make another attempt admissible. Claim release
requires the existing capacity owner to confirm the migrated/bootstrapped outcome
and actual retained allocation; source retirement/reclamation remains separate.

## Concrete implementation leaves and validation

1. [OPT-2116](https://linear.app/boostnlvp/issue/OPT-2116): pre-lease inspector/policy/requirement: implement bounded exact version 11
   observation on the reservation's actual binding, runtime exclusion and checked
   VFS allocation model. Prove normal-build read-only negatives for foreign roots,
   prior sidecars, unknown DDL/runtime/version, limits and elapsed budget. This
   leaf grants no migration authority and leaves normal-open rejection intact.
2. [OPT-2117](https://linear.app/boostnlvp/issue/OPT-2117): admitted migration participant: implement the prepared metadata command/codec,
   capacity requirement/grant bridge and one atomic source receipt transaction;
   then integrate separately accounted existing lease bootstrap/reopen. Prove
   intent-before-write, exact/conflicting replay, failure/unknown commit retention,
   unchanged original rows, current reopen, root substitution and competing owner.

Both are portable design/implementation work. Missing primitives or allocation
bounds require a concrete implementation decision, not a Linux-blocked waiver.
If actual capacity/native durability qualification needs an unavailable external
resource, identify that resource in its own scoped issue; retain these portable
leaves. The original OPT-2098 keeps both leaves and every original acceptance.

The existing normal-build baseline is reproducible with:

```
cargo test --locked -p yield-library --test versioned_profile_migration
cargo test --locked -p yield-library --test profile_admission
```

Those tests establish current rejected source adoption and actual global migration,
ownership and preservation. They do not execute this proposed migration or qualify
physical allocation/durability. No new production migration success is claimed.
