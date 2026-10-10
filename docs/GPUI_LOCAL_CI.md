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
lanes run that suite serially: each session test starts several
SQLite-backed owners, and the host's default test parallelism can exhaust its
existing five-second event deadlines before the owners finish initializing. This limit
does not change those deadlines or the tests' assertions. Run the lane on a
macOS host with enough free space for Cargo's shared target and temporary test
profiles; inspect the exact run log and free space if startup times out.

The journal suite runs with four test threads. Its real SQLite and custody
fixtures include an unchanged 400 ms preparation deadline; unconstrained
host-sized concurrency can expire that deadline before readiness is observed.
This bounds simultaneous fixture owners without changing production deadlines,
assertions, test selection, or required pass counts.

Install the macOS automatic trigger once with
`bash scripts/install-gpui-local-ci.sh`.
It installs a `post-commit` hook in this repository's shared Git directory,
so commits from its worktrees run validation before the Git command returns.
The hook runs the **exact committed SHA** in a temporary detached worktree,
not the dirty working tree. Runs are serialized against a shared Cargo target
with an OS file lock. The temporary checkout is removed afterward.

For a commit SHA, inspect:

```text
<git-common-dir>/local-ci/runs/<SHA>/<run-id>/result
<git-common-dir>/local-ci/runs/<SHA>/<run-id>/check.log
```

`result` is `RUNNING`, `PASS`, or `FAIL` with a UTC timestamp. `PASS` is scoped
to the recorded SHA and host. A failed check keeps its log and can be rerun
with `bash scripts/internal/ci/run_gpui_commit_ci.sh "$(pwd)" <SHA>`; each invocation records a separate run. The hook cannot retroactively validate earlier commits. To
update an installed runner after changing these scripts, rerun the installer.

The trigger requires Python 3.11 or newer and Rust/Cargo; it uses Python
`fcntl.flock` and does not require a separate lockf/flock executable. It does
not publish artifacts, merge code, or change branch state. An installed runner
is a copy; rerun the installer after updating the scripts. Its result is macOS
evidence only.

For a private bundle from an exact validated commit, follow
[GPUI macOS packaging](GPUI_MACOS_PACKAGING.md).

The real-packer payload tests exercise stable, RC and nightly archives across all
three declared targets. The macOS release ZIP uses `ditto --norsrc` so its regular
members match the authenticated manifest exactly: host resource forks, extended
attributes and ACLs are not separate payload files. Bundle file bytes and Unix
modes, including the code-signature seal files, remain in the archive. Verify
the extracted app's signature after packaging. Ad-hoc signature roundtrips do
not qualify preservation of a notarized Yield build's stapled ticket; that
requires a genuinely ticketed artifact and before/after `stapler validate`.

## Focused original recording custody verification

The old `Native recording custody` hosted workflow is retained with a `.disabled`
suffix. Native validation runs locally through `scripts/yield-local-ci`, using the
repository toolchain and genuine per-user registry. Each selected production case
gets its own reversible clean-and-restore window. Existing state is parked by
same-filesystem rename and restored afterward, including dirty ownership evidence.
The runner records the exact checkout, SDK, APFS resources and test output and
requires the original nonzero pass count with zero failures or ignored cases.
See [Scripts](../scripts/README.md) for local commands and recovery instructions.

A passing run verifies the retained pre-start recording route and slot only. It
does not qualify recording capture, required-WAL/effect permission, terminal seals,
publication, audible/UI workflows or release acceptance. A compilation or genuine
bootstrap/admission failure remains an implementation failure to resolve.

The lane also exercises the original recording initial transaction. Its funded
control reservation and committed readback compare the already prepaid frozen
encoding against the borrowed original SQLite BLOB, with the original journal
family, cell type, byte bound, positive sequence and cross-relation sequence
uniqueness checks. It avoids decoding a second full graph of a just-inserted
record. Historical reads still use their complete decoder and classifier. Pool
ceilings and physical admission requirements are unchanged. Portable SQL tests
exercise exhausted-pool comparison, changed bytes and corrupt sequence/cell
refusal; those tests do not supply native qualification.

Native recording prestart custody runs its two original acceptance cases separately
at the same committed head, with `--exact` and exactly one pass per case. The local
runner restores the original state between cases. Prestart leaves an active
registration and durable admission claims; worker join releases memory custody but
cannot prove clean volume handoff. The clean test state, including its dirty marker,
is retained when the original state is restored. This uses the genuine account and
canonical state location, without a fixture registry or substituted home directory.
It supplies no capture, terminal or settlement authority.
