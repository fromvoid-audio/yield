# Pre-lease legacy source adoption (OPT-2112)

This decision resolves the allocation and recovery ownership prerequisite for
OPT-2098. Retained source/journal/profile observers now return bounded historical
images and conditional logical allocation allowances. They install no admitted migration
command, capacity grant or receipt. The explicit passive command family described
below does not supply those effect authorities. Normal catalog opens continue to reject
version 11 before source lease bootstrap. OPT-2098 and OPT-1745 remain open for implementation and
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
boundary with an explicit source-adoption input/work kind. The explicit `SourceLegacyAdoption` family now describes passive original-owner
input and pre-lease topology. Its dedicated initial adapter now retains the
original V19 intent and ordinary claims on the issuing worker (OPT-2373 below);
mutable migration and current lease bootstrap remain separate. Do not relabel migration as CreateFile, ExternalReconciliation or
a tag edit. Reuse the existing journal and
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

## Retained read image prerequisite (OPT-2239)

`SourceDatabase::open_legacy_read_owner` is an explicit normal-build read route,
separate from ordinary opens. It holds the original profile acquisition, media and
database roots, fresh source-sidecar reservation and main-file binding around a
read-only descriptor-bound SQLite connection. It never bootstraps `source_lease`.
Existing sidecar/WAL/SHM/journal evidence is refused without deletion or reset.

The owner accepts the exact compiled v11 two-table/implicit-index DDL image and
bounded typed metadata/audio rows. Bounded lookahead precedes retention; memory
sorting does not ask SQLite to create sort files. Capture runs in one read
transaction with a two-second cooperative progress deadline. Main bytes come from
SQLite's retained VFS descriptor. The versioned digest includes bounded schema,
rows and original physical/profile bindings. The initial digest is frozen;
subsequent capture refuses changed rows even when an external SQLite writer
ignores Yield's sidecar. All retained physical bindings are revalidated.

`LegacySourceImage` is a privately constructed historical observation. It has no
current source lease, audit, writable connection, allocation estimate or adoption
permission. Inspection Drop preserves the created sidecar; such evidence requires
an explicit later recovery route, so a fresh inspection cannot simply be retried.

Normal-build tests exercise this actual owner and preserve original source bytes,
rows and normal-open `MigrationRequired`, with unknown-schema, malformed/bounded
row, prior-sidecar, replacement and foreign-acquisition refusals:

```
cargo test --locked -p yield-library --test legacy_source_read_owner
```

OPT-2116 remains open for its complete known-runtime/VFS allocation policy,
page/sidecar facts, conservative DELETE/future-WAL bounds, profile/journal overhead,
and deadline/overflow qualification. OPT-2117, OPT-2098 and OPT-1745 remain open for
admitted migration, atomic receipt, crash recovery and original acceptance. The
read image neither enters a persistent page cap nor proves physical allocation.

## Source pager policy observation (OPT-2240)

`LegacySourceReadOwner::capture_pager_observation` captures the source-side policy
through the same frozen image and retained owner. It checks bundled SQLite 3.50.2,
process extension exclusion, the known connection-owned Unix forwarding VFS and
its actual sector callback. An unregistered/unknown pager or tainted runtime is
refused. It reads actual page size/count/current maximum and selects the smaller
of that maximum and 64MiB/page_size without changing the page cap or journal mode.

The checked J/B formula above produces separate main, DELETE rollback, future-WAL
and future-SHM estimates. Main-file length remains a retained VFS observation;
page count and these estimates are logical bounds. The reopened volume-inspection
handle is no-follow, checked against the original retained file identity and used
only to capture volume/allocation-unit facts. Free bytes are not carried into the
result. The result is non-deserializable and privately constructed, with no
conversion into allocation, migration or capacity authority.

