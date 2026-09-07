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

The root workspace composes `crates/wavecrate-gpui` from the core, media, and UI
crates described in `GPUI_ARCHITECTURE.md`. Its dependency tree must not include
the archived Radiant application or Radiant. Keep the existing library,
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

The original application source now lives under `reference/radiant/src`.
Unqualified reference-area names below are relative to that archived frontend.

## Parity register

Every row below requires GPUI implementation and validation. The presence of
reference source or backend code does not satisfy the row.

| Workflow | Reference area | Required acceptance |
| --- | --- | --- |
| Startup, profile, shutdown | `reference/radiant/src/native_app/shell`, `app/state` | Isolated profile, error handling, worker shutdown, reopen |
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
| Platform behavior | `reference/radiant/src/native_app/shell`, `reference/radiant/src/native_runtime` | macOS and Windows build/input/window lifecycle coverage |

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

### Root workspace and browser slice, 2026-09-07

The active root workspace now has a thin GPUI executable plus defined core,
media, and UI crates alongside the existing four engine crates. The core crate
has no dependencies. The launcher depends only on the UI crate. All 2,327
relocated reference source/test/tool files were checked against their original
Git blob hashes and match exactly. The original pre-migration manifest and dirty
root lockfile were also preserved in the local ignored `.sandbox` backup.

Implemented filename filtering, Name/Size/Extension sorting, stable focused-file
projection, native text-edit actions, search/browser focus routing, visible
virtual-list sizing, full-width rows, and constrained waveform drawing. The
native folder picker now uses GPUI's asynchronous path prompt; this fixes the
observed reentrant-borrow crash from the prior synchronous dialog.

Passed on macOS: root workspace check and build; 10 targeted tests (four core,
four media, two UI); formatting checks; development-bundle signature verification.
Cargo metadata confirms eight members, the GPUI launcher as default, and the
documented dependency direction. The existing engine crates were unchanged.

GUI checks with disposable fixtures confirmed visible rows, pointer/arrow
selection, scrolling a 2,000-file folder, filename filtering, native select-all
and deletion, spaces in search, selection preservation after Size sorting, and
the corrected folder picker. The final root-workspace bundle loaded a selected
folder through that picker and showed the correct filtered-result count.

Clipboard automation timed out despite a later text update; Unicode text
injection did not establish GUI acceptance. Clipboard/IME/Unicode interaction,
manual audible/DAW acceptance, and Windows behavior remain unverified. This
slice does not complete full feature parity.

### Theme foundation and reference appearance, 2026-09-07

`wavecrate-design` owns the versioned, renderer-independent theme contract.
The default follows Wavecrate's charcoal/coral palette and embedded Ioskeley
Mono font. The GPUI shell uses a compact toolbar, leading source sidebar,
waveform above the sample table, and bottom status bar. Theme loading, reloading,
and reset are available from the toolbar settings control.

The media projection now includes up to 2,048 low/mid/high/raw waveform columns,
using the reference frequency filters, envelope followers, and normalization.
The projection retains the existing six-second and source-size decode limits;
it does not provide full-length zoom, editing, or shader-rendering parity.
Source and library data remain untouched. Sidebar collections, tags, ratings,
and the other unported controls in the reference screenshot remain parity work.

Validation: 26 targeted tests passed (core 4, design 8, media 9, UI 5), along
with the locked GPUI build/check, scoped Rust formatting, diff checks, and
macOS development-bundle signing/verification. Independent theme-contract
review led to composite layout limits, removal of unused required tokens,
pending-reload protection, and lifecycle regression tests. Full workspace
formatting still encounters pre-existing archived test-module paths in the
inherited engine crates; this pass formats the changed frontend/media/design
modules directly.

GUI checks confirmed default startup with and without a folder, layered preview,
live custom colors/font size/row height, native search selection, edited-file
reload, invalid-file rejection preserving the current theme/sample, default
reset, and the 960×560 minimum layout. The Play button invokes the existing
transport and the stopped state remains visible. Audible quality and DAW
acceptance remain manual checks. Theme choice is currently session-only.

