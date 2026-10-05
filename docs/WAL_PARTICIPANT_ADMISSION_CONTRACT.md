# Retained participant WAL admission

OPT-2126 resolves the missing ownership decision under OPT-1685. This document
specifies implementation work; it does not implement universal admission, issue
capacity authority, finish OPT-1685/1348/1688, or qualify physical recovery.

## Current boundary

The checked source is `operation_journal/owner.rs`:
`OwnerRuntime::admit_prepared` validates `PreparedAdmissionInput`, refreshes the
durable claim image and live descriptor capacity, creates a provisional capacity
lease, and only then allocates an ordinary `OperationId` and settles initial
claims with the intent. Recording keeps its existing frozen identity; that
identity alone is not durable acceptance or a WAL permission. `LiveAdmissionWitness` borrows profile/volume/reserve authorities; it
does not include the affected source/global SQLite connections. The journal's
DELETE-mode connection is not evidence of those databases' WAL health.

`sqlite_wal/admission.rs::enforce_wal_admission` observes the retained SQLite
`SQLITE_FCNTL_JOURNAL_POINTER` VFS object without a path reopen. Source curation,
ordinary global metadata and source readiness claims use it under their own
immediate transactions (OPT-2077/2085/2102). Those checks are useful commit gates,
but they cannot prove every participant was admissible before durable intent.
`SourceWriter`, the global library writer and the journal remain separate owners.

The target is `IO_ARCHITECTURE_TARGET.md`, “WAL maintenance and reader snapshots”:
soft 32 MiB, hard 64 MiB, unknown fails closed, and only bounded already-admitted
required/recovery commits have a reservation-backed exception. No external reader
is killed, blocked by an interactive checkpoint, or treated as a Yield reader.

## Exact inventory and identity

Before owner preparation, derive the affected database inventory from the validated
family command, immutable participant set and full allocation plan. A participant
that will change SQLite, WAL or SHM must identify its actual retained database.
Source media roots, journal volume registration and `SourceWalShm` allocation
classes alone cannot establish that identity. An omitted or unknown database
rejects the preparation; it cannot become a zero-byte or read-only participant.

The initial bound is 64 distinct affected databases, source and global combined,
consistent with the existing `participants.rs::MAX_PARTICIPANTS` total ceiling. An
operation needing more databases is unsupported until a separate bounded
protocol exists. Bound the complete handshake's owned identity/descriptor payload
to 64 KiB with checked arithmetic before queue retention. Path strings and user
labels are not handshake identity and must not be copied into telemetry.

Each owner validates and seals its descriptor from retained authority:

* Profile identity and writable ownership generation; global writer lifecycle.
* Source ID, source lifecycle, root and root-binding identity, database file
  identity and lease epoch for a source participant.
* Owner registration generation, intended command digest, database role, current
  revision and the exact bounded DB/WAL/SHM allocation claim identities.

Use a versioned digest of the validated bounded immutable command encoding;
ambiguously encoded or conflicting commands reject rather than share a reservation.
The digest matches the command but does not grant ownership.

An owner-issued opaque preparation nonce is not an `OperationId`. It cannot appear
as durable acceptance, progress, restart status or an effect capability. Descriptor
observations are diagnostics; copying or serializing one cannot preserve a live
reservation. Deduplicate aliases using verified physical database identity, not
caller source IDs or matching path text. Conflicting roles/owners for the same
physical database reject rather than acquire it twice.

## Reservation and ordering

Use a non-UI asynchronous coordinator with one bounded in-flight handshake per
operation. It never owns or transfers a SQLite connection. Acquire participant
reservations in a stable total order: global first, then sources sorted by verified
database identity. Never acquire a later participant and wait for an earlier one.
The cooperative preparation deadline is two seconds from coordinator admission,
including queue wait; each SQLite writer acquisition is one nonblocking
`BEGIN IMMEDIATE` attempt. This bounds queue/poll policy and the amount of SQL
work, not an operating system call that cannot be interrupted. Busy/locked releases the acquired prefix and returns a bounded scheduler
retry; it does not sleep while holding earlier participants.