Actual normal-build capture preserves source bytes and creates no WAL/SHM/journal.
Private real-owner fixtures verify a smaller page cap remains unchanged, altered
read policy and expired deadline refuse, and an actual failed extension load in a
separate process taints/refuses capture. Checked-formula tests cover page-size
boundaries and overflow; a genuine unregistered SQLite connection cannot qualify.

These source-only estimates do not include profile/journal intent/claim/receipt
requirements, prove native physical allocation, enter a future migration policy,
or authorize the separately admitted WAL transition. Complete-owner/common-
capacity composition remains OPT-2116 and migration remains OPT-2117.

## Actual journal/profile observation prerequisite (OPT-2241)

`OperationJournalStore::capture_legacy_journal_observation` uses the existing
retained journal connection, database-root capability and original profile-owner
acquisition. It creates no second journal connection, intent, claim or receipt.
Capture validates exact bounded schema 12 and current DELETE/EXTRA/exclusive
configuration, excludes process extension loading, and measures the actual known
retained Unix VFS/main file and sector callback. A read transaction and cooperative
two-second progress deadline bound inspection. Unknown runtime/VFS, attached/temp
schema, changed objects, oversized schema/control files, or elapsed budget refuse.
The original retained profile-lock descriptor supplies its actual length, volume,
and allocation unit; neither journal nor lock observations expose free capacity.
Ordinary retained DELETE sidecar bytes/identity are observed without deleting them.
A narrow no-follow, nonblocking observation open rejects substituted FIFOs and
non-regular children without waiting; other existing open helpers are unchanged.

For P/page size and proposed M=min(current max_page_count,512MiB/P), the conditional
whole-journal logical allowance is P*M plus J=(M+1)*65536+M*(P+8). This covers the
whole journal database, including every index and intent/claim/receipt family,
rather than discounting to a small observed record. Capture leaves max_page_count,
cache_spill, temp_store and journal mode unchanged. Their actual values are carried
as historical facts. Before this estimate can be consumed for an effect, the
existing owner must separately enter/verify the selected cap, non-spilling DELETE,
MEMORY temporary storage, no attachments/savepoints and extension exclusion.
The existing recording configurator is not invoked and must not raise a smaller
cap for adoption. This conditional estimate is neither entered policy nor physical
allocation qualification, a provider claim, write permission or adoption result.

Normal-build actual store tests preserve main/profile-lock bytes and empty recovery
inventory, reject foreign acquisition and main/root/lock substitution, and retain
unexpected WAL/SHM evidence. Private actual-owner tests preserve smaller caps and
current settings, reject unsupported mode/schema/attachments/active transactions,
and test checked overflow/deadlines plus a real extension-load taint in an isolated
process:

```
cargo test --locked -p yield-library --test legacy_journal_observation
cargo test --locked -p yield-library --lib operation_journal::store::legacy_allocation_observation::tests -- --test-threads=1
```

OPT-2116 remains open for owner-bound common-volume requirement composition,
including the original source reservation/control descriptor, alias accounting,
and all original acceptance. OPT-2117, OPT-2098 and OPT-1745 remain open for genuine
admitted implementation. Contract fixtures and these historical observations do
not complete migration, settlement or native physical qualification.

## Common original-owner observation (OPT-2116)

`LegacySourceReadOwner::capture_common_allocation_observation(profile,journal)`
composes the actual retained source reservation/read connection with the existing
journal/profile owner. One original cooperative two-second deadline is passed to
both observers; it is not renewed between participants or revalidation. The
extension exclusion remains held throughout. Original source image, journal
settings/bindings and control facts are revalidated before returning. Foreign
profile acquisitions, changed rows/objects/settings, conflicting units, duplicate
object identities, oversize controls and checked overflow refuse without writes.

The eight distinct logical objects are source main, DELETE rollback, future WAL,
future SHM, original source reservation lock, whole journal main, journal DELETE
rollback and original profile-owner lock. Main allowances already include their
retained original bytes. The initial fresh source lock must remain empty; changed control bytes refuse.
Each control descriptor is observed and conservatively allowed 4KiB; this enters no control-file write permission. Exact original file
identities and directory/direct-child identities for absent future sidecars remain
explicit. No object is relabeled as an unrelated recovery payload.

