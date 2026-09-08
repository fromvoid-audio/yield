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

### Persisted preference controls and recovery, 2026-09-08

The browser now projects the profile-owned UI preference stream into the GPUI
settings controls. Motion, selection autoplay, loop preference, and complete
theme imports are submitted as ordered typed patches. A late full snapshot is
installed as the new baseline and all still-pending patches are replayed in
submission order, so an early toggle or theme import cannot be overwritten by
an older asynchronous load. Request-applied and durable-persisted revisions
are fenced separately; the settings panel reports Saving, Settings saved, and
save or load failures without exposing internal revision numbers.

The persisted theme contains the complete validated theme and its optional
source path. Reopening the app restores that theme directly from the snapshot;
the source file is read only when the user explicitly chooses Open or Reload.
An invalid stored payload disables ordinary edits while leaving Reset saved settings
available as the explicit recovery action; the normal Default control restores
only the theme. A preference-only write
failure keeps that recovery action available and does not close the source
registry lane; a shared owner failure remains terminal. Passive preference
snapshots update controls and theme projection without replaying transport
actions or retargeting current playback.

Validation: `cargo check -p wavecrate-ui` passed; the focused preference state
run passed 4 tests; and `cargo test -p wavecrate-ui` passed 53 tests with one
manual waveform benchmark ignored. `cargo fmt -p wavecrate-ui` completed
successfully and `git diff --check` passed. The locked full-workspace suite and
signed macOS build passed. Live disposable-profile checks confirmed custom
theme, motion, autoplay, and loop restoration after restart with the source
theme JSON moved away; failed Reload preserved the saved appearance. Default
reset only the theme while preserving autoplay and loop choices. A malformed
payload remained untouched on startup, and Reset saved settings persisted valid
defaults and allowed a clean quit. Backend regressions cover write-failure
isolation, lookup fairness, load-failure shutdown, and rapid revision reversions.

### Audition output volume, 2026-09-08

The browser toolbar now exposes a compact normalized audition-volume slider using
the reference 112×16 logical control and 8px track. Pointer presses and captured
drags clamp at both ends; Left/Right and Up/Down change volume by 5%, Home/End
select an endpoint, and Shift changes use a 1% fine step. The slider keeps
search and browser keyboard focus separate and remains usable at the 960px
minimum window width. There is no mute action: the control changes Reson
audition output volume only.

Values are sanitized to finite 0..=1 input, quantized to 1/1000 before typed
preference patches and runtime commands, and submitted on each changed
quantized value so the session service owns its 250ms save coalescing and quit
flush. A changed volume updates a pending playback-init intent, while a
restored preference applies to an existing runtime without replaying playback.
Runtime queue failures retain the latest intent, show an explicit status, and
retry only within a bounded budget on a fresh runtime or later user change; a
rejected preference submission leaves the prior volume and audio state intact.

Validation: `cargo check -p wavecrate-ui` passed; `cargo test -p wavecrate-ui`
passed 58 tests with one manual waveform benchmark ignored; `cargo fmt -p
wavecrate-ui` completed successfully; and `git diff --check` passed. The
focused volume tests cover non-finite and endpoint clamping, pointer
quantization, reference keyboard steps, fine keyboard steps, and bounded
quantization. Session tests passed 30 cases, including immediate shutdown and
reopen with the latest volume. The signed macOS build passed. Live disposable
fixture checks confirmed pointer and outside-drag clamping, 5% arrow and 1%
Shift-arrow steps, volume adjustment without restarting playback, and a final
0.51 value persisted on quit and restored on reopen. Manual audible acceptance
remains separate.

### Audition audio output routing, 2026-09-08

The browser now exposes Backend, Output, and Sample rate controls in a compact
audio panel. Host changes clear the selected device and rate; device changes
clear the rate, matching the reference cascade. The panel lists discovered
choices asynchronously, keeps keyboard focus separate from the volume slider,
and supports ArrowUp/ArrowDown/Enter navigation with Escape closing the
dropdown before the panel. A pointer press outside the panel closes an open
dropdown without selecting a new output. The floating panel and bounded option
lists preserve the browser layout at the minimum window size.

Requested routing is stored through the typed `AudioOutputPreferences` patch.
The first audition waits for the preference baseline before opening output.
Output opens are single-flight and generation-fenced, so a rapid sequence of
hardware choices cleans up the stale completion before opening the latest
request. The background worker waits for the old output's teardown before
opening its replacement; a timeout retains that pending teardown for explicit
retry. The resolved Reson host, device, sample rate, actual buffer, and
fallback state are displayed independently from the requested values. Passive
preference snapshots take effect on the next audition; an explicit choice
reopens the runtime and resumes only the still-current playback intent. A
successful open precedes persistence, and rejected or unavailable preference
writes retain an explicit unsaved state with a Retry save action.

Validation: the locked UI check and focused browser tests passed, with the
manual waveform benchmark ignored. Formatting and the signed macOS development
build passed. Disposable-profile GUI checks exercised Core Audio discovery,
keyboard selection, the already-active Scarlett output at 44.1 kHz, playback
resumption after reopening that output, and Escape dismissing a dropdown while
playback continued. Quit flushed the explicit host/device/rate selection; reopen
restored it alongside volume and loop preferences, and the first audition used
that output. Tests did not change the hardware to another sample rate. Audible
quality, other output backends, and Windows hardware acceptance remain manual
platform checks.

### Playback navigation history, 2026-09-08

