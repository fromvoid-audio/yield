# Production admission inventory (OPT-2003)

Reviewed active GPUI and archived reference in `PORTALSURFER/yield` at
`a3e326e829dd8879d5dd42469bc8b90604d9a332`. The reference is the checked-in
`reference/radiant` tree at that SHA, not a separately executing application.

Reproduce the production witness inventory with:

```sh
python3 scripts/internal/check/index_gpui_admission_callers.py > /tmp/yield-admission-callers.json
```

The saved [baseline witnesses](gpui-admission-callers.json) include input
digest, source hashes and exact lines. The lexical scan excludes inline and
external test modules and scans all active crates, including catalog and
session services. It includes declarations and worker forwarding; those are
not family producers. Aliases/macros/conditional compilation remain a source
review limitation. The archived native/controller methods are comparison
evidence, not active GPUI callers.

## Shared boundary and precise missing pieces

`operation_journal/owner.rs:496` defines crate-private `PreparedAdmissionInput`
with private fields and no production constructor. `validate_shape` checks
participant/command/plan/claim relationships. `OperationJournalClient::admit_prepared`
at line 3836 queues that input; the only production invocation is worker
forwarding at line 5017. There is no active UI/catalog/session invocation or
production input construction in this inventory. `admit_full` is a declared
fallback request, not evidence that family entrypoints use the common gate.

The owner reaudits durable claims and descriptor capacity in `admit_prepared`
at line 1464. Estimator results are authority-free. A known staging byte bound
does not resolve the other eight allocation classes, produce claims or grant
dispatch. `DecodedEdit`, `DecodedExport` and `DecodedExportBatch` additionally
fail initial prepared admission because they have no initial-intent codec
(`validate_shape`, lines 547–560). Encoded full-progress fixtures do not repair
that initial admission gap.

`file_operation_owner.rs:2959` executes only `ValidateStaging`; all other
effects return `Unsupported` with no producer/qualified backend. Source/role
registration, effect enums, live chunk permits, replay tests and staging
validation are foundations, not file-copy/move/restore execution.

## Family caller and handoff table

Every row below has **no active GPUI/session prepared-admission producer**.
Its original bounded admission issue stays open. Control gaps are also mapped
in [the action register](GPUI_REFERENCE_ACTION_REGISTER.md).

| Admission owner | Typed source shape / capacity | Active surface or missing producer | File-owner effect / unresolved handoff | Bounded prerequisites |
| --- | --- | --- | --- | --- |
| OPT-1693 Copy | `coordinator.rs`: `CopyIntentArgs`; `estimate_capacity(Copy)` uses positive checked source file bytes for `DestinationStaging`; other classes require descriptor-bound facts | `handoff.rs::copy_selected_files` publishes clipboard path text; it does not perform a durable file copy. Physical copy producer remains OPT-1584 | staging/write/validate then `PublishNoReplace`; no qualified effect dispatch | OPT-2001 publication grant handoff; OPT-1634 verified checkpoints; OPT-1584 producer |
| OPT-1694 Duplicate | `coordinator.rs`: same-file copy; `render_commands.rs`: double output checked WAV arithmetic. Other classes remain unresolved | No durable duplicate producer; native reference menu messages are gap rows | same publication boundary as copy; preparation/enum fixtures do not publish output | OPT-2001, OPT-1634; OPT-1578 producer |
| OPT-1695 Import/drop | `coordinator.rs`: `ImportIntentArgs`; checked external file size for staging only | No admitted import/drop producer. Source-root registration/scanning is not importing a physical file | source capture, bounded staging, `PublishNoReplace`, metadata outcome | OPT-2001, OPT-1634; OPT-1593 producer; external capture/provider qualification retained |
| OPT-1696 Export | `decoded_export_commands.rs`: checked single-output WAV bound; multi-output batch returns `OutputBoundNotExposed` | No production export producer; full-progress command lacks initial-intent codec | bounded output then no-replace publication; no publish authority from preparation | OPT-2001, OPT-1634; initial codec/producer work remains OPT-1696; region lifetime OPT-1622/1623 is separate |
| OPT-1697 Extraction | `selection_output_commands.rs`: single extraction checked output bytes; extract-and-trim is explicitly unknown, not a summed guessed bound | No source-owned selection-extract producer. Playmark selection/DSP preparation is narrower | incremental chunk claim/grant before each write; staging validation and publication after drain | OPT-2001/2002; OPT-1610 producer; composite OPT-1613 separate |
| OPT-1699 History restore | `history_commands.rs`: folder/waveform restore actions; all classes `MetadataBoundNotDefined` in `history_matrix` | Transport playback history and dictionary undo are not committed filesystem history | immutable retained restore evidence, capability-bound restore/replace, journal disposition, cleanup | OPT-1998 rename/move contract, OPT-2002 cleanup proof; OPT-1611/1612 committed history adapters |
| OPT-1701 Same-source move | `namespace_commands.rs`: `NamespaceMoveIntentArgs`; all classes `MetadataBoundNotDefined` | No physical move producer; source selection/order and collection rename are distinct | `MoveNoReplace` exists only as a request shape; worker returns `Unsupported` | OPT-1998 producer/capacity handoff contract; OPT-1571 action |
| OPT-1702 Cross-source move | No independent prepared multi-volume production route. Namespace move shape does not establish a destination-first saga or capacity plan | No admitted cross-source producer | destination publish and verify before origin retirement; independent cleanup proof/absence and participant checkpoints | OPT-2001/2002; OPT-1641 ordering contract; OPT-1840/1846/1848; OPT-1585 action |
| OPT-1707 Relocation undo | `history_commands.rs::HistoryAction::FolderMove`; metadata bound unresolved | No application committed relocation-history producer | admission plus `MoveNoReplace`/retained restore proof; replay must preserve OperationId and outcome | OPT-1998, OPT-2002; OPT-1620 relocation history adapter |

## Queue reconciliation

Completed common foundations OPT-1337, OPT-1338, OPT-1563, OPT-1566 and
OPT-1630 are not current blockers by themselves. Their strict non-goals did
not implement family producers, metadata/database peak bounds or qualified
physical effects. The family issues now link actual bounded publication,
cleanup or rename/move handoff prerequisites, and retain their producer
requirements. This audit adds no duplicate physical implementation task.

Linux-ready contract work is OPT-1997/1998/1999/2000/2001/2002. Review its
concrete output before enabling any command. Unsupported metadata peak
bounds, missing initial codecs and absent effects must remain explicit;
native verification cannot waive these source/ownership gaps. Once a contract
is accepted, each existing family admission issue is the bounded portable
producer owner. Production effects still need the corresponding native
qualification; release acceptance remains separate.

## Validation

Source architecture/owner guards and active `yield` compilation validate the
current ownership boundary. Portable estimator fixtures cover known/unknown,
overflow and missing-row rejection; they do not prove initial admission or a
physical operation. The real owner admission/descriptor fixtures gated to
macOS were not run here and remain in the native queue. No fake successful
file operation or synthetic capacity authority was introduced by this audit.