An owner first performs strict read-only profile/source validation and confirms
that its authority lifetime covers the preparation deadline. Source
`validate_current_owner` already exposes a read-only expiry check;
`current_revision_for_command` may renew an idle lease and must not be used here.
A near-expiry or expired lease returns `LeaseRenewalRequired` before reservation,
without changing the lease row. Renewal is a separately admitted bounded owner
maintenance write, with its own WAL/capacity accounting under OPT-2137. Perform it
before retrying preparation, never through the retained control connection while
the ordinary connection holds that database's writer lock. Preparation itself
cannot hide a pre-intent renewal side effect.

The owner then holds a write reservation on its existing SQLite connection and
reads retained VFS WAL health before any metadata, revision, receipt or claim row
update. An application mutex or a sampled revision alone is insufficient: an
external writable connection could otherwise grow WAL between observation and
intent. Holding SQLite's actual reserved writer lock prevents that growth. This
is a bounded empty transaction, not an admission write or a checkpoint.

While reserved, its event loop must continue handling cancellation, deadline,
profile-loss and shutdown controls without executing other SQL on that connection.
It must not synchronously wait for the journal or another owner. Ordinary queued
writes remain queued with their existing admission bounds; do not misreport them
as committed. No analysis, filesystem work, checkpoint, lease renewal or row-page
query runs in the reservation. The journal must not call back synchronously into
an owner whose reservation response it needs.

After the final participant is reserved, the coordinator submits the exact
reservation inventory to the journal. The journal verifies inventory coverage,
current registrations, deadline and live owner acknowledgements before provisional
capacity admission and **before** identity allocation. Reservation expiry/release
and the journal's intent decision require an explicit decision latch: every owner
acknowledges the same sealed decision nonce while still reserved. Merely receiving
earlier “ready” messages is not a latch. No timeout may autonomously release a
participant after it acknowledges the commit decision until the journal's outcome
is resolved. The latch has a cooperative settlement observation deadline and a retained
unresolved state if the journal outcome cannot be read back.

Check the remaining deadline before allocating an identity or starting settlement;
expiry at that point aborts. Once settlement starts, timeout cannot prove rollback
or revoke an in-flight SQLite commit. Keep the reservation and unresolved decision
until the journal gives a definite result or exact durable readback. A missing
response stops ordinary admission and dispatch for the affected owners. A timeout
must not release their writer locks while the journal could still commit intent.

A logical timer cannot bound a stalled VFS `xSync` or forcibly cancel SQLite I/O.
This contract promises finite payload/SQL and cooperative deadlines, not a hard
wall-clock lease while settlement is in flight. Such a stall remains observable
as unresolved settlement; UI work continues asynchronously with its last good
view. Never drop the borrowed connection, forcibly unlock its file or manufacture
a rollback to satisfy the timer. Shutdown must retain unresolved owners until the
journal worker returns; process restart reconciles the durable journal/claims.
Implementations must report a stalled owner separately from pre-intent retry.

The journal may accept only a nonce backed by live owner registration and the
current decision latch. A caller-supplied scalar, timestamp, trait implementation,
cached snapshot, pathname, or serialized acknowledgement cannot mint the latch.
Use separate opaque observation and reservation types. A dropped client request
must still drive abort/reconciliation through bounded owner control channels.

## Watermarks and commit behavior

| Observed state | New intent | Existing required/recovery work |
| --- | --- | --- |
| Below 32 MiB | May continue after complete reservation and capacity checks. | Revalidate exact durable scope and capacity at commit. |
| 32 MiB to below 64 MiB | May continue; request non-UI passive catch-up after reservations release. | Same capacity and ownership checks; record catch-up. |
| At or above 64 MiB | Abort preparation before identity/intent; preserve exact retry input. | Only the bounded exception below; routine/rebuildable work stays paused. |
| Unknown mode/descriptor/size or identity | Abort; never substitute zero or a path size. | No unknown-size exception; retain unresolved work for owner recovery. |

