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

### Bounded tag identity resolution

Curation tag assignment now resolves only the numeric stored identity. It no longer
decodes an existing legacy display label that the receipt does not need. The legacy
API that returns full tags retains its behavior. Four focused curation-tag tests
passed, including assignment against a one-MiB non-text legacy display payload,
playback-type replacement, input bounds and curated timestamps.

### Bounded session vocabulary access

The session exposes typed nonblocking vocabulary request handles through a narrow
client of the existing profile library owner. Eight outstanding handles cap retained
results; consuming or dropping a handle returns its permit. Dropping an admitted
write does not cancel persistence. Owner and client use shared admission/lifecycle
helpers, and session shutdown closes admission before draining accepted work.
No relay worker, extra database connection or vocabulary preference snapshot is added.

All 42 session tests and two targeted owner tests passed, including reopen, stale
revision, invalid input, permit release, retained results after shutdown and dropped
write durability. Library/UI compilation, formatting and diff checks passed.
Independent review found no correctness, durability, shutdown-race or permit-leak
issues. Its startup coverage gap was closed with passing deterministic initialization
and unavailable-profile public-API tests.

### Large-selection tag union pages

Selected-tag pages now accept up to 65,536 paths and 16 MiB of encoded path data.
Each revision-fenced read stages its deduplicated selection in a transaction-local
temporary table, uses indexed assignment lookups, and returns bounded pages with
exact selected-file counts. Transaction rollback removes the temporary scope on
success and failure; staging checks cancellation in bounded chunks. The catalog
limits retained selection commands to an estimated 32 MiB including path containers,
releasing that credit after processing independently of result-event consumption.

The architecture was independently reviewed and the implementation diff reviewed
by the primary agent. Library tag tests, all 50 catalog tests, session tests and
compilation passed. A further positive 65,536-path boundary test passed, confirming
the last selected path participates without exceeding SQLite bind limits. Six
focused catalog tests passed after final cleanup; publication tests wait for
command-credit release rather than assuming it precedes event receipt. Formatting
and diff checks passed. The initialization test also releases its worker gate before
asserting, so assertion failure can shut down normally.

### Source-local tag identity mutations

Curation now supports assigning/removing existing numeric source tag IDs, with
positive-ID admission validation and typed missing-ID failures. SQL resolves
identity and playback classification without loading legacy label strings.
Assignment retains native playback-type replacement; removal affects only the
exact file assignment and leaves the source vocabulary intact. Existing source,
root and revision fences, batch atomicity and durable operation receipts apply.
Numeric IDs contribute to catalog payload accounting.

All 16 library curation tests and all 51 catalog tests passed, along with library,
catalog and session compilation, scoped formatting and diff checks. Tests cover
oversized/NUL legacy labels, playback conflicts, stale/invalid IDs, preserved
metadata and compound receipt accounting. The primary reviewed the implementation
diff. The UI must retain source/page identity when wiring chips to these operations;
shortened display labels are never mutation identities.

### Add-tag editor and category chooser

The sidebar now exposes bounded native tag entry, add submission and a compact
four-category chooser. Single unknown tags use revision-fenced profile category
persistence before source assignment; known tags, playback aliases and multi-tag
paste use the existing curation pipeline. Captured source/root/revision and target
paths prevent delayed work from applying to a replacement source. Input generation
checks preserve later edits and failed-admission drafts. Category choice supports
arrows, Enter and Escape; focused input isolates browser shortcuts and respects IME
composition. Long status text remains clipped to a compact row.

Existing source polling handles outstanding requests and stops while the chooser
is idle. A source change lets an admitted category write drain but cancels its file
assignment, and prevents stale-write refresh retries. UI feedback distinguishes
queued commands from durable completion; chip projections will follow receipts.

The full UI suite passed 177 tests with one ignored. Four focused editor tests,
normal UI compilation, formatting and diff checks passed after final cleanup. A
separate peer reviewed captured intent, retry, failure and keyboard/polling behavior
with no remaining actionable findings. Completion lists, selected-tag chips and
tag filters remain follow-on work.

The tag field uses the reference's transparent, underlined treatment and the
existing theme focus tokens; filename search retains its boxed appearance. UI
compilation, formatting and diff checks passed for this appearance adjustment.

A clean `0a111c48f` build was packaged and signature-verified at
`target/qa/Wavecrate Tag Preview 0a111c4.app`, then opened against a fresh three-file
fixture and isolated profile. The native input check was interrupted by detected
concurrent user interaction, so it is not counted as passed. The running preview
and its profile were preserved for manual testing.

### Selected-tag chips and exact removal

The sidebar now reads selected-tag unions in bounded 64-entry pages, labels each
assignment All or Mixed against the complete effective selection, and retains a
bounded 32-cursor history with a First control. Legacy label previews retain their
numeric source tag identity for removal. Rendering borrows the stored page instead
of cloning its label payloads each frame.

Selection changes coalesce behind outstanding reads. Source, root, revision and
request fences reject obsolete replies; source replacement drops obsolete request
correlation. Removal rechecks the displayed page and current selection, uses the
bounded exact-ID curation planner, and refreshes from durable receipts. Inactive
source receipts do not invalidate active chips. Correlated stale-revision failures
settle without repeated polling against unchanged authority.

The UI suite passed 183 tests with one ignored before the final idle-loop fix.
Five focused chip tests and normal UI compilation passed after that fix; formatting
and diff checks passed. Primary diff review found and resolved page cloning,
misleading empty-state text, inactive-source invalidation and stale-failure retry
issues. Native chip interaction remains unverified; existing running previews were
preserved. Completion and assigned-tag filtering remain in progress.

A clean detached checkout of `2e4b38c19` produced
`target/qa/Wavecrate Chip Preview 2e4b38c.app`. Compilation and strict ad-hoc signature
verification passed. This separate bundle was not launched by the agent; existing
preview sessions remain untouched.

### Assigned-tag filter database primitive

The library now streams matching live supported relative paths at one exact source
revision. Required tokens are bounded, trimmed, ASCII-lowercased and deduplicated;
matching requires every distinct token and preserves punctuation. The visitor
borrows each path and does not receive tag-label payloads or a materialized result
set. SQLite progress cancellation and scoped cleanup cover expensive query work,
visitor failures and normal completion.

A NOCASE index supports candidate lookup, while a binary lowercase residual keeps
embedded-NUL suffixes exact. Counting distinct required token ordinals handles
multiple legacy IDs differing only by case. The additive index is created through
normal writable schema repair; legacy read-only databases remain compatible but
may use a slower query plan until repaired.

All 493 library tests passed, including six focused filter tests, along with
library compilation, formatting and diff checks. Query-plan tests require indexed
searches on both normalized tags and tag assignments. Primary review corrected the
initial binary-only lookup, case-variant counting and embedded-NUL equality issues.
Catalog publication and browser filtering are separate follow-on integration.

### Core tag-filter projection

The dependency-free core now parses comma-only filter requirements with atomic
input bounds and retains validated immutable positional masks. Disabled, Pending
and Ready states distinguish an inactive filter from an enabled filter awaiting
results. Matching intersects existing browser predicates and collection counts;
exact history reveals still bypass filters within their folder boundary.

Entry replacement invalidates ready masks even when the entry count is unchanged.
Mask validation rejects wrong word counts and set tail bits, and application
rejects disabled or differently sized projections. The catalog/UI integration must
still bind each result to its source, revision and manifest order.

All 103 core tests passed, with core compilation, formatting and diff checks. The
primary reviewed the parser, mask validation and projection integration. This is a
backend prerequisite; the sidebar filter is not exposed yet.

### Catalog tag-filter publication

The catalog now admits revision-fenced tag-filter requests and publishes a compact
mask in retained-manifest order. Source/root/lifecycle checks bracket the database
read, and the requested manifest length must match the retained snapshot. Matching
paths are streamed into mask bits without copying path sets or tag labels.

Admission caps each mask at 4 MiB and reserves a separate aggregate 32 MiB result
budget. Result credits remain held until event consumption; input credits last
through command processing. Failed, discarded and shutdown requests release their
reservations. Session exports and compatibility event arms are included while the
sidebar integration proceeds separately.

Catalog and session suites passed before the final regression additions. After
hardening, all eight focused filter tests passed (51 other catalog tests filtered
out), with catalog compilation, formatting and diff checks. Coverage includes mask
positions/tail bits, typed bounds and stale revision, retained result credits,
supersession/removal, shutdown, manifest mismatch and root replacement. Primary
review corrected a test race between event receipt and command-input release and
bounded a shutdown wait. The explicit SQL-path-outside-manifest guard was reviewed
but does not yet have a catalog injection test.

