# Private Linux test build

This manual ZIP targets x86_64 Linux. For local qualification, build it from
the exact clean Yield checkout on an x86_64 Linux development machine.

```bash
python3 scripts/package-gpui-linux.py --sha "$(git rev-parse HEAD)" \
  --out-dir /absolute/path/to/a/new/private-directory
```

The packager uses the locked workspace and optimized `release-local` profile.
From the output directory, run `sha256sum -c SHA256SUMS` and extract
`yield-linux-x86_64.zip` into a fresh directory. The archive contains a
`yield/` directory with the application and a `package-manifest.json` that
records its source commit, target, build profile and payload digests.

Launch the extracted `yield/yield` binary from a Linux desktop session. To add
Yield to your application menu, run
`python3 yield/scripts/register-linux-desktop.py` from the directory containing
the extracted `yield/` folder. Use a permanent ASCII extraction path free of
control characters, `%` and `=`. Spaces are supported. Keep the folder in that
location while the launcher is registered.
The command prints the new `org.portalsurfer.yield.desktop` path under
`${XDG_DATA_HOME}/applications/` or `~/.local/share/applications/`. Its icon
comes from the extracted package.

For isolated QA, set `YIELD_CONFIG_HOME` to a disposable config base and add
only fixture sources. Use a desktop login session with that environment for the
menu launch. Run `yield/scripts/doctor.sh --installed` to confirm the profile.
Use `yield/scripts/run.sh logs` to inspect logs locally or
`yield/scripts/run.sh bug-bundle` to create a private status archive.

Update manually by closing Yield, verifying the newer artifact and extracting
it into a fresh directory. Remove the old test launcher at the printed path,
then register the new build. To uninstall the private build, remove that
launcher and its extracted directory. Yield configuration and sample sources
remain in their own locations.
