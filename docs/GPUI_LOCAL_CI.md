# GPUI local CI

The GPUI fork validates committed source on macOS. The old Windows
GitHub Actions workflow is preserved as `gpui-windows.yml.disabled` for its
command inventory, but it is no longer an automatic gate.

Run `bash scripts/gpui-local-ci.sh full` from any directory for the complete
local GPUI lane, or `bash scripts/gpui-local-ci.sh storage` for storage-focused
changes. Both modes check formatting and use the pinned lockfile. The full
mode checks the GPUI app and runs the library, journal, core, platform, and UI
tests previously listed in the Windows workflow. The storage mode checks the
storage library and its focused edit tests. The runner exits on non-macOS
hosts. Windows and Linux implementation and validation are deferred until
release-ready macOS acceptance in OPT-1863. The old PowerShell commands remain
in `scripts/gpui-local-ci.ps1.deferred` for later reassessment; they have not
been validated on a Windows host.

The user's 2026-09-30 instruction permits development on Linux while Mac checks
are tracked for later. Run `bash scripts/gpui-development-checks.sh check` for
formatting and the normal locked app check, or `test` to also run core, shared
reserve, filter-handler and enlarged-browser fixtures. Use `preflight` for branch
policy, script fixture checks, source-writer/GPUI owner guards, format and the
locked app check. It requires Python 3, ripgrep and cargo-nextest (qualified on
Linux with 0.9.114); see [Scripts](../scripts/README.md) for setup and the archived
Radiant preflight distinction. This portable development
command requires the host's GPUI build dependencies and preserves normal lint
settings. It does not run native AX, Metal, FSEvents, audible or release checks,
and does not replace the macOS local CI gate or claim Linux desktop support.
Remaining native sessions are indexed in [OPT-1953](https://linear.app/boostnlvp/issue/OPT-1953),
including new filter controls (OPT-1954) and target-gating regressions (OPT-1955).
Linux retained-directory SQLite opens use a private adapter to the bundled Unix
VFS, tracked in [OPT-1957](https://linear.app/boostnlvp/issue/OPT-1957). It preserves
the `/proc/self/fd` directory alias during pathname normalization and delegates
file I/O, locking and final-child no-follow protection to SQLite. The `test` scope
includes rollback/WAL root-replacement and main/sidecar symlink fixtures, volume
ownership, journal reserve and profile admission. Namespace syncing reopens the
directory itself relative to the retained capability because Linux O_PATH handles
cannot be fsynced (OPT-1958). Physical reserve allocation remains unsupported on
Linux; these checks do not establish native release or crash-durability acceptance.

The full lane also runs the vendored GPUI Metal post-pass regression tests with
its own locked manifest. Application tests do not run dependency unit tests.
These checks exercise shader compilation, cached-target reuse, resize allocation
failure, and restoration of the normal compositor after a rejected shader. The
vendor test workspace keeps its development dependencies out of Yield's lockfile.
Cross-display and visual acceptance remain separate native checks.

The full and storage lanes run the `yield-session` library suite. Use
`bash scripts/gpui-local-ci.sh session` to run that suite by itself. OPT-1861
records the test fixture repair that restored this coverage on macOS. All three
lanes limit that suite to four test threads: each session test starts several
SQLite-backed owners, and the host's default test parallelism can exhaust its
existing five-second event deadlines before the owners finish initializing. This limit
does not change those deadlines or the tests' assertions. Run the lane on a
macOS host with enough free space for Cargo's shared target and temporary test
profiles; inspect the exact run log and free space if startup times out.

Install the macOS automatic trigger once with
`bash scripts/install-gpui-local-ci.sh`.
It installs a `post-commit` hook in this repository's shared Git directory,
so commits from its worktrees run validation before the Git command returns.
The hook runs the **exact committed SHA** in a temporary detached worktree,
not the dirty working tree. Runs are serialized against a shared Cargo target
with an OS file lock. The temporary checkout is removed afterward.

For a commit SHA, inspect:

```text
<git-common-dir>/local-ci/runs/<SHA>/result
<git-common-dir>/local-ci/runs/<SHA>/check.log
```

`result` is `RUNNING`, `PASS`, or `FAIL` with a UTC timestamp. `PASS` is scoped
to the recorded SHA and host. A failed check keeps its log and can be rerun
with `bash scripts/internal/ci/run_gpui_commit_ci.sh "$(pwd)" <SHA>`; a passed
SHA is reused. The hook cannot retroactively validate earlier commits. To
update an installed runner after changing these scripts, rerun the installer.

The trigger requires a local `lockf` or `flock` command and Rust/Cargo. It does
not publish artifacts, merge code, or change branch state. An installed runner
is a copy; rerun the installer after updating the scripts. Its result is macOS
evidence only.

For a private bundle from an exact validated commit, follow
[GPUI macOS packaging](GPUI_MACOS_PACKAGING.md).

## Focused original recording custody verification

The `Native recording custody` workflow runs the two OPT-2330 production-bootstrap
fixtures on a real `macos-15-intel` runner, using the repository toolchain and
original per-user registry. It runs on changes to its workflow/verification script
or explicit workflow dispatch. It records the exact checkout, SDK, hardware/resource
limits, APFS home/test volumes and test output, including failures. The verification
script refuses non-macOS hosts and requires exactly two passed, zero failed/ignored
tests; a zero-test filtered run supplies no qualification. Local invocation is
`bash scripts/internal/validation/verify_recording_native_custody.sh`.

A passing run verifies the retained pre-start recording route and slot only. It
does not qualify recording capture, required-WAL/effect permission, terminal seals,
publication, audible/UI workflows or release acceptance. A compilation or genuine
bootstrap/admission failure remains an implementation failure to resolve.