A clean detached checkout of `8e3796cca` passed
`cargo check --locked -p wavecrate-gpui`, independently of the in-progress sidebar
changes in the main worktree. Its checkout remained clean after verification.

### Bounded source-watcher adapter

`wavecrate-source-watcher` now owns native recursive root and nonrecursive parent
watch handles. Callbacks perform lexical event classification and coalesce one
lifecycle-fenced hint, with separate descendant, root-ambiguity, overflow, and
backend-error evidence. They retain no event path queue and perform no database,
scan, or UI work. Ordinary database/WAL and AppleDouble events are excluded;
backend rescan flags override ordinary filtering. Closing disables acceptance
before native teardown, including callbacks already being classified.

Eight adapter tests passed, including an isolated native temporary-root smoke;
all-targets check and package formatting passed. Adapter Clippy reached existing
library warnings, so a clean Clippy result is not claimed. Catalog scheduling,
retry/rewatching, and root-generation recovery remain the next integration slice.

### Runtime assigned-tag filter and metadata-only mask reuse

The compact TAGS row now edits comma-separated exact AND requirements and toggles
without discarding the draft. Invalid input remains blocked across off/on; the
input rejects oversized edits without replacing the prior valid text. Active
queries coalesce subsequent edits and passive revisions behind one correlated
request. Source lifecycle, root identity, directory generation, revision, draft,
manifest length, and entry-index allocation identity fence accepted masks.

Unknown active scope stays pending; selection and file mutation/handoff actions
wait for authoritative matches. Explicit projection changes retain priority over
passive refreshes, while passive tag/metadata updates preserve loaded transport.
Confirmed non-tag curation receipts reuse masks after validating their prior
context and complete receipt chain. Synchronous metadata sorting reapplies the
positional mask before selection reconciliation, avoiding a blank/refetch cycle
for Last Played writes. Tags, failed/partial receipts, and changed provenance
invalidate instead.

Final UI sources were copied to the isolated acceptance checkout based on
`74dbf9a3a`, excluding concurrent watcher-catalog edits, and byte-compared with the
working UI sources. Its full UI library suite passed: 192 passed, 1 ignored.
Focused evidence includes eight filter and 32 curation tests, with production
transition/provenance helpers covering fast edits, invalid toggles, focus
priority, stale/partial reuse rejection, and metadata-sort mask identity. Package
formatting and diff whitespace checks passed. Native visual/input acceptance of
this slice remains separate from these automated checks.

The filter acceptance checkout was advanced to clean commit `092c97c74` and the
GPUI executable rebuilt there. `Wavecrate Filter Preview 092c97c.app` was packaged
under `target/qa` and passed strict ad-hoc signature verification. It was not
launched or used for native acceptance by the agent; existing previews remain
untouched.

### Native watcher convergence through the catalog

The catalog now owns watcher lifecycles and a separate scheduling module.
Callbacks coalesce evidence; the worker debounces for 400 ms with a 2 s maximum
burst delay and reserves the existing event budget before an automatic audit.
Queued commands and retained results take priority. Changes received during an
audit survive for a follow-up; failure backoff starts after audit completion.
Watcher outages retain continuity loss, use bounded restart retries and 30 s
fallback audits, and require an audit after watching resumes. Root identity is
probed periodically; an unchanged missing/replaced-root failure is reported once
until root state changes. A replaced physical root remains fenced until a fresh
source lifecycle is established; automatic new-lifecycle recovery is follow-on.

Registered catalog sources keep the UI's existing nonblocking event poll alive.
Unchanged polls do not notify GPUI or mark loading animations busy. Source removal
and shutdown close native handles before terminal publication. The final Closed
event and queue closure now occur under one lock; late sends are rejected. The
session shutdown test waits for that terminal event instead of assuming it is
immediately adjacent in time to a curation receipt.

Verification: 70 catalog tests and 43 session tests passed, with catalog/session
checks, owned formatting and diff checks. The isolated watcher acceptance checkout
based on `10f4e504b` also passed 192 UI tests with 1 existing ignored test. A
standalone native production-mode smoke harness created then removed temporary
WAV files and observed automatic authoritative snapshots at revisions 2, 4, and 6,
without explicit refresh commands after the initial audit. Its backend sources
were byte-compared with the committed candidate. This validates native event to
catalog convergence; visual/audible app acceptance remains separate.

`Wavecrate Watcher Preview f334852.app` was built from the clean watcher acceptance
checkout at `f33485214`, packaged under `target/qa`, and passed strict ad-hoc
signature verification. The agent did not launch it or modify existing previews.

### Bounded native ADD TAG completion

Single-label drafts now merge defaults, source vocabulary and profile vocabulary
into one six-row suggestion page. Independent 64-row windows retain their
unconsumed suffixes; a 32-step composite cursor history supports First, Previous
and Next without collecting the dictionary. Exact duplicates retain source tag
IDs. Legacy bounded previews remain selectable by persisted numeric ID, including
when the query is longer than the preview; they do not become mutation labels or
claim full lexical ordering beyond their visible preview.

Only explicit arrow/hover selection substitutes a suggestion on Enter. The first
suggestion supplies a display-only ghost suffix, hidden during composition,
selection or an interior cursor. Native hit testing clamps to real input content.
Editor/source cancellation and page restoration clear the ghost immediately.
Source context, revision, menu epoch and candidate identity fence activation;
source suggestions submit bounded ID assignments through existing curation.

Verification: the fresh-target full UI library suite passed 200 tests with one
existing ignored test after the preview-prefix and ghost-reset fixes. Ten focused
completion tests then passed after extracting production page/checkpoint helpers,
including real database cursors across a 64-row refill, profile carry, source-ID
deduplication and identical replay after restoring a composite checkpoint.
Formatting, UI check and diff checks passed. Native visual/input acceptance is
still separate from automated verification.

`Wavecrate Completion Preview bc501c1.app` was built from clean commit `bc501c166`
in its isolated acceptance checkout and passed strict ad-hoc signature checks.
The agent did not launch it or alter existing previews.

### Identity-checked source registration foundation

The catalog exposes an opaque verified-root value and a background-only resolver
that checks canonical target and physical identity. `RegisterVerified` rechecks
that proof before releasing an existing source database, after opening the
candidate, and before publication. Legacy registration also captures/rechecks its
observed target and identity, so a missing root is refused before its old handle
is dropped. Registration pins root identity even without a cached snapshot,
fencing a replacement before the first audit.

This is a backend foundation; automatic browser rebind remains the next slice.
Admission still advances the lifecycle on a refused registration, requiring a
fresh generation for retry. Exclusive profile leases mean failures after opening
the replacement do not have atomic rollback; no lease handoff is claimed.

Verification: 75 catalog tests and 43 session tests passed, alongside catalog,
session and workspace checks. Temporary-root tests cover normal verified audit,
missing-root handle retention, physical replacement, symlink target replacement
and initial-audit identity fencing. Modified-file formatting and diff checks
passed.

An isolated native production-mode harness against clean `b64b3ae08` moved a root
away, observed its stale-root failure, and restored the same physical folder.
The watcher produced a new authoritative audit in the same generation without an
explicit refresh. Replacing the folder instead remained fenced; resolving a new
proof, registering generation 2 and auditing produced only the replacement file.
The observed revisions were 2 → 3 → 6. This covers backend native convergence;
browser recovery integration is still in progress.

### Browser recovery for missing or replaced source roots

The browser retains worker registration state separately from command admission,
and retains the last known root identity across metadata resets. Root failures
cancel pending projections and retire stale tag work while preserving the last
rows and loaded transport. One correlated background probe retries quietly;
repeated Refresh coalesces it instead of starting additional filesystem calls.
A returning known identity uses the existing generation. A fresh matching watcher
audit can win a pending probe and retire its correlation immediately.

A changed root waits for active and retired curation receipts before starting a
verified generation and a fresh audit. Old snapshots cannot establish authority
during rebind. Changed canonical targets remain unavailable instead of silently
changing a durable source row. Source switches/removal/closure cancel recovery;
registration and audit failures retry through typed operation handling. Manual
Refresh renders its pending state immediately even if filesystem probing stalls.

Verification: the full UI library suite passed 210 tests with one existing
ignored test. Eight focused recovery tests, UI check and formatting passed again
after the final notification fix. Production transition tests cover coalesced
Refresh, immediate retry from waiting, stale correlation retirement, identity
classification and curation-drain decisions. Native backend return/replacement
convergence is recorded above; this is not a claim of native visual/input recovery
acceptance.

### Selected assignment counts for displayed tag identities