After definite `IntentDurable`, release empty preparation transactions in reverse
order. They are not cross-database atomic commits. Every actual participant commit
must reacquire its own immediate transaction, revalidate profile/source ownership,
revision/command identity and retained WAL health. Later growth cannot be hidden
by the earlier observation. A new intent does not grant unrestricted writes.

For a known hard-watermark participant, required completion is permitted only when
the journal owner confirms an existing durable operation and exact not-yet-spent
claim for that participant's bounded DB/WAL/SHM allocation. Fresh live capacity
must prove the bounded allocation fits above the protected reserve, and the
single-use owner permit must bind operation, participant, database, claim,
allocation ceiling and commit direction. No “foreground”, “recovery”, caller
boolean or old receipt grants this exception. Routine feature rebuilding cannot
reuse a required-commit permit.

If protected reserve must be consumed, use the existing serialized per-volume
`RecoveryReserveCharge`/spend/reconstitution protocol, with exact purpose, bounded
charge and control-plane margin. Ordinary capacity claims cannot be relabeled as
reserve authority. Unknown or unsupported physical providers fail closed; an
allocation estimate and a contract fixture cannot qualify non-sparse reserves.
Unknown commit/spend keeps the claim charged until exact readback/reconciliation.
Release unused or transferred DB/WAL/SHM claims only through the existing durable
settlement accounting. WAL truncation alone is not proof a claim can be released.

An empty preparation rollback must be confirmed before returning the writer to
ordinary use. On rollback failure, retain it in a private unavailable state and
retry rollback or close its connection before releasing ownership; do not expose
an active transaction as a reusable `SourceWriter`. Preparation contains no row
mutations, so connection close can discard the empty transaction without a fake
metadata receipt. This rule differs from an uncertain participant commit, whose
durable metadata and claims require exact reconciliation.

## Failure and lifecycle matrix

| Boundary | Required result |
| --- | --- |
| Queue full, inventory mismatch, duplicate physical DB or counter overflow | No participant SQL/ID/intent; existing typed pre-intent rejection. |
| Busy, hard, unknown or stale owner during preparation | Roll back empty acquired prefix in reverse order; no operation ID/row; exact new-attempt retry. |
| Cancel/timeout before decision latch | Abort all reservations, release provisional capacity, no durable acceptance. |
| Missing owner acknowledgement before settlement | Abort; an earlier health observation cannot substitute. |
| Definitive settlement rollback | Release all reservations/provisional claims; no durable status. |
| Committed intent, result delivery lost | Persist the sealed nonce atomically with intent/claims, then read that exact nonce/operation/claims from journal; never allocate a replacement operation. |
| Ambiguous settlement or owner loss after latch | Retain unresolved decision, claims and live reservation until definite settlement/readback; stop ordinary admission and dispatch. A timer is not proof of rollback. |
| Restart | No live reservation survives. Rebuild durable claims/intents and revalidate current retained owners; an old lifecycle/lease cannot resume. |
| Commit finds new hard/unknown state | Retain intent and exact legal retry overlay; only an actual bounded required/recovery permit can pass known hard. |
| Shutdown | Stop preparation, drain/abort or reconcile every latched decision before disposing retained owners; no journal-to-owner synchronous wait. |

## Concrete implementation leaves

1. OPT-2133, source owner reservation: implement bounded retained-connection preparation,
   sealed descriptor/nonce, cancellation/deadline and rollback under the current
   profile/source guard. Actual SQLite tests must show external writers cannot
   grow the reserved WAL, hard/unknown/stale rejection changes no rows, other
   source commands do not execute during reservation, and idle/near-expiry lease rejection performs no implicit renewal. Do not reuse `SourceDatabaseWriteFence`: its snapshot route opens a separate
   retained connection. This preparation consumes exclusive ownership of the
   current writer. No journal consumer or permit from a model fixture.
