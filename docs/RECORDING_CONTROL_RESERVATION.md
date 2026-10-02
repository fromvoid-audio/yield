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
four expected rows with valid keys, positive sequences, and exact decoded bytes.
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