An additive selected-ID tag-page query returns bounded existing source tags with
exact all/mixed/zero assignment counts for the current path set. It accepts at
most 64 positive IDs, canonicalizes duplicate IDs/paths, includes zero matches,
and omits unknown IDs. Cursor identity includes the selected paths, tag IDs and
revision. Existing root/lifecycle/live-manifest fences and shared input credits
cover the new query; label previews remain bounded and never replace IDs.

Transactional TEMP tables and indexed joins avoid loading the whole vocabulary
or selection tag union. The count uses schema-enforced uniqueness rather than
retaining redundant DISTINCT sets. SQLite progress cancellation and rollback
release temporary scope before the next query.

Verification: 26 library tag tests, 77 catalog tests and 43 session tests passed.
After the final COUNT/validation-order refinement, five focused selected-ID and
query-plan/cancellation tests passed, along with the catalog check and diff check.
A deterministic SQL-progress test cancels before the grouped read emits its first
row, then successfully reuses the same database connection. The backend is ready
for the native Tag Editor library panel; that panel remains follow-on UI work.

`Wavecrate Recovery Preview ba50dab.app` was built from the clean detached
`ba50dabf1` acceptance checkout and passed strict ad-hoc signature verification.
The agent did not launch it; existing previews remain preserved.

### Exact WAV span peak preparation

Added a cancellable, bounded-memory media reader for raw interleaved WAV peaks over an exact half-open frame span. Requests retain the overview file revision and reject replacement, modification, or truncation before returning a result. This is normalization infrastructure; the audition toggle is not wired yet. Cancellation checks are bounded by 4,096 samples independent of channel count.

Validation: media suite passed 24 tests before the cancellation refinement; all seven focused span-peak tests and the media all-targets check passed after it. Fixtures cover PCM/float amplitude, channel maxima, silence/nonfinite values, span bounds, stale files, and cancellation with high channel counts.

Normalized audition now has a persisted, default-off preference and an optimistic session patch. Legacy documents omit the field safely, reset restores the disabled default, and the setting round-trips without changing output volume. All 16 focused session preference tests passed. The toolbar/playback integration remains in progress.

### Playback-scoped asynchronous gain updates

Reson now accepts a precomputed gain for a specific playback request ID. The worker silently rejects stale or post-stop updates without scanning audio, and queue admission/coalescing preserves different IDs and intervening controls. Same-scope tail replacement is constant-time; existing unscoped controls retain their behavior. Admission remains best-effort and does not acknowledge application.

Validation: all 186 Reson tests and its all-targets check passed. After simplifying the tail lookup, all four scoped execution/coalescing/order tests passed again.

Peak requests can now capture a WAV header and strong revision directly from a normalized span, without waiting for visual overview decoding. Fractional bounds become a clamped half-open frame range; the subsequent reader still rejects changed files. Neutral visual fields in this peak-only request are explicitly not a visual-detail normalization context. Validation: all 28 media tests and its all-targets check passed.

### Native Tag Editor panel

The ADD TAG arrow opens a compact Tag Editor in place of the leading sidebar. The panel merges source IDs, profile vocabulary, and built-in tags through the shared bounded pager used by completion: 64 source/profile rows per window, 64 panel candidates, and 32 previous-page checkpoints. Five collapsible categories describe the current page. Source tag counts distinguish All, Mixed, None, and pending; source actions retain exact database IDs, including legacy previews. Canonical profile/default labels use bounded label assignment. Category drag/drop changes the profile dictionary while keeping source assignments intact; playback-type aliases and noncanonical legacy labels remain locked.

Metadata requests use the existing source-registry poll lane, preserve in-flight work across same-lifecycle invalidation, and retain bounded admission retries. Reply/action fences cover source identity, catalog and profile revisions, page epoch, and captured selection. Rendering retains cheap drag payloads; full selection paths are captured only when a validated gesture starts. GPUI drag/drop payloads are checked against the originating browser and candidate.

Validation so far: an isolated snapshot passed the full UI suite (217 tests, one existing ignored test). After the final drag-lifecycle change, all six focused panel tests passed with normal lint settings. Independent review found a continuation-page recovery deadlock when the profile vocabulary changed revision. The panel now clears its old target and restarts through the normal refresh path; a production-helper regression covers that transition. Review found no additional evidence-backed issues. Native preview acceptance is pending.

The combined UI suite subsequently passed 231 tests with one existing ignored test, including the profile-revision restart regression and normalized-audition work in progress. The panel slice was committed separately as `5c9282f86`. A clean detached snapshot built `target/qa/Wavecrate Tag Library Preview 5c9282f.app` in 14.19 seconds; strict ad-hoc signature verification passed. The agent did not launch this preview or modify existing preview apps. Native interaction acceptance remains pending.

### Normalized audition integration

The compact, themed speaker control beside volume now toggles persisted normalized audition. New normalized Play requests prepare a raw interleaved peak in the background before admission. One running task and one newest pending intent bound work during navigation; a one-entry cache is revalidated against a fresh strong file revision and exact frame span. Manual volume remains independent and delayed playback uses its current value. Stop, source/output lifecycle changes, autoplay cancellation, and disposal cancel preparation.

Live gain updates use playback-ID-scoped commands and bounded retries on the existing transport monitor. Hidden selection does not invalidate retained accepted playback. Loop changes retain accepted spans, repeated pending toggles use the newest desired mode, and changed resolved spans require fresh preparation. Disabling during deferred Play resumes the current intent with unity gain; disabling during a prepared loop change preserves that loop change without seeking.

Validation: the full UI suite passed 231 tests with one existing ignored test before the last loop-disable correction. After that correction and compact icon rendering, all 13 normalized-audition tests, both focused transport regressions, the UI check, formatting, and diff checks passed. Independent review found no additional actionable lifecycle or persistence issue. Native audible acceptance remains pending. The peak reader fences its file revision, but the later runtime WAV path open is not atomically bound to that captured file object; this source-open race remains a known limitation.

A clean detached `35a451c63` snapshot built `target/qa/Wavecrate Normalized Preview 35a451c.app` in 17.26 seconds. Strict ad-hoc signature verification passed. This preview includes the accepted tag panel and normalized-audition integration; it was not launched by the agent, and existing previews were preserved.

### Display-name projection foundation

Core now exposes the native Disk/Label display mode and a pure name projection: Disk shows the filename stem, Label uses an already ordered metadata label, and unavailable/empty labels fall back to the disk stem. This changes no file identity or filename and is not yet wired to the browser. Both focused parity tests passed, including nested paths, Unicode, final-extension handling, preserved label order, and fallback. Bounded targeted label hydration follows separately.

### Profile vocabulary deletion primitive

The library owner and session vocabulary service now expose `delete_profile_tag_vocabulary(expected_revision, label)`. A single profile transaction validates the canonical label and locked playback aliases, checks the revision, deletes the exact dictionary entry, and returns its bounded label/category before-image. Missing entries at the expected revision are no-ops; overflow or malformed stored categories roll back. The receipt can be restored through the existing category-set API. This is dictionary-only infrastructure: source assignments remain intact, UI deletion/undo and native combined assignment deletion are not yet implemented.

Validation: 13 library vocabulary tests, six session vocabulary-service tests, targeted owner/drain tests, library/session checks, formatting and diff checks passed. Coverage includes stale/no-op semantics, before-image restoration/reopen, validation before admission, overflow rollback, and accepted deletes surviving dropped result receivers through shutdown.

### Bounded source display labels

The source database can now project metadata names for at most 64 requested paths at one exact revision. It preserves native tag ordering and first-playback-tag selection, returns disk-fallback markers for empty labels, and limits each label to 4096 UTF-8 bytes and 64 tag rows plus an overflow sentinel. Legacy malformed or oversized text is bounded before decoding and explicitly marked as truncated. Reads preserve assignments and the source revision.

Validation: all 28 tag-module tests and the library check passed, including independent malformed-text fixtures, path budgets, typed stale revisions, SQL cancellation/recovery, and query-plan inspection. SQLite seeks assignments by path and may use a temporary B-tree for exact per-path ordering; cancellation remains active during that work. Catalog delivery and the Disk/Label UI follow separately.

### Dictionary definition removal and scoped undo

The Tag Editor now offers removal for positively loaded canonical, unlocked profile definitions, with a tooltip identifying the dictionary-only action. Source assignments remain intact and confirmed source-only values are marked Local. The last successful removal retains one bounded before-image and an explicit Undo removal control. Restore requires the exact removal revision and never rebases over subsequent dictionary edits.

Dictionary requests survive panel/source/page changes. Category writes also retain their admitted request across resets; stale-context completion refreshes the current panel without grafting old row data. Removal and restoration invalidate active completion results. The undo control occupies its own row to fit the compact sidebar.