2. OPT-2135, global owner reservation: implement the same worker control boundary and
   actual registered retained global database; validate alias/profile/lifecycle
   identities and blocked queued writes. Neither whole registry participation
   nor one scalar in `PreparedAdmissionInput` completes this leaf.
3. OPT-2136, journal coordinator/latch consumer: wire the exact full participant inventory
   before `OperationId`/settlement, bounded asynchronous decision handshake,
   capacity ordering, rollback, definite/unknown readback and restart. Use two
   real source owners plus the global owner; fail every acquired-prefix boundary,
   reverse order contention, dropped delivery, stale generation and deadline.
4. OPT-2137, required commit/recovery permits: implement exact durable DB/WAL/SHM claim
   validation, bounded spend and single-use scope, actual participant commit,
   retention on uncertainty and release after settlement. Qualified provider
   execution and physical reserve/native recovery remain separate acceptance.

These are portable design/implementation prerequisites, not platform waivers.
OPT-1685 remains open until the actual routes and exception are implemented;
OPT-1688 keeps sidecar growth/release acceptance and OPT-1691 keeps fault/telemetry
coverage. OPT-2120's checkpoint work is independent and does not grant admission.

Relevant existing commands (baseline evidence, not new handshake tests):

```sh
cargo test --locked -p yield-library --lib sqlite_wal
cargo test --locked -p yield-library --lib readiness_wal
cargo test --locked -p yield-library --lib global_wal_tests
cargo test --locked -p yield-library --lib operation_journal::admission_claims
bash scripts/agent.sh preflight
```

New leaves must add the real-owner cases above, rebase and rerun affected checks,
and record exact commands/results. Source-model latches, finite fixtures and Linux
VFS observations alone are not production multi-owner or physical qualification.

## Original source reservation binding (OPT-2247)

`SourceWalPreparation::capture_wal_binding` now produces a non-Clone,
non-deserializable `SourceWalBinding` from the actual retained empty MAIN
transaction and its genuine `SourceAuthorityRegistration`. It captures the
source/lifecycle, original profile acquisition and lease owner/epoch, media and
database roots, MAIN/root binding, current completed audit and observed local
registration phase. The initial bounded read checks the compiled source schema
and SQLite runtime; missing/stale audit, unknown schema/runtime, foreign
registration and unverified native objects reject without releasing MAIN.

The binding also observes actual retained forwarding-VFS MAIN and WAL objects
and SQLite's mapped SHM object: distinct native identities, the same database
device/allocation unit and captured lengths. Its historical version-1 descriptor
is bounded to 16 KiB while being encoded. A coordinator still must account for
all owned in-memory copies and the entire 64 KiB handshake; these bytes are not
a new durable journal inventory format. The private binding additionally retains
original writer liveness, preparation nonce and the unchanged original deadline.
Another preparation on the same writer cannot reuse it.

`validate_wal_binding` performs no ordinary SQL. It checks original custody,
read-only CONTROL ownership, the genuine registration observation and the
current retained native objects, with the original deadline checked again after
validation. Captured object lengths must still match exactly; any change fails
closed and requires a fresh pre-intent capture, never renewal or retry permission.
Before any future decision latch, expiry/drop/abort keep the existing empty
preparation semantics: failure itself does not prove rollback, and reuse requires
confirmed abort on the original connection.

A genuine local Prepared registration is deliberately observable and remains
insufficient for complete journal/reserve installation. These APIs establish no
selected allocation policy, physical qualification, capacity claim, operation
ID, decision latch, required/recovery permit or settlement. Retained native
object capture currently supports the Linux forwarding VFS only; other native
adapters return unavailable. OPT-2136 still owns actual catalog/global transport,
complete database inventory and all-owner acknowledgements before admission,
the atomic decision nonce, and indefinite lock/custody retention once settlement
can commit. Those requirements are unchanged by this binding prerequisite.