### Session folder browser, motion tokens, and responsiveness, 2026-09-07

Implemented session-only source add/switch/forget with canonical alias
deduplication, an expandable virtual folder tree including empty directories,
direct/descendant scope, and refresh reconciliation. Each source retains its
folder, expansion, and scope; failed refreshes retain the last successful scan.
Cancelled scans cannot restore a forgotten source. No database is opened.

Motion timing is now part of the validated theme contract, with backwards
compatibility for themes that omit the new group. Pure focus envelopes, overflow
transitions, worker pulse, and source sweep primitives cover the reference
timings and an explicit motion-disable setting. UI animation integration is
pending: performance feedback paused the rollout before adding frame cadence.
Hover/focus rails, loading motion, scroll fades, and playback cursor remain to
be connected; mark flashes and waveform gesture effects depend on those future
workflows. These tokens alone do not establish animation parity.

The responsiveness pass follows the development-profile and width-bounded
waveform approach inspected in
[Rox at 332b257](https://github.com/zealsprince/rox/tree/332b2579b0d72ed4b9e1ca76163e789be7c9732e).
GPUI dependencies and the UI/media crates now compile with optimization in
development. Waveform paths retain tessellated geometry, invalidate on preview,
layout, or peak-scale changes, and preserve per-lane extrema when reducing to
the visible logical width. Folder rows are projected only when their tree or
expansion changes. Playback polling stops when idle and only notifies the UI
when visible playback state changes.

Validation: 45 targeted tests passed (core 10, design 16, media 10, UI 9),
plus the manual waveform cache benchmark. On this machine, 100 geometry misses
took 89.302 ms and 100 hits took 1.606 ms; this measures geometry preparation,
not complete frame latency. Workspace formatting and diff checks passed.
Archived engine test references were restored under their owning crates;
66 targeted Reson/library tests passed.

Disposable-fixture GUI checks confirmed direct scope, descendant scope excluding
similarly prefixed sibling folders, empty-folder preview clearing, independent
disclosure/selection, and per-source state restoration. Full application frame
latency and audible acceptance remain distinct from these tests. The final
optimized macOS bundle was rebuilt and signature-verified; the user tested it
and confirmed responsiveness was "much better". The cached layered waveform
remained visible after changing folders.

### Hover and loading motion, 2026-09-07

Sample, source, and folder rows now show the reference's immediate pale 2-point
trailing hover rail while retaining their coral selection marker. Pending work
drives a 1.15 Hz circular status pulse, and source scans show the 0.45 Hz
wrapping sweep with a fading tail. The gear panel's session-only Animations
toggle disables continuous motion while retaining static busy feedback.

One browser-level frame gate checks current scan, waveform, and audio-init
intent. Completed waveform tasks and cancelled audio-init intents cannot keep
the animation cadence alive. No per-row timers were added, and waveform and
folder caches remain unchanged. Pending source rows fit inside the existing
source viewport rather than overflowing into folders.

Validation: locked UI check, 15 UI tests (one manual benchmark ignored),
workspace formatting, diff checks, and signed macOS bundle build passed. Tests
cover frame eligibility, cancelled/terminal work signals, disabled motion, and
sweep bounds. GUI checks confirmed sample/source hover rails, the toggle in
both directions, valid preview recovery after an invalid WAV, and absence of a
lingering busy indicator after the error. Short fixture loads did not establish
manual timing acceptance of the pulse/sweep. Focus fades, scroll-edge fades,
playhead motion, and effects belonging to unported workflows remain pending.

### Focus and scroll-edge motion, 2026-09-07

Sample, source, and folder focus uses stable path identity and a pale 6-point
rail over the persistent coral selection marker. Pointer focus fades over
250 ms; keyboard focus holds for 800 ms before fading. Selection reconciliation
clears stale sample focus without restarting effects during sorting or rendering.

Scrollable lists show 48-point directional edge gradients driven by actual
viewport bounds and scroll range. Their 260 ms transitions reverse from the
current opacity and stop requesting frames when settled. The reference's folder
edge affordance also applies to GPUI source and sample lists. Paint-only overlays
preserve row input. Disabling animation keeps focus static and updates edges
immediately; waveform geometry and folder projection caches remain unchanged.

Validation: 17 UI tests passed (one manual benchmark ignored), launcher check,
workspace formatting, diff checks, and signed macOS bundle build passed. Focused
independent review found no actionable issues. Disposable-fixture GUI checks
confirmed edge direction at both scroll limits, row clicks through gradients,
keyboard focus settling to the coral selection rail, and immediate disabled-motion
feedback. Filtering to one file removed overflow gradients and stale focus;
changing the settings panel height updated overflow feedback. Animations were
restored after testing. Playhead motion, waveform gestures, and effects belonging
to unported workflows remain pending.

### Playback cursor, 2026-09-07

The waveform now draws a pale playback cursor using the optional semantic
`waveform_playhead` color. It maps engine elapsed time into the actual preview
duration for the current zero-start one-shot transport, then hides outside the
preview. Interpolation is bounded to 80 ms between received samples; disabled
animation uses sampled positions. Stop, cancellation, errors, completion, and
selection/source changes clear the cursor. Progress polling drains responses
before scheduling the next request and remains inactive when transport is idle.
The overlay does not invalidate cached waveform geometry.

UI and design tests, UI check, formatting, diff checks, and a signed macOS build
passed. Disposable-fixture GUI checks confirmed a cursor around the preview
midpoint at three seconds in a twelve-second file, disappearance beyond the
six-second preview, replay reset, selection clearing, and two-second one-shot
completion. This establishes visual behavior, not audible synchronization or
full waveform/seek/loop parity. A separate metadata fix removes a duplicate WAV
channel-count division; two targeted probe tests passed, including mono,
stereo, and four-channel duration coverage.

### Browser multiselect, 2026-09-07

An identity-based core selection model now supports replacement and additive
ranges, toggling, Select All, and projection reconciliation. Pointer modifiers
and Shift+arrows drive ranges; Cmd/Ctrl+A selects visible rows while browser
focus is active, and the reference X shortcut toggles the focused file. Plain
arrows retain batch membership while moving preview focus; a plain click clears
it. Source/folder navigation clears membership, and filtering removes hidden
identities. Selected count, batch highlighting, and focused-sample feedback are
separate. Range navigation borrows projection identities rather than cloning the
whole list, and ordinary arrow navigation does not allocate that list.

Validation: 19 core tests and 22 UI tests passed (one manual benchmark ignored),
formatting and diff checks passed, and a signed macOS bundle was built. Live
fixture checks covered keyboard ranges, focus movement preserving membership,
Select All without scrolling, query reconciliation from 80 to 10 to 1 selected,
search-field Select All routing, plain-click reset, and focused toggle. Modified
pointer routing has code/core coverage; manual modifier-click acceptance remains
unrecorded. Batch file mutations and persisted selection remain outside this slice.

### Full-length WAV waveforms, 2026-09-07

WAV waveforms now cover the complete source timeline, including silence, using
two buffered streaming passes and fixed 180-peak/2,048-band output. The first
pass finds the global mono peak; the second normalizes and projects the existing
frequency layers at the source sample rate. The six-second and 256 MiB preview
limits are removed. Cancellation checks run during both passes, and the UI
cancels superseded loads while retaining generation fencing. Malformed or
truncated files fail without publishing partial output.

Validation: 14 media tests and 22 UI tests passed (one manual benchmark ignored),
plus UI check, formatting, diff checks, and signed macOS build. Independent
review found no meaningful issues in memory bounds, decoding, cancellation, or
cache invalidation. Live checks confirmed a late transient at eight seconds in
a twelve-second stereo file, successful loading of a 300 MiB WAV, and prompt
switching from that pending load to a one-second file. Sampled process RSS was
104,560/104,608/104,624 KiB before/during/after the large-file load; these are
samples rather than a peak-memory benchmark. CPU returned to idle after loading.

A broader workspace test run reached the library test binary, which reported
439 passes and six profile ownership failures involving temporary paths
(`Not a directory`). Those failures
also reproduced in the baseline. The profile test fixtures now resolve the
ambient macOS temporary-directory alias before exercising the no-follow root
capability. All seven ownership tests pass, including replaced-root, replaced-lock,
and symlink rejection coverage. Production ownership rules are unchanged. The
broader workspace suite has not been rerun after this targeted fixture repair.
That failure report is historical: at baseline commit `288430e10`,
`cargo test --locked --workspace` was rerun after the fixture repair and passed
all workspace binaries and doc tests, with two tests ignored. The output is
retained at `/tmp/wavecrate-gpui-workspace-tests.log`.
Non-WAV preview and zoom/edit gestures remain outstanding.

### Waveform seek and loop audition, 2026-09-07

Clicking the waveform auditions from its normalized position, including when
search previously held keyboard focus. Pending audio initialization retains the
latest fenced playback intent. The cursor follows normalized engine progress
and snaps on loop wrap. The L shortcut and toolbar toggle select looping for the
next audition; changing this preference does not retarget the active stream.
Live cycle-end loop disabling remains outstanding.

Validation: 27 UI tests passed (one manual benchmark ignored), 17 design and 19
core tests passed, plus UI check, formatting, diff check, and signed macOS build.
Live fixture checks confirmed seeking near 75%, repeated two-second loop playback,
and one-shot completion with looping disabled. These are transport/UI checks,
not manual audible acceptance.

### Waveform play-region audition, 2026-09-07

Primary-button dragging over a loaded waveform previews a normalized play
region and commits the ordered, non-zero range on release. A release within the
reference two-logical-pixel horizontal click slop clears the prior region and
auditions the release position; releases outside the waveform clamp to its
captured bounds. Escape cancels the provisional drag and preserves the previous
committed region. Space and toolbar Play use the committed region for bounded
one-shot playback or the next loop audition, while a direct waveform click
clears it before seeking. The selection fill and playhead theme tokens are
paint-only overlays, so waveform geometry caching and idle rendering remain
unchanged. Source/sample changes, generation invalidation, and waveform load
errors clear both provisional and committed regions.

Secondary edit selections, region resize/move/shift gestures, zoom/pan, and
extraction/editing remain outside this slice.

Validation: the isolated clean-base region build passed all 49 UI tests (one
manual geometry benchmark ignored), formatting, diff checks, and signed macOS
build verification. Live fixture checks confirmed drag edges, bounded one-shot
completion, sustained region looping, and click-to-clear seeking. Manual audible
acceptance remains separate.

### Selection autoplay, 2026-09-07

Selection autoplay is a session-only preference enabled by default. A changed
sample selected with a plain pointer click or unmodified Up/Down queues the
latest selection-autoplay intent. Shift/Cmd/Ctrl/Alt/Function selection gestures,
range and toggle membership changes, X, Select All, and passive projection/source
restore remain silent. Waveform seek, Space, toolbar Play, and explicit replay
retain their explicit transport origin and work when autoplay is disabled.
Disabling the preference cancels only a pending selection-autoplay audio
initialization; explicit pending and active playback remain intact. Existing
epoch, generation, and selected-path fences still reject stale completions.

Validation: the focused autoplay routing run passed 3 UI tests; the full UI
library run passed 34 tests with one manual benchmark ignored, and the core and
design libraries passed 19 and 17 tests. The UI test commands used a temporary
module-level `dead_code` allowance while the parallel external-clipboard module
is registered but not yet consumed; no global lint suppression was used, and no
selection-autoplay warnings or failures were reported. Formatting and diff
checks (`cargo fmt --all -- --check`, `git diff --check`) passed. The signed
macOS app passed live checks for pointer/arrow autoplay, silent Shift selection,
disabling autoplay, and explicit Space playback with autoplay disabled.
Audible acceptance remains separate.

### Native file handoff and reveal, 2026-09-07

When browser focus owns Cmd/Ctrl+C, the current visible explicit membership is
copied in display order through the native file clipboard service. If no
explicit visible member remains after filtering, the focused sample is the
single fallback target; an empty browser has no platform side effect. Search
focus keeps its native text-copy action, and entering or copying from search
invalidates pending file publication. Reveal is a toolbar action for the
focused sample only. Filesystem validation runs on the background executor;
clipboard publication and `cx.reveal_path` run on the UI thread. Command
generation plus source, focus, selection, and visible-projection context fence
late completions, and browser drop cancels the handoff task.

Validation: 7 focused handoff tests and the full UI library suite passed (41
passed, one manual waveform benchmark ignored). `cargo check -p wavecrate-ui`,
`cargo fmt --all -- --check`, and `git diff --check` passed. The signed macOS
app copied two selected WAV files into Finder; both pasted copies matched their
sources byte for byte. Reveal selected the focused file despite batch membership,
and search text copy/paste retained its text behavior.

Native clipboard review found a Windows format-ordering issue, now fixed by
publishing mandatory CF_HDROP before the optional drop-effect hint. An isolated
Windows-target harness type-checked the service and tests successfully after
repairing a format-ID type and test borrow errors. Full Windows app compilation
remains blocked by the missing C cross-toolchain; Windows execution and DAW
paste acceptance remain unverified.

### Isolated source registry and application shutdown, 2026-09-07

`wavecrate-session` now owns the ordered registry of accepted canonical source
roots in the isolated `gpui-development` profile. The browser submits `AddRoot`
only after a scan succeeds and consumes immutable snapshots through the
app-owned `SourceRegistryGlobal`; restored roots receive durable IDs and empty
session data without an automatic scan or audition. Removal hides a source
while its request is pending and reconciles it only after the corresponding
request and durable snapshot acknowledgements. Duplicate add/remove requests
are resolved by explicit `RequestApplied` events, including idempotent no-ops,
so UI state does not infer completion from an intermediate snapshot. Persistence
failure closes the mutation path and leaves an error in the status bar.

The service is retained until application quit. The quit observer closes
admission and joins the owner synchronously before returning its ready future;
dropping a Browser only cancels its event poll. The service writes only the
isolated source registry through the existing library writer. It does not write
source-local databases, metadata, or audio files; theme, autoplay, selection,
and scan projections remain session-only. The application Quit menu item and
Cmd/Ctrl-Q action use the same graceful boundary, so normal exit exercises the
service shutdown observer.

Validation: `cargo test -p wavecrate-ui --lib` passed 45 tests with one manual
waveform benchmark ignored, and `cargo check -p wavecrate-ui` passed after the
integration. The focused tests cover restored-ID matching, in-flight scan
preservation, stable-root failure mapping after source reorder, and the
selection session projection. `cargo fmt --all -- --check` and `git diff --check`
passed. The session service passed 11 tests and the library writer passed 15
targeted tests. The full locked workspace suite and signed macOS build passed.
Independent review identified idle ownership-failure reporting and unresolved
requests; the fix was reviewed with no residual finding in those paths.

Live validation used a disposable config base under `target/qa`: successful
scan/add and save, graceful Cmd-Q, restored source without scanning or audition,
source removal, another graceful quit, and an empty registry on reopen. Source
audio files remained unchanged and the database was confined to the
`gpui-development` profile. Windows lifecycle execution remains unverified.

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