Validation: two dictionary-state tests, nine tag-panel tests, the completion refresh regression, UI test compilation, formatting and diff checks passed. Request retention across panel resets was code-reviewed; no new end-to-end GPUI lifecycle fixture was added. Backend session tests cover stale revision rejection. Native interaction acceptance remains pending, and this scoped undo does not implement global assignment deletion or application-wide undo.

A clean detached `cd1830f82` snapshot built `target/qa/Wavecrate Dictionary Preview cd1830f.app` in 19.74 seconds. Strict ad-hoc signature verification passed. The agent did not launch this preview or replace existing preview apps.

### Catalog display-label delivery

The catalog and session now expose targeted `RequestDisplayLabels` reads with typed admission failures and ready/failed events. Reads check the profile lease, source lifecycle, retained live manifest, physical root, directory generation, and exact database revision. Admission rejects raw or combined path-byte overflow before queueing. The shared 32 MiB tag-read result budget reserves the worst-case bounded label reply until its event is consumed or discarded; existing input and lifecycle credits remain separate.

Validation: all 84 catalog tests passed, including seven display-label tests, and catalog/session checks and formatting passed. The new UI loader is still being implemented; this delivery layer does not change the browser display by itself.

Independent review found no actionable lifecycle, cancellation, or result-credit issue. The supersession test was strengthened to reject any label event encountered before the replacement snapshot; that focused regression passed after the assertion change.

### Disk/Label browser mode

The Name header now toggles Disk and Label display. Disk shows the filename stem; Label uses the bounded ordered metadata projection, with filename fallback and an ellipsis for truncated labels. Sorting, selection, and playback continue to use the existing file identities. Visible rows defer demand to the catalog lane; no database or filesystem work runs during rendering.

The loader retains at most 256 visible-demand paths, with separate 1 MiB label/path budgets, and submits at most 64 paths within the combined raw/normalized byte limit. Offscreen cache and fallback keys are removed together. Same-lifecycle projection changes retain admitted requests until stale replies drain; source changes cancel the old lane. Tag-changing or uncertain curation clears projected labels. Confirmed non-tag receipts, including playback-history and no-op writes, preserve and rebase the cache. Pending writes in another source do not pause the current source. Disk mode stops queued work while draining admitted work. Context comparison uses the shared manifest-map identity rather than comparing the full manifest.

Validation: all 12 display-label UI tests, 36 curation-focused UI tests, the UI check, formatting and diff checks passed. Tests cover successive windows over 320 files with maximum-sized labels, byte-counter reclamation, long-path batching, stale replies, missing-label fallback, invalid payloads and mode cancellation. Native interaction acceptance remains pending.

Independent review found playback-history cache flicker and unrelated-source curation blocking. Both were corrected with exact non-tag receipt rebasing and lifecycle-scoped pending checks. Focused re-review approved the fixes with no introduced lifecycle or request-gating issue.

A clean detached `ac4344fc7` snapshot built `target/qa/Wavecrate Label Preview ac4344f.app` in 15.01 seconds. Strict ad-hoc signature verification passed. The agent did not launch this preview or replace existing preview apps.

### Native file-drag platform foundation

`wavecrate-platform` now provides bounded AppKit copy-drag admission for up to 256 prepared absolute UTF-8 paths and 1 MiB of path data. It borrows the actual window handle, checks the originating mouse event and current primary-button state, and retains native view/source/items/URLs/session objects until completion. Rejected resources are released outside the session-state borrow; completion notifications are nonblocking. File URLs explicitly represent files, and generic drag images require no source-file probing.

Validation: nine platform tests, strict all-target Clippy, macOS checking and checking against the already installed `x86_64-unknown-linux-gnu` target passed. Independent review identified the stale-event/late-start issue; the current-button guard fixed it. Browser integration and native OS drag acceptance are still pending, and other platforms currently return a typed unsupported error.

### Windows file-drag adapter

The platform crate now exposes a separate, explicitly blocking Windows copy-drag entry point. It accepts bounded exact UTF-16 paths, supplies CF_HDROP through independently owned HGLOBAL results, rejects optional foreign data writes, and balances OLE initialization. Cancellation and current-button checks guard admission; a thread-local guard prevents nested drag admission. It retains no HWND or GPUI state. The existing window-handle entry point still returns unsupported on Windows: browser integration must defer the blocking call onto the foreground executor after all GPUI borrows have ended.

Validation: 14 platform tests passed on macOS, including the production bounded UTF-16 encoder. Windows-target test compilation and strict Clippy passed. Independent static review found no additional COM ownership or reentrancy issues. Native Windows drag acceptance and browser activation remain pending. Cancellation is a request observed by OLE callbacks and can race an already completed drop.

### macOS whole-file browser drag-out

Sample rows now start a native copy drag after bounded background preparation. A selected anchor captures visible selected files; an unselected anchor captures itself. Requests reject the complete intent above 256 files or 1 MiB of paths, or when any file fails validation. Preparation retains one running task and one newest queued intent. It checks no-follow leaf metadata, canonical root containment, and physical root identity before returning a retained path list. The OS still opens paths later; this is not atomic file-object binding.

Capture-phase presses, pointer release, Escape, and source/selection/projection changes cancel unadmitted work. Gesture/request identities keep delayed callbacks from clearing newer drags. Native admission borrows the actual window, reuses the prepared paths across retries, and explicitly starts completion polling. AppKit owns retained native resources independently of the themed GPUI ghost. Windows browser activation remains gated on a separate deferred-call integration.

Validation: the broader UI suite passed 262 tests with one existing ignored test before the final review corrections. After correcting retry allocations and replacement-link acceptance, all 13 focused drag tests, the UI check, formatting and diff checks passed. Independent re-review confirmed both findings resolved with no new issue. Native macOS drag acceptance remains pending.

A clean detached `ed4f6fcc0` snapshot built `target/qa/Wavecrate Drag Preview ed4f6fc.app` in 43.63 seconds. Strict ad-hoc signature verification passed. The agent did not launch it or replace existing previews.

### Source and folder directory opening

Source and folder rows now expose a themed right-click Open action using the existing context-menu keyboard and dismissal behavior. Directory targets remain distinct from sample mutation targets. Inactive registered sources and nonselected folders can be opened without navigation or audition. Preparation checks directory type, canonical containment and root identity off-thread, retains one running task plus one newest queued request, and rejects stale targets before requesting the system file manager. Status reports the launch request rather than claiming OS completion.

Validation: eight directory-open tests, seven context-menu tests, the locked UI check, formatting and diff checks passed. Primary review corrected unbounded preflight replacement, an unnecessary tag refresh, and rejection of nonselected folders. No OS application was launched during validation; the later OS path open remains subject to path replacement.

### Raw WAV span export foundation

The media crate now exposes a revision-fenced half-open WAV span request and a streaming exporter into a caller-owned `Write + Seek` sink. It preserves channel count, sample rate, integer depth and float values without visual gain or processing. The exporter validates RIFF/data bounds before output, reconstructs its buffered reader after inspecting the retained file handle, checks cancellation through chunk/sample processing, and checks source identity and revision before and after reading. The sink must be fresh and must not alias the source; any failed or canceled output must be discarded by the caller. This layer performs no destination publication or replacement.

Validation: all 39 media tests and the media check passed, including exact spans, format preservation, nonzero seeks beyond the reader buffer, odd final 8-bit data, stale sources, malformed/oversized RIFFs and sink failures. Formatting and diff checks passed. Strict Clippy remains blocked by existing core/library warnings. Independent static review found no actionable issue. UI export and owned temporary-file handoff remain separate follow-on work.

### Whole-BPM resize snap calculation

Core now exposes the native selection-edge snapping calculation, preserving its `f32` arithmetic, drag direction, nearest whole BPM, audio-edge constraint and invalid-input behavior. All 113 core tests passed, including eight snap tests; the core check and targeted formatting passed. The toolbar toggle, beat-count editor and waveform gesture integration follow separately.

### Windows browser drag integration

Prepared whole-file drags now queue an owned foreground task on Windows. A final live-window/gesture/projection check runs inside GPUI, then the blocking OLE call runs after that borrow ends. Queued and admitted phases distinguish cancellation before launch from normal button-release drop handling. A separate ghost marker survives windowless cancellation and permits only matching cleanup. Completion uses the task callback, with no Windows drag polling loop. Paths remain owned once across native admission, and teardown requests cancellation without introducing a stale HWND.

Validation: the macOS UI check and all 17 focused drag tests passed; platform Windows-target checks, formatting and diff checks passed. Local full Windows UI checking is blocked by `ring` requiring the MSVC C toolchain. Independent review identified queued ghost cleanup after source invalidation; the marker correction passed focused re-review with no remaining issue. The new manually dispatched/PR Windows workflow supplies ASIO setup and actual Windows core/media/platform/browser tests plus the application check. Its first execution follows this commit; native OS interaction acceptance remains pending.

