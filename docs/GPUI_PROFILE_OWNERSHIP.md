# GPUI profile ownership

This document records the active fork's OPT-1336 ownership boundary. The
normative ordering remains in `IO_ARCHITECTURE_TARGET.md`.

The session acquires a `WritableProfileGuard` before starting the global library
writer or catalog service. A losing process receives
`ProfileOwnedByAnotherProcess`; it cannot silently start a writable participant.
The guard retains the operating-system lock and validates the live profile-root
and lock-entry identities. Replacement of either entry invalidates the owner.

Each successful acquisition has an opaque UUID recorded alongside the PID in
the version-2 diagnostic lock record. Clones retain that UUID; reacquisition
creates a fresh one. Old diagnostic contents do not prevent acquisition after
the operating-system lock is released. Neither the PID nor the UUID is proof
that takeover is safe, and neither replaces the retained lock capability.

The subprocess validation lane uses isolated profiles, verifies typed conflict,
and checks reacquisition after both orderly release and abrupt process exit
without Rust destructors. Helpers use bounded handshakes and clean up child
processes on failure. Same-process tests separately exercise root and lock-entry
replacement and cloned guard lifetime.

On 2026-09-12, the focused library ownership suite passed 10 tests (one helper
test intentionally ignored outside subprocess dispatch), and the session
subprocess regression passed. The session compilation, formatting and diff
checks passed on macOS. Windows runtime coverage was not executed locally.
These are profile-boundary checks, not source-lease or universal-journal
acceptance.

## Production database admission

Path-only source write opens and legacy global-library convenience opens return
`ProfileOwnershipRequired` in normal builds, before creating directories,
opening writable SQLite connections, or applying schema changes. This is an
admission error, not a conflict with another process. Read-only access remains
available. The catalog's explicit guard-bearing opener and the profile-owned
global writer are the production write routes.
Public file-operation recovery validates this same retained authority before
reaching filesystem finalization; a read-only journal handle cannot authorize
recovery mutations.

Authorized source databases retain a clone of the profile owner for their
entire lifetime. A snapshot write fence must also retain the profile owner and
database-root binding until its reservation is released, even if the original
database is dropped. Passing a guard only during construction is insufficient.

The macOS production admission integration suite passed six tests covering
path-only rejection without filesystem effects, retained profile and child-lease
lifetimes, root replacement, and read-only versus owned snapshot/recovery
admission. Focused suites passed: 9 snapshot, 26 journal, 27 database opening,
10 profile ownership, and 4 catalog profile tests. The full session suite passed
57 tests (one subprocess helper ignored outside dispatch); the existing
readiness process test passed with its fixture feature explicitly enabled.
The production GPUI check, formatting, diff check, and normal dependency-feature
inspection passed. Windows CI includes separate production admission and
fixture-enabled readiness lanes; execution evidence must be recorded separately.

The full UI and catalog suites also passed (394 UI tests, one existing ignored;
91 catalog tests). Tag-completion fixtures use the guarded catalog opener under
a canonical isolated profile rather than enabling fixture write access for the
UI dependency graph. Ownership tests read diagnostics through the retained lock
handle, matching Windows exclusive-lock semantics.
The complete library unit suite subsequently passed 536 tests (one subprocess
helper ignored outside dispatch). Windows validation passed in
[`34702160315`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34702160315),
against `1a8fbafa5`, including production profile admission, profile-owned
settings maintenance, audio/core/media/platform tests, GPUI compilation, and
browser state/interaction tests. The earlier run was cancelled after the test
fixes and is not acceptance evidence. This run predates the subsequent neutral
journal-model extraction.

Legacy fixture creation remains available only under library unit-test
configuration or the explicit `test-support` feature. That feature must not
appear in the normal GPUI dependency graph. The standalone `profile_admission`
integration lane exercises the library without fixture support; running only
fixture-enabled unit tests cannot establish production admission correctness.

This boundary does not implicitly acquire a profile, adopt a source, or enable
legacy standalone scanners to write. Their callers must use an owner-routed
workflow. An owned database may open a snapshot reservation through its retained
capability; a read-only database handle does not authorize that writable reopen.

## Remaining integration contracts

The active `operation_journal::OperationJournalStore` now retains a profile clone
and an exclusive child lease until its SQLite connection closes. It provides
bounded point reads and initial intent insertion with a monotonic sequence;
duplicate identities cannot overwrite existing records. Its receipt proves a
SQLite commit and post-commit identity checks, not coordinator admission or
filesystem publication authority. New intents require a consistent capacity
plan, but this store does not acquire capacity claims or queue permits.

The dedicated database uses DELETE journaling, exclusive locking, and
`synchronous=EXTRA`. Unix opening flushes the retained profile/child namespace;
Windows relies on the bundled SQLite VFS for committed file content and has no
independently verified directory-entry power-loss guarantee. SQLite's actual
open file is checked using platform file controls in addition to root and
pathname identity checks. Unknown headers, unsupported schemas, and unexpected
sidecars remain retained for audit. A pre-existing rollback journal is accepted
only when it is a regular file that is empty or has an entirely zeroed 28-byte
header. Its identity and inactive state are checked before SQLite executes SQL.
Active, truncated, replaced, and otherwise ambiguous sidecars remain retained;
this does not enable hot-journal replay.

A subprocess test commits an intent, is forcibly terminated, and proves that a
new profile owner can reopen the acknowledged record. The restart addition passed
32 store tests (plus one ignored subprocess helper), GPUI compilation, formatting,
and diff checks on macOS. Windows storage validation passed in
[`34709638982`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34709638982)
at `87887937b`, including the subprocess restart test and participant model.

The initial storage checkpoint passed 45 focused journal tests, GPUI compilation,
formatting, and diff checks on macOS. Tests cover normal reopen persistence,
retained ownership, duplicate IDs, malformed/oversized records, and replacement
both during opening and before writes. Windows storage validation passed in
[`34707094509`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34707094509)
at store commit `3534c37b6`, including journal tests and production profile
admission. Phase updates, participant execution, and recovery execution remain
open; later checkpoints below cover persisted participants and session lifecycle.

Recovery observation now uses indexed sequence paging with at most 16 rows and
2 MiB of encoded records per page. Malformed, unsupported, oversized, or
noncanonical-key rows remain typed audit observations; unusable sequence values
stop the scan rather than being silently skipped. SQLite's value-size limit also
bounds materialization before Rust inspects a damaged key. A late-cursor test
checks database work against a 4096-row prefix, and byte-budget tests verify
that deferred rows appear on the next page. All 55 operation-journal module tests
and GPUI compilation passed for this addition. This is read-only observation,
not legacy-file import, replay, participant reconciliation, or recovery admission.
Windows storage validation for recovery paging passed in
[`34708286537`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34708286537)
at `e17684aa2`.

The pure participant model adds bounded descriptors, source identity/revision/lease
observations, and one checkpoint per participant. Only optional rebuildable
artifacts can be deferred; required work cannot be silently dropped. Malformed
decoded inventories never report required completion. Eleven focused tests and
GPUI compilation passed. Schema V3 now carries these observations in a strict,
bounded initial universal-intent envelope. V3 requires a capacity plan, object
payload, stable status key, and pending participant checkpoints; it rejects
legacy execution evidence and all later phases until their transition contracts
exist. Unknown fields and unsupported command values remain retained raw bytes
for audit rather than being accepted as execution data; metadata-only commands
remain outside this foundation until their actor/family contract is defined. The V3 codec and store
do not implement live source leases, queue admission, or authorize phase
transitions.

The V3 addition passed 82 focused journal tests and GPUI compilation on macOS.
An additional recovery regression passed after review: an unsupported V3 phase
remains byte-for-byte unchanged while a later valid intent stays observable.
Windows storage validation passed on `aa135f587` in
[`34711107753`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34711107753).
Retry/cancellation metadata, typed command arguments, full multi-class capacity
claims, execution transitions, and recovery execution remain unfinished.

The GPUI session now opens the profile-owned operation journal after acquiring
the profile guard and before starting the global writer or catalog. It scans one
bounded recovery page; an empty page permits ordinary startup, while any row or
audit error reports `RecoveryUnavailable` and leaves both writable services
uninstalled. This is a temporary fail-closed gate until recovery execution and
phase transitions exist. The session retains the journal through maintenance,
catalog, and writer shutdown, then releases it before the final profile owner.
Journal rows are never deleted, replayed, or treated as execution authority by
this startup check. Focused session lifecycle tests cover empty-journal reopen,
retained/corrupt journal blocking, external catalog-handle drop ordering, and a
losing profile process leaving the journal untouched.
All 61 session tests and GPUI compilation passed on macOS; the pending-recovery
test also passed after review of the user-facing status wording. Windows storage
validation passed on `42610b214` in
[`34711763569`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34711763569).

A profile acquisition UUID is not a monotonic source-lease epoch. The physical
lease foundation below supplies OPT-1336's independent source-conflict contract;
OPT-1349 still owns propagation through asynchronous work and publication.
An unavailable profile must never be mislabeled as a source-lease conflict.

Source write batches now revalidate retained profile/database-child ownership
before `BEGIN` and immediately before commit. Direct content/manifest audit,
directory cleanup, and source file-journal mutations use the same checks.
Replacement during a batch rejects commit and rolls back metadata and revision
changes. WAL maintenance uses the already-owned connection and skips maintenance
when post-commit ownership validation fails; the legacy path-only checkpoint
entrypoint is deprecated and performs no writes. This does not yet validate a
physical source lease or database-file identity.

The addition passed 283 source database tests, including four ownership
regressions, plus two WAL tests and GPUI compilation on macOS. Windows CI now
includes the portable ownership and WAL regressions. After gating Unix-only test
imports, Windows storage validation passed on `449082c4d` in
[`34713357213`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34713357213).