## Bounded catalog binding handoff (OPT-2248)

`CatalogService::prepare_source_wal_with_binding` explicitly requests original
scope from the actual catalog source worker. That worker requires its retained
genuine registration, captures the binding under the newly-held empty MAIN
transaction before reporting Prepared, and retains the non-Clone binding on its
original owner. Missing registration, unknown schema/audit/VFS, insufficient
credits or substituted native objects reject; returning a callable writer still
requires confirmed original rollback. The original diagnostic-only API is
unchanged. Parked strict slots validate the genuine binding without ordinary
SQL, renewal or a new deadline.

The existing 64-slot/64-KiB context pool also pays for this handoff. Strict input
identities are bounded before capture. A short accounting step reserves bounded
capture staging before owner reads, then charges actual binding headers and
buffer capacities plus the shared descriptor buffer/wrapper before copying it.
No accounting/control mutex is held during SQL or owner/native validation.
`scope_descriptor_bytes` borrows historical bytes; observation clones share
that buffer. Their shared byte credit and closed accounting object outlive
abort, request drop and worker shutdown until the final view drops. The genuine
binding is dropped before its owner-side byte custody is released. Retained
views can therefore cause a subsequent capture to report Full even when no SQL
preparation slot remains; dropping them restores available context bytes, not
physical claims or retry authority. Capture scratch for the separately bounded
schema/runtime inspection is not a handoff descriptor and remains subject to
its original observation limits.

Real worker tests use actual locally Prepared registration guards and retained
Linux SQLite/VFS objects. They do not install pretend fully-registered journal
or reserve authority. The strict pre-intent holder still refuses
`acknowledge_settlement`. OPT-2136 must supply the actual registered all-owner
bridge, aggregate source/global inventory and direction slices, a sealed common
decision and atomic durable nonce, and cleanup fencing after settlement can
commit. Pre-intent timeout/drop/shutdown behavior here does not implement that
post-settlement retention requirement. Native physical qualification and the
unsupported retained-object adapters remain separate.

## Bounded original global scope handoff (OPT-2251)

`GlobalPreparationRequest::with_wal_binding` selects exact original native scope
on the existing global worker. Inside its original empty immediate transaction,
the worker captures the forwarding-VFS MAIN and WAL objects and SQLite's mapped
SHM object. Distinct identities, a common native device/allocation unit and exact
observed lengths are required. The version-1 historical descriptor additionally
binds the actual profile acquisition, worker registration, command digest,
preparation nonce and checked mutation revision; encoding stops at 4 KiB. It is
not the durable journal inventory or a capacity claim. It can additionally carry
existing bounded lifecycle allocation observations from the same held transaction;
all three objects must match that capture exactly. The existing lifecycle and
diagnostic-only routes remain available.

All observation clones now share complete immutable data. Each strict request
reserves a conservative 16-KiB local handoff credit before queue retention,
covering its finite capture/encoding staging, retained identity/descriptor data
and queue/reply headers. The owner's aggregate strict handoff pool is 64 KiB;
four retained strict handoffs exhaust it even after their empty transactions
have rolled back. A worker and every delivered historical view share the credit;
only the last holder releases it, including after shutdown. Input/schema/native
inspection scratch remains subject to its independently bounded original
observation limits. A future coordinator must still account the complete
source-plus-global handshake; these local credits are not that proof.

While parked, the original worker revalidates all three native objects without
ordinary SQL or a changed deadline. Missing or same-size substituted sidecars
refuse and retire the connection after original rollback rather than dispatching
queued writes against a changed binding. Unknown native adapters fail closed;
capture currently supports the Linux retained-root adapter. Real worker tests
exercise external and queued write exclusion, native substitutions/missing
objects, original expiry, shared view exhaustion and shutdown. The strict capture
also runs under a real SQLite authorizer that denies row reads, mutations and
checkpoint SQL, preserving the exact original busy timeout.

