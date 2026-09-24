# Private GPUI macOS package

Run the local macOS full CI lane on the intended commit, then use a clean
checkout of that exact commit to build a private bundle:

```sh
python3 scripts/package-gpui-macos.py \
  --sha "$(git rev-parse HEAD)" \
  --out-dir "$(pwd)/target/private-macos/$(git rev-parse HEAD)"
```

The command builds the `release-local` profile and creates a new artifact
directory containing `Yield.app` and `manifest.json`. It refuses a
dirty checkout, a mismatched SHA, missing assets, or an existing destination.
The bundle contains the executable, icon, font license, and `Info.plist`. The
font itself is embedded in the executable. The manifest records the exact
source commit, workspace package version, macOS build number, host architecture,
and each file's size and SHA-256 hash.

`CFBundleShortVersionString` comes from the `yield` Cargo package.
`CFBundleVersion` is the commit count at that source SHA, so private builds from
later commits have a larger build number. The development bundle identifier is
`org.portalsurfer.yield.dev`. This package is unsigned; controlled
signing and notarization belong to OPT-1761, artifact verification to OPT-1865,
and isolated install and launch to OPT-1771. Rebuild from the eventual release
candidate head before those acceptance steps.
