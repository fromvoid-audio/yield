# Wavecrate GPUI

A private GPUI reimplementation of [Wavecrate](https://github.com/PORTALSURFER/wavecrate).
Full current feature parity is the target; the migration is in progress.

## Workspace

The root Cargo workspace follows Rox's approach of a thin executable composing
separate crates with defined ownership:

| Crate | Responsibility |
| --- | --- |
| `wavecrate-gpui` | Thin executable and launch arguments |
| `wavecrate-core` | Pure sample types, filtering, sorting, selection projection |
| `wavecrate-media` | Background file discovery and waveform preparation |
| `wavecrate-design` | Validated semantic theme schema and canonical design tokens |
| `wavecrate-ui` | GPUI views, input components, UI and transport coordination |
| `reson` | Existing audio engine |
| `wavecrate-library` | Persistence and source-ownership primitives |
| `wavecrate-catalog` | Audited source projections and bounded metadata operations |
| `wavecrate-source-watcher` | Native watch handles and bounded lifecycle-fenced change hints |
| `wavecrate-platform` | Native file-drag foundation with explicit platform-session ownership |
| `wavecrate-session` | Profile, source registry, preferences, and catalog lifecycle |
| `wavecrate-scan` | Existing persistent scanner |
| `wavecrate-analysis` | Existing decoding and analysis |

See [crate boundaries and dependency direction](docs/GPUI_ARCHITECTURE.md).
The original frontend is preserved in [reference/radiant](reference/radiant/README.md)
and excluded from the active build.

The current GPUI frontend uses the original Wavecrate editorial-terminal
palette and compact layout. Theme tokens are defined independently in
`wavecrate-design`; the complete schema and a copyable default document are in
[themes/wavecrate.json](themes/wavecrate.json). Open the gear settings control
to import, reload, or reset a theme while the app is running. Theme choice and
animation, audition, volume, and audio-output preferences persist across launches. Row hover rails remain immediate; loading
indicators animate only during pending work and remain static when disabled.
Focus rails fade independently of selection, and clipped lists show directional
scroll-edge fades. Disabling animation keeps focus feedback visible and updates
scroll-edge feedback immediately.

## Run

```sh
cargo run -- /path/to/samples
# Or open the folder picker:
./run-gpui.sh
```

On macOS, build a development application bundle with:

```sh
./scripts/build-gpui-macos.sh
```

The script prints the `.app` path under the ignored Cargo target directory.
Rust and the platform's native audio/graphics build tools are required.
Development builds optimize dependencies and the UI/media crates so rendering
and decoding remain responsive. The first build after this profile change
recompiles those crates; subsequent edits reuse the optimized dependencies.

## Current behavior

Source folders and preferences persist in the isolated GPUI profile. Add, switch,
or forget sources from the sidebar, expand the relative folder tree, and choose
direct-only or descendant scope. File changes refresh automatically through bounded
native watcher hints and authoritative catalog audits. Refresh preserves valid navigation state; an
unavailable source keeps its last successful view while recovery probes run in
the background. A returning folder is audited again; a physically replaced folder
gets a verified new catalog generation after earlier metadata saves finish.

Search filenames with Cmd/Ctrl+F. Sort by name, rating, Type, collection, extension,
size, or Last Played. Type and Last Played columns appear when the window has
room. Collection, rating, and TYPE filters intersect the folder and name scope.
TYPE recognizes Loop and 1-Shot tags; clearing both chips disables that filter.
Use TAGS for comma-separated exact tag matches; every entered tag must match.
Click the TAGS label to toggle filtering while retaining the draft. Tag filtering
intersects the other filters, and confirmed metadata-only edits reuse its matches
so playback timestamp updates do not clear and reload the filtered list.

Select a row with the pointer or Up/Down; Space plays/stops and Escape stops
playback. While searching, Space enters text and Escape returns browser focus.
Shift-click or Shift+Up/Down selects a range. Cmd/Ctrl-click toggles a row;
Cmd/Ctrl+Shift adds a range. Cmd/Ctrl+A selects visible files while the browser
has focus, and X toggles the focused file. Plain arrows move preview focus while
preserving a batch selection; a plain click starts fresh.

Plain [ and ] adjust ratings for the explicit selection or focused file. Keys
1–6 toggle collection membership. These edits use bounded catalog operations and
persist across source changes and application shutdown. The sample context menu
provides reveal, path copying, conditional unlock, and collection removal.
Accepted audition requests update Last Played through the same metadata lane.

Use ADD TAG for the selection or focused file. Enter submits typed tags; a single
new tag opens a category chooser with Up/Down, Enter, and Escape controls. Multiple
comma-separated tags can be added together. Single-tag drafts show a bounded,
paged suggestion list and a faint completion suffix. Up/Down or hover explicitly
selects a suggestion; Enter otherwise submits the text you typed. Existing source
tags retain their database identity when selected. Selected-tag chips show All or Mixed
membership, and their × control removes that assignment from the selection.
Larger tag unions have paging controls. Chips update after metadata is saved.
The ADD TAG arrow opens a paged Tag Editor with five collapsible categories.
Click a tag to toggle its assignment across the selection; drag supported tags
to another category to update the profile dictionary. Legacy tags keep their
source identity, and playback-type categories stay locked. Profile definitions
can be removed without removing file assignments; Undo removal restores the last
definition if no later dictionary edit has changed its revision.

The Name header's Disk/Label control switches between filename stems and ordered
tag labels. Labels load for visible rows and fall back to the filename when absent.
This display choice does not rename files or change their selection or sort order.

The normal browser, audition, and waveform previews support WAV files. Waveforms
cover the full file with cancellable, bounded-memory decoding, zoom and pan,
play-region selection, and a playback cursor. Play regions can be moved, resized,
or shifted with the keyboard. The BPM toggle snaps resized regions to whole BPM
values, using the adjacent beat-count field (2–32 beats). The separate beat-guide
toggle divides the selected region visually, including during resizing and zoom.
Playback navigation history, native file clipboard
handoff, Finder reveal, source/folder context-menu opening, and macOS/Windows whole-file
drag-out are implemented. Windows build and native interaction acceptance are pending. Drag-out
prepares up to 256 files in the background and rejects incomplete selections.
Loop changes apply to active playback
with runtime identity checks and retain the accepted audition span. Loop and
one-shot tags select the next-play mode automatically; a manual choice remains
in effect for the loaded file. The speaker control beside the volume slider toggles
normalized audition. It prepares the active span in the background, retains your
volume setting, and saves the toggle between sessions.

Use the ? toolbar control or Cmd/Ctrl-/ to open keyboard help. The dialog
supports arrow/page-key scrolling and restores focus when closed.

Full parity is not complete. Remaining workflows include full tag-library
management and naming, physical file operations and recovery, waveform
editing/export, analysis and timing controls, region drag-out, and additional settings
and platform acceptance.
See the [parity register](docs/GPUI_MIGRATION.md) for detailed validation evidence
and outstanding work. Inherited release workflows are disabled.

## Validate

```sh
cargo check --locked -p wavecrate-gpui
cargo test --locked -p wavecrate-core -p wavecrate-design -p wavecrate-media -p wavecrate-ui -p wavecrate-gpui
```

Third-party component attribution is in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