This remains pre-intent custody and historical scope only. Existing drop,
cancellation, expiry and shutdown abort behavior cannot acknowledge a decision.
OPT-2136 must implement complete family-derived inventory, current genuine
registrations, exact direction allocations, sealed all-owner acknowledgements,
atomic intent/claims/nonce settlement and unresolved lock retention. Neither a
shared view nor its encoded bytes can supply those authorities or physical
qualification. All original parent acceptance stays open.

## Genuine library source transport (OPT-2252)

`sample_sources::wal_transport` defines the bounded requester and single original
source-actor inbox independently of the catalog crate. The catalog's existing
public names remain aliases, and `CatalogService::source_wal_transport` exposes
the same actual producer route to library consumers. Its original ceilings remain
64 pending-plus-held slots, 64 KiB of complete retained context and a two-second
common deadline including queue wait. Closing admission rejects queued requests;
it does not confirm cleanup of an already acquired native owner.

A non-Clone command can acquire only a genuine `SourceWriter`. Private producer
custody retains the actual preparation or failed native writer. There is no public
positive status setter, callback or producer trait. Publication revalidates the
original owner; strict publication additionally requires that owner's genuine
source registration and captures the actual original binding. Missing or
mismatched registration refuses. A second publication from the same command
refuses before native work. The catalog retains source desired-state, lifecycle,
alias exclusion, normal dispatch and failed-owner retirement on its original
worker; it never moves the SQLite owner into the requester or reply.

Complete immutable observations share their revision, correlations, nonce and
optional original binding. Historical binding access borrows the actual private
binding, without decoding caller bytes or making a second descriptor buffer.
Capture staging and retained scope use the same checked local byte pool; copies
retain all observation and scope credits across abort and shutdown until the last
view drops. Even diagnostic-only copied observations retain their memory credits.
These local source bounds do not replace combined source-plus-global coordinator
accounting or prove current journal registration.

Only actual rollback and busy-timeout restoration returns the original writer and
finishes `Aborted` or `Rejected`. Failed cleanup retains inaccessible native
custody. `Retired` requires the original failed owner's genuine source-close
acknowledgement; journal finalization may still be pending. Producer Drop publishes
`Unavailable(OwnerLost)` before native custody drops, never inferred reuse or a
retirement acknowledgement. Existing catalog tests exercise real rollback denial,
timeout-restoration failures, lifecycle exclusion, external writer pressure,
sidecar substitution, copied memory custody and shutdown. Normal-build source
tests exercise the library route and missing/wrong original registration directly.
The native protected-reserve retirement case retains its OPT-2026 prerequisite.

This is still the existing pre-intent preparation route. A holder explicitly
refuses settlement acknowledgement. Full OPT-2136 must implement complete family
inventory and direction allocations, genuine journal registration validation,
sealed decision custody and cleanup fencing on both real owners, atomic durable
nonce/intent/claims and uncertain readback/restart. Historical scope, transport
privacy or a local reservation cannot issue that latch, a permit or physical
qualification. The original parent acceptance remains open.

## Limited original global requester (OPT-2257)

`GlobalLibraryWriter::wal_preparation_client` privately issues a cloneable
`GlobalWalPreparationClient` for the same actual global worker. It exposes only
registration/lifecycle diagnostics and bounded preparation submission. It retains
no SQLite connection, writable profile capability or join handle, and exposes no
mutation, shutdown, positive acknowledgement or settlement method. Both the
original writer API and requester clones use one submission implementation with
the existing queue, admission mutex, lifecycle, metrics and strict handoff budget.
No alternate owner, preparation queue or caller-defined producer is introduced.