Cmd/Ctrl+Left and Right now navigate up to 50 accepted audition locations,
including the file and normalized playback span. New audition requests branch
history after going back; replay and automatic output resumption do not record
duplicates. Navigation restores cached registered sources, reveals the containing
folder and sample, clears a hiding filename filter, and uses the current volume
and loop preference. Forgotten or unavailable cached sources report an error.
Search, volume, and audio settings retain their own keyboard focus routing.

Cached activation cancels obsolete source scans and advances their completion
fence before restoring the projection. Waveform and audio initialization retain
their existing generation and playback-epoch fences. History is session-only;
persistent listen counts and last-played metadata remain separate parity work.

Validation: locked UI check, 66 browser tests (one manual benchmark ignored),
seven core history tests, diff checks, and signed macOS development build passed.
An isolated two-source GUI fixture confirmed Back/Forward, same-file range
restoration, cross-source navigation into a nested folder, search-focus isolation,
and clearing a hiding filter when navigating from browser focus. The test app
quit cleanly. Audible quality and Windows input acceptance remain unverified.

### Profile-owned catalog foundation, 2026-09-08

Added the GPUI-free `wavecrate-catalog` worker with bounded commands and completion
events. It clones an existing writable profile capability, retains exclusive
leases on profile-child catalog databases, and never opens a source-local
database. Snapshots require matching source/lifecycle/root identity and an exact
manifest, directory-truth, and audit-metadata revision. Catalog enumeration stops
before optional content verification can advance that revision.

The shared scanner now rejects incomplete directory evidence before issuing a
complete-audit receipt. Directory staging and cleanup use bounded transactions;
the final activation records the authoritative revision atomically. Metadata-only
revision changes are accepted only after exact manifest equality and a final
transaction fence. Catalog-owned maintenance reclaims interrupted staging and
inactive generations, revalidating ownership and cancellation between batches.

Validation: all 452 library and 203 scanner tests passed, followed by all 17
catalog tests after the final registration-cancellation change. Doc tests,
formatting, and diff checks passed. Regressions cover opaque root identity bytes,
incomplete traversal, interrupted audits, revision neutrality, source/profile
replacement, bounded admission, shutdown, orphan recovery, and repeated-audit
retention. The foundation is not yet composed into the browser; watcher
convergence and persistent browser projection remain integration work.

### Waveform zoom and pan navigation, 2026-09-08

Implemented pointer-anchored wheel zoom, horizontal wheel pan, middle-button
drag pan, and waveform-focused Z/X framing/reset. Z frames the play region or
zooms around the current anchor; X restores the full file. Shift zoom-out can
expose bounded silence margins. The pure core viewport uses an exact frame-count
scalar and a 256-frame minimum. Geometry caches include viewport coordinates;
out-of-file columns stay blank and off-screen markers are hidden. Regions and
history retain their full-file positions.

Validation: 29 core, 14 media, and 75 UI tests passed (one manual benchmark
ignored), followed by 12 targeted waveform tests after correcting native GPUI
wheel signs. Locked UI check, formatting, diff checks, and signed macOS build
passed. An isolated timeline fixture confirmed vertical wheel direction and
pointer anchoring, Z/X framing/reset, range creation within a zoomed view, and
browser-focused X/Z isolation. Horizontal/middle-drag live acceptance was not
established by the available GUI automation. Zoom currently uses the 2,048-column
overview; finer detail decoding and the reference scrollbar remain outstanding.

### Session catalog composition, 2026-09-08

The session now starts the catalog from its existing writable profile guard and
exposes nonblocking command admission, event polling, and explicit lifecycle
status. It reuses the catalog's bounded queues directly. Catalog startup failure
does not disable working registry or preference persistence. Closing session
admission is ordered against submissions; startup cannot republish Ready after
closure, and the session worker joins the catalog before releasing ownership.
Sources are not automatically registered or audited through this interface yet.

Validation: all 37 session tests, the locked root application check, formatting,
and diff checks passed. Independent lifecycle review found no concrete defect.
Tests cover shared profile ownership, preference independence, startup/close
races, closed admission, and ownership release. Browser projection remains the
next integration boundary.

### Bounded waveform detail decoder, 2026-09-08

The media layer now retains optional strong file identity, revision, WAV spec,
and normalization context with an overview. Detail requests decode an exact
frame range with at most 8,388,608 visible frames and 65,536 output columns,
without retaining PCM. A bounded preceding warmup reduces filter/envelope seams;
detail reuses full-file gains rather than normalizing each viewport separately.
Opened-file and named-path identity, change markers, length, timestamp, and spec
are checked before and after decoding. Platforms without strong markers retain
the overview without enabling detail.

Validation: all 18 media tests and locked media/UI checks passed. Tests cover
richer detail, overlapping ranges beyond the warmup window, stereo frame
coordinates, request limits, cancellation, atomic replacement, same-size edits
with restored modification time, and truncation. A final targeted test confirms
the local macOS fixture actually exposes detail context. UI request scheduling
and rendering are still pending; this decoder alone does not improve displayed
zoom resolution.

### Waveform detail display, 2026-09-08

Zoomed views now request bounded detail after a 45ms debounce when the overview
is too coarse for the measured canvas width. File generation, exact request,
and cancellation-token identity fence completions, including A/B/A navigation.
The renderer independently checks the live request and maps detail columns back
to their full-file range. Shared columns and a projection revision preserve
geometry caching; overview data remains visible while detail is pending or
unavailable. Layout changes request one follow-up frame, and settled detail
does not maintain an animation loop.

Validation: locked UI check and 20 targeted waveform/detail tests passed, with
one manual benchmark ignored. The combined UI suite passed 93 tests. Formatting,
diff checks, and the signed macOS build passed. A disposable timeline fixture
visibly resolved individual oscillations in a short zoomed range, retained detail
through X/Z restoration, resized correctly to 960x560, and quit cleanly. Five
seconds of settled display consumed 0.15 seconds of process CPU time on this
machine; this is an idle observation, not a full frame-latency benchmark.