Readiness writes now borrow the retained ownership context through
`SourceDatabase::readiness_store()`. Publication, work claims and completion,
artifact invalidation, and cleanup validate ownership before transactions and
before commit. In production builds, a raw `ReadinessStore::new` connection can
serve queries but returns `OwnershipRequired` for mutations. Fixture-only
unowned writes remain gated behind tests or `test-support`.

Validation passed 52 readiness tests in normal and test-support builds, the
production admission regression, and GPUI compilation. The ownership regression
invalidates the retained binding from a SQLite update hook after a real row
write and verifies rollback. Windows storage validation, including that portable
regression, passed on `9aa0a74e1` in
[`34714685207`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34714685207).
These checks establish profile
ownership, not physical source lease or main database-file identity authority.

Profile-owned source databases now retain a no-follow main-file identity proof.
Their opener validates the bound directory before file creation, opens SQLite
without `CREATE`, and checks the retained pathname and native connection identity
before pragmas, schema setup, and later mutations. Readiness borrows this proof;
snapshot writer connections must match the original identity and cannot recreate
a missing database. Replacement, unavailable checks, and native-handle mismatch
have typed errors. Legacy layouts requiring pathname migration are retained with
a migration-required error. WAL/SHM symlinks and nonregular entries are rejected;
regular WAL handling and the existing source SQLite configuration are unchanged.

The source and journal share the platform identity primitives while keeping their
own opening and recovery policies. Seven focused identity regressions and seven
production admission tests passed, including actual-write readiness rollback and
snapshot rejection with no output. The earlier local pass covered a 51-test source
database subset, 47 readiness tests, and 35 journal store tests (one subprocess
helper ignored). GPUI compilation and formatting passed. Windows storage
validation passed on `10bd70527` in
[`34717029864`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34717029864);
local Windows cross-compilation lacked the MSVC tools and headers. This
milestone does not implement physical source leases or qualify WAL durability.

## Physical source ownership

Profile-authorized source opens acquire a separate operating-system lock in the
database namespace before opening writable SQLite. An additive singleton lease
table binds the media root, database directory, main file, lock sidecar, profile
root and profile lock identities to an acquisition UUID and monotonic epoch.
The media root and database directory are distinct capabilities: profile-local
catalog databases keep their existing placement. An explicit guard-bearing
external-source opener allows independent profiles to contend for the same
physical database and receive a typed source conflict.

Snapshot fences share the retained lease. Ordinary connections close before
the final lease release, and its control connection closes before the OS lock
and profile owner. Consuming `SourceDatabase::close` reports release failures;
outstanding snapshots keep ownership until they drop. Implicit cleanup logs a
release failure and retains the durable record for conservative recovery.

Mutation admission validates identities and the durable owner/epoch. Healthy
leases avoid renewal writes; near-expiry or idle leases renew before beginning
the caller transaction. Commit checks cannot renew inside that transaction.
Renewal uses a retained control connection with a bounded busy timeout and
`synchronous=EXTRA`; macOS also requests and verifies `fullfsync`.
This is not a power-loss qualification of source WAL storage.

A released row can advance to a new owner. An expired active row can be reclaimed
only with matching live source identities and the same retained profile-root
and profile-lock identities. The OS locks must also be available. Expiry alone
cannot establish that a foreign profile owner is no longer live. Unknown schema,
missing prior lease state, identity replacement and malformed rows remain audit
errors; reopening cannot reset an established epoch or recreate its missing DB.
Lease schema and row reads are bounded and validated before control writes.

Explicit catalog source swaps consume the previous database owner. For a
profile-child database with no snapshot holders, the handoff closes the ordinary
connection and atomically advances the lease epoch, acquisition UUID and media
root binding through the retained control connection. The OS lock remains held;
the old and new root capabilities and unchanged database identity are validated
before commit. External databases cannot use this catalog-only transition.
Ordinary opens still reject a different root. Catalog audits check root identity
before maintenance so accidental replacement reports a stale-root outcome.

The ownership foundation alone did not fence asynchronous publication. The
OPT-1349 extensions below carry source identity through catalog delivery, browser
projection and readiness publication, including results queued outside the database.

The lease and handoff milestone passed 33 source identity tests, seven production
admission tests, all 91 catalog tests and 61 session tests (one subprocess helper
ignored outside dispatch). GPUI compilation, formatting and diff checks passed
on macOS. Tests include independent-profile subprocess conflicts, abrupt-owner
expiry recovery, unchanged malformed-state retention, actual control-connection
write counts for healthy renewal, and implicit drop with an outstanding SQLite
transaction. Windows storage validation passed on `c6dc01b11` in
[`34723711255`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34723711255),
including production admission, journal, source identity/lease and session/catalog
checks. This completes OPT-1336's ownership foundation; OPT-1349 publication
fencing is recorded below; OPT-1337 coordinator execution remains unfinished.

## Source result publication

`SourceDatabase::source_publication_fence` captures the owner UUID and epoch in
a weak fence. Keeping a result cannot keep its source lock alive. Validation
uses the retained control connection, checks persisted ownership and live
identities, and never renews or reopens by pathname. Dropping or handing off the
owner invalidates earlier fences.

Catalog projection publication checks the fence outside the admission mutex,
then rechecks lifecycle admission under that mutex before sending. Durable
curation and rating-decay receipts are still delivered for accounting when they
are no longer current, with `CatalogPublicationStatus::NonProjectable`. Browser
consumers drain these outcomes without updating projections or continuing work
from stale revisions/cursors.

The initial catalog-send boundary left post-queue projection and readiness
publication uncovered. Subsequent extensions below address those boundaries;
diagnostic policy and final acceptance are recorded below.

The initial publication slice passed three library fence tests, the 92-test
catalog suite before final regression refinement, two corrected live-admission
publication tests, and all 396 UI tests (one existing ignored). The receipt
regression uses an actual committed curation outcome and verifies it remains
unchanged when delivered as non-projectable. GPUI compilation, formatting and
diff checks passed. Windows storage validation passed on `c383a2ade` in
[`34749559530`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34749559530),
including production admission, source identity/publication fence tests and
catalog/session checks. Queued delivery and readiness claim fencing were added
in the following extensions.

The queued-delivery extension carries the same fence through
catalog/session delivery and asynchronous browser projection. Its UI boundary
reads a separate in-memory publication-generation token; that token owns no
source lease, SQLite connection, or filesystem lock. Background projection still
performs full validation before and after building the projection. Retired
terminal receipts remain available for accounting, while stale projections are
discarded. Readiness claim and independent artifact publication use the explicit
origin contracts described below.

Local validation of the extension passed three library fence tests,
94 catalog tests, 61 session tests (one existing ignored), and 396 UI tests
(one existing ignored), plus GPUI compilation. Three additional focused tests
passed after final review: owner retirement after enqueue preserves the exact
terminal outcome and reclaims all queue credits once; retirement between the
background projection checks rejects the projection while a live owner succeeds;
and an expired receive deadline stops stale-event draining without consuming a
fresh queued event. Formatting and diff checks passed. Windows storage validation
passed on exact `7df74e1ca` in
[`34750834289`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34750834289),
including production admission, journal/source mutation ownership and
catalog/session checks. Native Windows UI validation is outside that storage run.

## Readiness claim origin

The readiness claim extension retains a weak physical-source
fence in `ClaimedReadinessWork`. Logical target and claim generations remain
mandatory, but do not identify the physical database owner. Claim mutation first
compares the issuing lease with the receiving store's retained lease, before any
receiving-lease renewal. Current ownership validation may then renew a legitimate
same-owner idle lease, followed by full publication-fence validation. The checks
also run at the transaction's commit boundary.

An owned store rejects a missing or mismatched claim fence. Explicitly unowned
fixtures retain their legacy claim construction only in test/test-support builds;
they cannot receive an owned claim. Keeping a claim does not retain the source
database lock.

Local validation passed three focused claim-fence tests and the full library
suite (665 passed, three existing ignored). Tests cover every terminal mutation
after handoff, equal logical claims in simultaneously live databases, rejection
without renewing an expired receiving lease, and legitimate same-owner renewal.
The no-default production library check and seven profile-admission tests passed;
the test-support readiness process fixture passed (one helper ignored).
GPUI compilation, formatting and diff checks passed. Windows acceptance passed
on `84ab021fc` in
[`34752500946`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34752500946).
That run selected the full readiness suite (55 passed), including all three new
claim-fence regressions. The earlier successful `b9c83f399` baseline run used a
narrower filter and is not the regression acceptance evidence.

## Independent readiness publication

`ReadinessPublicationOrigin` captures an immutable expected source owner for work
before its result is produced. Capture is cheap and does not grant writable
authority. Artifact, full-target, cancel-aware full-target and delta publication
require that explicit origin; publication cannot silently bind a stale result to
a new owner. The receiving store checks identity before any lease renewal, then
validates current ownership and the original fence before mutation and commit.
Persisted artifact data remains unchanged.

Handoff and cross-database tests preserve existing targets, source revisions and
artifacts when the origin is stale. An expired receiving lease is unchanged on
mismatch, while the receiving owner's own origin can renew and publish. Retaining
an origin does not prevent source handoff. Unowned origins remain fixture-only;
normal raw connections cannot capture one.

Local validation passed two new origin regressions, the three claim regressions,
and the full library suite (667 passed, three existing ignored). Production
no-default compilation, seven profile-admission tests, the readiness process
fixture (one passed, one helper ignored), GPUI compilation, formatting and diff
checks passed. Windows storage acceptance passed on exact `fb278b708` in
[`34752810963`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34752810963).
Logs confirm both new origin regressions passed within the 57-test readiness
selection. Diagnostic and data-preservation acceptance is recorded below; no readiness executor
has been ported from the excluded reference application in this work.

