# Scripts

Use `scripts/yield-local-ci` for exact-commit local checks, channel builds and
reversible clean state on macOS. Test builds use separate development state;
beta/release builds use existing production state. Examples:

```sh
scripts/yield-local-ci doctor
scripts/yield-local-ci check --sha <full-commit-sha> --lane preflight
scripts/yield-local-ci build --sha <full-commit-sha> --channel test
scripts/yield-local-ci run --artifact <artifact-manifest.json> --state normal
scripts/yield-local-ci run --artifact <artifact-manifest.json> --state clean
scripts/yield-local-ci state restore --backup <printed-transaction-id>
scripts/yield-local-ci state reset --namespace production
```

`build` never launches or resets state. A clean app launch prints its transaction
ID and leaves restoration explicit after exit. Native/full check lanes restore
the original production state automatically after their owned processes exit.
Clean state is parked and retained; no recursive wipe or dirty-marker deletion is
performed. See [TEST](../docs/TEST.md#local-macos-ci-and-reversible-state) for
namespace rules, safety boundaries, native selectors and interrupted recovery.

`state reset` is the explicit development reset for starting a fresh **normal**
session. Quit all Yield versions first. It uses the same backup-preserving whole
root transition as `state clean`; keep the printed backup transaction. Subsequent
normal launches retain the newly created state. Restoring the backup brings back
its original state, including any unresolved ownership or recovery fault. This
does not reconcile a lost journal or modify metadata beside your audio files.

When the owner explicitly chooses to keep a superseded reset backup only as
historical evidence, run `scripts/yield-local-ci state archive --transaction
<transaction-id> --reason '<owner decision>'` after quitting Yield. The current
normal root must exist and differ from the retained original. This moves the
whole transaction by same-filesystem rename into `.yield-local-ci/history/`,
preserving its original record, files and identities. It records the reason and
observed identities separately. It never rebases a legacy device number or grants
restoration authority; `state restore` and `state recover` no longer select that
archive. An interrupted operation retains all evidence at the source or its
deterministic history location. Retry with the same transaction and reason: an
unfinished private reservation is reused only while the original transaction,
current root and any committed decision receipt still match. Temporary receipt
files are retained; existing archived custody or unknown entries refuse the retry.
No automatic archival is performed during startup or checks.

For the active GPUI fork, use `scripts/gpui-local-ci.sh` on macOS and
`scripts/install-gpui-local-ci.sh` for its automatic commit hook; see
[GPUI local CI](../docs/GPUI_LOCAL_CI.md). Windows and Linux native release qualification
is deferred until the macOS release-ready gate OPT-1863 is complete.
The older `scripts/ci.sh` lanes below still target the legacy Radiant app and
are not GPUI acceptance gates.

For portable GPUI development, use `scripts/gpui-development-checks.sh check`
(format and locked application check), `test` (the documented portable regression
subset), or `preflight` (branch policy, script fixtures, source writer/GPUI owner
boundary guards, format and locked application check). The preflight requires
Python 3, ripgrep, the repository Rust toolchain and cargo-nextest; this Linux
executor was qualified with cargo-nextest 0.9.114. Nextest validates stale binary
selection in a disposable minimal crate, without compiling archived app tests.
Install it with `cargo install cargo-nextest --version 0.9.114 --locked`.

`scripts/agent.sh checks` and `preflight` now run active-workflow toolchain pinning
and this same GPUI workspace preflight. All `.yml.disabled` workflows are deliberately
inactive; their presence is not a successful CI run. The archived monolith readiness,
app-core and Radiant resolver guards are no longer mandatory agent checks. Older
`scripts/ci.sh` lanes remain historical tools, not GPUI acceptance.

`preflight` coalesces only the same worktree, branch, HEAD, tracked/untracked bytes,
executable modes, check command and relevant Rust build environment. Changed inputs
during checks fail and require a rerun. Different worktrees never share success even
with an explicitly shared state directory. This fingerprint coordinates checks; it
is not a filesystem snapshot lock. Use isolated worktrees and keep inputs stable.
An explicit state directory must be outside checked inputs (or ignored by Git).
PowerShell agent wrappers forward to the same Bash runners (Git Bash, Python 3
and the existing Rust/tool dependencies required on Windows), preserving exit status.
Windows wrapper execution remains native verification, not a qualification claim.
`request` runs preflight by default; `--quick-ci` adds the documented portable test
subset, and `--full-ci` explicitly requests the native macOS lane. Portable checks
do not certify native runtime, Metal, Finder or release acceptance.

For a reproducible CPU waveform baseline (OPT-1713), use Python 3 on Linux or
macOS in a clean checkout with the repository's Rust toolchain:

```sh
python3 scripts/internal/perf/gpui_baseline.py --out target/qa/baseline-1.json
python3 scripts/internal/perf/gpui_baseline.py --out target/qa/baseline-2.json
```

The harness runs the existing ignored waveform geometry/cache benchmark once,
with five measured cases: geometry, gain preview, and fade preview (the latter
two at 2,048 and 65,536 input columns). Each capture includes the exact Git
head/tree, Rust/Cargo versions, build profile/configuration hashes, constant-band
fixture and source hash, CPU/memory identity, Linux affinity/cgroup limits, command,
iteration counts, and total nanoseconds for misses and hits. Compare captures
only when build, fixture, host and measurement configuration match; timing values
will vary. `--profile release-local` selects the existing optimized local profile.
Custom Rust flags, Cargo target/profile overrides, dirty checkouts and unidentified
CPU hosts are blocked. Output paths must be new; keep reports under ignored
`target/qa/`. Command failure, timeout, output flooding, zero matched tests, or
missing/duplicate records cannot produce a captured result.

`captured` means the CPU measurements were obtained, with no performance threshold
or release verdict. Compilation time is excluded from those measurements. Native
frame/input/idle-memory/audio acceptance is explicitly unsupported on Linux and
unverified on macOS; rendering and GPU work are excluded on both. Large-source
durable-owner evidence remains OPT-1683's separate workload, and the existing
backend/perf scripts remain its entrypoints. Do not use this CPU capture to clear
native or large-library gates. Harness regressions can be checked with
`python3 -m unittest discover -s scripts/internal/perf -p test_gpui_baseline.py`.

Top-level `scripts/` is intentionally small. `scripts/command-inventory.json`
is the checked inventory for public entrypoints, compatibility wrappers, and
dispatcher maps. These are the public entrypoints people should run directly:

- `bootstrap.{sh,ps1}`: set up the repo and install hooks.
- `build.sh`: build Yield and create a uniquely identified, ad-hoc-signed
  macOS app bundle that can be targeted independently by UI automation.
- repo-root `run.{sh,ps1}`: compatibility wrappers for `cargo run`.
- `run.ps1 logs debug-overlays` or `run.ps1 logs debug-layout`: convenience
  aliases for `cargo run -- --log` with debug layout overlays shown. The
  repo-root `.\run.ps1` delegates these public run commands to `scripts/run.ps1`.
- `doctor.{sh,ps1}`: diagnose environment issues.
- `agent.{sh,ps1}`: agent request, preflight, checks, and hook install helpers.
- `ci.{sh,ps1}`: validation lanes (`smoke`, `agent`, `quick`, `local`) plus the
  opt-in `isolation-stress` lane. `local` is the broad local validation lane;
  `isolation-stress` repeats the library harness in fresh parallel processes
  with JSONL evidence. Perf and GUI checks use their own entrypoints.
- `check.{sh,ps1}`: focused guardrails and report helpers.
- `run.{sh,ps1}`: sandbox, cleanup, log, and bug-bundle helpers.
- `perf.{sh,ps1}`: performance commands. `scripts/perf.sh browser-vm` is the
  host-calibrated 0.1 CPU regression lane for browser projection; it does not
  measure native frames. `scripts/perf.* guard` retains the standalone Radiant
  historical frame/input guard and is not the active Yield/GPUI browser gate.
  The Bash-only `calibrate-startup` command is optional Linux developer tooling
  for refreshing startup threshold lock files; it is not shipped Linux product
  support.
- `release.sh`: compatibility wrapper for local CI builds. Use
  `scripts/release.sh build --sha <40-character-commit-sha> --channel <test|beta|release>`;
  it delegates to `scripts/yield-local-ci build` and preserves its result. Test
  apps use isolated state, while beta and release apps use the saved Yield
  state when launched. Building does not launch or reset state. The former
  prepare, RC, stable, and GitHub dispatch commands are
  obsolete; this wrapper performs no release publication.
- `gui.ps1`: Windows GUI validation lanes.

PowerShell compatibility wrappers also remain available for the older
single-purpose entrypoints:

- `devcheck.ps1`
- `ci_agent.ps1`
- `ci_quick.ps1`
- `ci_local.ps1`
- `run_sandbox.ps1`
- `clean_sandbox.ps1`
- `latest_log.ps1`
- `bug_bundle.ps1`

Those wrappers delegate to `ci.ps1` and `run.ps1` so existing local muscle
memory and external instructions still resolve during the migration. Keep them
until the inventory marks a wrapper as retired and every canonical repo
reference has been moved to the dispatcher form.

Everything else under `scripts/internal/` is implementation detail. Generated
training dataset artifacts should stay under ignored workspace paths such as
`tmp/training_dataset/`.