### Catalog-owned WAV display metadata, 2026-09-08

Catalog snapshots can now include optional WAV frame counts and sample rates.
The scanner reads headers through a retained no-follow source capability, checks
manifest identity/size/timestamp and descriptor change markers, rechecks path
binding, and rejects changed roots. The catalog revalidates profile/physical
database ownership and revision after header reads. UI consumers need no file
opens to derive duration. Replaced links, non-regular files, malformed headers,
and changed entries retain membership without borrowing replacement metadata.

Reads have a 1 MiB per-parser limit and aggregate limits of 64 MiB and 4,096
entries per call, including buffered reads and both header passes. Cancellation
is checked between reads. Budget exhaustion returns the metadata collected so
far; enrichment of remaining entries is still follow-on work. Unix no-follow
opens use nonblocking mode and regular-file checks to reject FIFO replacements.

Validation: 14 metadata tests and 19 catalog tests passed, along with locked UI
check, formatting, and diff checks. The full scanner suite passed all 218 tests
serially in 2.48 seconds. Its parallel run passed 217 and failed the existing
global descriptor-count stress assertion; that test passed in isolation, with
no resource leak reproduced. Independent review identified the path-probe and
aggregate-budget issues addressed in this implementation. Windows execution
remains unverified.

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

### Browser catalog adapter, 2026-09-08

Known sources now activate the session-owned catalog with a fresh lifecycle
generation. The browser consumes bounded completion batches on its existing
source-service timer and projects manifests and directory evidence in a
cancellable background task. Projection performs no filesystem probes; optional
WAV display fields come from the catalog snapshot. Newly added roots retain the
initial read-only scan until their durable source identifier is acknowledged.

Source switches retire admitted registrations and reject stale completions by
source, lifecycle, and cancellation-token identity. Passive updates preserve
playback when the focused file facts match; explicit refresh reloads it. Failed
audits preserve the last good rows and show the source error, including in a
filtered folder view. Restored sources do not automatically scan or audition on
startup. Navigation history reactivates the catalog without restarting accepted
playback.

Validation so far: nine catalog adapter tests passed, and the integrated UI
suite passed 99 tests with one ignored benchmark. Independent review identified
a path-based metadata probe and an aggregate header-work budget gap; both were
fixed by the bounded catalog-owned metadata path documented above. Earlier
isolated-profile GUI checks covered refresh after file addition and rename,
nested WAV metadata, and empty directories. Final signed-bundle GUI checks confirmed 2-second/16-kHz and nested
3-second/22.05-kHz metadata. Moving the disposable source root offline and
refreshing retained the nested rows and displayed the source error; restoring
the root and refreshing recovered the waveform and cleared the error. The
isolated app quit cleanly. Filesystem watching and metadata enrichment beyond
the initial bounded pass remain follow-on work.

### Waveform viewport scrollbar, 2026-09-08

A compact themed scrollbar now appears beneath zoomed waveforms. The thumb
tracks the visible audio fraction, track clicks center the viewport, and thumb
drags pan while preserving the committed playback region. Full-file reset hides
the bar. Silence-margin views remain represented by a full-width thumb. The
optional theme field `waveform.scrollbar_height` defaults to six logical pixels
and is validated with the complete waveform-panel layout.

GPUI drag promotion and final release both apply their pointer coordinates,
including a release outside the bar. The scrollbar consumes only its own drag
release, and waveform presses must lie inside the waveform bounds. This prevents
scrollbar gestures from starting waveform selections or swallowing their release.

Validation: viewport tests (5), design tests (18), and scrollbar tests (5)
passed. The final waveform-focused suite passed 25 tests with one ignored
benchmark. A signed isolated-profile GUI run confirmed thumb and outside-release
panning, track clicks, unchanged play region, no audition, and full-view reset.
A subsequent review fixed capture-phase release propagation; targeted tests and
formatting passed after that fix. Integrated gesture GUI validation will also
cover waveform releases while this bar is visible.

### Playback-region move, resize, and slide, 2026-09-08

Playback regions now have the reference-style top move grip and endpoint resize
handles. Hit testing prefers the nearest resize edge in overlapping narrow
regions, maps through the current zoom, and ignores offscreen endpoints. Pure
`WaveformRegion` geometry in the core crate preserves width during movement and
whole-width keyboard slides, clamps to the audio domain, and normalizes crossing
resize endpoints. Provisional drags retain their baseline until movement exceeds
click slop; release applies its final position, and cancellation preserves the
committed region.

Move and resize change the next audition region without interrupting the current
transport. Unmodified Left/Right while the waveform owns focus slides the region
and pans to keep it visible. Sliding cancels unfinished pointer gestures before
updating the region. Existing click-to-audition and drag-create behavior remains.

Validation used a detached checkout at `b16d1d893` plus the four gesture files,
whose bytes matched the implementation checkout. Thirty waveform UI tests and
four core region tests passed, along with formatting, diff checks, signed build,
and signature verification. GUI checks on the disposable 12-second fixture
covered create, move, resize, endpoint crossing, Left/Right and framing, creation
with the scrollbar visible, outside-waveform release, and edits while playback
continued. Escape stopped playback; full-view reset preserved the committed
region and hid the bar. The app quit cleanly. Mid-drag Escape was covered by
state handling rather than a GUI injection test; audible acceptance remains
manual. Regions extending into silence margins, edit selections, and export
handles remain follow-on work.

