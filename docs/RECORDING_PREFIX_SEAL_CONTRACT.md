# Recording retained-prefix seal decision (OPT-2131)

This decision extends [recording sink admission](RECORDING_SINK_ADMISSION_CONTRACT.md).
It does not implement a seal, create live admission, qualify storage, or activate recording.
OPT-2058, OPT-2057 and OPT-1339 remain open. OPT-2141 owns actual reson terminal sample
accounting; journal prefix reconstruction and physical validation are separate work.

## Current production boundary

`operation_journal/recording_commands.rs` freezes V17 proposal/output/work/namespace
identities and finite float32 format. `store/recording_chunks.rs` retains actual immutable
bytes before dispatch, admits contiguous PCM ordinals, and prohibits PCM after the first
post-initial header revision. Pages contain at most eight chunks. Retention is globally
capped at 8,192 rows and 128 MiB encoded bytes, with no eviction or reclamation API.
`owner.rs` checks the current profile/source pin, admitted claim and capacity provider;
historical proposal/root observations cannot reconstruct those capabilities.

There is no durable recording seal. Header rows may contain arbitrary bounded bytes;
neither their existence nor a recorder frame count proves a final WAV. Actual staging
checkpoint/publication code in `store/staging_checkpoints.rs` and `store/chunk_staging.rs`
accepts V16, deliberately excluding V17. Changing that version predicate alone is unsafe.
The default physical file-operation backend remains Unsupported.

The actual reson writer uses hound 3.5.1, 32-bit Float and a bounded buffered transport.
Hound chooses WAVEFORMATEXTENSIBLE for every 32-bit format, producing a 68-byte header.
The initial `writer.flush()` rewrites the RIFF and data lengths before any PCM arrives.
Naively retaining these as Header revisions 1/2 closes the current journal PCM graph.
Stop drains the ring; Cancel abandons it at a worker barrier. Existing frame/path/health
observations alone do not reconcile accepted but discarded or partially encoded samples.

## One owner and an explicit terminal boundary

Keep the existing operation-journal owner as the only coordinator and SQLite writer.
The session/capture service requests Stop or Cancel; the callback does no journal I/O.
The bounded serializer adapter serializes its terminal request after closing callback
submission and settling any in-flight submission. No new microphone input may enter the
same proposal after that boundary. A caller-supplied `stopped=true`, operation UUID, hash,
or decoded record is not a live terminal capability.

The production adapter must hold the same profile/source acquisition and live output
registration used for retention, and settle its one outstanding batch/reply before closing.
Issue a private owner-local terminal token bound to operation/proposal hash/output/work,
adapter instance, current acquisition and terminal request identity. It is consumed once,
is not serializable or publicly constructible, and cannot authorize physical publication.
Loss of the adapter, token, reply, source pin or worker result leaves NeedsAudit.

The intended durable sequence is:

| State | Permitted new work | Recovery meaning |
| --- | --- | --- |
| Capturing | Existing admitted contiguous PCM; initial header only | Incomplete unpublished bytes |
| Finalizing | Only frozen terminal PCM tail and bounded final-header slots captured at the terminal barrier | No resumed capture; incomplete finalization needs audit |
| Frozen | Read-only bounded reconstruction against an immutable terminal inventory | No new byte identities; verification may restart |
| Sealed | Exact seal replay/readback only | Valid retained byte observation, no live grant |
| Unconfirmed | Read-only retained inspection/reconciliation | No invented successful closure |

Persist Finalizing and its exact terminal inventory before acknowledging that transition.
Include the actual last PCM ordinal/length and admitted pending chunk identity, expected
final header revision range, terminal kind and complete sample accounting. The producer
cannot choose additional PCM later. A transition is serialized with retention and checks
the current registration/effect pin and capacity; it does not bypass them. Exact already
retained chunks remain replayable, but changed inventory/bytes/terminal kind conflicts.
Freeze only after every declared terminal chunk has been retained and acknowledged.
An incomplete declared inventory never becomes Sealed.