## Source diagnostics and recovery policy

The binding layer distinguishes the external media root, the profile's child
database directory, and the profile root itself. Source database and scanner
adapters preserve that distinction into catalog errors; a second profile check
is not used to infer that a later profile failure actually means a child failure.
Diagnostics retain their specific cause and identify when automatic updates are
paused.

| Failure | Automatic recovery policy |
| --- | --- |
| Media root unavailable or confirmed replaced | Existing bounded root probes; no adoption of an unverified replacement |
| Source identity or lease authority uncertain | Pause automatic audit retries and root reopening |
| Database namespace, identity, schema or lease state uncertain | Pause automatic audit retries and root reopening |
| Profile ownership unavailable | Preserve the profile-specific failure and pause automatic retries |
| Active source writer conflict or explicit database busy | Existing bounded retries |
| Other transient audit/database failures | Preserve existing retry behavior |

Local validation passed the full library suite (671 passed, three existing
ignored), catalog suite (98 passed), and UI suite (398 passed, one existing
ignored). Production no-default library and GPUI compilation passed, as did
seven production profile-admission tests and the readiness process fixture
(one passed, one helper ignored). A final focused catalog test passed after
review preserved the specific retired/expired lease cause in diagnostics.
Formatting and diff checks passed.

The new filesystem regressions reject missing media before database bootstrap,
reject writes while media or a catalog child is absent, preserve the rejected
metadata state, and permit the same owner to write after the original directory
is restored. A combined first-lease regression preserves all seeded columns in
ratings/history, tags, collections, readiness sources/targets/artifacts and work,
and pending rename/file-operation recovery tables. Read-only access remains
usable before lease bootstrap and after release.

The parallel library run encountered a failure in the unchanged cwd-sensitive
`test_runtime::tests::panic_unwind_restores_environment_and_current_directory`.
That test passed alone, and the complete library suite passed with one test
thread. Windows storage acceptance passed on exact `2981d22e4` in
[`34755296770`](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34755296770).
The logs confirm the portable missing-media and first-lease preservation
regressions passed within 28 source identity tests, and the catalog mapping and
retry-policy regressions passed within 90 catalog tests. The run also passed
production storage/admission, 57 readiness tests, journal and session checks.
An initial Windows run stopped on a Unix-only test import; `2981d22e4` guards
that import. Native Windows UI validation is outside this storage run.

## Active publication coverage

In the compiled GPUI crate graph, catalog is the asynchronous publisher of
source database state. Scanner calls run through catalog's retained writer;
synchronous library mutation and maintenance APIs remain within their owner's
call stack. Catalog delivery and browser projection retain the source fence,
while readiness claims and independent publication require their captured
origin. Read-only media and content artifacts use their own content/revision
checks and do not publish authoritative source database state.

Together with the physical lease foundation and the validated publication,
readiness and diagnostic extensions above, this completes OPT-1349 for the
active GPUI implementation. This inventory excludes `reference/radiant`. The
future readiness executor and source-writer routing tickets must consume these ownership contracts when they
enter the active crate graph. This does not establish completion of those ports
or full GPUI feature parity.

## Journal coordination

OPT-1337 owns universal journal integration. The session now opens that journal
before the global writer and closes it before final profile release, but durable
operation admission, checkpointing, and recovery execution remain requirements
for the completed integration. Existing legacy journal recovery does not
establish a universal coordinator. OPT-1473 must consume the completed
contracts rather than bypass these remaining requirements.

Shutdown closes admission and stops maintenance/catalog work before closing
the global writer. Retained guard clones keep profile ownership alive until
their participants finish. Future journal and source-lease participants must
preserve this ordering and release the profile owner last.

### Cancellation request persistence

The journal records a cancellation request while a user operation
is still at `IntentDurable`. The V4 record retains the universal intent,
capacity and pending participant observations, adds a typed request timestamp,
and uses the nonterminal `CancelRequestedBeforePublish` disposition. Existing
V1/V2/V3 records retain their encoding and interpretation.

The store preserves immutable intent data, compares the expected row sequence
before a changed update, advances the global observation sequence only for an
actual update, and acknowledges an identical persisted replay without sequence
churn. Namespace and connection checks surround the transaction, and success
requires commit and readback. A cancellation request neither proves cleanup nor
releases capacity, publishes files, or authorizes recovery execution. The
session's retained-operation startup gate remains closed.

Local validation passed: 98 journal tests (one subprocess helper ignored),
61 session tests (one helper ignored), the production library check without
default features, GPUI compilation, formatting and diff checks. Coverage includes
strict V4 decoding, stale/conflicting requests, exact replay, rollback, recovery
cursor movement, and acknowledged cancellation surviving abrupt process exit.
Independent review found no meaningful issues.
[Windows storage validation](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34756869645)
passed at exact commit `c800405dbc3f9263d4cc58ed31d2cdf7a4c032e5`, including
88 journal tests and the abrupt cancellation restart regression, 90 catalog tests
and 60 session tests. This storage run is not native Windows UI acceptance. Full
coordinator admission, phase transitions and recovery execution remain incomplete.

### Typed coordinator envelope

The journal supports a strict V5 envelope for typed command arguments
and retry observations. Its initial Copy contract covers one regular file between
registered sources, including within-source copies. Source and destination
references must resolve to stored participant fences; relative locators remain
non-authoritative hints. Existing V1–V4 formats are retained without inferred
command arguments or automatic conversion of legacy recovery evidence.

Typed persistence and retry updates do not establish runtime queue admission or
protected-capacity enforcement. Those require live owners and the dependent
capacity integration; persisted plans and retry lease observations are not
execution capabilities. The session retains its nonempty-journal startup gate.

The source-local file journal remains a compatibility participant. Its existing
Move/Copy rows can lack the source, content and capacity observations required by
the new envelope. Compatibility import must retain such evidence for explicit
reconciliation instead of manufacturing a valid new command from stored paths.

Copy intent persistence constructs the stored envelope from validated inputs. Retry
updates use the expected observation sequence, increment the attempt count exactly
once and retain immutable command data. An identical persisted retry is a
read-only replay; stale or conflicting updates leave the row unchanged.
Cancellation retains the command and prior retry observation, rejects impossible
retry chronology and prevents further retry updates.

Local validation passed: 107 journal tests (one helper ignored), three restart
tests (one helper ignored), 61 session tests (one helper ignored), the production
library check without default features, GPUI compilation, formatting and diff
checks. Review identified and repaired V3/V4 foreign-field classification and
retained retry chronology gaps; dedicated regressions cover both.
[Windows storage validation](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34758799969)
passed on exact `8fb486ff436e03d6e295f64439d5ede6fafce727`: 97 journal tests,
including retry restart and both review regressions, 90 catalog tests and 60
session tests. This is storage validation, not native Windows UI acceptance.
Other command families, later phase transitions, runtime
admission and compatibility import remain open under OPT-1337 and its dependent
tickets.

### Bounded legacy journal observations

OPT-1337 provides a separate paged reader for source-local `file_ops_journal`
evidence. Its results distinguish decoded legacy observations from retained
malformed or oversized rows; neither outcome grants execution authority or
establishes a valid V5 command. Each page is bounded by row count, decoded text
bytes and diagnostic preview bytes. Signed rowid ordering preserves visibility
of zero and negative rowids as well as ordinary rows.

The reader uses non-renewing validation of the retained source connection and a
weak owner fence. Its cursor captures an upper rowid boundary but is not a durable
import watermark or a snapshot across pages. Existing reconciliation remains
unchanged. Durable import requires a separate profile-owned transaction that
atomically records duplicate detection and the corresponding imported evidence;
it must preserve the original source-local row and must not infer missing
command arguments or execution authority.

Each page uses a read-only rollback transaction so metadata preflight and bounded
materialization share a consistent SQLite snapshot. Existing caller transactions
are rejected without being consumed. Preview truncation is UTF-8 byte bounded.
Local validation passed: all 12 reader tests, all 38 file-journal tests, the 12
reader tests without default features, production library compilation without
default features, GPUI compilation, formatting and diff checks. Regressions cover
4,096-row paging, signed rowids through `i64::MAX`, concurrent same-length updates,
weak cursor invalidation after reopening, and reads that do not renew the lease.
The dedicated Windows reader step passed all 12 tests in
[storage run 34760528283](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34760528283)
on exact `d0704c333daefa17d1aee4463b0d414fa5a64287`; the full storage run
passed as well. This is storage-scope acceptance, not native Windows UI acceptance.

### Typed journal status snapshots

OPT-1337 provides compact immutable status snapshots from scanner-produced
recovery entries. Observed phase/disposition is kept distinct from retained audit
evidence. Retry and cancellation summaries reflect only stored observations;
legacy missing retry fields are not synthesized, and requested cancellation is
not presented as completed cancellation.

The projection carries no payload, execution capability, ownership handle or
permission to retry. Status presentation uses bounded text and stable retention
diagnostics while original recovery evidence remains available in the journal.
The session recovery gate and persisted record formats remain unchanged.

Status snapshots preserve the validated phase/disposition and observation sequence
without cloning the opaque record payload. Retained reason categories remain
typed, while malformed-record diagnostics and invalid key previews use safe
status wording instead of copied parser content. Original recovery entries remain
unchanged. Local validation passed: 111 journal tests (one helper ignored),
including three focused status tests and the key-retention regression, production
library compilation without default features, GPUI compilation, formatting and
diff checks.
[Windows storage validation](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34761336321)
passed on exact `7e6e39d997b17a2984d8a4811b7f048549e11292`, including
101 journal tests and the status, legacy-field and key-retention regressions.
This is storage-scope acceptance, not native Windows UI acceptance.

### Intent write outcomes

