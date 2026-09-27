# GPUI reference edit-action inventory (OPT-1734)

Checked against GPUI main `32c78b988` and the active reference action catalog,
controller dispatch, native waveform menu, and destructive-edit enum. This is
an inventory and routing decision. A prepared WAV operation is not a durable
GPUI command or native acceptance.

| Active reference action | GPUI owner and current boundary |
| --- | --- |
| Normalize focused file or waveform selection | OPT-1711 prepared the bounded audio output. OPT-1723 still owns the durable command, source reconciliation, and native action. Normalized audition is a separate playback setting. |
| Crop, silence-padded crop, trim, reverse, mute | OPT-1475, through its prepared-edit contract/dispatcher and operation-specific children OPT-1602, OPT-1603, OPT-1601, OPT-1606, and OPT-1607. Preparation exists in `yield-media`; no completed destructive GPUI route is claimed. |
| Selection gain, left/right fades, and Apply edit-mark effects | OPT-1475 children OPT-1604 and OPT-1605, sharing its dispatcher. The reference applies pending gain/fade handles on Enter/Apply. |
| Circular audio slide | OPT-1608. The prepared whole-file rotation is present, while the durable overwrite and native gesture remain open. Moving a selection marker alone is a different, nondestructive waveform interaction. |
| Extract selection, extract to destination, crop to a new sample, save selection to browser with Keep rating, and extract-and-trim | OPT-1477 children OPT-1610, OPT-1613, and OPT-1621. Destination naming/conflicts, new-row/selection convergence, metadata, composite recovery, and native evidence belong there. External drag/clipboard artifact lifetime belongs to OPT-1478. |
| Delete loaded sample | OPT-1479 and its typed file-operation owner; no direct GPUI pathname shortcut. |
| Play/zoom a selection and find similar sections | Transport/waveform and similarity owners OPT-1492 and OPT-1484. These do not constitute destructive edit parity. |
| Align waveform start to the hovered marker | OPT-1608. The reference calls its circular-slide commit with the captured marker; this is a whole-file audio rotation, not a separate edit algorithm. |
| Detect silence/duplicate slices; select, focus, audition, exempt, merge, or delete review markers | **Gap A** below. These are active reference actions, but GPUI transient-marker playback does not provide the reference slice-review workflow. |
| Clean exact duplicate slices after review | **Gap B** below. The reference exposes a destructive cleanup action; its review state and durable outcome are both missing from GPUI. |

`ShortEdgeFades` and `ClickRemoval` exist in the reference internal destructive
enum and worker dispatch, but neither appears in its active UI action catalog,
native waveform menu, or shortcut help. They are not claimed implemented or
waived; reopening either as a product action requires a separate user-facing
contract and evidence. Browser-wide duplicate/file cleanup remains with
OPT-1479, separate from waveform slice cleanup.

## Bounded follow-up proposals

**Gap A — restore one slice detection and review path.** Project the existing
reference silence and exact-duplicate slice results into GPUI with source/file
revision fencing; retain selection, focus, export marks, and duplicate
exemptions while the loaded file is unchanged. First slice: one detector and
its review controls with no file mutation. Verify stale results and marker
edits. A second bounded slice adds the other detector and its distinct
exemption/audition behavior against the same review-state contract. No new DSP
algorithm or destructive cleanup is part of either slice.

**Gap B — commit reviewed duplicate cleanup.** After Gap A establishes review
state, capture the accepted windows and exemptions, stage output through the
same durable edit owner, and publish exactly once with undo/recovery. Verify
cancel, stale file, publication failure, retained original, and native review
before accepting a destructive result. No automatic cleanup without review.

OPT-1487 remains open until the implemented actions and these gaps receive
their own GPUI acceptance. Reference actions were checked in
`reference/radiant/src/app_core/actions/catalog/data.rs`,
`reference/radiant/src/app_core/controller/waveform_actions/editing.rs`,
`reference/radiant/src/native_app/app_chrome/waveform_context_menu.rs`, and
`reference/radiant/src/app/state/controls.rs`; the marker-alignment route was
verified in `reference/radiant/src/app/controller/ui/waveform_slide.rs`.
