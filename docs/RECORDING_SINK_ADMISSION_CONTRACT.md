# Bounded recording sink admission decision (OPT-2041)

This decision names the missing portable contracts before activating recording in Yield.
It does not implement namespace admission, a journal recording adapter, a qualified creation
backend, native device capture or physical durability. OPT-1339 remains open.

## Actual producer and current hazards

Repository search `rg -n 'AudioRecorder|RecorderWriter|WavSampleWriter|RecordingOutcome'
crates -g '*.rs'` finds the exported `reson::AudioRecorder` / `RecordingOutcome`, recording
implementation and its tests. There is no current Yield GPUI/session/core producer of this
recording API. `reson/src/lib.rs` reexports it; absence of an app caller does not make its
path-opening behavior safe for future activation or external use.

| Route | Current owned work | Missing boundary |
| --- | --- | --- |
| `reson/src/recording/mod.rs` `AudioRecorder::start(config, PathBuf)` | Calls capture bootstrap, then CPAL `stream.play`; retains diagnostic pathname | No prepared output, live owner grant or absent-target predicate. |
| `recording/capture.rs` `start_recording_runtime` | Resolves device/config; spawns writer, creates monitor, builds input stream | Writer opens before stream construction/play succeeds; later failure can leave a created/truncated file. |
| `recording/writer.rs` `RecorderWriter::spawn` → `WavSampleWriter::new` | `File::create(path)` → `hound::WavWriter<BufWriter<File>>` | Follows pathname creation semantics and can truncate an existing target; no retained namespace/object fence. |
| Capture callback → `RecorderCapture::submit` | Two-second lock-free ring; whole callback block dropped on insufficient space, health counters increment | Ring size derives from device rate/channels with saturating arithmetic; no explicit accepted format/frame/file budget. |
| Background `writer_loop`, stop/join/drop | Drains at most 4096 samples per read, finalizes WAV header and returns frame count | No owner flush/validate/no-replace publication receipt. Drop finalizes; it is not a cancellation/cleanup protocol. |

The existing serialization and temporary-file tests are codec/worker behavior. They do not
qualify file creation, collision handling, source/profile authority, physical reserve or sync.
The direct writer call remains a tracked unsafe implementation exception until the migration
leaf removes it. This decision does not mark that exception or the parent complete.

## Chosen bounded transport and admission

Keep audio conversion/callbacks in reson and the joinable background serializer. Reson must
consume an already admitted bounded `Write + Seek + Send` transport, with no pathname opener
or fallback. That is byte transport, not filesystem authority. In the Yield integration,
the transport is a channel adapter: FileOperationOwner retains the actual staging descriptor
and live root/namespace/object capability; no File/Dir/SQLite handle is given to GPUI, the
capture callback, or a second coordinator. Diagnostic output paths never reopen an object.

Resolve and validate input selection and build a paused CPAL stream/ring first. A construction
failure must not create output. Before playing or emitting a WAV header, the session prepares
one finite capture request: current profile acquisition, stable OperationId, participant/work
key, OutputIdentity, destination registration/generation, bounded relative staging/final
locators, captured absence/collision policy, exact format and maximum frames/bytes. Persist that
proposal through the existing operation-journal owner; obtain its actual live admission/effect
grants before FileOperationOwner creates absent staging. An ID/digest or historical receipt
alone is not admission. Recheck retained capabilities at each effect and completion barrier.

For the first slice, admit float32 WAV only, sample rate 1..=192000, channels 1..=8, positive
finite maximum frames. Compute checked `frames * channels * 4`, reserve 128 bytes for the
bounded codec header/patches, and reject if total exceeds `u32::MAX` (conservative RIFF limit).
A future RF64/unlimited capture is a separate scope. Ring capacity is checked
`rate * channels * 2`, maximum 3072000 samples / 12288000 bytes. Drain batches remain at most
4096 samples / 16384 PCM bytes. The serializer stops before accepting frames beyond its budget;
a malformed/incomplete interleaved frame fails instead of rounding frame count down.

The sink adapter has one outstanding immutable write batch and a bounded header/seek scratch,
not an unbounded recording buffer or independent disk worker. All offsets, lengths and header
patches must stay within the admitted allocation. It emits checked FileOperationOwner
`WriteStagingChunk` requests with stable output/chunk/issue identity and the actual live chunk
grant. Backpressure may block only the background serializer; never the capture callback.
A full/unavailable transport stops capture and records an unconfirmed/partial outcome; it cannot
fall back to direct file I/O. Existing FileOperationOwner queue/result/input/receipt limits
remain in force. Header finalization, seeking and shorter-than-budget allocation settlement
must be explicitly covered by the prepared chunk graph; a generic hound sink supplies no grant.

