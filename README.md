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
direct-only or descendant scope. Refresh preserves valid navigation state; an
unavailable source keeps its last successful view available for retry.

Search filenames with Cmd/Ctrl+F. Sort by name, rating, Type, collection, extension,
size, or Last Played. Type and Last Played columns appear when the window has
room. Collection, rating, and TYPE filters intersect the folder and name scope.
TYPE recognizes Loop and 1-Shot tags; clearing both chips disables that filter.

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
comma-separated tags can be added together. Selected-tag chips show All or Mixed
membership, and their × control removes that assignment from the selection.
Larger tag unions have paging controls. Chips update after metadata is saved.

The normal browser, audition, and waveform previews support WAV files. Waveforms
cover the full file with cancellable, bounded-memory decoding, zoom and pan,
play-region selection, and a playback cursor. Play regions can be moved, resized,
or shifted with the keyboard. Playback navigation history, native file clipboard
handoff, and Finder reveal are available. Loop changes apply to active playback
with runtime identity checks and retain the accepted audition span. Loop and
one-shot tags select the next-play mode automatically; a manual choice remains
in effect for the loaded file.

Full parity is not complete. Remaining workflows include tag completion, filtering,
vocabulary management and naming, physical file operations and recovery, waveform
editing/export, analysis and timing controls, external drag, and additional settings
and platform acceptance.
See the [parity register](docs/GPUI_MIGRATION.md) for detailed validation evidence
and outstanding work. Inherited release workflows are disabled.

## Validate

```sh
cargo check --locked -p wavecrate-gpui
cargo test --locked -p wavecrate-core -p wavecrate-design -p wavecrate-media -p wavecrate-ui -p wavecrate-gpui
```

Third-party component attribution is in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