The retained main descriptor determines the main volume; the retained database
**directory** descriptor determines absent sidecar placement. These can differ.
An existing journal rollback descriptor must agree with its directory volume/unit
for this initial model; separately mounted/unknown rollback objects refuse. Distinct
objects are each counted once, while identical descriptor volumes aggregate in
stable device order. Unknown or inconsistent filesystem units never become zero
allowances. Returned sums are conditional **logical** allowances, without free
capacity, rounded physical charges, protected-floor claims or provider authority.
Native physical allocation/directory metadata/durability qualification remains a
separate precondition for the existing owner's future effect admission. The
explicit adoption command, capacity-class/claim bridge, entered-policy verification,
source transaction/receipt and separately accounted lease bootstrap remain OPT-2117.

Normal-build integration executes genuine source/journal/profile custody and checks
all eight objects, same-volume aliases, replacements, foreign acquisitions, changed
rows and byte preservation. Linux additionally runs a real bind-mount case in a
private user/mount namespace: the v11 main resides on `/dev/shm`, while its retained
database directory and sidecars reside on the workspace filesystem. A second
actual case independently mounts the genuine bootstrapped journal main on tmpfs
and reopens it under the original profile guard, with rollback on the workspace.
Actual open file/directory descriptors confirm distinct devices and correct
per-volume sums.
No parent mount is changed. This verifies descriptor placement, not physical
allocation or power-loss qualification. Positive pager fixtures apply only to the
known Linux/macOS VFS; unsupported targets continue to refuse observation.

```
cargo test --locked -p yield-library --test legacy_common_allocation
cargo test --locked -p yield-library --lib sample_sources::db::legacy_observation::common_allocation::tests -- --test-threads=1
cargo test --locked -p yield-library --test legacy_source_read_owner --test legacy_journal_observation --test profile_admission --test versioned_profile_migration
```

Ordinary v11 opens still return `MigrationRequired`. These observations create no
source lease/audit, operation ID, intent, claim or adoption receipt. OPT-2098 and
OPT-1745 remain open until genuine admitted implementation and original acceptance.


## Original-owner command preparation (OPT-2242)

`LegacySourceReadOwner::prepare_legacy_adoption_input` uses the original retained
source reservation/read connection and actual existing journal/profile owner.
It captures and revalidates the full common observation under one original
cooperative two-second deadline, holding process extension exclusion throughout.
Cancellation is checked before capture and before/after final revalidation. It
writes no database/control bytes and allocates no OperationId. The producer never
accepts decoded command bytes or caller-authored scalar observations as custody.

Version 1 freezes participant/source-command correlation, original profile owner
and root, media/database directories, exact bounded combined original schema/row/
physical digest, original semantic revision/row counts, source and journal pager
facts and all eight fixed-order logical object identities/device/unit/length/
conditional allowances. Source schema 11, intended target 13, journal schema 12
and bundled SQLite 3.50.2 are explicit. Future changes need a reviewed codec/policy
version; they are not silently adopted from a current schema stamp. Main and
future sidecar placement remain distinct; checked totals count each original
object once and reject conflicting volume units. They expose no free capacity or
rounded physical charge.

The binary envelope is `YLADOPT\0`, a little-endian u64 version and canonical
strict finite JSON. Its cap is 24KiB minus three bytes, so canonical base64 plus
JSON quotes occupies at most 32KiB, leaving at least 32KiB of the original combined
64KiB source-local request/receipt ceiling for a future actual receipt. Duplicate/
unknown supported fields, incorrect policy/arithmetic, ambiguous identities,
trailing/noncanonical data and unbounded input refuse. Bounded unknown envelope
versions remain exact historical bytes; successful decode never reacquires owners.

