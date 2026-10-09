# Recording terminal metadata reservation (OPT-2162)

The actual operation-journal initial transaction now inserts four exact control
observations before it can acknowledge a recording intent. This is production
SQLite persistence and accounting; it does not create terminal capabilities,
physical reserve authority, output publication, or retirement permission.

## Transaction and budget

`prepare_recording_initial_policy` validates the activated proposal and configures
the existing conservative SQLite metadata policy before BEGIN. The common initial
intent transaction inserts the recording intent and four control rows, then
performs ordinary claim settlement and the existing commit checks. Any insertion
failure rolls back the new intent and controls together. Lost commit replies keep
the committed evidence; acknowledgement requires exact committed control readback.
The normal initial-insertion duplicate-operation rejection remains unchanged.
Private helper replay checks an existing complete reservation without inserting
replacement rows or issuing a capture grant.

The controls use existing `operation_journal_recording_chunks` SQL shape and
positive global sequence allocation. Slots 20000–20003 represent reserved
Finalizing, Frozen, Seal, and Retirement observations. Each costs one row and the
full 4096-byte reservation even while its serialized payload is smaller. The
existing global ceilings remain 8192 rows and 128 MiB of charged recording
metadata. Activated proposals include this additional 16 KiB in their worst-case
encoded metadata bound, accept the actual 68-byte WAV format, and permit at most
8172 PCM slots plus 16 header revisions and four controls. A smaller encoded-byte
ceiling can further reduce usable PCM slots. Existing descriptor-volume,
capacity-provider, profile-owner, SQLite database/rollback peak, and effect gates
remain required; caller scalar byte counts confer no authority.

## Retained observations

Each strict version-1 reservation binds the complete encoded proposal hash,
operation, output, work key, role, and deterministic domain-separated control ID.
Its JSON is bounded to 4096 bytes and rejects duplicate/unknown fields, unknown
versions, and conflicting bindings. Target reservations must contain exactly the
four expected rows with valid keys, consecutive positive role sequences preceding
any captured chunk, and exact decoded bytes. The capture ordering query scans a
capped per-operation inventory and rejects overflow on recovery.
Partial, corrupt, extra, and unsupported terminal states stay retained and reject
capture and recovery observation; none is treated as vacant capacity. The closed V2
Finalizing observation described below is readable without recreating a close capability.

Only verified reserved controls are omitted from the existing eight-chunk recovery
pages. Initial reservations do not masquerade as header finalization. Unknown
rows remain visible as errors, and globally retained rows retain their charge.
Historical captures with zero controls can still be observed, but cannot append,
resume a live byte replay, or acquire a retrofitted reservation after capture.
Historical proposals remain decodable and retained even when unsupported for
new capture. Any recording row, including a reservation before capture starts,
continues to block clean handoff and ordinary/chunk charge release.

## Validation and remaining ownership

Tests exercise actual SQLite atomic insertion, partial insertion constraint
failure, precommit/commit failures, lost replies and reopen, committed readback
corruption through the retained owner, exact helper replay, wrong proposal/key,
unknown or partial controls, global row/byte exhaustion charged at the full
reservation, historical capture observation, and unchanged retained bytes and
release gates. Capacity inputs in these persistence tests are observations, not
qualified physical reservations. The postcommit corruption seam is test-only;
it preserves the production exclusive connection policy.

OPT-2145 still owns genuine producer closure, single terminal ownership,
Finalizing/Frozen transitions, bounded actual-byte prefix verification, seal
persistence, and live capability integration. OPT-2058/2057/1339 remain open for
retained serializer transport, physical qualification, publication and retirement.
The original reservation leaf understands empty controls. The subsequent V2 Finalizing
reader below adds one explicit observation schema; Frozen and Seal remain unsupported
until the actual terminal owner explicitly validates and enforces them. Linux paths
that lack a qualified platform owner remain unavailable without synthetic grants.

## Original capacity rebuild audit (OPT-2323)