Original registration/deadline/policy validation still runs on the retained worker;
strict captures retain the same MAIN/WAL/mapped-SHM identities and shared historical
byte credit. All requester clones and the writer share the existing 64 KiB strict
handoff ceiling, including copied observations retained after empty rollback.
Closing the original owner's shared sender disconnects every requester; a clone
cannot prolong worker shutdown. Restart creates another registration even within
the same profile acquisition, and the original request is stale. Pending Drop,
confirmed abort, lost delivery, failed acquisition, deadline and panic retain the
original typed outcomes. Global shutdown reports remain the existing typed drain
observations; a closed client does not assert successful settlement.

Actual worker/SQLite and normal-build Linux tests request through limited clones,
verify original strict scope and external/queued writer exclusion, exhaust shared
historical byte custody, cancel lost pending replies and reject original requests
on restart. Queue failures, expired queued deadlines and original worker panic
preserve the same fail-closed boundaries. A compile-fail test prevents constructing
a requester from caller fields. These are genuine empty preparation tests, never
fake physical grants or all-owner decision qualification.

The requester enables the missing journal-to-global access without transferring
external owner lifetime. Full OPT-2136 still requires family-derived complete
inventory/directions and accounting, current genuine acknowledgements, native
custody after a shared decision, cleanup fencing, atomic durable nonce/claims/
intent and uncertain readback/restart. Existing pre-intent timer, Drop and shutdown
abort behavior must not be used after settlement starts; this requester cannot
acknowledge that transition. Required permits and native qualification stay open.

```sh
cargo test --locked -p yield-library --lib global_requester
cargo test --locked -p yield-library --test global_wal_requester
cargo test --locked -p yield-library --doc GlobalWalPreparationClient
cargo test --locked -p yield-library --lib
bash scripts/agent.sh preflight
```

## Genuine source family policy transport (OPT-2258)

The sealed source requester consumes an original `SourceTagAdmissionObservation`
or `SourceLayoutMetadataBudget`, deriving the revision and the minimum original
observation/request deadline. The existing catalog inbox moves the same actual
source writer into its original typed tag/layout preparation. Full family facts
are checked before entry and inside the empty immediate transaction. The original
finite settings and extension guard remain held during strict MAIN/WAL/SHM scope
capture. No alternate SQLite owner or caller-defined positive policy is accepted.

After initial complete scope capture, parked validation checks original CONTROL
custody, native objects, source registration and entered settings. It performs no
ordinary source row, schema, page, assignment or generation reads, and no lease
renewal. Actual SQLite authorizer tests deny those operations while parked.
Policy family in copied diagnostics describes successful original entry only;
it carries no current journal acknowledgement, physical grant or settlement right.

The existing 64-slot/64 KiB handshake pool charges complete typed input allocations,
including selection vector backing capacity, path/string capacities and retained
revision copies, in addition to native/context bookkeeping and strict scope staging.
A valid standalone selection that cannot fit this transport is rejected before
acquiring SQLite custody. Pending rejection drops policy payloads before refunding
input credits. Historical diagnostics retain independent shared credits after abort.

Cancellation returns the identical writer only after real empty rollback and exact
settings restoration. Failed rollback/settings restoration retains typed custody;
a preexisting transaction is never adopted or returned. Failed custody uses the
original source authority close path, and Retired requires its genuine returned
acknowledgement. Prepared authority rejection is tested without a manufactured
journal drain. Positive protected-reserve retirement qualification still requires
OPT-2026 resources; the portable rejection tests do not complete that qualification.

Full OPT-2136 remains open: complete affected-family inventory and directions,
genuine current journal registration, sealed decision custody, atomic nonce/intent/
claims, uncertain settlement readback/restart and native qualification are required.
This transport retains pre-intent abort semantics and cannot acknowledge settlement.

```sh
cargo test --locked -p yield-library --lib policy_transport
cargo test --locked -p yield-library --test source_wal_policy_transport
cargo test --locked -p yield-library --lib
cargo check --locked -p yield-catalog
bash scripts/agent.sh preflight
```