`SourceLegacyAdoption` is a distinct operation/command/work role. Work-plan version
3 contains one required `LegacySource` participant and one adoption obligation.
The pre-lease participant explicitly forbids a current `StoredSourceFence`.
Changed, omitted, extra or completed participants and V1/file topology coercion
refuse. Ordinary source/database revisions and lease epochs are not invented.

Existing initial operation codecs, full-progress observations/persistence, progress
construction and journal owner admission explicitly reject this passive family
until the genuine original-owner capacity-class/claim and durable adoption bridge
exists. Normal-build owner fixtures prove deterministic preparation, byte/row
preservation, cancellation and foreign/replaced/stale-owner refusals, strict wire
handling and topology isolation. Actual journal owner/store fixtures also prove
rejection before rows, unchanged recovery inventory and reopen retention. Neither
these tests nor the passive work plan certify migration or capacity.

```
cargo test --locked -p yield-library --test legacy_adoption_input
cargo test --locked -p yield-library --lib operation_journal::legacy_adoption_commands::tests -- --test-threads=1
```

OPT-2117 remains open for genuine per-volume capacity claims and durable intent,
retained writable promotion and entered policy, the preserving atomic migration/
receipt transaction, exact replay/uncertainty/recovery and separately accounted
lease bootstrap. OPT-2098/1745 and native allocation/durability acceptance remain
open. Normal v11 opens still return `MigrationRequired`; journal schema stays 12.

## Original Windows DELETE callback observation (OPT-2281)

`known_delete_sector_size` now validates the original private MAIN wrapper,
context, role and exact pinned native SQLite source before invoking its actual
sector callback on Windows as well as Linux/macOS. Windows uses the existing
reviewed `win32` native-prefix checker; Unix uses the retained descriptor checker.
Unknown/unowned files or unsupported sector values refuse. The observed sector
is a pager scalar, not the volume allocation unit or physical allocation policy.

The actual readonly v11 owner test invokes the callback repeatedly with all
ordinary SQL denied, preserving main bytes, live ownership and absent sidecars.
Existing source pager smaller-cap, changed-policy, deadline, unknown-owner and
extension-taint tests now compile on Windows too. Native execution is tracked
in OPT-2282; Windows production and all-test-target cross-compilation is separate
from execution. These observations do not implement admission, writable promotion,
migration/receipt/version stamping, lease bootstrap, retries/refunds or settlement.
Full OPT-2117/2098/1745 and physical qualification remain open.

## Windows retained-root observation opens (OPT-2283)

The bounded observation opener now supports Windows direct regular children.
It first requests only attributes through the original retained directory with
no-follow semantics. Disk type, regular/non-reparse attributes and nonzero native
identity are checked before `ReOpenFile` enables readonly data access through the
original handle. Original and new identities are compared while both handles are
live. Unknown shapes and alternate data streams refuse; no creation, write,
delete, path-based data reopen or ownership grant is requested.

The genuine Windows harness checks original-handle reads after rename/replacement,
write refusal, directories/nested/parent/stream refusal, symlink preservation and
character-device refusal. Native execution with genuine symlink privilege or
Developer Mode is OPT-2284; compile checks and portable record fixtures do not
execute Windows APIs. Existing Unix FIFO and staged-target producer tests remain
required. This producer supplies observations, not migration/publication admission,
qualified physical allocation, lifecycle slots, receipts, retries/refunds or
settlement. Full OPT-2117/2194/2215/2218 remain open.

## Original pre-lease journal custody (OPT-2286)

`OperationJournalClient::prepare_legacy_adoption_custody` transfers the genuine
`LegacySourceReadOwner` into one slot on the existing journal worker. It captures
the passive input against that worker's original store and profile acquisition;
no decoded request, root path or caller observation reconstructs the slot. The
original readonly SQLite connection, bound roots and locked fresh reservation
move together. This route opens no new SQLite connection and creates no intent,
current source registration, lease row or adoption receipt.

