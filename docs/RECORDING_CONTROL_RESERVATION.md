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
Partial, corrupt, extra, and future terminal states stay retained and reject
capture and recovery observation; none is treated as vacant capacity.

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
This leaf understands reserved controls only; a future Frozen value is unsupported
until the actual terminal owner explicitly validates and enforces it. Linux paths
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
temporary 4,104,192-byte debit from the existing ordinary input pool covers the
complete supported read/decode/rebuild envelope, then shrinks to a separate
448 KiB retained proposal/transport/context debit after temporary backing is gone.
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