The existing journal ordinary/admission rebuild read transaction now audits the
actual recording relation before its capacity image can be installed. It first
checks the capped 8193-row global inventory and 128 MiB retention ceiling,
including the full 4096-byte charge for each reserved control. Private indexed
operation and slot pages contain at most eight keys; only one bounded immutable
chunk is decoded at a time. Exact canonical row identities, original recording
proposal bindings, complete reserved controls, cross-relation sequence uniqueness,
contiguous header/PCM order and the final short PCM boundary must agree.
Orphan, unknown, conflicting and malformed retained observations refuse rebuild.
The read transaction preserves all rows and charges and returns to autocommit
on refusal, including after reopening the actual database.

Historical recordings without controls remain read-only observations and still
block handoff/release. Valid byte/control inventory does not prove WAV grammar,
producer closure, a live terminal capability, physical writes, publication or
settlement. Those production requirements remain with OPT-2145 and OPT-2058.

## Original pre-start producer custody (OPT-2326, partial)

The journal client now accepts an existing original source registration object and
its journal receipt to request memory-only pre-start custody. The original worker
checks its retained Registered entry, current source/profile pin, actual stored
V17 Recording intent with no disposition or captured bytes, exact four reserved
controls, current physical capacity boundary, rebuilt claims and provider recovery.
It then creates the actual inseparable Reson transport binding, receiver and
settlements and retains all three privately in one fixed runtime slot. A checked
448 KiB command debit from the existing ordinary input pool funds fixed read
scratch and the retained proposal/transport/context. Complete borrowed record
inventory determines an additional same-pool working loan before owned decoding.
The original reply retains that loan through validation and encoding, then releases
it on consumption or loss; the slot keeps its independent 448 KiB debit.
The narrow complete-snapshot resource refusals and pinned implementation evidence
are in [RECORDING_PRESTART_MEMORY_BOUNDARY.md](RECORDING_PRESTART_MEMORY_BOUNDARY.md).
No producer, serializer, capture ring or physical writer starts. The weak, non-Clone facade exposes only operation,
proposal digest and original-worker revalidation. It cannot extract parts or
construct a startup, terminal, SQL, publication or settlement capability.

The slot is installed before its reply is sent, remains occupied after reply loss,
and retains original parts/pin/credit after revalidation refusal until original
worker teardown. Linux tests exercise genuine original journal/source refusal
without installing an unissued receipt, private-facade forgery/drain refusal,
original byte-budget exhaustion and shutdown accounting. Actual SQLite tests
verify read-only empty-control inspection/reopen and preserve captured/cancelled,
missing and unknown inventories on refusal. These tests do not exercise a positive
qualified production slot or lost reply after such issuance.

Two genuine positive harnesses use the production journal constructor, actual
registered source and file-owner root, native reserve descriptor facts and common
recording admission. Their complete helper is type-checked on Linux and Windows;
execution is gated to macOS. OPT-2330 specifies the exact two-test native command,
resources and retained-slot/lost-reply requirements. OPT-2326 remains open for
that genuine original-source/provider/admission verification. Production reserve
creation/reopen and revalidation reject Linux before namespace access; native
macOS implementations exist, and fixtures do not qualify them. Required-WAL
startup, current physical recording admission and the actual byte/terminal
consumer remain separate implementation requirements under OPT-2145/OPT-2058.
No startup, capture, Finalizing/Frozen/seal or physical qualification is completed
by this pre-start entry point.

## Original terminal observations and append fencing (OPT-2145, 9 October 2026)

The original retained producer now polls its private one-shot settlement receivers without
waiting on the actor. Finalizing inventory capture and joined comparison require the same
opaque producer instance, exact format and complete accounting. The bounded closed V2 value
binds proposal, original profile acquisition, terminal generation, pending offset/length/hash,
object length, final RIFF/data patches and stable domain-separated pending/header write IDs.
These write observations do not substitute for admitted normalized PCM chunk identities.

The owner retains the original barrier and its actual pending bytes. It can persist the
observation only after current registry/Source pin, complete operation, capacity/provider and
recovery checks inside the original exclusive SQLite transaction. The dedicated funded
terminal reader accepts existing capture rows; it does not widen the strict pre-start factory.
Its chunk audit uses eight-key pages and one original funded typed row, checking ordinary
charge, participant and release relationships without copying an entire chunk page. Input
pressure refuses before ownership; retained errors keep their original debit and typed cause.