The worker retains 64 KiB of ordinary input credit on its existing aggregate
pool for frozen input and custody metadata. It transfers the already-owned
source connection; this metadata credit does not qualify native SQLite memory
or physical capacity. One occupied slot rejects further sources/commands. A
privately issued non-Clone, non-serializable observation queues revalidation
against the same worker, original profile/source/image and journal namespace.
It cannot authorize migration, bootstrap, release or source retirement.

Lost replies and dropped clients preserve the original slot, reservation lock
and metadata debit. Only actual journal shutdown drops this readonly custody,
before journal/profile teardown, and refunds transient metadata credit. The
sidecar remains evidence; dropping readonly custody does not delete it or reset
admission for another attempt. Source row changes, substituted roots, foreign
profile acquisition and closed/unavailable owners reject without repair.

Real owner/SQLite tests use the existing explicit no-capacity startup fixture:
they exercise the actual queue, original descriptors, bytes and lock lifetime,
without qualifying production journal reserve bootstrap. Full OPT-2117 remains
open for actual per-volume capacity claims, writable promotion, entered strict
policy, preserving migration/receipt transactions, recovery and separately
admitted lease bootstrap. Custody and passive input alone confer none of those
authorities; native durability/physical qualification remain separate.

## Complete original custody revalidation (OPT-2322)

The original journal worker revalidates the entire frozen legacy input through
its retained source, journal and profile owners under the existing cooperative
two-second preparation bound. The comparison includes both pager policies,
all eight object identities, devices, allocation units, observed lengths and
conditional allowances. A matching source row digest alone is insufficient.
Changed reservation control contents or newly retained journal rollback evidence
refuse without deleting or repairing them. Rejection retains the original source
slot, locked reservation and metadata debit until actual shutdown. Revalidation
remains readonly observation; it issues no capacity claim, migration authority,
retry/refund or settlement. Full OPT-2117 remains open.

Evidence preservation here covers refusal while the original journal owner is
retained. The source reservation survives shutdown. Existing confirmed SQLite
close can remove its own ordinary cold journal rollback file; this observation
does not promise a durable migration receipt or change that teardown behavior.

## Exact legacy DDL token boundaries (OPT-2327)

Known-v11 schema inspection compares the finite unquoted compiled ASCII tokens,
allowing case and whitespace variation while preserving declaration boundaries.
For example, SQLite accepts `T E X T` as a distinct type with NUMERIC affinity;
it cannot impersonate the required `TEXT` token. The bounded inspector borrows
DDL token slices without allocating a token collection. Unknown tokens, quoted
or commented alternatives, additional objects and unsupported versions still
refuse before migration or lease bootstrap. Genuine readonly-owner tests prove
both alternate-affinity refusal and harmless formatting acceptance with preserved
source bytes. This supplies no capacity, adoption receipt or physical qualification;
full OPT-2117/2098/1745 remain open.

## Original no-effect RW preparation (OPT-2341)

The original journal custody worker now prepares an existing RW MAIN connection
with an immutable connection-owned VFS fence installed before `sqlite3_open`.
Installing `query_only` after an unrestricted RW open would be too late to stop
SQLite's automatic hot-journal recovery. This preparer allows only the existing
MAIN without creation or delete-on-close. It refuses auxiliary opens, writes,
truncation, sync, deletion and SHM mutation before native callbacks, disables
mapped database access and rejects unreviewed native file-control operations.
The pinned SQLite core handles PRAGMA requests; a scalar setting cannot remove
the VFS fence. Extension loading and native system-call replacement are refused.
No API in this leaf disables the fence or issues a mutable admission grant.