Before runtime admission can manage provisional claims, callers need to distinguish
a failed persistence attempt from a commit whose acknowledgement was lost.
OPT-1337 provides opt-in typed insert outcomes while retaining the existing
Result-based APIs as adapters. Failure observations distinguish work not attempted,
a confirmed pre-commit rollback, an unconfirmed rollback, an unknown COMMIT
outcome, and a reported commit with failed post-commit validation/readback. None
of these failure values carries a success receipt or grants capacity-release
authority; even a failed attempt does not prove that no preexisting row exists.

The shared insert path validates its namespace immediately before COMMIT.
The full public boundary rejects an active SQLite transaction, preventing a
failed rollback from exposing uncommitted rows through point reads or recovery
scans. Internal transaction validation remains separate from that public guard.

Independent durability review found no material issues. Local verification passed
all eight typed insert tests, including the final test-only addition; the
serialized journal suite passed 118 tests (one helper ignored) before that last
addition. Production library compilation without default features, 61 session
tests (one helper ignored), GPUI compilation, formatting and diff checks passed.
Fault coverage includes confirmed/unconfirmed rollback, commit errors with and
without a persisted row, post-commit acknowledgement failure, duplicate rows,
actual pre-commit namespace replacement and the V5 Copy adapter. Windows storage
validation [34762689278](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34762689278)
passed at exact commit `6b1344049c18c4f28638b17b839bfddf159ce09a`. Logs confirm
107 journal tests (one helper ignored), including the conservative commit outcome,
unconfirmed rollback, duplicate and V5 Copy outcome regressions. The namespace
replacement tests are Unix-only and were covered locally. The full storage run
also passed; this does not establish native Windows UI acceptance. Runtime
queue/capacity admission remains unimplemented.

### Bounded journal storage owner

Session persistence moves the already-open, recovery-checked journal into one
storage worker. The boundary bounds both queued payloads and outstanding results,
serializes journal operations, and drains submitted writes before releasing the
journal child lease. Client and result handles do not extend the profile lease
after the worker has stopped.
Dropping a result abandons observation, not the submitted write.

The implementation uses 32 queue slots, a 4 MiB queued-input budget and 32
outstanding result permits shared by all clients. Initial records use the bounded
journal codec before enqueueing; retry and cancellation observations are validated
and serialized into owned bounded bytes. Result permits remain charged until the
caller consumes or drops the result handle. The worker blocks on its command
channel while idle; closing the last internal sender wakes it to finish draining
without depending on external clients being dropped.

Session availability requires the journal worker to be available. Shutdown uses
an atomic lifecycle transition so a concurrent worker panic cannot be overwritten
by `Closing` and reported as a clean drain. Worker failure remains `Unavailable`
for retained clients after resource release. Independent review confirmed the
repair to this race; a deterministic test pauses shutdown after its lifecycle
read, triggers a real worker panic, then resumes the compare-and-swap.

Local validation passed nine owner tests. They cover queue/result saturation and
refunds, abandoned results that still persist, retained-client shutdown and lease
release, invalid decoded inputs, the shutdown race, and a successful V5 Copy
intent/retry/cancellation/recovery round trip. The broader journal suite passed
128 tests (one helper ignored) before the final two regression replacements;
61 session tests (one helper ignored), production library compilation without
default features and GPUI compilation also passed. Windows storage validation
[34764655033](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34764655033)
passed at exact commit `96ff3439acaa681dd6528989d21b235b31068c4e`. Its logs confirm
all nine owner regressions within 116 journal tests (one helper ignored), including
the shutdown race and successful V5 retry/cancel round trip. The full storage run
also passed, including 90 catalog and 60 session tests. This is storage-scope
acceptance, not native Windows UI acceptance.

This storage queue is distinct from universal operation admission. Submitting a
storage request does not acknowledge an operation as `Accepted`, reserve disk
capacity, or authorize a filesystem participant. The existing nonempty recovery
gate remains closed, and the session must stop maintenance, catalog and global
writers before stopping the journal owner and releasing profile ownership.

## V6 regular-file command envelopes (OPT-1337)

The explicit V6 wire format carries tagged Copy, external Import and Duplicate
Same arguments. V1–V5 retain their existing wire shapes and reject foreign V6
fields. Import binds an observed external root UUID, physical root identity,
relative locator and file expectation to a required filesystem participant and
a fenced destination source participant. It does not invent a source catalog
revision for an external file. Duplicate Same requires one source participant,
one source identity, the same parent directory and a distinct destination leaf.
All three commands retain no-replace collision semantics.

V6 supports the existing initial durable intent, retry and pre-publication
cancellation observations. Sequence-checked transitions preserve the typed
arguments and schema across exact replay and restart; changed stale requests
leave both the row and sequence unchanged. The generic bounded journal owner
can persist and recover an Import envelope. Unknown variants, role mismatches,
invalid nested identities and foreign fields fail closed.

Local validation passed 135 journal tests (one helper ignored), including ten
owner tests, plus production library compilation without default features, GPUI
compilation, formatting and diff checks. Independent compatibility and durability
review found no meaningful issues. Windows storage validation
[34766605426](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34766605426)
passed at exact commit `54e91717e118bd207461e68573ef80b5f346993d`. Its logs confirm
the V6 compatibility, owner Import and retry/cancel regressions within 123 journal
tests (one helper ignored), plus 12 legacy reader, 90 catalog and 60 session tests
(one session helper ignored). This is storage-scope acceptance, not native UI
acceptance.

These are stored command observations, not runtime admission or permission to
read or write a file. The SQLite schema and legacy default record version remain
unchanged. Duplicate Double, rendering/edit recipes, later execution checkpoints,
live capacity ownership and startup replay remain separate implementation work.

## V7 exact WAV edit observations (OPT-1337)

The next explicit journal envelope adds the seven operations already supported by
WAV edit preparation: gain/fades, reverse, mute, trim, crop, whole-file slide and
signed crop with silence padding. It retains the source participant and relative
locator, strong file identity and change marker, optional exact modification time,
source format/frame count, exact selection, operation parameters and float32 output
layout. It does not persist the absolute media path or display normalization gains.

Edit parameters use validated IEEE-754 bit patterns so replay preserves signed
zero and does not depend on decimal float formatting. Existing interactive core
constructors retain their normalization behavior; additive exact constructors
reject out-of-domain or nonfinite parameters without clamping. Source and output
WAV layout arithmetic is checked separately. Reverse and mute preserve the whole
file; trim and crop change its length. Empty decoded selections are invalid.

The media adapters perform no filesystem I/O. Reconstructing a preparation request
compares the caller's context against the stored revision, format and frame count.
This equality is an observation check, not proof of current ownership. The existing
renderer still checks the live source before decoding, before output and after
finalization. A successful prepared output remains unpublished.

This milestone remains at initial durable intent with retry/cancellation
observations. Replacement qualification, history backups, later execution
checkpoints, live capacity ownership and recovery execution remain future work.
V7 also preserves Copy, Import and Duplicate Same, while the existing V6 entry
points still emit V6 and reject edit arguments. Later-state authority is unchanged.

Local validation passed 142 journal tests (one helper ignored), including 11 owner
tests; four media adapter tests; 16 core edit-settings tests; and 19 existing WAV
edit preparation tests. The adapter timestamp regression includes both exact i64
limits and rejection beyond them, rather than saturating. Production no-default
library and GPUI checks, formatting and diff checks passed. Independent review
found no meaningful issues after the range/layout repairs. Windows validation is
pending; its storage lane now includes the core edit-settings and media adapter
tests.

