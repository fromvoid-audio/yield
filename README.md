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
| `wavecrate-library` | Existing persistence and source-ownership primitives |
| `wavecrate-scan` | Existing persistent scanner |
| `wavecrate-analysis` | Existing decoding and analysis |

See [crate boundaries and dependency direction](docs/GPUI_ARCHITECTURE.md).
The original frontend is preserved in [reference/radiant](reference/radiant/README.md)
and excluded from the active build.

The current GPUI frontend uses the original Wavecrate editorial-terminal
palette and compact layout. Theme tokens are defined independently in
`wavecrate-design`; the complete schema and a copyable default document are in
[themes/wavecrate.json](themes/wavecrate.json). Open the gear settings control
to import, reload, or reset a theme while the app is running. The same panel
toggles animations for this session. Row hover rails remain immediate; loading
indicators animate only during pending work and remain static when disabled.

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

Add one or more source folders from the Sources sidebar or toolbar, switch or
forget them for the current session, expand the relative folder tree, and choose
direct-only or descendant scope. Refresh keeps valid folder, search, sort, and
sample focus state; an unavailable source reports the failed refresh and leaves
its last successful session view available for retry. Search filenames with
Cmd/Ctrl+F and sort by name, size, or extension. Select a row with the pointer
or Up/Down; Space plays/stops and Escape stops playback. While searching, Space
enters text and Escape returns browser focus. Filtering and sorting preserve the
focused sample when it remains visible.

Audition and waveform previews currently support WAV. Other recognized audio
formats are listed. Waveforms show a bounded initial segment; files over 256 MiB
are not previewed. The application does not persist library state or modify
sample files.

See the [parity register](docs/GPUI_MIGRATION.md) for remaining workflows and
validation evidence. Editing, persistent curation, recovery, multiselect,
full-length waveforms, and DAW handoff remain outstanding. Inherited release
workflows are disabled.

## Validate

```sh
cargo check --locked -p wavecrate-gpui
cargo test --locked -p wavecrate-core -p wavecrate-design -p wavecrate-media -p wavecrate-ui -p wavecrate-gpui
```

Third-party component attribution is in [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
