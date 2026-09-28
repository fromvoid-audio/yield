# Prepared waveform edit handoff (OPT-1570)

Decision at GPUI main `96df7b4e3`: use the existing media preparation and V7
waveform-edit intent contract. A GPUI dispatcher may prepare an unpublished
result, but it must not report a committed edit without a qualified file
operation owner. The current macOS publication and cleanup capabilities are
unsupported under [the platform decision](GPUI_MACOS_PUBLICATION_DECISION.md).

## One reverse-selection fixture

The fixture in `crates/yield-media/src/edit_intent.rs` uses source `edit-source`,
source/database and filesystem participants, a stored source-fence observation
(lifecycle 1, lease epoch 7, revision 1), a relative source endpoint, and a
strong WAV file observation. It captures a stereo, 48 kHz, 16-bit source with
10 frames and the half-open selection `[2, 8)`. The operation is `Reverse`.

| Boundary | Required input or outcome |
| --- | --- |
| UI admission | Capture source/profile authority, lifecycle and lease epoch, source revision/root identity, relative path, file revision, format, total frames, selection, and edit settings as one immutable request. The worker-only absolute path must not become durable authority. A changed selection or source cannot reuse this request. |
| Durable intent | `waveform_edit_intent` validates the source endpoint and participant set and records the file revision, format, frame count, `[2, 8)`, operation, and Float32 output specification. Reconstructing with a different file revision fails with `StaleContext`; a stored path does not grant a reopen. |
| Background preparation | Run `prepare_wav_edit_with_cancel` off the UI thread into a fresh, empty, unpublished sink with a decoded-byte budget and a cancellation token. It reopens and checks the strong source revision before, during, and after decode/encode. Reverse preserves frame/channel order outside the selected span. Any cancellation, stale observation, budget error, or write error makes the sink disposable and cannot publish success. |
| Progress and cancellation | Report bounded lifecycle phases (queued, preparing, awaiting owner disposition, committed or blocked) with an operation identity. Preparation has cancellation checkpoints but no trustworthy completed/total progress callback; do not invent a percentage. Before publication, cancellation discards or retains staging only through the owner disposition. After an uncertain effect, retain journal evidence for recovery. |
| Publication handoff | Pass the completed staged object, captured owner/operation identities, exact source observation, output specification, and validated bytes to a retained file-operation owner. That owner must durably record `CopyValidated`, revalidate source and destination authority, claim the final name without replacing an unrelated object, update the source database, and provide a receipt before UI reconciliation or undo authority. The media sink/path alone is never a publication command. |
| Current macOS result | `staged_file_finalization_capabilities()` reports unsupported publication and cleanup. The dispatcher must surface a blocked/unsupported outcome and preserve staged data and journal evidence if an intent was admitted. It cannot replace the source, claim undo, or call preparation a committed edit. |

The same request shape covers gain/fades, mute, trim, crop, padded crop and
circular slide; operation-specific frame semantics stay in `yield-media`. Whole
file normalization has a separate preparation result and remains OPT-1723.
This decision changes no algorithm or UI.

## Missing implementation contracts

OPT-1598 still needs a typed GPUI command dispatcher and source-lifecycle
capture through its background queue. Production code does not yet connect a
prepared sink to an owner-routed edit command or progress disposition. A
qualified macOS publication and cleanup path remains blocked by OPT-1632 and
OPT-1868; the platform capability query alone does not implement it. No
destructive GPUI edit can pass native success acceptance until that boundary
is resolved. The current adapter and preparation tests prove request shape,
staleness, cancellation and output, but not publication or recovery.