### Bounded catalog metadata patch service, 2026-09-08

The catalog accepts exact-path WAV metadata requests for at most sixteen paths.
Each request carries source, lifecycle, revision, and caller request identity;
its response is a metadata patch with terminal requested paths, including
malformed or missing entries that have no metadata result. Failures are separate
from audit failures. Obsolete or unregistered lifecycle requests are discarded
by the existing publication fence.

The worker uses a bounded exact-path database query, checks it against the
retained snapshot, and reuses descriptor-checked scanner reads. Profile, root,
and revision checks surround the operation. The sixteen-path cap keeps the two
1-MiB header passes per file below the scanner's 64-MiB aggregate budget, avoiding
unattempted paths masquerading as terminal omissions. Snapshot manifests are
sorted once by Rust path ordering for bounded binary lookups; SQLite text order
is not assumed to match nested path ordering. Successful patches update only
the retained optional metadata, without source mutation or another audit.

Validation: 25 catalog tests and 37 session tests passed, along with workspace
check, formatting, and diff checks. Cases include exact and nested punctuation
paths, malformed/missing terminal omissions, invalid and oversized admission,
revision mismatch, cancellation, and removal fencing. Independent backend
review found no remaining meaningful issues in admission, authority checks,
cache patching, or event accounting. Visible-row scheduling and integrated
large-library GUI validation remain the next slice.

### Visible-row catalog metadata loading, 2026-09-08

The virtual file list now requests missing WAV metadata for visible rows after
a 45-ms delay, with one sixteen-path request in flight. Requests start only
after a successful catalog audit and projection. Source, lifecycle, root,
revision, projection identity, and request identifiers fence completion; source
switches and refreshes reset the lane. Empty filters clear demand, and omitted
malformed paths remain terminal until the next projection. Metadata errors are
separate from source audit errors.

An index built during background projection lets each response patch the
browser and retained source entries directly. Completion does not rebuild the
folder tree or list model, change selection, or request animation frames.
Polling ends when the bounded metadata lane is idle. Poll task retirement is
atomic with its stop decision, preventing deferred admission from losing its
poller.

Validation: 109 UI tests passed with one ignored, including six focused metadata
cases; the signed macOS build succeeded. A disposable 4,202-file source verified
metadata loading beyond the initial 4,096-file budget, further batches after
scrolling, empty filters, and a malformed WAV beside a valid three-second,
22,050-Hz WAV. A long preview-error layout issue found during this check was
fixed with shrinkable, clipped status and pane boundaries. A fresh signed build
verified that the malformed-file error preserves all table columns and that
selecting a valid file restores the normal preview status. The app quit cleanly.

### Bounded curation continuity transactions, 2026-09-08

The library now exposes a dedicated profile-guarded curation transaction for
ratings, keep locks, and fixed collection memberships. It accepts at most 128
allowlisted updates, checks every requested path is a live supported manifest
member, and requires the exact audited root and source revision. Invalid values
constructed through deserialization are rejected before mutation.

The transaction advances the generic source revision, active directory
publication revision, and retained authoritative-audit revision together. It
does not change directory entries, manifest membership, path/identity/index
revisions, or issue scanner audit authority. Validation uses the existing
complete active-generation header rather than rescanning directory entries for
each edit. A private receipt returns the exact committed revision and only the
requested browser metadata rows, read inside the write transaction. The internal
commit helper rejects batches carrying manifest or index changes.

Independent review found no correctness or authority issue and requested a
profile-child replacement regression test, which was added and passed. The full
library suite passed 459 tests before the lock/replacement additions; all ten
curation-filtered tests passed on the final transaction implementation. Catalog
admission, shutdown draining, and visible curation controls are separate
follow-on work.

### Rating rules and curation colors, 2026-09-08

The core crate now models the reference rating transitions without UI or
persistence dependencies: ratings span -3 through +3, direction changes skip
neutral, a fourth positive step locks +3, and locked states ignore adjustments.
A fourth negative step produces an explicit trash request for the filesystem
transaction layer; the helper itself performs no file operation.

Themes include semantic keep, trash, and neutral indicators plus six collection
swatches matching the reference defaults. These additive fields default when
loading older themes and preserve explicit overrides. Four focused rating tests
and all nineteen design tests passed. Visible controls and trash transaction
integration remain follow-on work.

### Durable catalog curation and browser display, 2026-09-08

Catalog curation admission now validates bounded updates and the active source
lifecycle. Every accepted write produces a terminal receipt or failure, including
when source removal, replacement, or shutdown follows admission. Writes require
the exact audited root and revision; they do not silently rebase. Successful
receipts patch retained metadata without rescanning the source. Snapshot loading
hydrates curation through bounded, revision-checked metadata pages.

Shutdown closes admission and drains accepted work. Independent review identified
that the session wrapper discarded the event receiver too early; it now retains
the closed worker's bounded event queue until consumers drain terminal receipts.
A session regression verifies immediate shutdown, receipt delivery, and the exact
persisted rating/revision after reopening. Catalog tests also cover a full command
queue and removal/replacement ordering. All 33 catalog and 38 session tests passed.

The browser displays ratings, keep locks, and all six collection swatches, with
rating/collection sorting and source-scoped collection filters. Counts follow the
folder and filename query while ignoring the active collection filter, and are
cached outside rendering. Metadata updates preserve curation. New column widths
have backward-compatible theme defaults, including older wide layouts.

Validation: 42 core, 20 design, 18 media, and 109 UI tests passed (one UI test
ignored). The signed macOS build succeeded. A disposable 4,202-file catalog
verified audit preservation of seeded ratings, multiple memberships, accurate
counts, collection/query intersections, passive sorting without autoplay, visible
sort arrows, and empty-result text. The app quit cleanly. This slice displays and
filters curation; editing shortcuts and filesystem trash remain follow-on work.