### BPM snap toolbar and numeric input

The toolbar now exposes the transient native-default BPM snap toggle and a 2–32 beat-count editor, initially four beats. Resize previews and committed regions use the core calculation; moves and new selections retain their existing behavior. The numeric field supports bounded ASCII-digit paste, UTF-16 composition selection, click-select-all, arrow/wheel stepping and Enter/blur clamping. Focused editing does not trigger browser transport shortcuts. Rendering adds no file reads, waveform-cache invalidation or continuous animation.

Validation: the integrated UI suite passed 285 tests with one existing ignored test; focused waveform and numeric-input tests, the UI check, formatting and diff checks passed. Primary acceptance corrected an unbounded numeric-filter input scan and duplicate focus ownership before integration. Native visual and interaction acceptance remains pending.

A clean detached `e5fb7b212` snapshot built `target/qa/Wavecrate BPM Preview e5fb7b2.app` in 19.63 seconds. Strict ad-hoc signature verification passed. The agent did not launch it or replace existing previews.

### Selection beat guides

A separate transient toolbar toggle now paints the internal beat divisions of the current play region using the shared beat count. The bounded projection follows live selection changes and clips through the waveform viewport, without invalidating waveform caches. It remains independent from BPM snapping. Separate edit-selection guides await the edit-selection workflow.

Validation: 35 focused waveform tests passed with one existing ignored test, and the UI check, formatting and diff checks passed. Primary inspection corrected guide-height clamping for subpixel-height canvases. Native visual acceptance remains pending.

### Windows media read consistency

The first Windows workflow run (`34286241825`) exposed three stale-source test failures. Windows media reads now retain a descriptor that shares reading/deletion but excludes data writers for the complete decode/export operation. Existing writers reject admission; concurrent writes cannot pass unnoticed while Windows defers file-time publication. Identity/revision checks still detect replacement. The shared library change-marker format remains unchanged. Tests close completed mutations before inspecting their revision and exercise Windows writer exclusion and lease release.

Validation: all 39 media tests passed on macOS and the media check passed. The Windows share-mode API compiled in isolation; full local cross-checking remains blocked by the SQLite C toolchain. Primary acceptance added the direct Windows dependency and corrected a wrapped-error assertion. Independent review found no production/lifetime issue and requested in-operation replacement coverage; the added Windows sink fixture verifies replacement succeeds under delete sharing and export rejects the stale source. A fresh native Windows CI run follows.

### Shortcut help

The themed shortcut dialog is available from the ? toolbar control and Cmd/Ctrl-/. It lists existing GPUI commands by focused area, scrolls with the keyboard or wheel, and restores prior focus on close. Opening it cancels unfinished UI gestures without stopping accepted playback. The modal blocks background commands and adapts to available window bounds.

Validation: the UI suite passed 290 tests with one existing ignored test before final review adjustments. The three focused routing tests and UI check passed after adding gesture cancellation, modifier-specific toggle routing and keyboard scrolling. Primary inspection made sections retain their natural height within the scroll area. Native focus/pointer acceptance follows separately.

The clean `355db7cef` help preview built in 19.33 seconds and passed strict ad-hoc signature verification. Native validation used a new isolated profile and silent test WAV: toolbar/Command-/ opening, End scrolling, Space suppression, Escape dismissal and restoration of search editing all worked. The test app exited cleanly. A clipped description was shortened and toolbar tooltips are now suppressed while help is open; the isolated UI check passed after these visual corrections.

The corrected clean `06867f2f6` preview built in 21.23 seconds and passed strict signature verification. Native checks confirmed the shortened help row fits, toolbar tooltips remain suppressed over the modal, the close button works, and changing the beat count from four to five changes the selection from three to four internal guide lines. The isolated app exited cleanly; screenshots are retained in `target/qa/help-native-355db7c/`.

### Windows content revisions

The second Windows run (`34289070619`) passed 41 media tests but exposed the remaining same-size/restored-write-time case: Windows `ChangeTime` covers metadata, not data-stream writes. Media revisions now incorporate a raw BLAKE3 fingerprint on Windows. Admission validates the WAV header, hashes once in cancellable 64 KiB blocks under the retained write-excluding read lease, rewinds and reconstructs the decoder. Rechecks accept the leased `OpenWav` and reuse the fingerprint only after a fresh open proves matching identity. Scanner markers and the public revision shape remain unchanged.

Validation: 40 media tests passed on macOS, including raw fingerprint, rewind and same-length mutation coverage. The portable fingerprint check also passed against the full expected digest. Actual Windows CI remains required; local cross-checking cannot complete without the MSVC C toolchain.

Independent review approved fingerprint ownership, reuse and reader offsets. Windows refinements perform one full raw source read per distinct request; the work remains off-thread and cancellable. This is the cost of detecting metadata-preserving writes with this design. [Microsoft documents the ChangeTime distinction](https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_basic_info).

### Zero-crossing snap media foundation

Media now exposes precise normalized/frame requests for the native five-millisecond zero-crossing search. It streams channel means using the native playback conversion, preserves candidate distance/level/boundary ordering, and fences source identity and revision before and after decoding. Invalid edges and reversed results preserve native no-op behavior. No sample vector or file mutation is introduced. On Windows, the shared revision fence additionally hashes the full raw file once at admission under the read lease.

Validation: temporary module registration passed 51 media tests and doc tests; final registration passed the UI dependency check and formatting. UI gesture integration follows separately.

### Edit-selection geometry and theme foundation

Core now has a separate `WaveformEditRegion` that preserves finite bounds outside the audio interval for silence-margin gestures. Movement and crossing resizes derive from the unchanged baseline; nonfinite, overflowing and collapsed results are rejected. Conversion to a playback region explicitly intersects the audio interval. Existing playback-region behavior is unchanged.

The defaulted `waveform_edit_selection` theme color matches the native neutral overlay and is included in the example theme. Existing theme files remain compatible. Five focused core tests, the core check, 21 design tests, the updated example check and scoped formatting passed. Browser edit-selection interaction follows separately.

### Windows application build follow-up

Windows run `34291072626` passed the core, media and native-platform tests, including the restored-write-time revision regression. Application checking then rejected an unused non-Windows drag retry helper under `deny(warnings)`. The helper is now compiled only outside Windows or for its portable unit test. Browser tests were not reached in that run; a fresh Windows run is required for application acceptance.

### Zero-crossing waveform interaction

A compact transient toggle now enables native five-millisecond snapping for play-region creation, movement and resizing, including keyboard slides. It defaults off as in the native waveform. Whole-BPM snapping takes precedence for resize only. Raw pointer bounds render immediately; exact matching results refine the preview or released range. Missing context and failed reads retain the raw region.

One background task and one newest queued intent bound work. Canceled tasks retain ownership until completion. New gestures, playback, keyboard navigation, source changes, help and toggle-off fence pending refinement; a new gesture uses the visible committed range. The status line reports preparation or unavailable snapping without a new polling timer or waveform-cache invalidation.

Validation: the integrated UI suite passed 295 tests with one existing ignored test before the final admission correction. Independent lifecycle review found disabled keyboard nudges could retain a phantom preparing status. The corrected production admission path rejects disabled ownership; five focused state tests, UI checking and formatting passed, including a real core nudge followed by disabled admission. Primary acceptance also corrected canceled-intent reuse, missing-context preview fallback and short-circuit cleanup. Native interaction verification follows.

The clean `8b8378b58` preview built and passed strict ad-hoc signature verification. An isolated two-second WAV/profile confirmed default-disabled keyboard movement without a phantom status, enabled creation/resizing, and help dismissal restoring waveform keys. Clicking the snap toolbar control exposed a focus loss; its handler now restores browser focus. The preview quit cleanly without touching the live library. Exact sample-boundary selection remains covered by media tests rather than screenshot measurements; audible acceptance was not attempted.

Final native retest on clean `269fa441a` confirmed the toolbar must focus the waveform handle specifically: enabling then Left moved the region immediately, and disabling then Right moved it back without pending status. The preview built in 20.24 seconds, passed strict signature verification, and quit cleanly. Evidence is saved under `target/qa/zero-snap-native/snap-269fa44.png`; the isolated profile and every prior preview remain preserved.

### macOS extraction primitive investigation