## Journal and physical adapter gaps to implement

`file_operation_owner.rs` already names `CreateStaging`, `WriteStagingChunk`, `FlushStaging`,
`ValidateStaging` and `PublishNoReplace`, descriptor-bound registrations and typed Unsupported /
Collision / RetainedUncertain outcomes. Its default accepted effects are still Unsupported;
there is no production recording sink constructor. Do not relabel these foundation types as
a qualified WAV sink.

`operation_journal/model.rs` has `UniversalCommandKind::CreateFile`, but that name alone does
not encode microphone input, immutable finite capture bounds, progressive captured chunks or
header patch/partial-prefix recovery. Add a concrete bounded recording preparation/payload
variant for that family (including codec/version, output/namespace and maximum budget), freeze
its work graph and define per-chunk immutable data retention before dispatch. Audit existing
CreateFile preparation/codec/recovery callers before extending it; do not fabricate a live
recording from a process-local request ID, pathname, old receipt or recaptured microphone data.
Unknown recovered variants remain retained/unsupported until their decoder/adapter is available.

Crash cannot reproduce samples never durably retained. Restart may observe retained exact
chunks and a validated sealed prefix, or retain incomplete staging for audit. It cannot resume
microphone capture as replay of the same proposal, manufacture missing frames, or announce
RecordingOutcome success from a header/file existence check. Exact dispatched chunk duplicates
must compare original immutable data/offset/output and replay their retained result; a changed
payload under that identity conflicts. Payload-retention capacity exhaustion stops before new
writes, preserving existing evidence. Safe receipt/spool reclamation requires journal retention
and namespace closure, not recorder Drop.

## Stop, cancellation and result

User Stop first stops/drops the CPAL stream, then drains the already accepted bounded callback
ring, finalizes the header within its budget and hands exact frame/byte/hash observations to
the owner. The owner flushes, validates the retained staging object and performs an admitted
no-replace publication only when the target absence claim is still current. A failed/uncertain
flush or changed namespace provides no Published result; retain evidence for reconciliation.

Cancellation before creation has no output effect. Cancellation/drop after creation stops
capture and the writer at an owned safe barrier, preserves admitted partial staging and reports
CancelledRetained/NeedsAudit; it never follows the diagnostic path to delete or overwrite.
Explicit KeepPartial is a new publication decision with validated prefix evidence. Cancellation
after publication is TooLate and cannot silently unlink the final file. Stream-build/play,
write/seek, worker panic and result-channel loss all retain typed failure/unconfirmed state;
none returns the old pathname-only RecordingOutcome as persisted success.

Separate results: RejectedBeforeEffect, Capturing, StagedPrefix (frames/bytes/hash observations),
CancelledRetained, NeedsAudit/Unconfirmed, and Published (actual namespace receipt, exact output
identity and captured data/format/health). Dropped input blocks remain visible; distinguish
PublishedWithInputLoss from a loss-free capture instead of claiming every microphone frame was
recorded. A writer's frame count/codec finalize result is not a physical publication receipt.

## Evidence and finite follow-ups

`recording/sink_contract_tests.rs` is an explicitly private decision fixture. It exercises checked
format/ring/RIFF/frame bounds; pre-effect stale/cancel/unsupported/collision rejection; and actual
hound negative Write/Seek/finalize transport behavior in memory. Its synthetic gate states and
codec results are not owner grants, production wiring or physical qualification.

Implement three separately reviewable leaves under OPT-1339:

1. **OPT-2056** — Portable reson sink migration: generic retained transport, checked recording format/frame
   budget, bounded writer and drop/cancel/result states; retire pathname File::create entrypoint
   by making unsupported unadmitted startup fail before opening anything. Files recording
   writer/capture/mod and input error API; tests header/write/seek/finalize failure, frame budget,
   callback overrun, stop/drop/join and existing output preservation. No fake live owner adapter.
2. **OPT-2057** — Bounded recording CreateFile preparation/retention bridge: existing journal model/preparation/
   codec/recovery plus session admission and channel adapter work graph. Test durable frozen
   identities/budget, stale acquisition/namespace, exact/conflicting chunk replay, retained partial
   restart and capacity exhaustion. No GPUI activation before actual grants and result bridge.
3. **OPT-2058** — Qualified FileOperationOwner staging/write/header patch/flush/validate/no-replace adapter and
   final producer/native device integration. Name current retained roots/platform capabilities,
   grant/closure and crash/publication evidence; Unsupported remains a real gate when physical
   primitives are unqualified. Test collision/symlink/relocation, cancellation barriers and reply
   loss without destructive fallback. Native device/UI and physical durability acceptance is
   scoped separately after portable source work. Keep parent and future recording UI tracking open.