The genuine source owner validates its original reservation, profile, roots,
native MAIN identity and exact frozen schema/row image before and after candidate
preparation. It preserves a smaller original page cap downward with exact readback. It retains the original readonly connection until the candidate
passes every check, then retains exactly one connection in the same source owner
on the existing journal worker. Ordinary read-owner construction remains readonly.
The entire journal preparation shares one cooperative two-second deadline.
The private candidate preserves the live original connection until its caller
returns or drops it. An initial handler refusal closes the source normally while
preserving source/control evidence; it installs no slot. Successful and lost-reply
custody retain the original slot, lock and metadata credit until normal shutdown. The original 64KiB input debit covers retained metadata. OPT-2356 adds its
derived 28,888-byte capture scratch debit before capture and retains both on
the worker (OPT-2349 additionally charges one separate bound MAIN reserve-root
inventory before transfer/capture); native SQLite allocator memory is not certified by these debits. Candidate construction temporarily holds
two SQLite connections, then retains one. This is no whole-process heap/RSS bound.

Actual SQLite tests turn off SQL `query_only` and still observe zero forwarded
native mutation callbacks and unchanged bytes/sidecars. A real process-death
fixture first proves dirty spilled MAIN and a genuine hot DELETE journal, then
proves fenced opening/reading does not replay or delete it. Independent ordinary
SQLite recovery establishes that the fixture's journal is genuinely recoverable.
These tests restrict effects; they do not qualify native physical allocation or
power-loss durability. The permanent fence prepares custody only. Actual claims,
durable intent, admitted mutable promotion, migration/receipt transaction,
recovery and separately admitted lease bootstrap remain open under OPT-2117.


### Original pre-lease volume and reserve custody (OPT-2349)

The existing journal worker installs volume/reserve custody for its original
retained legacy source. A borrowed, sealed proof retains the same source,
profile, original reservation, frozen source image and permanent no-effect VFS
fence. It checks those facts and the original cooperative deadline after native
observations and immediately before each bootstrap effect. The fixed reserve
child, native allocation/length/full-flush operations, marker truncate/each write,
and each profile/child namespace sync have separate checks. Slow native I/O is
not preempted; after it returns, an expired proof refuses the next effect.
Separately mounted MAIN requires an explicit qualified original reserve-root
binding retained by the source owner; a pathname or decoded volume does not
supply that binding.

The additional consumer reserves its metadata debit before capture or effects.
The bound names compiler-sized headers, at most two entry inventories, roots,
owners, reserves, closed acquisition/namespace failure capsules, 8192-byte path
capacity bounds, 256-byte native identity capacities, both original-root Arc
controls and the aligned Arc<File> control/payload allocation, three bounded argument
copies and encoding scratch, one transient context owner, four canonical path
backings, the fixed 16KiB passwd lookup buffer plus its Vec header, marker
backing, and two simultaneous target vectors (capacity at most
four). Observation-map scratch additionally uses the actual source-volume count
plus three, with a conservative complete pinned-Rust tree-node inventory per
volume. Each returned copied checkpoint has its own header plus thirteen
1024-byte string debit. The original 64KiB metadata and 28,888-byte source-capture
scratch, plus the new explicit MAIN reserve-root inventory, remain charged independently: schema, metadata and scalar row-reference
captures are sequential within that original scratch bound. The pool ceiling and
supported source input limits are unchanged. Native SQLite allocations, physical
reserve extents and process RSS are outside these Rust metadata bounds.

Failures retain the exact affected raw File, reopened OwnedReserve, native
registry/lock descriptors and original ancestors in a closed charged slot. Raw
recovery backing discards diagnostic strings and cached protected qualification;
it is not a qualified reserve and supplies no successful ownership capability.
Added reserve/failure backing closes before volume locks, original roots and the
original source; metadata credits remain charged through that teardown. Partial
reentry refuses without new effects or an unverified refund. Successful or
lost-reply custody remains on the original worker until normal shutdown.

Each confirmed binding prefix advances a private complete journal/source
checkpoint on that worker. Existing public handles remain frozen. Only complete
installation and final recapture issue a new passive checkpoint; stale handles
refuse, and an exact completed duplicate returns the current checkpoint without
another bootstrap. These checkpoints issue no current lease, capacity permit,
mutable migration grant, retry/refund or settlement authority. The full adoption
contract remains open under OPT-2117.

