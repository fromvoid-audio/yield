# Wavecrate GPUI

Private development fork of [Wavecrate](https://github.com/PORTALSURFER/wavecrate),
reimplementing the desktop frontend in GPUI with **full current feature parity**
as the target.

The migration is in progress. The original Radiant application is retained as a
behavioral reference; this fork is not yet a feature-complete replacement.

## Run the GPUI application

```sh
./run-gpui.sh
# Or open a sample folder at startup:
./run-gpui.sh /path/to/samples
```

The GPUI application has a separate Cargo workspace under `apps/wavecrate-gpui`.
It uses published GPUI and the existing Reson engine without requiring a
Radiant checkout. Native Rust build tools and the platform audio/graphics build
dependencies are required.

```sh
cargo check --locked --manifest-path apps/wavecrate-gpui/Cargo.toml
cargo test --locked --manifest-path apps/wavecrate-gpui/Cargo.toml
```

## Migration

See [the parity register and architecture](docs/GPUI_MIGRATION.md) for the
baseline, extraction boundaries, remaining workflows, and completion evidence.
The first slice establishes browsing and auditioning. Editing, persistent library
curation, recovery, and platform handoff must pass their own parity checks.

Current controls: choose a folder or supply it on the command line, select a row
with the pointer or Up/Down, and use Space to play/stop or Escape to stop.
Audition and waveform previews currently support WAV. Other recognized audio
formats are listed, but their previews are not implemented. Waveforms show a
bounded initial segment; files over 256 MiB are not previewed in this slice.
The application does not persist library state or modify sample files.

The initial macOS build, compile check, formatting check, and seven focused tests
pass. Manual GUI/audio acceptance, Windows validation, and full feature parity
remain outstanding.

Inherited release workflows are disabled. The original application's README is
preserved in [README.upstream.md](README.upstream.md); its launch and distribution
instructions describe the Radiant reference application.
