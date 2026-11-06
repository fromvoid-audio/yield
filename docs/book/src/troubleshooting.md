# Troubleshooting

## Find Logs

Use **Options -> Open config folder** and look in the `logs` folder inside the
active profile.

- macOS: `~/Library/Application Support/.yield/`
- Windows: `%APPDATA%\\.yield\\`
- Linux desktop test build: `${XDG_CONFIG_HOME}/.yield/` when the XDG base is
  absolute, or `~/.config/.yield/` by default.

For a Linux test bundle containing the support scripts, run
`bash scripts/doctor.sh --installed` to print the active profile and log
directory. Use `bash scripts/run.sh logs` to inspect the latest log locally.
`bash scripts/run.sh bug-bundle` creates a private archive of basic system facts
and log availability in `./yield-support`. Use the same `YIELD_CONFIG_HOME` and
`YIELD_CONFIG_PROFILE` values as the app when you set these overrides.
