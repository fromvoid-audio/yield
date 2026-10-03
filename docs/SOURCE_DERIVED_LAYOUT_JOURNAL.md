# Source-derived layout journal foundation (OPT-2154)

The source-derived layout is a separate `UniversalCommandKind`, `OperationKind`
and `UniversalCommandArgs::SourceDerivedLayout`. It carries the actual
`SourceLayoutIntentArgs::from_prepared` output from OPT-2231: complete canonical
prepared generation, model/membership/digests, exact historical profile/source,
raw media-root bytes, database-directory binding, main-file identity,
lifecycle/lease/revision, source command/participant correlation, prior-generation
CAS digest and finite measured DB/WAL/SHM policy facts. Construction validates the
original retained writer, prepared generation and budget before and after capture.
Decoding restores historical observations only; the original writer, capture,
prepared generation and allocation budget must stay in real owner custody for
future admission/publication. A decoded value never reacquires that custody.

## Frozen family and topology

Full-progress observation schema **2** is reserved for this source-derived family;
other families keep schema 1. The source-only work-plan version **2** derives
exactly one required `SourceDerivedLayout` SQL obligation and no prerequisite
edges from exactly one required, initially pending source-database participant.
V1 file-command derivation rejects this command. The existing V1 algorithm,
versions V1–V17 of the legacy journal, file workflows and wire shapes stay frozen.
Legacy codecs explicitly reject a layout kind/command, including V1/V2 records
which otherwise have no universal argument validator.

The participant's bounded textual root/database fields are explicitly versioned,
domain-separated BLAKE3 projections. Root projection hashes the complete opaque
media-root bytes. Database projection length-frames the complete database-root
binding and main-file identity. The command retains all originals exactly, even
when opaque roots are non-UTF8 or exceed the old fence's 256-byte text cap.
Participant validation compares these projections plus exact source, lifecycle,
lease and revision. Extra filesystem/global/ANN/tag/history/artifact participants,
stale or mismatched fences and non-pending initial checkpoints are rejected.
These historical projections must never be converted into ordinary physical
`SourceFence` authority. OPT-2136/2155 must bind the complete original command
scope to genuine retained registrations/reservations and the original private
prepared capture, not use the projected strings as physical identity evidence.

The full-progress validator separately requires `SourceDatabaseCommit` to cover
maximum-pages times page-size, `SourceWalShm` to cover the rest of the complete
original peak (resident WAL/SHM and bounded growth), and `JournalRecord` to cover
at least the complete 1 MiB transport bound. No unrelated allocation classes are
accepted. Each full plan/claim still passes the existing checked arithmetic,
rounding, volume/claim coverage and aggregate bounds. These facts are observations,
not proof of actual available capacity, correct live volume mapping or admission.
The real all-owner latch must derive/verify those physical bindings independently.

## Bounded transport and retained evidence

There is one canonical binary YSLI request in one canonical padded base64 string.
No decimal JSON byte array or duplicate generation copy is stored. Generation is
at most 512 KiB; binary request at most generation plus 8 KiB; base64 is at most
709,976 bytes. A 64 KiB header allowance would still fit the 1 MiB record, but
**every complete snapshot** is actually encoded through the existing capped
writer and rejected at 1 MiB. The decoder checks complete input size, duplicate
JSON fields/depth, envelope/family version, base64 length before binary allocation,
canonical padding, strict versioned binary checksum/length/identity/plan fields,
complete generation/membership/model/digests and exact topology/capacity/claims.
Nested buffer allocations remain finite; decode is not an authority constructor.

The existing actual profile-owned SQLite full-progress store retains immutable
intent observations, expected-sequence CAS, exact read-only replay and same-ID
checks. Its combined legacy/full recovery scanner and owner queue read/validate
schema 2 without restoring live progress or admission. Unsupported future
versions, malformed commands and stale/inconsistent topology stay retained audit
conditions; neither read nor scan repairs or overwrites their bytes. Old source
incarnations stay historical after restart.

The new role deliberately rejects ordinary source-edit/file/tag completion
receipts. No source publication receipt exists yet. Source reconciliation phase
logic includes this required role, so it cannot be omitted to imply completion.
The normal queued admission route returns an explicit missing all-owner
pre-intent latch result before ID allocation or durable admission. Prepared legacy
admission also rejects the new family; historical serialized bytes cannot enter a
filesystem initial-intent schema. No new ordinary OperationId producer, source
SQL publisher, readiness upgrade, user undo, ANN/filesystem effect, claim issue
or recovery permission was added by this foundation.

## Verification and remaining consumers

Actual retained SourceWriter fixture producers feed both ordinary and maximum
256-member prepared generations into the real store. Tests cover strict binary /
base64/full-progress roundtrip, complete output cap, source-only derivation,
stale fences, insufficient allocation, non-layout receipt rejection, full store
insert/replay/reopen, unchanged raw unknown/corrupt/stale recovery evidence,
actual queued read/scan/replay, admission rejection and a second reopen with no
additional ordinary operation row. Fixture identities/claims and source generation
rows are explicitly historical test data; they do not simulate successful
production admission, source publication or Ready.

Run `cargo test --locked -p yield-library --lib source_layout` and the full serial
library suite, then `bash scripts/agent.sh preflight`. Existing source producer
cases also verify original writer/capture/budget deadlines, cancellation, source
changes, handoff, replacement, page policy and read-only restart behavior.

OPT-2136 still requires the actual all-affected live-owner pre-intent latch and
atomic intent/claim settlement. OPT-2155 still requires genuine admitted source
SQL generation CAS/publication/readback, exact layout completion observations,
claim settlement, uncertainty/recovery and BLOB measurement. The source read,
catalog/UI, physical qualification and tracking parents remain their own open
scope. This journal foundation completes none of those consumers or parents.