### Pure curation action planning, 2026-09-08

The core crate now plans rating adjustments, independent per-row collection
toggles, and unlocking a maximum keep rating. Plans preserve unrelated metadata
and timestamps; persistence assigns the actual curation time. Invalid collection
slots and no-op actions remain unchanged, while the negative threshold delegates
to the filesystem transaction layer. All 48 core tests and the locked core check
passed. Browser command admission and editing controls are the next integration.

### Durable compound curation operations, 2026-09-08

The catalog now admits a compound curation operation as one durable command.
Each operation contains at most 512 batches of 128 updates, with a 65,536-update
limit and bounded path/estimated payload accounting across queued commands and
undrained outcomes. Each batch reuses the profile-guarded, exact-revision library
transaction. Commands do not interleave between the operation's batches.

An operation starts at an exact revision or the exact latest fully successful
predecessor operation in the same source lifecycle. Predecessor state is retained
independently of UI event draining. A failed or partially committed predecessor
cannot authorize a successor. One terminal outcome retains every committed batch
receipt and the failing batch/error. Its final revision is the last receipt's
revision, or the initial fence when nothing committed; it is not a database
readback on failure.

Independent review found a remaining session receiver-lifetime edge case: a
rejected submit after catalog shutdown discarded the retained service. That path
now closes admission while preserving terminal event draining. The regression
covers accepted compound work, admitted shutdown, rejected follow-up, outcome,
Closed, and disconnection. Additional tests cover queued chains before event
draining, aggregate quota exhaustion/release/readmission, partial failure,
nonlatest predecessors, unrelated revisions, stale exact fences, and lifecycle
replacement. All 41 catalog and 39 session tests passed, along with locked checks,
formatting, and diff checks. Browser editing integration is validated separately.

### Browser curation editing, 2026-09-08

Plain bracket keys adjust selected ratings without advancing selection; keys 1–6
toggle each selected file's collection membership independently. Commands are
admitted immediately as durable catalog operations before optimistic metadata is
shown. Rapid edits name their exact accepted predecessor, and rating/lock pairs
stay together in a transaction when larger selections are split into batches.
Rejected admission leaves visible metadata unchanged.

Terminal outcomes are consumed before active-source filtering. Confirmed metadata
and targeted pending patches preserve newer edits when earlier receipts arrive;
failures retain committed prefixes, invalidate dependent overlays, and require a
fresh audit before further editing. Superseded lifecycle state is pruned after
pending outcomes drain. Lazy metadata has an explicit pause during writes and
resumes at the committed revision. Per-key planning and patching avoid full-map
copies, and unrelated metadata changes do not rebuild the sorted list. Quit drains
and logs failed operation outcomes before releasing the service.

Independent UI review found no meaningful issue. All 120 UI tests passed with one
ignored, including 11 curation-focused tests; locked checks, formatting, diff
checks, and the signed macOS build passed. Native checks on a separate disposable
4,202-file profile verified four rapid positive steps reaching a keep lock,
collection edits on locked rows, text-input shortcut isolation, two rapid toggles
across 1,000 selected files, immediate quit and reopen, and immediate source
switching. SQLite readback confirmed the expected ratings and membership counts.
Editing within an active collection filter updated membership, focus, and counts
without autoplay; lazy WAV metadata beyond the initial audit budget resumed after
curation writes. The first quit test exited cleanly; the reopened test app was left
available for user interaction.

Unlock is represented in the core planner but awaits its reference context-menu
control. Fourth-negative filesystem trash requests remain explicitly unavailable
until file-transaction integration; the UI does not silently treat them as saved
metadata edits. This does not complete full feature parity.

### Sample context menu and passive browser focus

The GPUI browser now provides an anchored, themed sample-row menu with Copy Path,
Reveal in the platform file manager, conditional Unlock, and Remove from the
active collection. Copy Path writes normalized plain text and quotes paths with
spaces. Reveal reuses the asynchronous file-handoff validation lane. Unlock uses
selected locked rows only when the context target belongs to that selection;
collection removal always targets the context file and is idempotent.

Passive browser focus is independent of the loaded preview. Opening and dismissing
a menu does not load another waveform or restart transport. Escape/W dismiss,
arrow keys navigate, and Enter activates; menu input suppresses browser shortcuts.
Captured source, lifecycle, root, revision, and projection identity fence actions.
Row rendering compares stable identities directly rather than scanning the file
list for every visible row.

Validation: nine core curation tests, six context-menu tests, twelve UI curation
tests, formatting, diff checks, and the locked application build passed. A
separately signed QA bundle rendered the menu on a disposable three-file source;
rating locks and collection edits were confirmed in its database. Further native
menu checks were inconclusive because the open QA window acquired edits outside
the scripted sequence; UI automation was paused and both open apps were left
untouched. Native interaction acceptance remains outstanding. Filesystem trash,
duplication, and other context-menu actions remain subsequent parity work.

### Rating filter projection

The sidebar now exposes independent T3, T2, T1, U, K1, K2, K3, and K4 filters.
K4 matches locked +3 only; K3 matches unlocked +3. Clearing every chip disables
the filter. Enabled filters exclude unavailable curation metadata and intersect
filename, folder, and collection scope. Runtime filter state survives source
changes without adding persisted preferences.

