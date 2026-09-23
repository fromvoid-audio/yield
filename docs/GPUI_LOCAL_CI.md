# GPUI local CI

The GPUI fork validates committed source on this machine. The old Windows
GitHub Actions workflow is preserved as `gpui-windows.yml.disabled` for its
command inventory, but it is no longer an automatic gate.

Run `bash scripts/gpui-local-ci.sh full` from any directory for the complete
local GPUI lane, or `bash scripts/gpui-local-ci.sh storage` for storage-focused
changes. Both modes check formatting and use the pinned lockfile. The full
mode checks the GPUI app and runs the library, journal, core, platform, and UI
tests previously listed in the Windows workflow. The storage mode checks the
storage library and its focused edit tests. These commands run on the current
host; a macOS pass does not establish Windows acceptance.

On a Windows host, run `powershell -ExecutionPolicy Bypass -File
scripts/gpui-local-ci.ps1 full` (or `storage`). That script preserves the
Windows workflow's ASIO provisioning and check list. It has not been executed
on this macOS machine; Windows acceptance requires a real Windows run.

The session library tests in current `main` time out waiting for session and
preference events on this macOS host, including when run individually. The
automatic macOS lane compiles `wavecrate-session` but marks its tests as a
visible exception. Run `bash scripts/gpui-local-ci.sh session` to reproduce
and repair that suite (OPT-1861). On other hosts, the full and storage lanes run the
session tests. A macOS pass therefore excludes session behavior until this
exception is fixed.

Install the macOS/Linux automatic trigger once with
`bash scripts/install-gpui-local-ci.sh`.
It installs a `post-commit` hook in this repository's shared Git directory,
so commits from its worktrees start validation without blocking the commit.
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
not publish artifacts, merge code, or change branch state. Windows checks
require a Windows host running the same GPUI sources; the macOS runner must not
be described as Windows validation.
