# Radiant implementation reference

This directory preserves the original Wavecrate application source, tests,
tools, build entrypoint, and launch/release metadata for the GPUI parity
migration. It is excluded from the active Cargo workspace.

`Cargo.toml.reference` and `Cargo.lock.reference` are historical manifests,
not runnable Cargo targets at this relocated path. Build and validate the
original application in its original upstream checkout. Relative paths inside
these archived tools and source files retain their original meaning.

The reference baseline is `ed024e40e7cee8b4405791b721a068c45183e6de` from
https://github.com/PORTALSURFER/wavecrate. The GPUI fork retains that history.
Product and persistence contracts remain in the root `docs/` directory.

The active application is composed by `crates/wavecrate-gpui`. See
`docs/GPUI_ARCHITECTURE.md` at the repository root for current crate ownership.