Collection counts honor rating scope while continuing to ignore the selected
collection slot. Curation patches refresh projection and counts when a rating edit
moves a row into or out of the active filter, including under Name sort. Context
menus capture rating-filter identity and invalidate after scope changes. The UI
reuses theme colors and hover/press styling without an animation timer per chip.

Validation: all 57 core tests, 13 UI curation tests, six context-menu tests,
formatting, and diff checks passed. Tests cover K3/K4, unavailable metadata,
combined scopes, selection restoration, and edited rows leaving and rejoining a
filter with correct collection counts. Native visual acceptance remains pending;
open user/QA applications were not replaced or operated for this slice.

### Playback timestamp metadata continuity prerequisite

The bounded source metadata transaction now accepts SetLastPlayedAt, including
explicit clearing. It uses the existing transaction batch setters and retains
profile/root/live-member/revision validation, directory/audit continuity, and exact
committed receipts. Playback-history updates preserve rating, lock, collections,
and curation timestamps. No schema or UI-trigger behavior changed.

Nine library curation tests and sixteen catalog curation tests passed; the updated
history-then-rating successor case passed again after its assertions were
strengthened. Catalog compilation verifies a static bound covering the command,
receipt row, and fixed collection storage within the existing row allowance.
Playback admission/debounce and shutdown integration remain separate work; this
backend API alone does not yet record audition history automatically.

### Last Played column and metadata-sort focus

The browser displays Last Played from catalog timestamps using Never, Today,
1 day, and elapsed-day labels. Ascending sort places recent listens first and
never-played rows last, with deterministic name/path ties. Formatting and sorting
use explicit clock samples with bounded negative/future/overflow handling;
unavailable curation metadata displays a dash rather than asserting Never.

The new theme width has a backward-compatible default. The column appears only
when the window can retain the existing sample-name width allowance, so previously
valid wide-sidebar themes remain valid. Duration remains available. Metadata
re-sorting preserves a passive context-row focus independently of the loaded
preview when that row remains visible; it does not switch playing files merely
because browser focus and preview differ.

Validation: 61 core tests, 20 design tests, 15 UI curation tests, six context-menu
tests, one responsive-layout test, the UI package check, formatting, and diff
checks passed. Native visual acceptance remains pending while open apps are left
untouched. Automatic playback timestamp recording follows as a separate slice.

### Automatic Last Played recording

Accepted explicit, selection-autoplay, and history-replay requests now record the
actual playback intent's source member. Passive preview loads, rejected audio
admission, and output-device resume do not record a listen. Core planning preserves
all unrelated curation fields and suppresses repeated identical timestamps.

History uses immediate bounded metadata-operation admission and the same exact
predecessor/receipt handling as rating edits. This intentionally avoids a UI-owned
debounce buffer: every admitted update participates in the existing source-switch
and quit drain, without unadmitted timer state that could be lost at shutdown.
Unavailable metadata or rejected metadata admission reports a metadata failure;
audio continues. No optimistic timestamp is shown before metadata admission.

Validation: all 62 core tests and 135 UI tests passed, with one UI test ignored;
UI checks, formatting, and diff checks passed. Coverage includes exact playback
path targeting, accepted origin selection, timestamp no-ops, and history/rating
pending overlays and committed receipts in both orders. Passive-focus re-sorting
preserves the loaded preview. Native audio/interaction acceptance remains pending.

### Bounded playback-type catalog projection

Browser metadata now carries a fixed-size optional Loop/OneShot classification
from canonical tags. Snapshot, paged, exact-path, and committed-operation reads
use the same ordering as the original tag model when recognized tags conflict.
Legacy catalogs without tag tables return no classification. Canonical separator
variants are recognized, while embedded-NUL labels are rejected.

The projection adds no tag vectors to bounded operation receipts and no schema
migration. Library browser/curation tests, catalog curation tests, UI projection
tests, locked catalog/UI checks, formatting, and diff checks passed. Additional
real-tag API coverage verifies plain oneshot and conflicting-label ordering.
TYPE controls and transport policy remain subsequent integration work.

### TYPE filters and catalog classification in the browser

The sidebar now provides independent 1-SHOT and LOOP filters above RATING. With
both cleared, all files remain eligible; enabled filters require recognized
catalog metadata. TYPE intersects folder, name, collection, and rating filters,
and collection counts honor TYPE while ignoring the selected collection slot.
The runtime filter survives source entry replacement without adding preferences.

Catalog classification maps into a pure core value. Lazy WAV display patches and
curation updates retain that value. Context menus fence their captured TYPE state.
Filter reconciliation uses the existing metadata-focus path, preserving a passive
focused row and the loaded preview when that focused row remains visible.

Validation: 70 core tests, 137 UI tests (one ignored), and 18 media tests passed;
formatting and diff checks passed. Native interaction acceptance remains pending.
The sortable column and automatic transport policy follow separately.

### Sortable Type column and responsive metadata widths

Type now appears between Rating and Col, displaying Loop, 1-Shot, or a dash.
Its explicit ascending rank matches Wavecrate: Loop, OneShot, then unknown;
descending reverses that order with the existing deterministic name/path ties.

The optional theme width defaults to the reference 76 pixels and accepts bounded
custom widths without invalidating older theme files. Header and rows share one
visibility decision that reserves sample-name space, prioritizes Type when it
fits, and adds Last Played when space permits. Narrow custom cells clip labels.

Validation: 71 core tests, two UI view tests, and 21 design tests passed, including
legacy theme compatibility, custom-width round trips, invalid dimensions, sort
direction, and responsive boundaries. Formatting and diff checks passed. Native
visual acceptance remains pending; open application bundles were not changed.

A clean-tree executable build of de963d4c9 also passed. A separate revision-named
macOS preview bundle was assembled and passed strict ad-hoc signature verification;
it was not launched for native acceptance, and existing bundles were preserved.

