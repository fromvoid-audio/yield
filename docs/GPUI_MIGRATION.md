# GPUI fork and feature parity

This fork targets full current Wavecrate feature parity with GPUI replacing
Radiant. A working GPUI window or browser is an intermediate milestone, not
completion of the reimplementation.

## Baseline

- Upstream: https://github.com/PORTALSURFER/wavecrate.git
- Fork point: `ed024e40e7cee8b4405791b721a068c45183e6de`
- Local repository: `/Users/portalsurfer/dev/wavecrate-gpui`
- Development branch: `codex/gpui-reimplementation`
- Private fork repository: https://github.com/PORTALSURFER/wavecrate-gpui
- The original checkout and application data remain independent of this fork.

Inherited GitHub release workflows are retained with `.disabled` extensions so
they cannot publish the reference application from this repository. Establish a
GPUI-specific build and release path before enabling distribution automation.

The original source remains available as the behavioral reference during the
migration. `docs/TARGET.md` defines the product direction;
`docs/IO_ARCHITECTURE_TARGET.md` and `docs/DATABASE_MIGRATIONS.md` retain authority
over data ownership and persistence. References to Radiant in inherited documents
describe the reference implementation, not a requirement for the new frontend.
Existing implemented behavior is the parity baseline; future target features do
not become implemented merely because they appear in the target document.

## Architecture

The new application lives in `apps/wavecrate-gpui`. Its dependency tree must not
include the root `wavecrate` package or Radiant. Keep the existing library,
analysis, scan, and Reson audio crates as shared engines. Extract application
services from the original frontend as each workflow is migrated; do not expose
Radiant widget messages, geometry, subscriptions, or runtime handles as the new
application contract.

GPUI owns windows, focus, input, layout, list virtualization, overlays, and
painting. Application services own commands, selection, background work,
operation identity, cancellation, and completion validation. Existing storage
owners retain filesystem, database, transaction, and recovery authority.

Blocking file enumeration, decoding, analysis, and database operations belong
off the UI thread. Results must identify their originating request so a late
completion cannot replace a newer selection or source. Retain the original
shutdown and ownership ordering when extracting persistent services.

Development builds must use an independent profile before enabling persistence.
They must not open the live Wavecrate database or recovery journal by default.
File mutation features require the existing capability, journal, and undo
boundaries; a path-based shortcut is not an equivalent implementation.

## Parity register

Every row below requires GPUI implementation and validation. The presence of
reference source or backend code does not satisfy the row.

| Workflow | Reference area | Required acceptance |
| --- | --- | --- |
| Startup, profile, shutdown | `src/native_app/shell`, `app/state` | Isolated profile, error handling, worker shutdown, reopen |
| Sources and folders | `sample_library`, `source_processing` | Add/remove, navigation, unavailable roots, scans, watcher convergence |
| Browser | `app_chrome/library_browser` | Virtual rows, sorting, search, filters, multiselect, keyboard navigation |
| Audition | `audio`, `crates/reson` | Immediate selection playback, transport, seek, loop, history, device errors |
| Waveform | `waveform`, `app_chrome/waveform_panel` | Real peaks, zoom/pan, play/edit selections, gestures, playhead |
| Extraction and editing | `waveform_edits`, `waveform_edit_effects` | Region export, trim/fades/gain/normalization, cancellation, correct output |
| Undo and recovery | `transaction_history` | Undo/redo, crash recovery, source/profile identity, failed operations |
| Tags and naming | `metadata` | Completion, category edits, multi-file edits, display names, safe disk rename |
| Curation | `sample_library` | Ratings, aging/listen history, color collections, physical organization |
| Analysis and discovery | `source_processing`, `readiness_execution` | Progress/cancellation, similarity, existing map behavior |
| Timing | `audio`, `waveform` | BPM/grid controls, existing warp and normalized-audition behavior |
| DAW handoff | `external_drag`, `external_clipboard`, `workflows` | Drag, copy, reveal, external open, region handoff |
| Settings and auxiliary UI | `app_chrome`, `release_update` | Audio options, dialogs, context menus, shortcuts, updates, diagnostics |
| Platform behavior | `shell`, `native_runtime` | macOS and Windows build/input/window lifecycle coverage |

## Completion evidence

### Initial slice, 2026-09-07

Implemented: standalone GPUI workspace, cancellable recursive folder enumeration,
virtualized list and selection, bounded WAV waveform preview, Reson WAV audition,
keyboard transport, background audio initialization, and fenced transport events.
No profile/database is opened and no source mutations are performed.

Passed on macOS: `cargo check --locked`, `cargo build --locked`,
`cargo test --locked` (seven tests), and formatting, using the GPUI manifest.
The resolved application dependency tree contains neither Radiant nor the root
Wavecrate package. Focused review found transport completion/initialization races
and preview allocation risks; the implementation adds request/intent fencing,
progress polling, background initialization, and WAV-only bounded decoding.

This is partial browser/audition coverage, not completion of any broad parity
claim. No manual GUI/audio or Windows acceptance is recorded. Persistent source
registration, search/filter/tagging, editing, recovery, full-length waveforms,
non-WAV audition, and handoff remain outstanding. Preview currently rejects
files above 256 MiB and renders only a bounded initial segment.

### Required final evidence

Record implementation status and exact checks for each migrated workflow.
Retain useful backend tests, add renderer-neutral command/service tests when
extracting behavior, and test GPUI focus/input and async completion boundaries.
Use disposable fixtures for edit/recovery validation. Validate large-library
responsiveness and long-file memory behavior before declaring performance parity.
Build success is not audible or DAW handoff acceptance; those need explicit
manual results. Remove Radiant reference code and legacy release wiring only
after the register is satisfied and the GPUI application is the verified release
path.