The original mutation replaces only the exact empty Finalizing reservation payload by CAS.
The four original control IDs, consecutive reservation sequences, row count and 16-KiB charge
remain unchanged. Precommit validation and postcommit readback compare exact bounded payloads.
Rollback/commit/readback uncertainty retains the barrier, original typed failure and input
backing; unconfirmed transaction outcomes make the original owner unavailable. General
historical readers understand V2 Finalizing, while the pre-start factory still requires exact
canonical empty controls. Generic retention checks existing exact-byte replay first, then
refuses every new identity behind the Finalizing fence. The private original producer also
closes generic mutation as soon as it observes the barrier, so failure to persist the SQL
phase cannot reopen new-byte retention. This private refusal still permits only exact existing
replay; a caller-supplied terminal ID cannot select or reopen that owner state.

This factory still has no qualified recording startup or private terminal-write admission.
It withholds acknowledgement, never restores one from decoded inventory, and never marks
Frozen/Sealed or releases capacity. Remaining OPT-2145 work includes original normalized tail
and header chunk declarations/admitted writes, cancellation/drain/append races through that
adapter, genuine successful join followed by immutable Frozen retention, bounded actual-byte
verification/seal persistence and restart/replay. Those missing implementations are not waived
by unavailable native runners. Physical producer startup/transport and publication remain in
OPT-2058/2057/1339 and their genuine qualification prerequisites.

## Normalized terminal data preparation (9 October 2026)

`RecordingPcmNormalizer` retains one fixed 16-KiB carry and the exact contiguous
raw-byte cursor. A raw write prepares at most two complete-frame chunks without
advancing that cursor. The non-cloneable batch is bound to its original normalizer
and generation; only accepting that exact batch advances the data cursor. Foreign
and stale batches, gaps, excess finite extents, nil or duplicate supplied identities,
and incomplete terminal frames are refused without advancing it.

The original terminal-barrier adapter verifies the original settlement instance
and producer format, combines its genuine pending fragment with the retained carry,
and declares at most two normalized PCM chunks and the exact RIFF/data patches at
offsets 4 and 64. Terminal identities are stable across retry for the same proposal,
terminal and ordinal/revision. Accepting a terminal batch closes this data cursor.
The data-only terminal preparation entry point is explicitly observational; it
cannot authenticate a producer, acknowledge closure or authorize any write.

The owner must separately prepay the carry, prepared batch and proposal/chunk
encoding scratch, retain exact admitted bytes and satisfy its original Source,
claim and physical-write checks before acknowledging requests or a barrier. This
API does not change the pre-start factory into a startup grant, reconstruct carry
from historical rows, persist a Frozen/seal control or confer physical authority.
CPU fixtures exercise the actual serializer transport and original barrier across
all channel counts, as well as arbitrary raw boundaries and unchanged retry data.
Full OPT-2145 owner integration remains open.

## Original normalized declaration custody (9 October 2026)

The original pre-start route now funds its fixed PCM carry within the unchanged
64-KiB route allowance. When it observes its original terminal barrier, it separately
debits the existing ordinary pool for the bounded prepared batch and transient
proposal/control encodings before materializing normalized terminal data. The
original batch, its credit, the barrier and any selected preparation/transaction
errors stay under the original retained slot. An unknown raw cursor or refused
debit retains only the previous V2 audit observation, with acknowledgement withheld.

The closed V3 Finalizing payload wraps that original V2 producer observation and
exactly four optional declaration entries: at most two PCM chunks followed by the
two fixed header patches. Each entry binds identity, role, range and byte hash.
Its validator derives the exact terminal slot/range partition from the raw offset
and finite final extent, checks stable identities and exact patch hashes, and
rejects missing, extra or reordered entries. Exact prepared bytes are compared
against declarations before retention; the original joined proof is also checked.
This is still data custody, never a physical receipt or live close capability.

The same funded exclusive transaction and reservation CAS writes either known V2
or V3 data, preserving the original four IDs, sequences and 16-KiB charge. Exact
postcommit readback, original current Source/profile/capacity checks, rollback and
uncertain-outcome custody remain required. Recovery understands only these known
Finalizing grammars; unknown versions and other control roles still fail closed.
Even an exact declared new chunk remains refused by the generic append path.

Full OPT-2145 remains open: actual admitted terminal retention/acknowledgement,
original successful close capability, Frozen/seal persistence and paged verification,
restart/cancel/drain races and native acceptance are not supplied by a declaration.