### History reveal under active filters

History navigation now temporarily includes its exact target row when rating,
TYPE, collection, or name filters would hide it. The exception remains confined
to the selected folder scope, uses normal sorting, and does not inflate collection
counts. It survives same-scope catalog replacement while the file still exists;
filter/navigation changes, a different focused row, or disappearance clear it.

Pointer and keyboard ranges are calculated before removing the temporary row.
Context menus include reveal identity in their validity checks, preserve focus
when targeting outside a multi-selection, and preserve the remaining visible
selection when a selected member receives focus. Arrow navigation also clears
passive context focus when reaching the already-loaded file, using the existing
same-file path without restarting playback. History still submits one replay.

Validation: all 73 core tests passed. Seven context-menu tests, six history tests,
one focused UI test, UI compilation, formatting, and diff checks passed. Review
corrected early reveal removal during range navigation, same-folder refresh loss,
selected-member context selection collapse, and passive-focus navigation. Native
interaction acceptance remains pending; application instances were untouched.

### Tagged next-play policy and output resume

A pure core policy now applies Loop and OneShot classifications when preparing
the loaded file and immediately before queue admission. Unknown files inherit
the session mode; a manual override survives same-file metadata refresh and
unrelated preference projections. Source changes clear file identity. Explicit
preference reset reapplies the current tag or saved fallback.

Playback admission validates the loaded path and rechecks selection autoplay.
Output reconfiguration preserves the accepted mode and span while using current
volume. Requests that have not started preserve their original origin across
repeated output changes, retaining autoplay cancellation and first-play history
semantics. Stop and current terminal events clear accepted intent. Live loop
switching is still a separate runtime prerequisite and UI integration.

Validation: six core policy tests and 18 UI transport tests passed, along with
locked UI compilation, formatting, and diff checks. Native audible acceptance
remains pending; existing application instances and bundles were preserved.

### Guarded active-span runtime retarget

Reson now offers an additive retarget API bound to the last successfully started
play request. Mismatched, inactive, or failed preflight state cannot invoke the
retarget executor. Non-seeking updates use finite dispatch-time progress; explicit
seeking and the existing unguarded API retain their offset semantics. Progress
reports runtime-owned play identity, retained through natural completion and
cleared by stop or failed replacement. Cancelling queued playback retains it.

The bounded command/event queues remain unchanged: acknowledgements are
best-effort, so the subsequent UI integration must reconcile using fresh polls.
This prerequisite alone does not change toolbar behavior or claim native audible
acceptance. Mode changes may replace the source within the existing output.

Validation: all 32 Reson runtime tests passed, including actual dispatch and
coalescing, completed/stale plays, consumed preflight errors, failed replacement
and stop, identity retention, and offset semantics. Locked UI compilation,
formatting, and diff checks passed. Application instances were untouched.

Independent review identified and corrected cross-play retarget suppression:
guarded commands now coalesce only for the same expected play identity, and
saturation cannot evict a different identity's retarget. Queue replacement
retains the newer command ID. A rejected guard does not advance the runtime's
retarget watermark; known stale IDs are discarded before consuming preflight
errors. Both queued arrival orders and delayed lower-ID current commands have
regression coverage. All 34 runtime tests passed after correction; independent
re-review found no remaining meaningful issues.

### Bounded audited tag-mutation backend

The existing catalog curation lane now accepts AddTag and RemoveTag updates.
Labels are validated before admission and transaction mutation: at most 1024
UTF-8 bytes, nonempty after whitespace normalization, and no embedded NUL.
Existing library tag APIs retain their prior normalization behavior.

Adding a recognized playback-type tag atomically removes other assigned type
identities, including synonyms, while preserving unrelated assignments and
locked curation metadata. Projection and mutation share the canonical SQL
classifier; the mutation queries only the addressed file's assigned tags.
Existing revision, profile, root, and directory-generation fences still govern
the whole transaction. Receipts remain fixed browser metadata records.

Label bytes count toward saturating payload estimates for both ordinary and
compound commands and remain reserved until their terminal event is consumed.
This backend prerequisite does not yet expose the tag editor or full tag lists
in the GPUI browser.

Validation: all 465 library tests and 43 catalog tests passed with no default
features, including normalized multi-file mutation, type replacement, legacy
NUL exclusion, invalid labels, and payload refusal/release. Backend formatting
and diff checks passed. Dependent UI verification follows its concurrent
transport-state integration; native interaction acceptance remains pending.

Independent review found no correctness, atomicity, or admission-accounting
defect. Two additional focused tests passed for duplicate/absent-tag timestamp
semantics and compound tag-operation payload credit release at terminal receipt.

### Live loop switching and progress recovery

The loop control now submits guarded updates to active playback, retaining its
accepted span and using runtime position without a new audition/history entry.
The toolbar distinguishes confirmed, pending, and next-play modes. Repeated
clicks alternate pending intent; after failure, a click acts on the confirmed
mode so retry does not require toggling an invisible preference first.

A bounded pending record is fenced by play identity, epoch, output generation,
and path. A later authoritative poll can confirm an update whose acknowledgement
was dropped. The existing 80 ms monitor expires lost polls after 400 ms; no new
render timer was added. Output reconfiguration preserves a matching admitted
mode and span, and ordinary progress reconciles actual mode after failed rapid
toggles. Unchanged loop progress retains the original accepted seek offset.

Review corrected stale error handling and identity-less fallback: another play's
error cannot clear the current play, while a current poll without an accepted
identity settles unconfirmed playback even if old executor audio remains active.
Independent re-review found no remaining findings.