Current cancellation/disposition checks prohibit new bytes after cancellation. Do not
weaken them globally. For normal Cancel, close input first and finish the narrowly frozen
serializer finalization under the still-current admission before persisting cancellation.
If journal cancellation, lease drain, error or audit has already won, preserve the prefix;
accept only exact existing-byte replay and read-only inspection. No new patch/write grant
is minted to make the evidence appear complete. Such a prefix stays Unconfirmed unless a
separately admitted reconciliation establishes the complete required terminal evidence.
Cancel never publishes. KeepPartial requires a new explicit admission and namespace check.

## Header normalization and real byte grammar

Before capture starts, the adapter holds at most 128 header bytes. Fold the initial hound
header writes/seeks/length rewrites into one complete Header revision 0; retain that exact
header before its initial physical dispatch. Flush cannot report success until retention
and the actual admitted write are acknowledged. Do not emit post-initial revisions for
initialization. After PCM starts, the first subsequent header rewrite enters finalization;
no periodic header checkpoint followed by more PCM is supported by this first slice.

Rechunk buffered writes into `floor(16384 / frame_bytes) * frame_bytes` PCM slots.
Hound/BufWriter writes may split interleaved frames for channels 3, 5, 6 or 7; an arbitrary
Write call is not one journal chunk. Keep only one bounded pending slot, allocate a stable
chunk identity before its retention request, and dispatch only acknowledged complete-frame
bytes. At the terminal seek/flush, retain and dispatch the actual short complete-frame tail
before any final patch; a partial frame fails retained/unconfirmed. This byte plumbing
cannot discard a remainder, acknowledge an uncertain durable write, or recapture it later.

Pin the producer format as a 68-byte hound-compatible extensible float32 WAV. Existing
proposals with another bounded header size remain retained, but are unsupported for this
producer/sealer; do not rewrite historical values or accept a synthetic 44-byte header.
Validate actual reconstructed bytes, not a proposed encoder specification:

- `RIFF`, exact little-endian RIFF length `file_bytes - 8`, and `WAVE`;
- one `fmt ` chunk of length 40, tag 0xfffe, exact frozen channels/rate, checked byte
  rate `rate * channels * 4`, block align `channels * 4`, and 32 container bits;
- extension length 22, 32 valid bits, channel mask `(1 << channels) - 1` for channels 1..8,
  and float subtype GUID bytes `03 00 00 00 00 00 10 00 80 00 00 aa 00 38 9b 71`, matching
  the supported hound encoder; no caller-supplied format/mask/GUID substitution;
- the `data` header at byte 60, exact data length at byte 64, PCM origin 68, and no
  extra chunks, unknown extension, holes, trailing payload or length truncation.

Reconstruct the complete initial header and apply all retained header revisions in order.
Every patch must be within the declared header and match the frozen identity; reject gaps,
duplicate/conflicting slot identities and PCM appearing after header finalization. Final
length fields must describe the actual retained PCM, including zero-frame recordings.
Do not manufacture a final header from a frame count when actual final patches are absent.
Repeated finalization of an unchanged zero-frame header still requires a genuine terminal
barrier and complete declared patch inventory.

PCM ordinal 0 begins at header extent; each earlier slot is full, the last may be short,
and all lengths are whole interleaved frames. Sum lengths with checked arithmetic and
require `frames = pcm_bytes / (channels * 4) <= max_frames`. Retained PCM must equal the
complete successful encoder coverage, not just a prefix selected by the caller. Hash the
reconstructed final header followed by PCM bytes in logical order using BLAKE3. Hashing
encoded JSON rows or dispatch order produces the wrong object hash.

## Terminal accounting and bounded verification