## Parked original pre-audit lease custody (OPT-2259)

`SourceLeaseMaintenancePreparation::revalidate_owner` validates only the retained
original issuer/source/lifecycle, current CONTROL lease and required expiry,
original native MAIN/WAL/mapped-SHM identities/lengths/device/allocation unit,
entered settings and empty-transaction custody under the original common deadline.
It performs no ordinary row, schema, page, scan or optional-audit queries, no renewal
and no nested transaction. The retained extension guard excludes extension entry.
A SQL-free original SQLite total-change counter rejects accidental uncommitted
ordinary writes that a separate CONTROL snapshot cannot see.

Initial acquisition and `capture_lease_binding` retain complete original schema,
scan/optional-audit/subject/allocation validation inside the empty transaction.
Existing full `revalidate` and cleanup retain their full checks. After capture,
`validate_lease_binding` checks its private original issuer/nonce/deadline and exact
current genuine source registration using parked validation. The original exclusive
writer prevents intervening ordinary row changes; a stored or decoded fence cannot
replace this preparation. Abort still requires actual rollback and exact original
settings/timeout restoration before the same writer becomes callable.

Actual never-audited SQLite tests deny ordinary reads, schema/page queries, writes
and nested BEGIN with an authorizer while original parked binding validation
succeeds. Settings/sidecar substitution or length changes, original epoch/owner
changes, uncommitted accidental mutation, wrong registration/issuer and expiry
reject without authorizing reuse. Normal-build tests use actual owners and genuine
Prepared registrations; they do not fabricate an authoritative audit or physical
grant. No V18/V2 encoding or journal/source schema changes.

Compatible pre-audit source transport and full OPT-2136 inventory/directions,
genuine current Registered acknowledgements, post-decision native cleanup fencing,
atomic nonce/intent/claims and uncertain settlement remain open. These original
parked checks provide no latch, permit, retry/refund, release or close acknowledgement;
qualified allocation OPT-2194 and genuine admitted release OPT-2202 remain open.


## Original current Registered source validation (OPT-2274)

The actual audited/tag/layout and pre-audit lease preparation owners expose an
explicit `validate_registered` boundary on the sealed source producer dispatch.
It requires the original published parked scope, original nonexpired preparation,
unchanged retained native objects and entered policy, and the genuine source
registration. The registration checks that the actual database retains the same
private lease, roots and MAIN binding; it validates current profile ownership,
lease liveness and read-only CONTROL under a transient original lifecycle gate
use. Locally Prepared registrations and historical Registered phase observations
cannot satisfy this check. Missing scope, foreign registration, abort/cancellation,
drain and expiry refuse without rolling back or returning the writer.

`SourceWalPreparation::validate_registered_wal_binding` and
`SourceLeaseMaintenancePreparation::validate_registered_lease_binding` perform
original parked scope checks before and after the current registration check.
They perform no ordinary SQL, lease renewal or deadline extension. The existing
diagnostic capture and parked validation APIs still accept genuine locally
Prepared registrations for their documented observation-only purpose.

Success is current validation only. The transient gate use ends before return;
it does not protect a subsequent journal transaction, publish an asynchronous
all-owner acknowledgement, retain post-start custody, issue a claim or permit,
settle an intent, authorize retry or refund reservations. OPT-2136 still owns
those complete production boundaries and atomic nonce/readback/restart behavior.

Portable real-owner tests deny ordinary SQL while checking refusal, preserve MAIN
exclusivity and exact lease/metadata rows and connection identity through confirmed
abort, and cover audited/tag/layout and pre-audit producer dispatch. The native
`native_registered_parked_source_boundaries_refuse_foreign_and_drained_authority`
harness registers through the actual journal, checks audited and pre-audit original
producers, and drains/retires through actual journal commands. Its macOS execution
requires native capacity/bootstrap resources; a logical registration model cannot
replace that execution or qualify the production per-user reserve registry.