The first V7 Windows run
[34768644704](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34768644704)
passed 130 journal tests (one helper ignored), the legacy reader, lifecycle checks
and all 16 core edit-settings tests. Its adapter timestamp fixture failed because
it assumed `SystemTime` could represent one nanosecond on Windows. Rust documents
[100 ns Windows intervals](https://doc.rust-lang.org/std/time/struct.SystemTime.html).
The repair uses representable platform times and tests exact nanosecond limits in
the shared checked-duration arithmetic separately. All five adapter tests pass
locally after this repair; a new exact-commit Windows run is required.

The repaired V7 Windows storage run
[34769426219](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34769426219)
passed at exact commit `c093ef7e1265faa4c664d1344902d0a63b7e046a`. Its logs confirm
all five media adapter tests and 16 core edit-settings tests, including platform
precision and exact-duration limits. It also passed 130 journal tests (one helper
ignored), 12 legacy reader tests, 90 catalog tests and 60 session tests (one helper
ignored). This accepts the V7 storage and pure-adapter milestone; it does not
establish native UI or filesystem-publication acceptance.

## V8 namespace command observations (OPT-1337)

The next envelope adds Rename, Move and Create Folder in a separate namespace
command module. Existing entries carry file/directory expectations, physical
identity, source revision and optional signed modification time; files also carry
length. Destination parents use explicit Root or Child locators and observed
directory identities. A root parent must match its source fence. Parent-plus-leaf
bounds are checked together, including the retained leaf of a move.

Rename stays within the same source participant and parent. File moves may cross
sources; folder moves require the same observed root and database. Physical-root
equality is also used to reject known same-target and descendant hazards across
source-ID aliases. Known source-directory/destination-parent identity equality
is rejected even when their relative locator spellings differ. These observation
checks do not replace live capability, alias and containment validation.

All namespace commands have fixed no-replace semantics. Source roots cannot be
renamed or moved, and no-op requests are rejected. Full participant inventories
are validated on the standalone command paths as well as the record boundary.
Folder manifests are not created or embedded before IntentDurable; any later
inventory belongs to prepared evidence. V8 retains all V7 command variants, while
V6 and V7 reject namespace arguments. Retry/cancellation remain intent-only.

Independent review identified a missing same-database folder-move guard. The
repair and isolated same-root/different-database regression were verified, with
no remaining findings. Local validation passed nine namespace tests, 38 codec
tests, targeted V8 store/owner tests, production no-default library and GPUI
checks, formatting and diff checks. The final serialized journal suite passed
157 tests (one helper ignored).

The initial serialized suite exposed an existing subprocess-test protocol flaw:
with inherited `RUST_TEST_THREADS=1`, libtest appended the helper's `READY` message
to its inline test-name prefix, so the parent ignored it and timed out. A raw
stdout probe confirmed this occurred after the helper persisted its record. The
helper now starts its protocol message on a new line and explicitly uses one
child test thread. All three abrupt-restart tests and the full serialized suite
pass without changing their timeout, strict parsing, abrupt-kill or reopen checks.
Windows storage validation
[34771230509](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34771230509)
passed at exact commit `10bfc6bb4ca25f4d9a12dc4f8902e0ca2ca7ae71`. Logs confirm
145 journal tests (one helper ignored), including all nine namespace tests, V8
codec/store/owner coverage and all three abrupt-restart regressions. The lane
also passed 12 legacy-reader, 90 catalog, 60 session (one helper ignored), 16 core
edit-settings and five media-adapter tests. This accepts the V8 storage milestone;
native Windows UI and filesystem execution remain outside this validation scope.

## V9 whole-file render observations (OPT-1337)

V9 adds a separate render command envelope for the original whole-file Normalize
and DuplicateDouble behavior. Normalize records one in-place source endpoint,
strong file revision, source WAV spec, full frame count, and explicit Float32
output preserving rate and channels. DuplicateDouble records source and distinct
destination endpoints, strong source revision/spec/frame count, explicit output
spec preserving the original encoding (including 8-bit, 24-bit and 32-bit
integer), and NoReplace. Its checked frame and WAV arithmetic matches the pinned
hound writer: PCM RIFF overhead is 36 bytes and extensible overhead is 60 bytes.

DuplicateDouble accepts cross-source and different-parent destinations. It
rejects a known same physical root/relative entry, including source-ID aliases.
The destination metadata policy is Fresh because the original registers a new
file entry after writing; no inherited metadata or snapshot reference is
embedded. Both commands require fenced SourceDatabase endpoints and a required
Filesystem participant. These are bounded observations only: no filesystem
execution, capacity reservation, source lease, or publication authority is
persisted.

V9 preserves all V8 command variants and remains IntentDurable with None,
RetryPending, and pre-publication cancellation observations. Codec raw bounds,
per-version allowlists, direct and JSON foreign-version rejection, SQLite
persistence/restart, CAS retry/cancellation, and generic owner persistence are
covered by local tests. The serialized `operation_journal` suite passed 173
tests with one helper ignored, including 10 render and 42 codec tests. Production
no-default library and GPUI checks, formatting, and diff checks passed;
independent review found no remaining issues. The existing V1 default and SQLite
schema remain unchanged.

Windows storage validation
[34773388901](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34773388901)
passed at exact commit `ba0ceacf21d2d6c7a205a2ac5a60eb8ee2871953`. Logs confirm
161 journal tests (one helper ignored), including all ten render tests and V9
compatibility, malformed-record, store replay and owner persistence coverage.
The lane also passed 12 legacy-reader, 90 catalog, 60 session (one helper
ignored), 16 core edit-settings and five media-adapter tests. This accepts the
V9 storage milestone; native Windows UI and filesystem execution remain outside
this validation scope.

## V10 removal observations (OPT-1337)

V10 distinguishes configured Trash from StageRetainedFolder. Trash accepts a
file or folder source plus an opaque configured-root binding, observed root
identity and required Filesystem participant. Its destination policy is fixed
NoReplace. StageRetainedFolder accepts only a non-root directory and requests
source-local owned recovery storage, also with fixed NoReplace semantics that
preserve an occupied recovery target. Both validate the full participant
inventory, source/database identity and captured source revision. Known exact
self-trash and source entries already inside the fenced trash-root namespace
are rejected; other physical containment requires later live validation.

These initial requests carry no actual destination leaf, staging locator or
metadata snapshot. The universal OperationId is their planned recovery
correlation key. Before any physical move, a later Prepared checkpoint must
bind the chosen locators and durable restore material. Verified output identity
is recorded afterward. Existing configured-trash code does not already provide
that durable restore record, and this milestone does not execute that code or
create recovery material. Protected-source, folder-lock and user-confirmation
admission guards remain required above this storage boundary. Existing retained
Restore/Purge resolution is a separate remaining command family.

V10 retains all prior command families while V6 through V9 reject Removal in
JSON and directly constructed records. V6 now uses an exhaustive variant match
so later command additions require an explicit compatibility decision. Local
validation passed 185 serialized journal tests (one helper ignored), including
removal, codec, retry/cancellation/restart and owner coverage. Library checks
with and without default features, GPUI compilation, formatting and diff checks
passed. Independent review identified the missing explicit fixed collision
policy for retained-folder staging; the policy is now exposed and documented,
and its focused regression passed. No other findings were reported. This adds
no filesystem executor. The V1 default and SQLite schema remain unchanged.

Windows storage validation
[34775353882](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34775353882)
passed at exact commit `e6b292c5403ec8e5cfe32e1e9d90a0e1b4ff2164`. Logs confirm
173 journal tests (one helper ignored), including all six removal tests and V10
codec, prior-version rejection, owner persistence, retry/cancellation and restart
coverage. The lane also passed 12 legacy-reader, 90 catalog, 60 session (one
helper ignored), 16 core edit-settings and five media-adapter tests. This accepts
the V10 storage milestone; native Windows UI and filesystem execution remain
outside this validation scope.

## V11 retained-folder resolution observations (OPT-1337)

V11 adds distinct RestoreRetained and PurgeRetained commands for an existing
legacy retained-folder record. The original location is only a bounded relative
path: the deleted directory may be absent or independently recreated. The staged
directory supplies the observed physical identity, source revision and fenced
SourceDatabase participant. Its source-relative path must match
`.wavecrate_delete_staging/<staged_relative>`, with composed path bounds checked.
Original paths in the reserved recovery namespace, including ASCII case aliases,
are rejected. These lexical checks do not replace live capability-bound
containment and identity checks.

The observation retains the legacy UUID, Deleted stage and a fixed BLAKE3 digest
of the exact selected-entry JSON bytes, including restore metadata. Legacy redo
reuses IDs; the digest detects changed bytes but is not a unique generation token.
The pure helper bounds each hashing call to 64 KiB and stores only the 32-byte
digest. It neither isolates nor parses the entry. A future capture adapter must
derive the fields and digest from the same complete entry; oversized input must
remain retained rather than be truncated. Large entries require a separate
bounded streaming capture path. Serialized admission and fresh source revision,
record and staged-directory identity checks are still required before execution.

Restore has a fixed PreserveConflictsKeepBoth policy. The later Prepared record
must bind the actual merge plan, conflict destinations and restore stamp before
any effect. Purge records an explicit UserConfirmed decision, not a live deletion
capability or protected-source override. RestorePendingDb belongs to recovery of
an existing action and cannot start a new restore or purge request.

This milestone remains initial-intent storage with retry and pre-publication
cancellation observations. It does not execute restores or purges, capture legacy
metadata, reserve live capacity or enable runtime admission. V11 preserves prior
command families; V3 through V10 reject the new kinds and arguments in raw JSON
and directly constructed records. Local validation passed 199 serialized journal
tests (one helper ignored), including eight retained-observation tests, 50 codec,
49 store and 15 owner tests. Library checks with and without default features,
GPUI compilation, formatting and diff checks passed. Independent review found no
meaningful correctness issues. The V1 default and SQLite schema remain unchanged.

Windows storage validation
[34777623562](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34777623562)
passed at exact commit `5e30d9a761a9e3a297284de19d6fef8b85ddf5c8`. Logs confirm
187 journal tests (one helper ignored), including all eight retained-observation
tests and V11 codec, owner persistence, retry/cancellation and restart coverage.
The lane also passed 12 legacy-reader, 90 catalog, 60 session (one helper ignored),
16 core edit-settings and five media-adapter tests. This accepts the V11 storage
milestone; native Windows UI and restore/purge execution remain outside this
validation scope.

## V12 selection-normalization observations (OPT-1337)

V12 distinguishes selection normalization from V9 whole-file normalization. Its
intent records a source endpoint, strong file revision, source WAV specification,
complete frame count, exact non-empty half-open selection, resolved edge-fade
frame count and explicit Float32 output specification. The output preserves the
source frame count, sample rate and channel count. The fade cannot exceed half
the selected frames, and output WAV arithmetic remains checked.

The fixed recipe normalizes the selected interleaved samples to full scale and
crossfades their edges against the original samples. The start blend runs from
original to normalized; the end runs from normalized to original, including the
original one-frame end-fade behavior. Persisting resolved frames avoids
reinterpreting UI duration settings during recovery. This does not claim that
the original peak helper sanitizes all non-finite audio.

The new family does not broaden V7 waveform-edit recipes or V9 render variants.
These are bounded observations, not a captured audio buffer, computed peak or
gain, live capacity reservation, replacement capability or new GPUI command.
Actual audio preparation and UI capture remain separate implementation work.
V12 preserves the earlier command families while older schemas reject the new
selection-normalization arguments. Local validation passed 210 serialized journal
tests (one helper ignored), including argument bounds, malformed records, prior
version rejection, retry/cancellation/restart and generic owner persistence.
Library checks with and without default features, GPUI compilation, formatting
and diff checks passed. Primary review found no remaining issue in the new typed
contract and version wiring. The V1 default and SQLite schema remain unchanged.

Windows storage validation
[34778990457](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34778990457)
passed at exact commit `c8a8b71b50994a645c58251a525d280982036147`. Logs confirm
198 journal tests (one helper ignored), plus 12 legacy-reader, 90 catalog,
60 session (one helper ignored), 16 core edit-settings and five media-adapter
tests. This accepts the V12 storage milestone; native Windows UI and audio
execution remain outside this validation scope.

## Shared V6–V12 codec envelope (OPT-1337)

The tagged command schemas share private owned and borrowed envelopes, initial
record construction and strict raw-field validation. Version-specific command
gates, diagnostics and encoder errors remain explicit. This changes neither
the twelve serialized fields and their order nor the V1 default or SQLite schema.

A frozen 53-case byte-length/BLAKE3 manifest from
`c8a8b71b50994a645c58251a525d280982036147` checks every supported schema/family
boundary. Negative baselines retain the V6 waveform diagnostic and required
nullable retry/cancellation fields. Local validation passed 212 serialized
journal tests (one helper ignored), library checks with and without default
features, GPUI compilation, formatting and diff checks. Independent compatibility
review found no meaningful issues.

Windows storage validation
[34866551245](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34866551245)
passed at exact commit `6c75d2fd8f04b62f2e111f9ea34ddcc67e9e45ab`, including both
frozen compatibility baselines and 200 journal tests (one helper ignored).
The lane also passed the catalog, session, core edit-settings and media-adapter
checks. This is storage acceptance, not native Windows UI or audio execution.

## V13 source-owned WAV selection output (OPT-1337)

V13 adds `SelectionOutput::Extract` and `SelectionOutput::ExtractAndTrim`.
The source observation contains its source-database revision, strong file
revision, WAV layout, total frames and exact half-open selection. The destination
is a concrete source-relative endpoint with fixed NoReplace policy. Source and
destination cannot identify the same entry under an observed physical root.

The raw recipe preserves the source format, including the observed bounded
`fmt ` chunk length and both format/data padding bytes. Checked arithmetic
rejects RIFF, block-alignment and byte-rate overflow. Before staging, the owner
must recapture the actual format bytes under the strong source revision; the
stored length is not a live header proof. Gain bits and edge-fade frames are
resolved observations. Validation bounds the fade by half the selection without
recomputing a duration from current settings.

The compound intent specifies extraction verification/publication before
trimming the same range from the original. Its trim output is Float32 at the
original channel count and rate, and cannot remove the whole source. This
captures the intended order; later coordinator checkpoints must enforce it.
Both variants use one operation ID and retain the existing Extract or
DestructiveEdit command kind respectively.

The intent also captures the output's one-shot/loop playback type. Extracted
metadata follows the existing fixed Keep1Unlocked policy, so recovery need not
consult a later loop setting. This records the metadata recipe without applying
source or global metadata writes.

This slice does not capture audio buffers, execute filesystem mutations, reserve
capacity, or enable a GPUI action. Decoded-source output and external export/handoff
remain separate command work. Local validation passed 224 serialized journal
tests (one helper ignored), including owner write/read, restart, retry/cancel,
old-schema rejection, metadata capture and odd-padding coverage. The library
check without default features, GPUI compilation, formatting and diff checks
passed. Primary review checked the version gates, participant binding, output
arithmetic and resolved metadata/fade semantics.

Windows storage validation
[34871875304](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34871875304)
passed at exact commit `6381dd2e1cd32c6b3804a95659d7fb94c0dd3a97`. Logs confirm
212 journal tests (one helper ignored), including V13 codec, generic owner,
selection-output and compound restart coverage. The lane also passed 12 legacy
reader, 90 catalog, 60 session (one helper ignored), 16 core edit-settings and
five media-adapter tests. This accepts the V13 storage slice; native Windows UI
and audio/filesystem execution remain outside this lane.

## V14 transaction-history observations (OPT-1337)

V14 records bounded Undo and Redo requests as User/FileHistory operations. A new
runtime-session history scope distinguishes the existing execution and transaction
counters without treating those counters as durable operation IDs. Ordered action
lists are limited to 32 actions, 128 total folder relocations and 256 KiB of path
and identity text, within the journal's existing record and participant limits.

Waveform restores bind the configured recovery-root identity, edit-directory UUID
and identity, and strong revisions for the fixed before/after/extracted backup
leaves. These are observations; future admission must retain the recovery material
and reacquire the root. An existing original target requires expected-identity
replacement, while an observed absent target requires NoReplace. Undo restores
the original before removing an expected extracted target; Redo restores the
original before recreating an observed absent extracted target. The selected
backup and compensation material must be present in the observation.

Folder history moves apply only to directories under a fenced source root. Their
fixed plan ensures contained destination parents before a NoReplace relocation;
it does not fabricate an existing-directory identity for a missing parent. The
source database remains independently identified by its participant fence.
Waveform restores without an extracted target must stand alone, matching the
native history routing contract; other supported actions preserve captured order.

The codec keeps V1–V13 gates and diagnostics unchanged. V14 history records use
the existing owner/store path for persistence, restart reads, retry, cancellation
and compare-and-swap updates. Local validation passed 235 serialized journal tests
(one helper ignored), the library check without default features, GPUI compilation,
formatting and diff checks. Focused independent review found no meaningful issues
in restore policies, bindings, bounds or version compatibility.
Filesystem execution and live recovery-material retention remain outside this
initial intent contract.

Windows storage validation
[34877410904](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34877410904)
passed at exact commit `c96cda4711f0311f03bf2115a72f153630424719`. The lane passed
223 journal tests (one helper ignored), including history codec, direction/absence,
directory-only moves, generic owner and restart/retry/cancel coverage. It also
passed 12 legacy-reader, 90 catalog, 60 session (one helper ignored), 16 core
edit-settings and five media-adapter tests. This accepts the history storage
milestone; native Windows UI and filesystem execution remain outside this lane.

## V15 external filesystem observations (OPT-1337)

The external reconciliation intent uses the ExternalFs actor and a capture-time
evidence batch identity distinct from its later operation ID. It retains ordered
events and duplicate paths, opaque root/backend identities, watcher generation,
capture sequence observations, uncertainty bits and optional backend metadata.
Native strings use explicit Unix-byte or Windows-UTF-16 representations.

Conversion is bounded by 64 events, 512 paths, 64 KiB of stored native path bytes
and 32 KiB of metadata. Original native capture accounting is retained separately
from durable encoding accounting. The source revision is bound to a required
source-database participant; watcher generation is not a source lifecycle epoch.

Recovered raw observations require a source audit until a later owner establishes
continuity. No serialized proof, live lease, filesystem authority or watcher
publication is created by this contract. The strict persisted codec requires
explicit nullable fields, while borrowed conversion checks provenance identity
sizes before cloning them. Earlier schema gates and diagnostics remain intact.

Local validation passed 246 serialized journal tests (one helper ignored),
including native-string, uncertainty, one-sided sequence, nullable-field,
owner/store, retry/cancel and restart coverage. Library and GPUI checks without
default features, formatting and diff checks passed. Primary review covered
allocation bounds, participant bindings and codec compatibility, and repaired the
provenance preallocation and nullable-key issues before acceptance.

Windows storage validation
[34881198244](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34881198244)
passed at exact commit `692bc3aaf6bd7bfdca9b21d0a3cb3eba138ad5d9`, after gating
Unix-only test imports found by the first Windows run. The lane passed 233 journal
tests (one helper ignored), including V15 raw-evidence, nullable-field, owner and
restart/retry/cancel coverage. It also passed 12 legacy-reader, 90 catalog,
60 session (one helper ignored), 16 core edit-settings and five media-adapter
tests. This accepts external-evidence storage; native Windows UI and filesystem
execution remain outside this lane.

## Versioned full-progress work plan (OPT-1337)

The V1 planner derives bounded work units and prerequisites from command arguments
and a complete initial participant inventory. Keys distinguish actions, steps,
participants and semantic roles. Source read and source affect obligations stay
separate even when they use the same database. Validation compares a recovered
plan against the frozen V1 derivation, including its exact units and edges.

This contract uses one required, unfenced filesystem coordinator for the profile.
That coordinator can cover multiple roots and devices; sharing its participant ID
does not establish device equality. Regular-file moves use conservative staging.
Qualified direct directory moves, retained-folder merges, history parent creation
and recovery-material retention have distinct obligations. Complete global,
Harvest, projection and readiness coverage is required for affected sources.

The planner is pure: it neither admits an operation nor advances a journal record.
Full-progress persistence, guarded transitions and legacy compatibility integration
remain separate work.

Local validation passed ten focused planner tests, library compilation without
default features, GPUI compilation, formatting and diff checks. Independent review
found and then verified repairs for same-source staged moves and direct
relocation/removal semantics. Staged file moves require the full destination then
origin-removal sequence even within one database. Direct relocations require
origin absence; retained purge instead verifies the selected retained object's
absence. Restore records precede retention relocation, while retained-payload
verification follows it. No material findings remained on recheck. Windows storage
validation passed at exact source commit `499802ee3c57f60b2a96125ef98e2f8e64a67704`
in run `34886224285`: 243 journal tests passed with one ignored, including all ten
planner tests. The legacy reader, source ownership, catalog/session and waveform
adapter checks also passed. This storage lane does not establish native Windows
UI or audio acceptance.

### Guarded work-progress core

The pure work-progress model derives an exact state for every frozen-plan unit.
Completion requires a private, non-serialized token bound to the operation, plan,
participant scope, role and execution epoch. Stored receipt summaries remain
observations and cannot construct that token. No production token issuer or
filesystem executor is supplied by this layer.

Prerequisites gate completion, including staged copy validation and compound
extraction-before-trim. The global phase records a monotonic visibility frontier;
one published action does not complete later actions. Source reconciliation also
requires all required filesystem obligations. Success requires every required
unit, no audit hold, and completion or explicit deferral of optional artifacts.

Source commits retain before/after revisions. Fenced coordinator receipts carry
source observations and must meet the latest accepted revision across participants
for that source. Retry resume requires a private exact-lease/time/epoch gate and
advances the execution epoch, rejecting callbacks from earlier attempts. Retry,
audit and cancellation observations preserve pending work; cancellation terminals
require private safety gates with no production issuer.

Eight focused progress tests and the library and GPUI checks without default
features passed, as did formatting and diff checks. Independent review verified
the fenced coordinator and retry-epoch repairs; its final empty retry-window
finding was repaired with an unchanged-state boundary regression. Windows storage
run `34890767898` passed at exact source commit
`71baad29317b96c2e9e41ad576c91406d3c151c9`: 251 journal tests passed with one ignored,
including all eight progress tests. The remaining storage-lane checks passed too;
this does not establish native Windows UI/audio acceptance. Durable progress
encoding, SQL CAS, owner integration, audit recovery and readiness-deficit scheduling remain
separate work; this core does not complete OPT-1337.

The following source-reacquisition guard preserves the immutable plan while
rebinding the current scopes for every participant of one stable source. A private
authority must match the current source-database scope and execution epoch, retain
source/root/database/lifecycle identity, advance the lease epoch, and meet the
greatest accepted source or coordinator revision. Reacquisition advances the
execution epoch and preserves retry, audit and cancellation holds. It cannot
complete work or resume a retry, and no production authority issuer is present.
Eleven focused progress tests, the library check without default features,
formatting and diff checks passed. Primary diff review found no material issue.

Audit recovery now uses one bounded generation-tracked overlay with a whole-operation
or exact sorted work-unit target. A private resolution binds the operation, frozen
plan, execution epoch, audit generation and observation. Exact requests replay;
conflicts preserve the active audit. Resolution restores the prior retry or
cancellation hold without completing work. Cancellation requested during an audit
is retained beneath it. Generic prepublication audits are rejected unchanged;
the raw ExternalFs source-audit requirement remains separate. Fourteen focused
progress tests, library compilation without default features, formatting and diff
checks passed. Primary diff review found no material issue. No production audit
authority issuer or durable progress encoding is provided yet.

Windows storage run `34893017246` passed at exact audit source
`0c892ae42ba9a0d135fd3ab80b04794450a4a711`, including the preceding source-lease
reacquisition change: 257 journal tests passed with one ignored and all fourteen
progress tests passed. The other storage-lane checks also passed. The older
reacquisition-only run was superseded; acceptance is based on this combined run.
This is not native Windows UI/audio acceptance.

The admission-claim observation contract is bounded to 256 positive, uniquely
identified claims. It requires exact allocated-byte coverage for every captured
volume/allocation-class pair, rejecting empty/zero, duplicate, unknown, missing,
under/over and overflowing coverage. Ambiguous duplicate capacity-plan pairs are
rejected by this new contract without changing legacy capacity-plan behavior.
Settlement observations cover every claim exactly once as Released,
ConsumedOrTransferred, or Held. Their classification predicates are not live
release proofs; future private admission/recovery authorities remain required.
Five focused claim tests, the library check without default features, formatting
and diff checks passed. Primary diff review found no material issue. The contract
is not yet wired into durable progress or the live OPT-1338 capacity provider.

WorkProgress can now retain validated immutable admission-claim observations through
a crate-private constructor. Its ordinary pure constructor retains no claims; no
record admission or live reservation is implied. Full progress uses its own
`FullProgressDisposition`, including after-observed-effect cancellation values for
effects that must not be called application publication. Legacy disposition types
and V1–V15 codecs remain unchanged. The new effect routes are not enabled until
the cancellation inventory guards exist. Sixteen progress tests, two full-status
compatibility tests, library and GPUI checks without default features, formatting
and diff checks passed. Primary diff review found no material issue.

Windows storage run `34894858329` passed at exact status/claim-attachment source
`086b63ff58839794eb37f995c276e88d8c4612d4`: 266 journal tests passed with one ignored,
including the five claim tests, two full-status compatibility tests and sixteen
progress tests. The remaining storage-lane checks passed too. This is storage
acceptance, not native Windows UI/audio acceptance.

Cancellation inventory source `8f76586a78b0803e12fcc83bab82b9762c1d3830` freezes
the exact work-step snapshot, entered-action classification and admitted claims.
Validation rejects a forged prerequisite-closed publication snapshot that differs
from current progress, and permits a retained capture epoch after source lease
reacquisition while rejecting a future epoch. Retained merge, destructive
disposition and audited external effects cannot masquerade as clean staging.
The narrow after-observed-effect audit overlay preserves its cancellation hold.
Twenty progress tests and one cancellation module test passed, along with library
and GPUI checks without default features, formatting and diff checks. Independent
review accepted the repaired snapshot and epoch guards. Terminal cancellation
outcomes remain unavailable pending scoped recovery and settlement guards.

Windows storage run `34897433109` passed at that exact cancellation-inventory
source: 271 journal tests passed with one ignored, including all twenty progress
tests and the cancellation module test. The remaining storage-lane checks passed.
This does not provide native Windows UI/audio acceptance.

The full-progress capacity contract now has its own bounded version-one plan and
allocation classes, including operation-created recovery payloads. Legacy plans
convert by copying and validating their facts; no floor or recovery class is
inferred. Validation covers row bounds, alignment, protected floors, per-volume
allocation-unit consistency and aggregate overflow. Prepublication settlement
requires released staging/new recovery claims, retained journal/existing-data
charges, and no held claims. These remain observations, not live release proofs.
Six capacity tests, six claim tests and twenty progress tests passed, together
with library/GPUI checks without default features and formatting/diff checks.
Primary review added the per-volume aggregate and unit-consistency checks.

Windows storage run `34899074677` passed at exact capacity source `f79e85257`:
278 journal tests passed with one ignored, including all six full-capacity tests.
The remaining storage-lane checks passed. Native Windows UI/audio acceptance is
separate.

Normal full-progress success now requires a private epoch-bound settlement
authority after all required work and optional-artifact decisions resolve. Exact
claim coverage is retained; held rows stay nonterminal and may later settle,
while settled outcomes cannot change. Exact replay is idempotent. Unadmitted
pure progress no longer reports success from work completion alone. Twenty-five
focused progress tests passed, together with library/GPUI checks without default
features, formatting and diff checks. Primary review repaired retention and
monotonic advancement of held rows. No live capacity authority issuer was added.

Windows storage run `34900358246` passed at exact settlement source `a9fcfc83b`:
283 journal tests passed with one ignored, including all twenty-five progress
tests. The remaining storage-lane checks passed; native Windows UI/audio
acceptance remains separate.

Prepublication cancellation now has a guarded terminal path over bounded cleanup
and settlement observations. It covers only entered actions from the frozen
inventory, distinguishes creation from preservation of an existing input, and
requires applicable staging/recovery absence even when a checkpoint is pending.
Known preserved material and uncertain or missing observations remain nonterminal.
Staging/recovery claims cannot be released before those absence checks pass;
settled outcomes cannot be rewritten. Recovery generations reject stale changes
while accepting an exact replay of the original result after acknowledgement loss.
The private authority binds the operation, plan, current epoch and exact inventory.
Thirty focused progress tests, library/GPUI checks without default features,
formatting and diff checks passed. Independent review found no material issue.
This adds no live cleanup issuer or filesystem execution. After-publication and
observed-effect outcomes, full progress persistence and the remaining OPT-1337
integration are still open.

The initial prepublication commit `8fa6f00b5` passed Windows storage run
`34991443605`, but final primary review found an eligible first-effect gap:
`SourceRead` may be complete while the first effect is pending/in flight and the
completion-only inventory still says `NotStarted`. The repair computes that
boundary once from the frozen prerequisites, keeps cancellation nonterminal and
blocks staging/recovery release. It never introduces cleanup for an unstarted
action. Explicit entered/quiescent owner authority is still needed to resolve
this boundary. Thirty-one progress tests, related cancellation/claim tests,
library/GPUI checks without default features, formatting and diff checks passed.
Independent review accepted the repair with no remaining material findings.

Windows storage run `34993593461` passed at exact repair source `4a81a779c`:
289 journal tests passed with one ignored, and the remaining storage-lane checks
passed. This does not provide native Windows UI/audio acceptance.

The pending-first-effect boundary now accepts a bounded, private-authority-bound
quiescence observation. Each supplied unstarted action must identify its exact
eligible pending work keys. A verified barrier must establish that none was
entered, together with applicable staging and recovery absence; missing or
uncertain observations retain the hold. These rows never become cleanup work for
future actions. Decisive observations cannot regress, exact acknowledgement-loss
replay is harmless, and source reacquisition requires current-epoch authority
even when an earlier observation remains retained. Claim release and terminal
cancellation share the same completeness guard.

Thirty-four focused progress tests passed, including retained-observation
reacquisition, malformed coverage, bounds, and replay regressions. Library and
GPUI checks without default features, formatting, and diff checks passed.
Independent production-path review found no actionable issue. This remains a
guarded observation model: no live barrier issuer, filesystem execution, or full
durable progress codec was added. Effects that actually entered still require
their own exact recovery observations; OPT-1337 remains in progress.

The cancellation-effect inventory now derives bounded observational bundles from
the frozen V1 cancellation snapshot and exact action/segment work-plan groups.
Public progress coverage exercises `StagedPublish` without future trim work,
`StagedMove` with unresolved origin absence, two distinct `QualifiedRelocation`
segments in one history action, `RetainedMerge` with its publication fact, and
standalone `HistoryUndoExtractedRemoval`; CreateFolder accepts its not-applicable
staging and copy-validation roles. Five cancellation-effect tests and thirty-eight
public progress tests passed, together with library and GPUI checks without default
features, formatting and diff checks. This remains observational: no recovery
issuer, filesystem execution, durable progress persistence, or native Windows
UI/audio acceptance was added. After-effect recovery, cancellation-local
reconciliation, owner integration and the remaining OPT-1337 work remain open.

Normal-success settlement now rejects `Released` for retained `JournalRecord`,
`ExistingDestination`, and `ExistingRecoveryPayload` claims before replay or
mutation. `Held` remains a nonterminal observation; `ConsumedOrTransferred` may
complete success after required work and optional-artifact decisions resolve;
unrelated allocation classes retain their existing released/consumed allowances.
Exact claim coverage, authority and epoch binding, monotonic settlement, and
acknowledgement-loss replay checks remain unchanged. Eight admission-claim tests
and thirty-nine progress tests passed, together with library and GPUI checks
without default features, formatting, and diff checks. This closes the
normal-success settlement-class policy gap without adding live capacity or
release authority, persistence/codec, cancellation, filesystem execution, or
native Windows UI/audio acceptance. After-effect recovery, cancellation-local
reconciliation, owner integration, and the remaining OPT-1337 work remain open.

In-memory full progress now retains a read-only completion context alongside
completed evidence: the accepted execution epoch and full participant scope,
including an optional source fence. Completed replay compares that exact
historical context and evidence before current-scope checks, so the original
authority can replay after source reacquisition while a payload relabeled with a
new epoch or fence is rejected. History validation requires evidence and context
exactly for completed steps, validates nested evidence shape, permits only a
stable-source fence rebind with bounded lease/base revisions, and checks old-lease
observations against the current fence baseline. Source revision floors use
accepted historical scopes while retaining the maximum observed revision and the
current fence baseline. The cancellation inventory remains an equality-stable
key/status snapshot.

Forty-three focused progress tests, one cancellation-recovery test, and five
cancellation-effect tests passed, together with library and GPUI checks without
default features, formatting, and diff checks. This is an in-memory observational
milestone: full progress persistence/codec, cancellation recovery and local
reconciliation, owner integration, and native Windows UI/audio acceptance remain
open under OPT-1337. Filesystem mutation implementation is outside OPT-1337 as
dependent owner work.

The cancellation-wide dispatch manifest now derives every frozen `Pending`,
applicable (`Required`), non-`SourceRead` work-unit key across all action and
history segments, including applicable global recipe units and later blocked work
that may become dispatchable after in-flight prerequisites settle. Completed,
`NotApplicable`, `Deferred`, and `SourceRead` units remain excluded. The immutable
manifest binds the operation, plan version, frozen inventory generation, capture
epoch and exact sorted unique key set; canonical validation reuses frozen-inventory
validation and permits the retained capture epoch after source reacquisition.
Membership, pending status, idleness, or absence remains observational and proves
neither entry nor barrier, recovery, filesystem, source, or terminal authority.
Three dispatch-manifest tests, thirty-eight cancellation tests, and forty-six
work-progress tests passed, together with library and GPUI checks without default
features, formatting, and diff checks. No barrier issuer/refinement, cancellation
outcome, persistence/codec, or filesystem mutation implementation was added;
filesystem mutation remains dependent owner work outside OPT-1337.

The cancellation dispatch lane now has an immutable observational outcome record
for the exact frozen manifest. Each bounded, sorted row is typed as
`NeverEntered`, `EntryUncertain`, or `ObservedAfterCancellation`; only the last
variant retains typed step evidence and its accepted historical completion
context. Record validation binds the operation, plan, inventory generation,
capture and observation epochs, exact key coverage, nested evidence shape, and
historical source-fence rules. Refinement accepts exact replay, requires a
newer recovery generation for changes, prevents observation-epoch regression,
allows uncertainty to refine, and prevents rewriting no-entry or accepted late
completion evidence/context. A capture-floor guard rejects observations predating
the frozen cancellation boundary.

Three outcome-record tests, forty-nine focused progress tests, and forty-one
cancellation tests passed, together with library and GPUI checks without default
features, formatting, and diff checks. This remains an in-memory observational
milestone: a future private current-epoch barrier authority, record acceptance
and storage, cancellation gating, recovery reconciliation, and durable
full-progress persistence/codec remain open. Native Windows UI/audio acceptance
also remains separate. Filesystem mutation implementation is outside OPT-1337 as
dependent owner work.

The private prepublication cancellation gate now requires one accepted,
serde-preserved observational dispatch record bound to the exact frozen manifest
and current execution epoch. Every manifest row must be `NeverEntered` before
cleanup, non-held claim settlement, or terminal cancellation can proceed; the
existing unstarted-action quiescence proof remains a second required gate.
`EntryUncertain` and `ObservedAfterCancellation` rows are retained with `Held`
settlements and remain nonterminal. New or changed records advance the shared
recovery generation and pass monotonic refinement checks; exact replay may reuse
the accepted record, while source reacquisition requires a fresh current-epoch
record and preserves historical late evidence.

The accepted record is retained in the observational prepublication value. Its
`Some` serde representation round-trips typed late evidence and completion
context; legacy observations without the optional field decode as `None` and
remain incomplete until a private authority supplies a record. This is not a
durable full-progress codec or a live barrier issuer. Fifty-two focused progress
tests and forty-one cancellation tests passed, including the atomic gate,
generation, replay, reacquisition, late-evidence, and serde regressions. The
no-default-feature operation-journal suite passed 328 tests with one ignored,
and the no-default GPUI check, formatting, and diff checks passed. Live barrier
ownership, after-effect recovery, readiness, persistence/admission integration,
legacy coverage/session recovery, and filesystem execution remain open; the
filesystem owner work is outside OPT-1337 under dependent OPT-1339.

After-effect recovery now has a read-only inventory that retains the complete
frozen cancellation snapshot and exact dispatch outcome record while deriving
canonical bundles from a temporary overlay of only `ObservedAfterCancellation`
keys. `NeverEntered` and `EntryUncertain` remain pending and are retained as
obligations; preparation-only observations produce no bundle, later blocked
trim work is not inferred, and each history segment remains distinct. Candidate
validation accepts an explicitly supplied expected dispatch record and requires
exact frozen facts, record rows, bundles, facts, and source closures. Retained
late source and source-scoped coordinator observations also extend the accepted
historical source revision floors: exact-fence checks require the same accepted
fence, while reacquisition checks require stable source and observation identity.
Unaccepted candidate records do not affect those floors.

Eight recovery-inventory tests, sixty-one focused progress tests, forty-nine
cancellation tests, and four cancellation-effect tests passed. The no-default
operation-journal suite passed 337 tests with one ignored; library and GPUI
no-default checks, formatting, and diff checks passed. This remains an
in-memory observational milestone: no recovery executor, live barrier issuer,
cleanup/settlement/terminal integration, durable full-progress persistence or
codec, or filesystem mutation was added. Native Windows UI/audio acceptance,
owner integration, and the remaining cancellation reconciliation work remain
open under OPT-1337. Filesystem mutation implementation is outside OPT-1337 as
dependent owner work under OPT-1339.

Cancellation-local source-closure observations now derive an exact sorted union
of pending source-closure keys from a validated after-effect recovery inventory,
while retaining the complete frozen snapshot, dispatch outcome, and bundle
lineage. Each bounded row is typed as `Pending` or `Observed`; observed
`StoredStepEvidence` and `CompletionContext` are checked through the shared
historical scope, fence, epoch, and role validation path. Record validation
requires positive generation, exact manifest and row coverage, a dispatch
observation epoch floor, and a current-progress epoch ceiling. Reacquisition
preserves valid historical receipts, while normal steps and frozen cancellation
facts remain unchanged. Explicit recovery and dispatch arguments keep candidate
validation observational and state-independent.

Three source-closure tests, sixty-four focused progress tests, and fifty-two
cancellation tests passed, together with no-default-feature library and GPUI
checks, formatting, and diff checks. This is an immutable observation milestone:
source-closure transition/rebase policy, accepting guards, cleanup/settlement,
terminal cancellation, owner/barrier integration, durable persistence/codec,
and filesystem execution remain open. Native Windows UI/audio acceptance is
separate. Filesystem mutation implementation remains outside OPT-1337 as
dependent owner work under OPT-1339.

Cancellation-local coordinator-closure observations now retain an exact,
validated source-closure record and select only required, applicable global
coordinator roles. Unfenced global roles are included when a recovery effect or
captured/late `SourceCommitted` observation exists; source-fenced roles are
limited to stable identities linked by non-`NotApplicable` effect closures or
those source commits. Optional artifacts, action-local history/trim units, and
preparation-only observations remain excluded. Rows are bounded, sorted, and
typed as `Pending` or `Observed` with coordinator evidence and historical
context; explicit source, recovery, and dispatch lineage keeps validation
observational and state-independent.

Source-scoped coordinator receipts and source-closure receipts now use the
maximum source revision from immutable completed progress and bound dispatch
lineage, with linked source-closure receipts extending the coordinator floor.
The existing role validator checks both sides of `SourceCommitted` intervals,
so a stale `before_revision` cannot pass behind a newer `after` observation.
Reacquisition preserves valid historical receipts without using a new current
fence as an arbitrary cancellation floor. Seven coordinator-focused tests,
seventy-one focused progress tests, and five cancellation tests passed,
together with no-default-feature library and GPUI checks, formatting, and diff
checks. This remains an in-memory observational milestone: no coordinator
receipt transition or accepting guard, cleanup/settlement/terminal integration,
owner/barrier integration, durable persistence/codec, filesystem execution, or
native Windows UI/audio acceptance was added. Filesystem mutation remains
outside OPT-1337 as dependent owner work under OPT-1339.