Consume OPT-2141's actual accepted/encoded/abandoned/unconfirmed sample observations and
completion/overflow states, captured after successful join and its callback/worker terminal
handshake; live snapshots are not atomic. Complete accounting alone can also describe a
failed codec worker, and cannot replace the successful join/no-error requirements. The
exact API remains owned by that implementation; these are observations, not admission.
Complete, non-overflowing accounting requires checked
`accepted = encoded + abandoned + unconfirmed`; each frame-bearing count is divisible by
the frozen channel count. Successful seal requires `unconfirmed = 0`, no writer/finalize
failure, and exact `encoded / channels = retained frames`. Stop additionally requires
`abandoned = 0`. Cancel may account for a bounded abandoned ring; it remains unpublished.
Rejected input samples and overrun events are retained separately from abandoned accepted
samples; monitor-only drops are not recorder input loss. Unknown/incomplete/overflowing
counters never imply loss-free input or a successful terminal seal.

The future publication bridge distinguishes known input loss from loss-free input using
these retained observations. Unknown input accounting remains Unconfirmed. A positive
physical validation result cannot repair missing producer accounting. Health snapshots
must not be allowed to choose a shorter durable prefix or erase an observed failure.

Use at most one 16,384-byte PCM batch, the bounded header, incremental hash state and
eight recovered chunk values per owner turn; do not assemble a 128 MiB recording in RAM.
Header prepass uses at most 16 slots. PCM verification yields after eight rows and can
resume only against the exact Frozen inventory/generation and same retained acquisition.
Bind the cursor to operation/proposal/output and freeze sequence; no bare sequence cursor
can choose a different snapshot. Cap concurrent verifier state at one per acquisition;
use existing bounded command/result credits, with each new control record <=4,096 encoded
bytes and no caller-controlled unbounded vectors/text. A vanished client leaves durable
Frozen evidence and releases only transient verifier memory, not retention/capacity.

The final same-owner exclusive transaction rechecks immutable inventory, namespace,
acquisition and all derived counts/hash, inserts one immutable versioned seal, and verifies
readback before acknowledging. It may not stream the full recording while holding a
transaction. Frozen rows cannot change between page verification and seal insertion.
Persisted seal binds exact proposal hash, operation/output/work, terminal inventory/revision,
final header hash, object byte length/hash, frame count and terminal/loss accounting.
Exact replay is read-only; conflicting seal identity/content fails and preserves evidence.
Commit reply loss is Unconfirmed until exact retained readback on the same owner succeeds.

Any schema extension must extend the actual schema whitelist/migration/reopen checks and
enforced recording SQLite metadata policy. Include control/seal rows in the global 8,192
row and 128 MiB encoded-retention ceilings, pending-input credits, handoff and release
queries. Reserve them during preparation, before PCM fills the capacity; exhaustion must
stop before new retention/effects and preserve all old bytes. Preserve the conservative
database/rollback-journal peak and actual descriptor-volume check; a schema decision alone
is not a reservation grant. Unknown schema/control/codec values stay retained Unsupported.

Reserve exactly four terminal control slots per new producer proposal: Finalizing, Frozen,
Seal and Retirement. With the existing 16 header slots, this first producer admits at most
8,172 PCM slots, so all rows fit the existing 8,192 ceiling. The old maximum 8,176-slot
proposal remains decodable/retained but cannot activate this producer without a separately
admitted compatible plan. Extend actual work/capacity preparation and global reservation
accounting; merely checking available rows at closure is too late. Bound the four control
values by 16 KiB total within the 128 MiB ceiling and reserve that space before capture.
Unconfirmed is a disposition/inspection result, not an unbounded series of new control rows.

## Physical validation and retirement gates

Only a genuine FileOperationOwner result can establish flushed/validated staging. Bind its
operation, participant, output, effect, retained root registration/generation, logical and
physical object identity, request fingerprint, exact length and content hash to the seal.
Require the full existing pre-publication phase/disposition/source/namespace checks and
current live grants. Substitution, stale root, short allocation, lost result or uncertain
flush remains retained NeedsAudit. The bridge must support V17 explicitly, with tests for
existing V16 behavior; it may not widen the version predicate without these bindings.
No path reopen/truncate/delete, reconstructed capability or fake Applied result is allowed.