A disposable C harness at `target/qa/fclonefileat_qualification.c` exercised `fclonefileat` on macOS 26.6.2/APFS with the installed SDK's `CLONE_NOFOLLOW_ANY | CLONE_RESOLVE_BENEATH` flags (`0x18`). Six checks passed: exact content/new inode, regular-leaf collision, symlink-leaf rejection, retained source FD after pathname replacement, retained destination directory FD after pathname replacement, and unsupported flags leaving no destination. The log is `target/qa/fclonefileat-qualification-20260909.log`; fixtures were removed.

This is primitive evidence only. Concurrent content mutation, global containment, cross-device failure, journal checkpoints, synchronization and crash recovery remain unqualified. The clone creates a distinct final inode, so the existing single staged/target identity journal cannot be reused unchanged. No production finalizer was enabled and non-Windows recovery remains fail-closed. Apple describes the API in its [clonefile manual](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2).

### Edit-effects sample transformation foundation

Core now exposes finite validated gain/fade settings. Media applies the native gain, inner fades and outer extensions to caller-owned interleaved samples, preserving f32 processing order, the two-millisecond minimum mute fade, complete-frame bounds and unclipped output. It performs no file I/O or publication and synthesizes no silence outside existing audio. Cancellation is checked before mutation and at most every 4,096 changed samples; callers must discard partially transformed buffers after cancellation.

Validation: 121 core tests and 61 media tests passed, including stereo, linear/S curves, outer extensions, frame clipping, single-frame fades, invalid settings and bounded partial cancellation. Scoped checks, formatting and diff validation passed. Primary inspection compared the transform with the active native Enter-effects implementation. UI fade controls, WAV encoding, durable replacement and undo remain separate work.

Core settings also now implement the native asymmetric gain pointer mapping and outer-gain vertical mapping as immutable baseline operations. They preserve other fade settings, clamp finite extremes and retain the native nonfinite-input fallback. All 124 core tests, the core check and scoped formatting passed. These helpers prepare the next edit-control UI slice.

### Windows browser-test follow-up

Run `34292837280` passed core/media/platform tests and the application check, then ran the browser suite: 284 passed, 11 failed and one was ignored. Five waveform-detail failures came from the fixture retaining its temporary writer while media correctly requested a write-excluding read lease. The fixture now retains only `TempPath` cleanup ownership, closing the writer before preview loading; all eight focused detail tests passed locally. The remaining root-identity failures are under investigation. This run did not time out.