Linux/Windows retain the original unsupported physical-floor gate before reserve
namespace effects. Small native-file/callback fixtures prove sealed refusal,
exact backing retention and native lock/credit drop order; they do not qualify a
positive physical floor or two actual mounted volumes. Native APFS qualification
and separately mounted MAIN/two-volume acceptance remain OPT-2354. Existing
Current-source post-effect failure retention and equivalent effect-proof gaps
remain OPT-2352; its APIs and existing gates are preserved here.


## Original V19 initial claims consumer (OPT-2373)

`LegacyAdoptionCustody::admit` queues only the privately issued source command,
checkpoint digest and one deadline captured before enqueue. The worker requires
complete retained volume custody, the original full checkpoint and permanent
no-effect source fence. It rebuilds the funded durable claim image, reconciles
the existing provider and consumes the existing genuine descriptor/live witness.
Unknown physical bindings and incomplete reserves still refuse before intent.

The eight original positive object allowances are independently rounded at their
actual units, then grouped by volume/class: MAIN, rollback and reservation control
use SourceDatabaseCommit; prospective WAL/SHM use SourceWalShm; journal MAIN,
rollback and profile control use JournalRecord. MAIN appears once. The passive
estimator reports these exact three classes without granting admission. No
current SourceLeaseFence, WAL binding inventory or recovery-payload substitute
is constructed.

The retained slot owns one frozen V19 record, exact encoding, ordinary claims,
original provisional lease, selected settlement outcome and original metadata
credits before pin/SQL. Store revalidates record/claims and exact frozen bytes
before borrowing that lease through the original atomic settlement. Claimless
inserts and generic owned settlement reject V19. Outer V19 is limited to the
initial User/SourceLegacyAdoption/None intent; V1–V18 family rules remain strict,
future versions remain unsupported, and the existing SQL schema stays 13.

Confirmed commit marks the original lease durable. Unknown commit, failure after
commit, unconfirmed rollback and ledger failure preserve the slot and charge;
confirmed rollback follows the existing exact release rule. A second request
cannot recreate a failed or uncertain admission. A confirmed duplicate compares
the same private command/digest, source-only original checkpoint and exact
committed intent/claims and current durable/live provider image, and returns
that same OperationId without readmission. Released/missing claims refuse.
The source-only checkpoint excludes only changes to this worker's journal.
Lost replies retain the same state until actual worker shutdown; frozen data
and lease drop before source/reserve/profile custody and their credit.

The original bounded source capture/closed eight-role headers are prepaid on the
existing pool. Borrowed construction inventory funds actual command clones;
counted original wire length funds geometric encoded output before encoding.
Fresh observation maps use full pinned tree nodes per actual reserve and exact
borrowed provenance lengths. Existing funded snapshot, initial settlement,
complete-record readback and provider growth retain their own original credits.
Legacy validation checks its eight roles on the stack; transport streams the
same canonical binary header/JSON into base64 without temporary binary/base64
buffers. A real original-owner System allocation test verifies zero allocations
in that transport counter, in addition to the older historical reader regression.

Real portable SQLite/queued-worker tests cover shape/role accounting, historic
family rejection, source-only checkpoint and deadline, missing physical custody,
changed private digest, frozen-byte mismatch, committed readback, confirmed
rollback, unknown/after-commit outcomes and ledger failure. Logical provider
fixtures qualify arithmetic only. Native macOS tests use actual original reserve
bootstrap/live witnesses for commit, duplicate/lost reply and fault retention;
these tests have not run in the Linux environment. Native qualification,
including actual separately mounted roots, remains OPT-2354. No mutable source
grant, schema migration, completion receipt or current lease bootstrap is issued;
OPT-2117/2098 remain open for those consumers.