Sealed means retained byte evidence only. It does not permit charge release, clean handoff,
publication or reclamation. Keep actual recording/control/seal rows and charges until the
coordinator has genuine terminal participant/namespace receipts, no in-flight/replay work,
and an explicit owner-fenced retirement decision. Cancel alone does not close staging.
Atomically retire exact rows with durable retirement evidence and capacity settlement;
uncertain settlement remains charged. No age/size eviction, recorder Drop or positive
source-model fixture permits deletion. Native descriptor/capacity/durability and input
lifecycle acceptance remain OPT-2066/2067 and the existing physical publication issues.

## Concrete implementation and checks

1. OPT-2141 implements actual callback/worker terminal accounting in reson; test in-flight
   submit versus Stop/Cancel/error, rejected versus abandoned samples, partial encoder errors,
   counter overflow, and complete interleaved frames. No journal/physical success claim.
2. OPT-2144 implements bounded production header normalization/prefix reconstruction over actual retained
   chunks, plus immutable seal values. Test real hound output for channels 1..8, zero/short/full
   slots, initial flush, final patches, malformed format/length/order/identity and streaming hash.
3. OPT-2145 implements actual same-owner Finalizing/Frozen/seal retention, admission-bound command and
   bounded resumable verifier. Test real SQLite reopen/replay, immutable inventory, missing
   tail/patches, raced cancellation/drain, unknown variants, transaction/reply faults, row/byte
   exhaustion and preservation of handoff/charge-release gates. Codec fixtures alone do not
   complete this production leaf.
4. Implement retained live serializer/file-owner transport and V17 validation/publication bridge,
   genuine participant retirement and session integration under OPT-2058. Verify actual result
   hash/length against the seal; qualify descriptor relocation/collision/symlink/namespace,
   flush/allocation/crash behavior on supported native platforms before recording activation.

Portable baseline commands are `cargo test --locked -p reson --lib recording::` and
`cargo test --locked -p yield-library --lib operation_journal::store::tests::recording
-- --test-threads=1`, plus canonical agent preflight. At this decision's source baseline they
pass 18 and 14 tests respectively. Existing journal tests include synthetic claim/capacity
observations; real SQLite bytes/reopen evidence does not qualify physical reserves. Future
leaves must run their actual implementations and record exact commands/results/limitations.

## Exact producer terminal byte custody (OPT-2249)

The actual Reson serializer now retains its original `std::BufWriter` behind a
private, single-producer inspector. The genuine pre-finalize barrier exposes a
bounded immutable `RecordingTerminalWriteInventory`: the actual pending offset
and bytes (at most 16 KiB), exact final object extent, and the pinned hound 3.5.1
RIFF/data length patches at offsets 4 and 64. The snapshot performs no transport
I/O. It checks the real transport position plus buffer length against the actual
successful encoded-sample coverage before issuing the barrier. Initialization
still uses the real hound flush, and no terminal bytes leave before acknowledgement.

The finalizer checks its actual tail/patch writes and seeks against this inventory,
including partial transport writes. Missing, changed, extra or reordered bytes
cannot produce successful completion. The successful actual join retains the same
immutable inventory; neither the plan nor accounting has a public constructor or
can reconstruct a producer proof. Denied, late, lost or failed acknowledgement,
transport errors and unwind preserve the existing fail-closed Drop fence.

A pending transport fragment can begin within an interleaved frame for channels
3, 5, 6 or 7. The future admitted adapter must combine it with its existing
bounded rechunking custody; this fragment is not a journal PCM slot. The journal
must still bind stable tail/header chunk identities, current source/profile/sink
admission, immutable Finalizing/Frozen rows and exact retained acknowledgements.
OPT-2145 remains responsible for those production transactions and seals. This
producer evidence supplies no file-owner result, physical qualification,
publication, retention retirement or charge release. OPT-2058 and native
OPT-2066/2067 remain open.

Actual hound tests cover all eight channel counts, zero/short/exact/multiple-buffer
recordings, exact final byte reconstruction, three-byte partial transport writes,
private buffer corruption and terminal trace refusal. Full Reson and compile-fail
privacy tests run separately from the canonical repository preflight.