Validation: the full UI suite passed with 154 tests and one ignored test. After
final review corrections, all 30 focused transport tests passed, covering rapid
opposing toggles, admission failure, stale responses, dropped acknowledgements,
resume snapshots, poll timeout, error fallback, and preserved spans/offsets. UI
compilation and formatting passed. Native audible/interaction acceptance remains
pending; running instances and existing application bundles were preserved.

A clean-tree executable build of 9c5542b1c passed. The separate macOS bundle
`target/qa/Wavecrate Parity Preview 9c5542b.app` passed strict ad-hoc signature
verification. It was not launched for native acceptance; existing bundles and
application instances were preserved.

### Bounded native tag input

The core crate now parses tag submissions using Wavecrate's native ASCII
normalization and newline/comma/semicolon separators. It preserves first-seen
order while deduplicating normalized labels. Input is bounded to 64 KiB,
individual normalized labels to 1024 bytes, and submissions to 64 distinct tags;
failure rejects the entire submission. Trailing punctuation does not reject an
otherwise valid label at the size boundary. All 86 core tests passed, including
seven parser behavior and boundary tests; core compilation and formatting passed.
The parser is a prerequisite for the tag editor, not a claim of completed tag UI.

The shared text field now converts IME selection ranges relative to the inserted
composition text before offsetting into the document. This corrects nonzero-prefix
and multibyte replacements, clamps selection to the inserted text, and clears
stale reversed-selection state after replacement. Two focused regression tests
passed with the UI test binary compiled; formatting and diff checks passed.

Core tag categories now preserve the five native category IDs, labels, order,
Playback Type lock, two default tags, and inference rules. Category inference
rejects inputs above 1024 bytes before allocation. Five focused category tests
and all 91 core tests passed, along with core compilation and formatting.

### Bounded source tag pages

SourceDatabase now exposes revision-fenced pages for selected-file tag unions and
literal vocabulary prefixes. Pages contain at most 64 entries; selected requests
allow 128 paths and 512 KiB of path bytes, with 1024-byte prefix bounds. Selected
assignment counts deduplicate paths. Opaque fixed-size cursors retain revision,
query context and the last tag ID, resolving the ordered key inside SQLite.

SQL bounds outgoing labels before Rust allocation and marks legacy long or NUL
labels as display-only previews, preserving their IDs for later exact mutations.
Vocabulary queries use indexed binary prefix ranges and cursor seeks; continuation
does not restart at the prefix beginning. The existing unbounded APIs retain their
behavior. Twelve focused library tag tests passed, including the real production
query's index-seek plan, paging, literal prefixes, revision fences and legacy
previews. Library compilation, formatting and diff checks passed. Catalog and UI
integration follow separately.

### Reusable bounded tag text field

The existing GPUI text field now supports a tag configuration with the native
ADD TAG placeholder and core input bound. Replacement size is checked before
building the new string; rejected edits retain existing text and composition.
Tag paste converts line breaks (including CRLF) to tag separators while filename
paste retains its existing behavior. A rejection event and composition-state
query support later editor status and Enter handling. Seven focused input tests
passed in the integrated UI test binary; the normal UI library check passed after
session/catalog plumbing was completed. Browser tag-editor wiring remains pending.

### Catalog-owned tag reads

The catalog worker now accepts bounded selected-path or vocabulary-prefix tag
requests and publishes ready/failed events with source lifecycle, request and
revision fences. Reads validate profile ownership, physical source-root identity
before/after, the retained revision and selected paths against the live manifest.
Missing selected paths fail explicitly; an empty selection returns an empty page.
Superseded ephemeral requests release their reserved event credit without
publishing. No tag vectors are added to retained file metadata or snapshots.

Session exports and UI exhaustive event/admission handling are integrated. The
full catalog suite passed 49 tests; five focused tag-page tests passed after the
final API cleanup. Normal UI library compilation passed. Independent review found
no actionable correctness, lifecycle, credit-accounting, fencing or bounded-query
issues. The tag editor will consume these events in the following UI work.

### Profile-owned tag vocabulary

The existing global library owner now persists known tag categories separately
from UI preferences. Its bounded APIs provide exact category lookup, literal
prefix pages and revision-fenced single-label updates. Identical updates preserve
the revision; changes commit the row and incremented revision atomically. Canonical
label validation and the native Playback Type category lock apply before admission
and at storage. SQL bounds stored text before allocation and rejects malformed
authoritative rows. Existing owner shutdown and profile-ownership handling cover
the new commands; no additional worker thread or whole-vocabulary snapshot is used.

The full library suite passed 478 tests. Independent review found a predicate cast
that prevented indexed exact lookup; it was removed, and a regression check now
uses the production query to verify primary-key search. Seven focused vocabulary
tests passed after that fix, including corrupt rows and pagination. Library
compilation, formatting and diff checks passed. Session/editor access follows.

### Browser tag mutation integration

Bounded add/remove tag planning now uses the existing durable curation operation
pipeline. Planning checks path, label and Cartesian update budgets before building
updates. Confirmed receipts carry playback-type changes separately from curation
metadata and refresh filtering/sorting without guessing optimistic tag state.

A passive type change that hides the loaded sample preserves its transport and
manual loop policy. Browser focus can detach from the loaded sample, including
across later metadata receipts and context-menu cleanup; explicit user navigation
retains its existing behavior. The full UI suite passed 173 tests with one ignored,
and normal UI compilation, formatting and diff checks passed. Independent review
identified the original passive-focus playback interruption; the fix was re-reviewed
with no remaining actionable findings. Tag editor controls follow separately.
