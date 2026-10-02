# GPUI update-check presentation (OPT-1775)

General Settings now renders the update status and a Check for Updates control.
`yield_ui::update_check::UpdateCheckService` is an optional, read-only app-global
port. A configured checker runs on GPUI's background executor. The ordinary app
does not register a production checker yet: it displays **Update checking is
unavailable for this app**, with the control disabled. It never labels that state
current, offline, downloaded, verified, or installed.

The renderer distinguishes checking, current release, newer release,
offline, failure, mismatched metadata, and unavailable. Offline/failure support
another explicit check. Newer release copy states that automatic installation is
unavailable. Candidate channel and target must equal the requested context; an
older version is rejected. Current additionally requires the exact installed
build number and full commit identity; a semantic version alone is insufficient.
A different commit with a strictly greater build number can be newer even when
the version is unchanged. Conflicting or unknown build identity is rejected.
All candidate values are finite typed enums/integers, with no raw
server error or URL rendered.

There is one request slot per browser. Repeated activation does not create a
second worker. Closing settings sets the cancellation flag and suppresses the
result; the slot remains occupied until the existing worker returns. Reopening
cannot start overlapping I/O. Generation, cancellation, configured checker
instance, channel, target, and installed build identity fence late results and retained terminal display.
Provider panics become failure; a provider must bound its own I/O and honor the
cancellation flag. This adapter does not forcibly terminate a blocked provider or
implement transport timeouts. There is no event queue or background polling.

## Portable verification

From the repository root, with the configured Rust/native build environment:

```sh
cargo test --locked -p yield-ui --lib update_check
cargo test --locked -p yield-ui --lib
bash scripts/gpui-development-checks.sh preflight
python3 scripts/internal/check/check_gpui_action_register.py --check
git diff --check
```

The controlled checker fixtures execute the actual settings worker and renderer
for current/newer/offline/failure, repeated activation, close/reopen, and checker
replacement. Reducer fixtures additionally cover channel/target/older-version
rejection, same-version build changes and unknown/conflicting identity, terminal duplicates, late generations,
cancellation, and old terminal display after service replacement. They establish
portable renderer/adapter behavior, not live update service or native acceptance.

## Remaining acceptance

[OPT-2104](https://linear.app/boostnlvp/issue/OPT-2104) owns the missing actual
bounded release metadata transport, installed build identity, and app bootstrap
registration. That is portable implementation work, not a platform blocker.
[OPT-1776](https://linear.app/boostnlvp/issue/OPT-1776) owns download/staged payload
integrity. [OPT-1866](https://linear.app/boostnlvp/issue/OPT-1866) requires an actual
macOS runtime and controlled verified private artifact for visual checking,
installation boundaries, and relaunch. Its fixture must verify settings at normal
and compact/enlarged sizes, default unconfigured status, current/newer/offline/error
from the real service after OPT-2104, close/retry, and installed-app preservation.
Those checks cannot be established on this Linux host. Tracking parent OPT-1498
remains open; these renderer fixtures do not complete a production updater.