The root failures were traced to Windows directory identity opening without `FILE_FLAG_BACKUP_SEMANTICS`. The library identity helper now supplies that required directory flag while preserving its reparse-point handling and attribute-only access. A directory identity regression, catalog verified-root check and directory-open tests passed locally. Full Windows cross-checking remains blocked by the absent MSVC toolchain; actual Windows CI is still required. [Microsoft's CreateFile documentation](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew) specifies the directory flag.

The Windows workflow now compiles the thin application and browser tests together with `cargo test --no-run`, then runs the browser library tests. This avoids the prior separate metadata-only GPUI check before full test compilation. The combined compilation command passed locally; its Windows execution remains to be confirmed.

### Independent waveform edit selections

The waveform now retains separate play and edit ranges with one tagged active gesture. Secondary dragging creates, moves or resizes the edit range; an unmoved secondary click clears it. Primary handle priority follows the native edit/play ordering. Edit coordinates preserve visible silence margins, and edit gestures do not change accepted audition or the play range. Neutral themed edit overlays use bounded guides on both idle ranges and only the active range during a gesture.

Right-button movement uses root mouse events because GPUI's built-in drag helper admits only the left button. New gestures cancel previous pointer ownership; missing/mismatched buttons and late release cannot commit canceled bounds. Zero-crossing requests share the existing bounded lane and carry selection kind, preventing a result from crossing between play/edit selections. BPM resize precedence remains intact.

Validation: the full UI suite passed 304 tests with one existing ignored test; targeted waveform/snap tests, UI checking, combined application/test compilation, formatting and diff checks passed. Actual GPUI pointer tests cover right-button create/release, outside release, click-clear, button loss and canceled late completion while asserting unchanged play range, accepted playback intent and epoch. Test setup starts no audio device, source registry or media worker. Independent review approved implementation and coverage with no remaining findings. GPUI test support is dev-only; its upstream transitive graph adds packages without changing existing package versions. Native secondary-drag acceptance remains manual because the available UI automation cannot hold a right-button drag. Fade/gain controls and destructive application remain separate work.

A clean detached `fbe4ccf41` snapshot built `target/qa/Wavecrate Edit Preview fbe4ccf.app` in 21.07 seconds and passed strict ad-hoc signature verification. Native checks against the isolated two-second sine fixture confirmed primary drag selection, arrow-key sliding, secondary-click isolation, and immediate waveform keyboard focus after toggling zero snap. Audio stayed idle and Last Played remained Never. The preview quit cleanly; `target/qa/zero-snap-native/edit-fbe4ccf.png` records the final state. This check does not claim native right-button drag acceptance.

The same clean `fbe4ccf41` preview also passed native tag-library and display-label checks in the disposable profile: the `TAGS` control opened the panel; creating `qa-sine` with a Sound Type category assigned it to the selected fixture; dragging it to Character retained the assignment; dictionary removal retained that assignment and exposed Undo; Undo restored the definition and category. Assignment and restored category survived a clean quit/reopen. Label mode showed `qa-sine`, then fell back to `crossings-100hz` after assignment removal. Screenshots are `target/qa/zero-snap-native/tags-fbe4ccf.png` and `labels-fbe4ccf.png`. The QA app quit cleanly and no live profile or audio playback was used.

### Edit fade preservation during resize

`WaveformEditSettings::for_resized_region` preserves absolute inner fade lengths, gain, curves and outer gains while applying native fade-in-first collision precedence and six-decimal length rounding. Outer endpoints are retained within the audio limits, including when the edit region extends beyond the file; empty fades disappear and valid outer-only fades remain. Nonrepresentable intermediate values fall back to zero only for the affected field. Moving a whole region retains its relative settings directly. All 12 focused edit-effects core tests, the core check, scoped formatting and diff checks passed. Primary review corrected an initially missing native audio-bound mute clamp before acceptance. UI fade controls remain separate work.

### Edit gain controls and preview

Edit selections now expose the native 12 by 10 pixel gain handle with an 18 by 18 hit target centered on the visible selection. Primary and secondary vertical drags derive provisional settings from one captured baseline, show the pending multiplier, and commit only gain on release. Pointer loss cancels the provisional value. Gain preserves play selection, accepted playback, and existing fade parameters; new edit selections and source invalidation restore unity. Gain gestures cancel obsolete snap work and never request zero-crossing refinement.

The preview scales raw signal lanes before nonlinear waveform shaping. Static and selected-span geometry have separate cache keys, shared immutable path storage, and exact clipping that works with translucent backgrounds. Narrow selections retain geometry and zoomed peak-only previews use the visible column count. Cache retrieval shares allocations; GPUI still requires owned paths and scaled vertices when painting. A local CPU geometry benchmark at 1120 pixels averaged 0.54 ms per gain rebuild for 2048 input columns and 2.05 ms for 65536; 100 cache hits took roughly 4–5 microseconds. These timings exclude scene painting and GPU rendering.

The final UI suite passed 313 tests with one existing ignored benchmark; scoped formatting and diff checks passed. Verification covers both-button GPUI pointer routing, release, button-loss cancellation and playback isolation, native handle geometry and gain mapping, zero/boost/narrow previews, cache sharing and invalidation, and zoomed peak fallback. Independent review found no additional gesture/correctness issue and identified cache-hit path copying. Primary inspection of GPUI's owned paint API led to removing avoidable retrieval copies with shared geometry; pointer-identity regressions verify the correction. The review agent could not complete a follow-up due to its usage limit. Destructive file application and fade controls are still separate work.

The Windows run [34296691111](https://github.com/PORTALSURFER/wavecrate-gpui/actions/runs/34296691111) passed at `fbe4ccf41`, including core/media/platform tests, application/browser test compilation, and browser tests. This confirms the directory-handle and closed-fixture-writer corrections on Windows; it predates the gain UI and fade-resize helper.

### Fade gesture and visual-envelope core

Core now exposes all six fade-handle gestures and native visual-envelope evaluation. Inner endpoint drags restore the opposite baseline fade when a collision recedes; selection-edge drags retain the captured inner endpoint and the opposite fade's absolute length; outer handles retain geometry and respect audio limits. Region-edge results clamp to audio, invalid collapsed ranges are rejected, and nonfinite input leaves the current state unchanged. Gain and original outer-gain values remain intact even when a handle rebuilds a region or temporarily removes a collided fade, avoiding the reference implementation's incidental resets. Native six-place length rounding and near-edge outer-endpoint snapping are retained.

Visual gain uses the native shader's inner/outer precedence and S-curve, including independent outer gain and no synthetic two-millisecond visual fade. Extreme unrepresentable outer spans do not produce nonfinite output. All 139 core tests passed, including six new gesture tests exercising both directions and all handle kinds and three visual-envelope tests; core checking, formatting and diff checks passed. These are renderer-neutral foundations; GPUI fade controls follow separately.

A clean `5d7cd4bc5` snapshot built `target/qa/Wavecrate Gain Preview 5d7cd4b.app` in 20.64 seconds and passed strict ad-hoc signature verification. This bundle was not launched for native gain-drag acceptance.

### Fade-shaped waveform preview, 2026-09-09

The retained edit preview now evaluates the core visual envelope across inner fades and outer extensions, applying it to raw band values before nonlinear waveform shaping. Peak-only fallback uses the same absolute-coordinate envelope after zooming. Preview geometry is sampled on the affected interval itself, so a nonzero selection narrower than one display column retains signal at both endpoints. The cache keys include the original selection and all effect settings, even when their clipped display bounds are unchanged; cache hits share retained path storage.

The GPUI overlay now exposes all six horizontal fade handles plus the two vertical outer-gain controls. Both mouse buttons use the shared edit gesture owner. Primary double-click on a standard outer handle clears its extension; vertical outer-gain handles retain priority where targets overlap. Hover changes and exit invalidate only when the highlighted handle changes. Moving retains relative fade settings; resizing retains the original effects through BPM and delayed zero-crossing adjustment so final bounds determine absolute fade duration.

Validation: the full UI suite passed 325 tests with one existing ignored benchmark; three additional GPUI root-pointer tests passed separately for both-button fade changes, button-loss cancellation and double-click isolation. Tests retain an inert waveform and start no audio device or source registry. Independent production review found no meaningful issue. CPU signal-geometry measurements at 1120px averaged approximately 0.83ms per changed fade for 2048 inputs and 3.84ms for 65536 inputs; 100 retained-cache lookups took about 10–11 microseconds. These measurements exclude painting/GPU work. Native secondary-drag acceptance remains manual; applying effects to WAV files and undo remain separate work.

### Metronome source-frame foundation, 2026-09-09

Reson now offers `PlaybackMetronomeConfig::with_source_cycle` to anchor a click grid to decoded source frames. Both static and live plans resolve the phase from their actual clamped source position, preserving the grid when the runtime advances a non-seeking retarget beyond the UI request position. Existing relative-cycle behavior remains unchanged. All 194 Reson library tests and checking passed, including before-origin/extreme arithmetic, seek, wrap, static/live agreement and compatibility. Independent review found no meaningful issue. The GPUI metronome toggle and transport acknowledgement are follow-on work.

### Metronome transport readback, 2026-09-09

Runtime progress now carries the configured metronome beat count together with playback request identity. The accessor reads the coherently published span configuration, avoiding a lagging audio-thread-applied value during retarget acknowledgement, and returns no count for inactive/disabled/unavailable transport. All 196 Reson library tests and checking passed; independent review found no meaningful issue. This is the acknowledgement foundation for the GPUI metronome control, which remains in progress.

### Last-window shutdown correction, 2026-09-09

Closing the macOS red window button previously removed the window while leaving the application process, Dock running indicator and menu bar active. This was reproduced with the isolated `61ad8563f` preview; explicit Quit then exited normally. The launcher now retains a GPUI window-closed observer and calls the existing application quit path only when no windows remain, preserving the source-registry and browser worker shutdown observers.

Application checking passed. A clean, signature-verified preview built from `edfba3ee8` was launched with the isolated QA profile and closed through its actual native close button. The process exited with status zero and a subsequent process query found neither the old nor fixed preview running. The fixed preview is `target/qa/Wavecrate Close Preview edfba3e.app`. No live profile or audio session was used.

### Random audition range foundation, 2026-09-09

Core now exposes deterministic random-section units and the native `f32` range calculation, including the 250 ms minimum, whole-file fallback for short or invalid durations, and native endpoint handling. It introduces no random generator, file access or playback side effects. All 143 core tests and checking passed, including four range parity tests. Browser candidate selection, loading and the three toolbar modifier modes remain follow-on work.

### Windows fade and metronome-backend validation, 2026-09-09

Windows run `34318763629` succeeded at `572aa5a3008cdf4634ee6093e1771aa2011352a9`: core/media/platform library tests, combined GPUI application/browser test compilation and browser tests passed. This run predates the last-window shutdown fix, the explicit Reson test step and the GPUI metronome UI. Those later changes require a newer run; native Windows input acceptance remains separate.

### GPUI metronome control and complete live transport updates, 2026-09-09

The compact toolbar now includes the native metronome glyph and beat-guide divisions. Metronome enable/count and loop changes share one complete desired transport tuple through live retargeting and normalized-audition preparation. Active presentation uses runtime confirmation with a distinct pending state. Each admitted playback retains its source-frame grid, including unavailable metadata, so later focus/selection/metadata changes cannot shift the active grid.

Output resume retains the complete admitted or confirmed configuration on its intent, with newer fence-valid normalized and runtime updates taking precedence. Repeated device changes can recover a still-current queued/deferred normalized resume; explicit stop and changed waveform generations reject it. Independent review found these resume races and the corrections passed focused re-review. All 340 UI tests passed with one existing benchmark ignored; application checking, formatting and diff checks passed. Native audible metronome acceptance remains outstanding.

### Native metronome toolbar and shutdown smoke, 2026-09-09

A clean `95aa470aa` snapshot built `target/qa/Wavecrate Metronome Preview 95aa470.app` in 25.58 seconds and passed strict ad-hoc signature verification. With the isolated QA profile, native toolbar clicks showed metronome enabled and disabled while audio remained idle. Closing the native window then exited with status zero; no Wavecrate preview process remained. This is idle UI/shutdown smoke evidence, not audible click/phase acceptance. Windows run `34322675452` targets the same revision and includes the new explicit Reson test step.

### Extraction final-claim ownership design review, 2026-09-09

Independent review confirmed that separate staging/final journal identities are necessary for clone publication but not sufficient. `fclonefileat` reports success without returning the new final handle or identity. A concurrent replacement before reopening can therefore yield a different object; matching content and a distinct inode alone do not prove operation ownership. The existing finalizer remains disabled. Future qualification must establish creation provenance as well as identity, containment, synchronization and crash checkpoints; ambiguous post-claim recovery must preserve evidence for audit rather than adopt or remove a content-matching file. Schema design should represent staging and final identities independently and retain explicit unproven-claim state before any macOS publication path is enabled.

### GPUI random audition, 2026-09-09

The native random toolbar control now plays a random section of the selected sample, uses Shift-click to choose another listed sample when possible, and uses Command/Ctrl-click to toggle sticky random sections for Space. The core 250 ms minimum applies. Normal fallback uses unfiltered folder/source candidates; listed fallback requires an already loaded selected waveform. One generation/path-fenced pending request waits for loading and is canceled by selection, explicit playback, stop or waveform cancellation. Completion consumes it before submitting through the existing playback, normalization, metronome, history and listen-recording paths.

Random selections remain in focus: a covered range retains the view, a fitting off-screen range pans, and a wider range zooms. Competing waveform gestures are canceled while committed edit effects remain intact. Independent review found and verified this viewport correction; no further findings remained. All 351 UI tests passed with one existing benchmark ignored, along with application checking, formatting and diff checks. Tests cover candidates/scope, stale and failed loads, one-time completion, cancellation, viewport focus and actual GPUI Command-over-Shift toolbar routing. Audible random playback acceptance remains manual. The UI directly uses the already locked rand 0.9.5 package; no package versions changed.


A clean `6d6a6ae00` snapshot built `target/qa/Wavecrate Random Preview 6d6a6ae.app` in 34.55 seconds and passed strict ad-hoc signature verification. This preview has not been launched for native random-audition acceptance.


### Windows shutdown and metronome validation, 2026-09-09

Run `34322675452` succeeded at `95aa470aa5703beb208e0cc5ae9d7f3d7f632aea`: Reson/core/media/platform tests, combined GPUI application/browser compilation and browser tests passed. This includes the last-window shutdown correction and GPUI metronome transport/resume changes. Random audition was added afterward; run `34326573383` targets `0427c65f7407e4ab9223e2241c0cacc921d36fac` for that coverage. Native audible acceptance remains separate.


### Similar Sections, 2026-09-09

The native Similar Sections toolbar control now scans the loaded sample against the playmark selection. Media retains a strong-revision-fenced WAV read lease, samples 192 bounded neighborhoods per candidate and groups native-score matches. Work is capped at 8,000 candidate windows with cancellation checked through decoding; no complete PCM buffer is retained. Short and quiet anchors return the native actionable messages.

The browser owns one active scan and one replaceable queued intent. Generation, path, strong context and raw play bounds fence results; source changes, committed play-selection changes and disabling cancel ownership. Clipped green fills/rails and hover colors use backward-compatible semantic theme tokens. Primary/secondary clicks on marks select and flash a fresh edit range with default effects; existing waveform handles retain priority and playback intent/range/epoch stay unchanged. Motion follows the theme and ends after the flash.

Validation: 354 UI tests, 68 media tests and 21 design tests passed; the UI and media suites each retain one explicitly ignored timing probe. Independent review caught the short/quiet status mismatch, which was corrected, and found no further meaningful issues. A release probe scanned a 60-second/48 kHz mono fixture with a one-second anchor, 7,978 candidate windows and radius-eight (17-frame) neighborhoods in 1.102 seconds. This is local background-scan timing, not external-drive or UI latency evidence. Native scan/click smoke and Windows coverage of this feature remain outstanding.


Native smoke of clean `6a51d547c` confirmed missing-playmark feedback, a completed scan and primary-click edit selection on the isolated two-second sine fixture. Secondary click changed the selection state but its handles appeared only after another toolbar action invalidated the view. Root right-release handlers now explicitly notify even when no drag was installed (match selection commits on press). Independent review confirmed this is an event-bounded redraw correction, with no extra mutation or idle loop. The first preview closed through its native close button with exit status zero; no preview process remained.


The redraw correction passed all three Similar Sections regressions and eight root pointer tests. A clean, strict-signature-verified `161e1b2f0` preview built in 23.81 seconds. Native secondary click immediately displayed the matched edit handles without an unrelated redraw; disabling removed the green marks while retaining the edit selection. Audio remained idle and Last Played stayed Never. The native close button exited with status zero and no Wavecrate preview process remained. Evidence: `target/qa/zero-snap-native/similar-right-161e1b2.png`; bundle: `target/qa/Wavecrate Similar Preview 161e1b2.app`. This verifies idle native interaction and shutdown, not audible transport or external-drive timing.


### Playmark timing foundation, 2026-09-09

Core now formats native duration/BPM playmark labels and parses bounded duration text: milliseconds, seconds, minutes, compound durations and beat-count-relative BPM. Frame edits keep the floored start, round the requested length and clamp the end to the file. The 64-character cap is checked before parsing allocation. Display rounding is intentional; the future editor must treat an unchanged draft as a no-op rather than reparsing it.

The shared text input now supports an optional Unicode scalar limit across programmatic, typed and composed replacements. Existing input modes remain unbounded by character count and retain their byte policies. Rejected edits preserve text, selection and composition; selected characters are reclaimed before checking a replacement. The label background is an additive semantic theme token with the native default. All 152 core tests, 12 text-input tests (including real GPUI composition rejection) and 21 design tests passed; application checking and diff checks passed. Read-only label display and live-span-safe editing remain follow-on work.


The read-only playmark label now displays duration or beat-relative BPM using the resolved live drag selection. Native placement prefers the selection interior, then either outside edge, with viewport clipping and a bounded fallback. Painting uses actual canvas bounds and the semantic theme background; no idle animation is introduced. Seven focused geometry tests passed, covering narrow and wide selections, edges, tiny bounds and invalid geometry. Interactive editing remains pending the active-span transport foundation.


Windows run `34326573383` succeeded at `0427c65f7407e4ab9223e2241c0cacc921d36fac`, adding random-audition coverage to the previously validated shutdown/metronome changes. Run `34331156363` targets `2665c45aea3de1f50230b8c3126a1948097da6a9` for Similar Sections and the read-only playmark label; it does not include the in-progress runtime span work.

The clean `a7f88617d` timing preview built in 22.92 seconds and passed strict ad-hoc signature verification. A native screenshot shows the centered `426 ms` label. The app state changed during interaction and Last Played became Today, so further native actions were paused; this is visual label evidence only, not controlled transport acceptance. Bundle: `target/qa/Wavecrate Timing Preview a7f8861.app`.


### Live span runtime foundation, 2026-09-09

Reson now exposes the configured playback plan's frame bounds and denominator together with a persistent last-successful retarget request identity. Later active progress polls can prove a dropped immediate acknowledgement; inactive, missing or invalid span metadata is cleared. New play attempts and stop reset the retarget identity; a failed newer retarget preserves the last successful identity. The denominator belongs to the quantized plan, not necessarily the source container's nominal frame count.

The opt-in outside-span seek policy resolves against one exact player frame snapshot after target-plan quantization. Explicit seek takes precedence; timing edits preserve frames before the native one-percent end guard and otherwise start at the selection beginning. Mode restarts retain full target bounds with a separate exact seek offset. One-shot decoded/file sources and edit/static fades honor that offset. Existing public retarget entry points keep their prior policy and requested restart offset. Browser controls currently opt out until timing editing is connected.

Review identified and corrected inactive cached metadata, floating-point boundary aliasing, one-shot restart bounds/offset round-tripping, and legacy restart-offset behavior. All 213 Reson library tests passed before the final provenance test was added; focused retarget and lazy-source regressions passed. The additional real-executor test uses decoded silence at volume zero and checks retarget/immediate-poll/later-poll/stop metadata, but explicitly skipped here because no audio output device was available. Application checking, UI test compilation, formatting and diff checks passed. Audible transport acceptance remains outstanding.


The browser now carries `PlaybackRetargetTarget` (span, loop/metronome configuration and seek policy) as one value through pending runtime updates, normalized preparation/disable fallback, and output resume. Existing controls still construct it from the accepted span with the new seek policy disabled. This is data propagation only; logical target-generation ordering and editable-span admission remain pending. All 39 transport and 13 normalization tests passed, together with the UI library check.


### Budget checkpoint, 2026-09-09

The repository is at a tested prerequisite checkpoint. Compact text-input sizing is opt-in; existing constructors retain their dimensions, and all 12 text-input regressions passed after complete-target propagation. Logical retarget-generation fencing, browser consumption of persistent runtime acknowledgements, and interactive duration editing remain deferred. No generation-fencing changes were made. The existing live-playmark contract in `GPUI_ARCHITECTURE.md` is the next-step authority.

Windows run `34331156363` finished cancelled, so it supplies no completed coverage for Similar Sections or timing work. The latest successful Windows evidence remains `34326573383` at `0427c65f7407e4ab9223e2241c0cacc921d36fac`. Current runtime and target-propagation validation is local as recorded above.


### Retarget generation ownership, 2026-09-09

Live retargets now retain a logical generation and playback ID, path, playback epoch, waveform generation and output generation anchor. Normalized retarget freshness uses that immutable ownership and the current logical target; it does not depend on the mutable accepted span. Independent gain preparation preserves/reschedules the current retarget instead of cancelling it while leaving it advertised. Current configuration lookup and resume ignore stale anchors and keep the captured span/seek policy. ApplyGain retains its stricter accepted-span fence.

Lifecycle stabilization fixed optimistic metronome fallback after a failed enable: confirmed disabled state remains disabled, with accepted resume configuration preferred over preferences when confirmation is unavailable. Stale completion/failure cannot clear a newer normalized target; failure of the current target clears only its logical ownership. Runtime acknowledgement consumption and interactive timing editing remain pending.

The full UI suite passed 367 tests with one existing benchmark ignored. A subsequent focused GPUI regression also passed, covering an older accepted-span update during newer preparation, supersession, stale completion/failure, and clearing only the current failed target. Complementary tests cover old runtime replies, repeated output resume and confirmed-disabled fallback. UI/application checks, formatting and diff checks passed. This is deterministic UI/state validation, not audible runtime acceptance. The compact input's rendered 18-pixel height was separately verified before/after focus in an isolated checkout.


### Runtime acknowledgement integration, 2026-09-09

The browser confirms retargets only when the persistent configured request identity, valid source-plan frame tuple, loop state and effective submitted metronome configuration agree. A later poll can acknowledge a dropped immediate reply. Accepted spans are projected from the runtime plan denominator rather than the requested preview span; incomplete or mismatched metadata remains pending without optimistic promotion.

The runtime executor now retains successful metronome configuration for lazy WAV playback, whose static metronome cannot be read through a live span control. Replacement attempts, stop and inactive playback clear this metadata; successful retargets update it. The public AudioPlayer live-control accessor keeps its existing semantics.

Validation passed: 214 Reson tests, 43 focused browser transport tests, application checking, formatting and diff checks. The real-output lazy-WAV regression explicitly skipped because no audio output device was available. Audible acceptance and interactive timing editing remain outstanding.
