# Yield

Yield is a sample manager for browsing, auditioning, and organizing sounds. Its GPUI frontend is in active alpha development.

- Add sample folders and browse them by name, type, tags, rating, or collection.
- Audition WAV and uncompressed AIFF files, inspect waveforms, and select or loop a playback region.
- Tag and rate batches, then drag whole files into your DAW.

## Build

Requires the Rust toolchain pinned in `rust-toolchain.toml` (`rustup`).

```sh
cargo run --locked -- /path/to/samples  # run with a sample folder
./run-gpui.sh                           # launch and open the folder picker
cargo build --release --locked          # build release binary
./scripts/build-gpui-macos.sh           # build a macOS development .app bundle
```

Windows build and native interaction acceptance are pending.

## About

I’m [PORTALSURFER](https://soundcloud.com/portalsurfer). I make music and build instruments and tools for making it under [fromvoid](https://github.com/fromvoid-audio). I use AI extensively in that work and share these projects so more people can make music, whatever their budget.

I build this software for myself first and foremost and offer no guarantees of support. That said, I welcome code, documentation, and interesting ideas, and I’m happy to add them when we align on the direction.

## Support

If you’d like to help cover development costs, you can chip in on Ko-fi:

<a href='https://ko-fi.com/F7X2225YCF' target='_blank'><img height='36' style='border:0px;height:36px;' src='https://storage.ko-fi.com/cdn/kofi6.png?v=6' border='0' alt='Buy Me a Coffee at ko-fi.com' /></a>

---

[Development docs](docs/README.md) · [GPUI migration status](docs/GPUI_MIGRATION.md) · [MIT license](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md)

Earlier releases were made available under [CC0 1.0](licenses/CC0-1.0.txt).
