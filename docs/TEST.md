# Development Workflow and Test Map

This document is the canonical developer workflow guide for local iteration,
validation, and test selection.

## Default validation ladder

Use the lightest lane that still gives trustworthy coverage for the change.

1. Tight edit loop
   - Windows PowerShell:
     `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1 smoke`
   - macOS/Linux/WSL:
     `bash scripts/ci.sh smoke`
2. Agent-safe validation
   - Windows PowerShell:
     `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1 agent`
   - macOS/Linux/WSL:
     `bash scripts/ci.sh agent`
   - load-sensitive selection-export background-job tests run serially here
     after the default library test pass.
3. Broader integrated local checks
   - Windows PowerShell:
     `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1 quick`
   - macOS/Linux/WSL:
     `bash scripts/ci.sh quick`
4. Full local CI parity
   - Windows PowerShell:
     `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1 local`
   - macOS/Linux/WSL:
     `bash scripts/ci.sh local`

### Opt-in parallel-isolation stress lane

Use the normal `agent` lane for routine compile and broad library validation.
Use the parallel-isolation stress lane after changing test runtime guards,
process environment/current-directory handling, mutable global test controls,
worker cleanup, or when investigating a failure that disappears on retry:

- Windows PowerShell:
  `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1 isolation-stress -Iterations 5 -TestThreads 8`
- macOS/Linux/WSL:
  `bash scripts/ci.sh isolation-stress --iterations 5 --test-threads 8`

This lane is deliberately not part of `scripts/ci.* agent`. It compiles the
Yield library harness once, proves that deliberately injected
process-state and mutable-global-control leaks are detected, and then starts a
fresh test-binary process for every bounded iteration. Each process uses the
requested explicit libtest parallelism and a 15-minute default timeout. The
lane stops at the first unexpected failure instead of retrying until green.

Every process writes one compact JSONL record containing the test binary,
iteration, parallelism, status, test name, failure class, and disposition. The
metadata record separately identifies known quarantined coverage and its
exclusion reason. Reports default to a timestamped file under
`target/test-isolation-stress/`; pass `--output` on Bash or `-Output` on
PowerShell to select a stable artifact path.

Focused runner coverage, including report parsing, fresh-process invocation,
timeouts, exit behavior, and injected-leak classification, is available with:

`python3 -m unittest scripts/internal/ci/test_parallel_isolation_stress.py`

## Agent preflight and Git hooks

`scripts/agent.sh preflight` / `scripts/agent.sh request` and their PowerShell
equivalents own the current portable GPUI preflight, including workspace format,
locked application build and current source/owner/script guards. Concurrent invocations
coalesce only for the same unchanged worktree/input image; different worktrees or
changed inputs cannot reuse the result. `request` defaults to preflight only; explicit
`--quick-ci` adds portable regression tests and `--full-ci` requests native macOS CI. Branch-changing `post-checkout`
hooks deliberately run only `run_agent_hook_checks.sh`: bounded
repository-state checks that preserve reviewed-head validation evidence during
merge cleanup.

On macOS, the supported Bash `scripts/agent.sh` and `scripts/ci.sh` entrypoints
use a Cargo target keyed by the Rust host/release and `Cargo.lock`. This keeps
validation independent from the unbounded general-purpose `target/debug/deps`
directory while preserving warm reuse across repeated runs. These entrypoints
override an inherited `CARGO_TARGET_DIR`; use `YIELD_VALIDATION_TARGET_ROOT`
to customize validation cache placement. A target whose
`debug/deps` directory itself becomes pathological is atomically quarantined
before Cargo starts; the emitted path can be removed after any needed
diagnostics have been retained. A per-target lease serializes supported macOS
validation commands so quarantine cannot race an active Cargo user; stale
leases are recovered when their recorded PID and process-start identity no
longer identify a live, non-zombie owner. Waiting for a genuinely active owner
is bounded and exits 124 without disturbing that owner's processes or lease.

Those entrypoints also own a process-group watchdog. A changing owned process
tree or increasing aggregate CPU time counts as progress, so a quiet but active
clean compile is not a stall. After five minutes with neither signal, the
watchdog records the exact command, Cargo/Rust/macOS versions, owned process
tree, and macOS samples under `target/validation-diagnostics/`, with one
30-second budget across all collection. It then allows a full two minutes for
recovery before exiting 124 and terminating only the process group it created.
With the default five-second polling and ten-second termination grace, the
maximum scheduled bound is 7 minutes 50 seconds after last observed progress.
Cancellation uses the same owned cleanup path.

Run `bash scripts/internal/agent/test_agent_preflight_coordination.sh` to
exercise hook installation, checkout ownership, concurrent coalescing,
and stale-lock recovery without invoking Cargo.
Run `bash scripts/internal/validation/test_validation_watchdog.sh` to exercise
progress classification, diagnostics, cancellation/stall cleanup, unrelated
process isolation, and pathological-target rotation.
The fixture also covers reused-PID stale-lease recovery and bounded waits for a
genuinely live lease owner.
It also emulates several unresponsive compiler samples to prove the global
diagnostic budget and post-collection recovery interval.
Pre-spawn cancellation coverage proves a signal cannot start and detach a new
validation process group after cancellation has already been recorded.
Nested wrapper calls bypass duplicate supervision only after verifying the
enclosing watchdog's live PID/start identity, lease record, command, and process
ancestry. An inherited recursion marker without that proof starts a fresh
watchdog normally.

## Local macOS CI and reversible state

Implemented for Pulse Yield 0.1 ticket 79, the commands below are available in
`scripts/yield-local-ci`. Passing safety checks does not establish packaged build,
manual browsing, audible playback or release acceptance; those require their own
recorded validation.
One Mac and one macOS account are sufficient. An OS account is a filesystem and
ownership boundary here; Yield does not need a product account or login system.

### Build channel and state mode

Build channel and launch state are independent choices:

| Channel | Build | Normal launch state | Local signing |
| --- | --- | --- | --- |
| `test` | Debug build with the fixed development-state feature | `.yield-development` | Ad hoc |
| `beta` | Optimized `release-local`, production persistence behavior | Existing `.yield` | Ad hoc; separately requested Developer ID distribution signing |
| `release` | Optimized `release-local`, production persistence behavior | Existing `.yield` | Ad hoc; separately requested Developer ID distribution signing |

The roots are under the effective macOS user's canonical
`Library/Application Support` directory. Resolve that home through the OS user
database, as the production registry already does, rather than trusting `HOME`.
The development feature must select one fixed root throughout profile resolution,
logs, caches and the canonical per-user volume registry. A profile or
`YIELD_CONFIG_HOME` override alone does not isolate that registry. There is no
arbitrary runtime override for a production registry. Beta and release share the
normal root deliberately: changing the build channel does not discard the user's
library. Development and production may run alongside each other only when their
external Source roots and live ownership allow it; separate app roots do not
permit two writers to the same Source database.

`normal` never resets state. `clean` is an explicit transaction on the selected
namespace, followed by launch with ordinary persistence and ownership rules.
Production qualification uses the production namespace; development results
cannot satisfy a genuine production-registry gate. Build labels do not grant
release acceptance or publishing permission. Stable publication remains disabled
under the existing release policy.

### Operator interface

The single entrypoint is `scripts/yield-local-ci` with these operations:

```text
doctor
check --sha <commit> --lane preflight|full|native --case <native-case>
build --sha <commit> --channel test|beta|release
run --artifact <manifest> --state normal|clean
state status --namespace development|production
state clean --namespace development|production
state restore --backup <transaction-id>
state recover --transaction <transaction-id>
```

Managed local-CI entrypoints resolve `TMPDIR` strictly and export its physical
existing directory before creating temporary files, so macOS `/var` aliases do
not create multiple apparent roots for one temporary volume. Each channel build
creates a durable unique run record before validation or Cargo starts; its
complete command output and `result.json` are retained under
`.git/local-ci/runs/<source-sha>/build-<channel>/<run-id>/`. Failed or cancelled
runs remain there for diagnosis and do not publish an accepted artifact
manifest.

Native `--case` accepts one of the exact recording selectors below,
`prerequisite:<name>` for the six named cases in the retired native-prerequisite
runner (`profile-generations`, `source-release-policy`, `startup-production`,
`startup-control-failure`, `startup-namespace-failure`, or
`sdk-and-startup-components`), `catalog-replay` for the existing macOS catalog
replay runner, or `audition-coreaudio` for the two named real-output CoreAudio
runtime checks. These routes are closed allowlists; each required frozen runner
must exist in the requested source commit. Multi-command validators keep a
shared read lease for their whole process tree, and their generated evidence is
copied into that run's retained evidence directory. The audition route requires
the caller to set `YIELD_TEST_AUDIO_OUTPUT=1` before invoking the command; the
CLI refuses before clean-state preparation if the inherited opt-in is absent.
The runner checks the opt-in and macOS platform again, records device/toolchain
and writable-APFS evidence, and requires each exact ignored selector to report
exactly one pass with no failures or ignored tests. Its callback probe verifies
the selected digital output route and callback behavior; it does not establish
analog output, audible quality, or blanket hardware compatibility.

The full GPUI lane and `catalog-replay` prepare the native metadata source under
the shared Git common directory before the first Cargo command. The content-keyed
fixture manifest and source path are retained in the local-CI run record; an
existing fixture is verified by exact tree, owner, mode, hash, and device/inode
identity and is never silently repaired. The catalog test requires
`YIELD_NATIVE_METADATA_FIXTURE_ROOT`, uses a disposable catalog profile, and
restores the fixture's original mtime after watcher shutdown.

The `prerequisite:startup-namespace-failure` compatibility case runs both the
original blocking-receive namespace failure selector and its poll-based twin.
Each selector gets its own clean production reset, read lease, runner, and
restore window; the second still runs if the first fails. The shell runner's
legacy `startup-namespace-failure` case refuses direct execution so it cannot
report partial coverage. Use the managed CLI aggregate or either explicit
`prerequisite:startup-namespace-receive-failure` /
`prerequisite:startup-namespace-poll-failure` subcase.

The full local lane filters all eight genuine production-registry selectors out
of the ordinary `operation_journal` suite, then runs each under its own clean
and restore transaction using `run_native_production_case.py`: `capacity`,
`control`, `namespace`, `namespace-poll`, `recording-slot-currentness`,
`recording-lost-reply`, `chunk-issuance`, and `provider-reconciliation`. The
runner requires evidence that exactly one requested test passed in each
transaction.

`build` does not reset state or launch the application. `run --state clean`
prints and records its restore transaction ID before launching. Restoration is
explicit after the app exits; do not silently restore over an active clean
session. Production reset is always explicitly selected, never inferred from a
default development command. No recursive-delete, force-unlock or marker-editing
option belongs in this interface.

Run the opt-in CoreAudio checks explicitly with
`YIELD_TEST_AUDIO_OUTPUT=1 scripts/yield-local-ci check --sha <full-commit-sha> --lane native --case audition-coreaudio`.
The local runner requires macOS and writable APFS, retains toolchain/device
inventory and both exact-selector transcripts in the CI run evidence, and
restores the prior production state on success or failure. This is evidence of
the tested callback's digital routing and native stream behavior only; it does
not prove analog output or that a person heard acceptable audio.

### Reversible clean transaction

#### Intentional development reset after a missing profile

If startup reports `session persistence unavailable` with an
`ambiguous legacy dirty volume marker`, a retained ownership marker belongs to a
different profile identity. Deleting only the profile leaves this evidence
behind; startup must refuse to claim that the old work was reconciled. Keep the
original state if you need to recover its session or journal.

For an intentional development reset, quit every Yield version, then run:

```sh
scripts/yield-local-ci state reset --namespace production
```

This parks the **entire** `.yield` root in a recoverable backup, including the
volume registry, journals and logs, and prints its transaction ID. Launch the
beta or release normally afterward. It creates fresh state; subsequent normal
launches preserve that state. Use `development` instead of `production` for test
builds. No media or source-side metadata is reset. To return to the old state,
quit Yield and use `state restore --backup <transaction-id>`; that also restores
any old unresolved startup fault. Keep the backup for recovery, and never clear
just the dirty marker or treat this reset as journal reconciliation.

For regression qualification, verify that adding a disposable source and changing
volume/loop settings persists across normal quit/reopen after reset. Test the
refusal separately: a foreign dirty marker must remain unchanged and persistence
must report its underlying cause. Physical disconnect/resume is a separate lane.

Put maintenance locks and transaction records outside the managed root. A clean
operation takes an exclusive namespace maintenance lock before inspecting state.
Every updated app takes a shared lock before logging or any app-root access and
retains it until all workers and database owners have stopped. Writable profile
and canonical registry admission must participate too, including native helpers
that bypass the launcher. This is a maintenance fence, not a substitute for
profile, Source or volume authority. It cannot fabricate reserve evidence or
perform unaccounted filesystem effects inside a prepaid bootstrap path.

Before any rename, verify each path component without following symlinks, current
user ownership, regular maintenance-lock identity, root identity and same-device
backup destination. Refuse live app/helper owners, active profile/volume locks,
foreign or malformed entries and unresolved prior transactions. Older binaries
do not know the new maintenance protocol: refuse reset while they are running;
do not promise race-free reset against an unmanaged old binary launched during
maintenance. Require all running versions to participate before enabling the
automatic clean-and-launch operation. Keep the lock inode permanently in place.

Under the exclusive lock:

1. Create and flush a versioned transaction record in a same-filesystem,
   user-owned backup namespace. Record intended paths, prior root identity,
   selected namespace and source/artifact identity without logging personal DB
   contents.
2. Atomically rename the entire selected root into a new backup transaction.
   Never copy-and-delete it: profile, Source and volume custody can depend on
   retained filesystem identities. Keep journals, WAL/SHM, dirty markers, logs
   and generations unchanged. Flush the containing directories and record the
   completed transition.
3. Leave the canonical root absent. A normal application startup creates its
   fresh state through the ordinary production code path. Hand maintenance
   ownership to the launched app without a reset/admission race.
4. Restore only while quiescent: first park the clean session as another retained
   transaction, then rename the exact original root back to its canonical path.
   Flush and record both transitions. Never overwrite either session.

A crash between transitions must leave both sessions recoverable. `recover`
uses recorded identities and observed directory entries to resume or roll back;
ambiguous identities fail closed with the retained transaction ID. No automatic
backup expiration or garbage collection. Report `already vanilla` without
manufacturing an original backup when there was no root. Lack of disk space,
EXDEV, denied access or a live lock leaves the original session intact.

These operations manage app state only. Source databases and sidecar leases can
live alongside media outside the app root. New-user tests use separately created
disposable media, including their Source metadata; they must not wipe metadata
beside the user's real media. A clean app root is not a clean media library.
The original historical recovery evidence remains in the parked backup and is
restored as evidence, not rewritten into a clean authority claim.

### CI execution and artifacts

Reuse `scripts/agent.sh request` for canonical preflight,
`scripts/gpui-local-ci.sh full` for the broad native suite, the existing explicit
native PR runners for their exact selectors/counts, and
`scripts/package-gpui-macos.py` for clean-commit bundle construction. Extend those
entrypoints where needed rather than duplicating their assertions. The existing
post-commit runner and new entrypoint must share one coordinator across Git
worktrees. Agree on the same OS lock primitive; `flock` and POSIX record locks are
not interchangeable. Serialize Cargo targets and default production-registry
tests. A clean-state test holds the production resource reservation across its
backup, test and restore sequence. Avoid recursively taking the same lock from
nested wrappers; ownership handoff must be verified rather than trusting an
environment flag.

The managed CLI pins each build or acceptance run to the requested exact commit
in a temporary `codex/local-ci-<id>` feature worktree. This preserves the
repository's integration-branch guard while keeping the caller's branch and
uncommitted files untouched. The managed CLI removes only that worktree and its
own temporary branch after the lane, including on failure. Uncommitted
experiments may have a separate explicitly marked preview artifact, never
release evidence.
Cache targets by host architecture, Rust version, lockfile, channel/features and
profile. Do not reuse a test-feature binary for a production artifact. Record
source SHA, dirty/snapshot status, OS build, SDK/CLT, Rust 1.93.0, architecture,
native filesystem qualification, every command and exit status, selector counts,
channel, compiled namespace, package version and binary/bundle checksums in a
versioned manifest. Retain complete failed logs; a failed or interrupted lane
must never yield an accepted manifest. Native macOS arm64, macOS x86_64 and
Windows are distinct evidence. An ExFAT build cache does not qualify APFS tests.

Keep the existing bounded watchdog and process-tree cleanup. Cancel only the
processes owned by the run. `doctor` checks disk space, pinned toolchain, Python
support, CLT SDK, required assets and the selected lane's actual requirements;
it must not require full Xcode when a CLT/runtime-shader lane supports CLT.
Packaging first produces a local app. Signing/notarization and publication are
separate explicit stages with separate results. Finalize checksums only after
all bundle mutation, including signing and stapling. Reuse existing signing
tools without printing credentials or changing Keychain permissions.

### Required acceptance before enabling resets

- Normal relaunch retains existing state; test builds consistently isolate every
  app-root and per-user registry path; beta/release retain production paths.
- Native process tests prove shared app/helper admission versus exclusive reset,
  clone/worker lifetime, live-owner refusal and admission during maintenance.
- Clean/restore retains original root and lock identities and bytes, preserves
  dirty evidence and the displaced clean session, and touches no media.
- Fault injection at each transaction boundary proves recovery after process
  death, refusal of symlinks/foreign roots and cross-device destinations, and
  safe behavior under insufficient space or a missing original root.
- Frozen-source builds produce the right namespace and manifest for all three
  channels; concurrent post-commit/manual/native runs serialize; cancellation
  retains a failed result and recoverable state.
- Run production startup success/control-failure/namespace-failure gates with
  the real account and registry, restoring the prior session afterward. Preserve
  original assertions and required counts. Test-mode success is separate.
- Manual GPUI/native-host acceptance remains explicit. A successful local build
  or unit suite is not audible, responsiveness, Windows or release acceptance.

## Validation and release lane contract

Yield uses local CI only. Active GitHub Actions workflows are retired; their
`.yml.disabled` files are historical documentation and do not execute. Use the
local commands below for development, native acceptance, and release-risk
validation. Release scripts and the existing stable-publication fail-closed
policy remain available; remote workflow dispatch is outside the CI contract.

### Release lifecycle and promotion policy

Yield develops continuously on `main` through the normal PR pipeline. The
first RC for a version is a project decision to enter stabilization, not a
branch freeze or an automatic precursor to an immediate stable release.

1. Develop normally through PRs into `main`.
2. When the current product is ready for stabilization, prepare `release/X.Y`
   from `main` and publish `vX.Y.Z-rc.1`.
3. Continue normal PRs into `main`, limiting the release train to stability
   work: bug fixes, regression fixes, reliability, performance, packaging, and
   release blockers. Feature work waits until stabilization ends unless the
   release scope is explicitly changed.
4. When fixes have landed after an RC, move the release branch to the current
   `main` commit with the same target version and validate the next RC locally:

   ```bash
   scripts/internal/release/prepare_release_train.py \
     --version X.Y.Z \
     --source-ref main \
     --push
   scripts/internal/release/run_release_validation.sh
   scripts/release.sh rc \
     --version X.Y.Z \
     --rc-number N \
     --branch release/X.Y \
     --dry-run
   ```

5. Continue RC preparation while stable publication is temporarily disabled.
   Ensure every change intended for the train is validated from the intended
   source commit. The nightly, RC, and stable workflow files are preserved with
   `.disabled` extensions; no hosted build or publication runs. The local stable
   command still fails closed. Re-enabling stable publication requires a
   dedicated reviewed change; PR approval, `approved`, sign-off, or RC
   acceptance does not authorize that change or a stable publication.

Keep the remote `release/X.Y` branch as the release coordination ref. It is not
a disposable feature branch and must remain available for subsequent RCs and
the eventual explicit stable promotion.

| Lane | Local command | Policy |
| --- | --- | --- |
| Format and guardrails | `scripts/ci.* local`; use `scripts/check.* <name>` for focused reruns | Run locally before broad or risky changes |
| Clippy | `scripts/ci.* local` | Run locally before broad or risky changes |
| Docs | `scripts/ci.* local` | Run locally before broad or risky changes |
| Public mdBook docs | `scripts/check.* mdbook` | Run when changing `book.toml` or `docs/book/src/` |
| Tests | `scripts/ci.* local` runs `cargo nextest run --workspace --profile ci-required --all-targets --no-fail-fast` on the local platform | Run locally before broad or risky changes |
| Quarantined/slow tests | `cargo nextest run --workspace --profile ci-quarantine --all-targets --no-fail-fast` | Advisory until the tests are hardened |
| Dead dependency sweep and env-var nudge | `scripts/check.* dead-deps --advisory` and `scripts/check.* report-env-vars` | Advisory Linux-only hygiene |
| GUI semantic contracts | `scripts/gui.ps1 contract` or `scripts/gui.ps1 suite` | Local/manual or issue-specific |
| Perf guard | `scripts/perf.* guard` | Local/manual or release-risk validation |
| Native acceptance | `scripts/yield-local-ci check --sha <commit> --lane native --case <allowlisted-case>` | Run a named production/native validator on native APFS; preserve runner evidence and exact selector counts |
| Release validation | `scripts/internal/release/run_release_validation.sh` | Run locally against the exact intended source commit before release preparation; no hosted job or publication is triggered |

The retired `.github/workflows/recording-native-custody.yml.disabled` cases
map to these exact local selectors:

```text
operation_journal::owner::recording_binding::tests::recording_custody_native_production_original_slot_currentness_and_drain
operation_journal::owner::recording_binding::tests::recording_custody_native_production_lost_reply_keeps_original_slot_and_credit
operation_journal::owner::recording_binding::tests::chunk_issuance_native_production_original_claim_transition_and_retained_backing
```

Run each with `scripts/yield-local-ci check --lane native --case
<selector>` on native APFS. This is the former workflow's local acceptance
route; it does not dispatch a hosted job.

Use `scripts/release.sh` from a clean repo root for local macOS/Linux release
orchestration. It derives major, minor, and patch target versions from the
package version at the resolved source ref and delegates release-train prep to
`scripts/internal/release/prepare_release_train.py`. Use dry-run mode to review
the intended RC command and follow-up steps. Do not pass `--dispatch` in the
local-only workflow. When `origin` is a GitHub remote, it must match
`YIELD_GITHUB_REPO` before release refs are fetched or resolved. Stable commands
fail before resolving release refs or invoking GitHub.

The nextest policy is:

- `ci-required` is the merge-blocking test subset and excludes known timing,
  platform, or permission-sensitive tests while they are being hardened.
- `ci-quarantine` owns that excluded coverage. Run it deliberately when working
  on source-DB contention, browser async controller integration, or updater
  stale-removal behavior.
- The default nextest profile stays unfiltered for maintainers who want the
  complete suite and are prepared to triage environment-specific failures.
- Local nextest runs are the primary development evidence. Former nightly, RC,
  stable, and native-custody workflow sources remain as `.disabled`
  documentation. Run `scripts/internal/release/run_release_validation.sh`
  locally before release preparation; package builds, signing, upload, and
  publication remain separate explicit release operations.

The non-blocking app architecture is enforced by
`scripts/check.* non-blocking-architecture`, and that check is required by
`scripts/ci.* agent` plus the agent preflight inside `scripts/ci.* local`. It
runs Radiant's reusable non-blocking guardrails, Yield's app-facing blocking
scan, and the deterministic strict slow-handler diagnostics harness.
The agent lane intentionally runs the focused Radiant non-blocking guardrail
module instead of the full `generic_surface_guardrails` suite, because that
broader suite also contains unrelated cleanup/source-shape guardrails that
should be fixed deliberately rather than used as the non-blocking architecture
gate.

Windows note:

- use the PowerShell wrappers in this repository
- the wrappers use direct `rustc` by default and still fall back to repo-local
  temp space when the default temp directory is unusable
- set `YIELD_ENABLE_SCCACHE=1` only when you explicitly want wrapper caching
- do not run multiple cargo test commands concurrently

Headless Linux audio note:

- `scripts/internal/setup_headless_audio.sh` is retained as developer
  validation tooling for Bash CI, agent, and perf lanes that may run on
  headless Linux hosts
- `scripts/ci.sh local` and `scripts/perf.sh guard` source it to set
  `ALSA_CONFIG_PATH=scripts/internal/alsa_headless.conf` only when the host is
  Linux, no display server is present, and the caller has not already set
  `ALSA_CONFIG_PATH`
- this dummy ALSA path reduces CPAL/ALSA missing-device warning noise in
  test/bench logs and does not represent shipped Linux app support

## Safe feature-change checklist

Before push:

1. Route the change to the right owner using `docs/TARGET.md`.
2. Add or update tests for non-trivial logic.
3. Update the canonical doc that owns the changed behavior.
4. Run the appropriate validation lane until green.
5. Keep the diff focused and avoid broad incidental cleanup.

## Test suite map

### Root crate tests

Use for most app/domain behavior under `src/`.

- all project tests:
  - `cargo nextest run --all-targets --no-fail-fast`
  - required CI subset:
    `cargo nextest run --workspace --profile ci-required --all-targets --no-fail-fast`
  - quarantined/slow subset:
    `cargo nextest run --workspace --profile ci-quarantine --all-targets --no-fail-fast`
  - `cargo test --doc`
- quick app-development subset:
  - `cargo nextest run --profile quick --lib --tests`
  - `scripts/ci.* quick` first lists the selected binaries with the same profile
    and target set, so stale inventory-dependent filter predicates fail before
    the test run starts
- agent-safe library suite:
  - `cargo test -p yield --lib`
  - `scripts/ci.* agent` runs the library suite with two known legacy
    controller failures skipped so the required agent lane can stay focused on
    deterministic architecture, compile, and broad non-ignored library coverage:
    `prepare_auto_rename_requests_logs_looped_provenance` and
    `rating_previous_random_history_entry_restores_waveform_for_replacement`.

### Committed file-mutation and watcher reconciliation

Use these focused lanes when changing Yield-owned create, copy/extract,
import/drop, edit, normalize, undo/redo, rename, move, trash/delete, source
manifest reconciliation, or watcher echo handling:

- mutation contract, source revision, identity, invalidation, partial failure,
  stale completion, and large-file background behavior:
  `cargo test -p yield --bin yield committed_file_mutations`
- watcher acknowledgement, deduplication, and external-change fallback:
  `cargo test -p yield --bin yield source_watcher`
- strict compile coverage for message routing and every native call site:
  `cargo check -p yield --all-targets`

The mutation tests should prove that path-only moves retain content identity,
content changes invalidate file-derived readiness, deletes invalidate source
membership, and one operation ID covers committed and failed parts. The watcher
tests should prove that an acknowledged internal echo is removed without
starting duplicate work and that later external changes still reach the
authoritative reconciliation path.

### Source-processing liveness, churn, and performance

Use this explicit lane when changing source readiness, the background supervisor,
watcher-to-manifest reconciliation, committed mutations, retry/lease recovery,
source removal/re-add, playback scheduling, or exact similarity publication.
The end-to-end tests are ignored by the default suite so normal CI does not
inherit a real-time soak:

- deterministic liveness oracle:
  `cargo test -p yield --lib liveness_oracle_rejects_actionable_deficits_without_observable_runtime_work`
- real temporary source through add, targeted and overflow reconciliation,
  same-size content change, rename, delete, playback pause, committed internal
  mutation, closed-app restart audit, root disappearance/reappearance, and
  source remove/re-add:
  `cargo test -p yield --lib source_processing_liveness_harness_converges_restart_churn_and_root_recovery -- --ignored --nocapture`
- 10,000-file / 30,001-target calibrated source-processing profile:
  `cargo test -p yield --lib profile_source_processing_churn_under_playback_and_browser_priority -- --ignored --nocapture`
- fully analyzed 10,000-file periodic-sweep and one-file readiness-delta profile:
  `cargo test -p yield --lib profile_revision_gated_readiness_sweeps_10k -- --ignored --nocapture`
- full mutation and watcher family coverage:
  `cargo test -p yield --lib committed_file_mutations`
  and
  `cargo test -p yield --lib source_watcher`
- supervisor and durable-readiness failure injection (busy/resource contention,
  retry deadlines, expired leases, cancellation, worker failures, unsupported
  inputs, and stale completions):
  `cargo test -p yield --lib native_app::source_processing::supervisor::tests`
  and
  `cargo test -p yield-library sample_sources::readiness::tests`
- seeded, replayable source lifecycle and scan-order state machine (six retained
  regression seeds in the bounded normal-CI scanner lane):
  `cargo test -p yield --lib source_processing_seeded_state_machine_normal_ci`
- integrated real-supervisor replay for the calibrated lifecycle seed:
  `cargo test -p yield --lib source_processing_seeded_state_machine_integrated_supervisor -- --ignored --nocapture`
- extended 1,000-sequence state-machine stress lane:
  `cargo test -p yield --lib source_processing_seeded_state_machine_stress_1000 -- --ignored --nocapture`
- replay a decimal seed or the minimized JSON artifact path printed by a
  failure:
  `YIELD_SOURCE_STATE_MACHINE_REPLAY=<seed-or-artifact> cargo test -p yield --lib source_processing_seeded_state_machine_replay -- --ignored --nocapture`
- UI frame and input budgets while exercising browser interactions:
  `bash scripts/perf.sh guard` on macOS/Linux/WSL or
  `powershell -ExecutionPolicy Bypass -File scripts/perf.ps1 guard` on Windows

Run the deterministic liveness test repeatedly when changing timing or wakeup
behavior. Its oracle reads committed manifest/readiness generations and
supervisor state directly; it does not infer completion from UI progress or
sleep for a fixed success delay. An active source with actionable deficits must
be dirty/scheduled, queued, in flight, resource-paused, waiting for a retry, or
waiting for an exact prerequisite. A stable state outside those categories
fails as silently idle.

The seeded state-machine lane runs only against the versioned
`small-multi-source` native fixture. It generates filesystem mutations,
coalesced watcher/audit/foreground causes, focus changes, cancellation,
restart, source removal/re-add, root loss/replacement, and deterministic
transaction/publication/watcher/hash/lifecycle failure boundaries. Every
observable commit checks the reference manifest and browser projection,
monotonic accepted revisions, at-most-once revision/cause publication, bounded
cause coalescing, and controlled-quiescence liveness. The integrated lane
submits duplicate deltas through the real supervisor queue and observes the
durable readiness publication/fallback output. Failures are greedily
minimized without removing lifecycle transitions and written below
`target/source-state-machine-failures/` (or the active `CARGO_TARGET_DIR`);
artifacts preserve scanner-only versus integrated execution mode and reject
fixture-version drift during replay.

Timeouts and invariant failures emit a JSON snapshot containing source and
readiness generations, availability, activity, per-stage deficits and
classifications, durable job status/lease/retry state, active runtime work,
watcher stimulus/root health, and supervisor resource counters. The calibrated
profile enforces these source-processing budgets in debug test builds:

- materialize 30,001 exact targets from 10,000 files within 180 seconds;
- sustain at least 200 materialized candidates per second;
- keep browser-priority update p99 at or below 10 ms while playback pauses
  processing;
- keep process-memory growth at or below 512 MiB; and
- keep average process CPU at or below two core-equivalents and process disk
  reads/writes at or below 1 GiB each for candidate materialization; and
- record zero supervisor database-contention events.

The calibrated source-processing profile prints wall time, completion
throughput, queue-priority latency, CPU time/core-equivalent, memory growth,
disk I/O, and database-contention events. The fully analyzed readiness profile
prints ten consecutive converged safety-probe costs and one-file
readiness-delta costs as JSON. Its steady-state and one-file sections include
wall time, CPU time, live heap growth, process disk reads/writes, and target-row
writes so their cost can be compared directly with an exact pre-change run.
The standalone Radiant perf guard for those historical frame/input budgets is
retained coverage and is not the active Yield/GPUI browser-performance gate.
For the bounded 0.1 browser CPU
slice, use [`GPUI_BROWSER_PERFORMANCE.md`](GPUI_BROWSER_PERFORMANCE.md) and
`scripts/perf.sh browser-vm`; it checks production view-model projection,
sorting and collection counts on a deterministic 20k catalog. It does not
measure GPUI frames or set a native display/input-latency threshold.

The macOS native catalog replay lane also checks metadata-only watcher delivery
against the persistent `.DS_Store` fixture. A live bootstrap capture spans the
fixture's inode-mtime mutation and must produce a certain, exact-path,
source/root/stream-bound metadata-only batch before a separate bounded
historical replay capture is considered. Neither a settled cursor nor history
completion by itself qualifies the event. The catalog phase then verifies the
durable checkpoint advances while manifest revision, audio index, and WAV
projection remain unchanged. The fixture restores its original mtime after
service shutdown.

The native watcher-restart case accepts the ordinary owner-backed bounded replay
publication for a known post-restart WAV change, with its exact prior revision,
source/root/lifecycle receipt, one-path delta, and durable readback. If the
capture instead retains uncertainty, only one authoritative audit containing
both the pre-restart and post-restart files is accepted. Both routes retain the
same mutation deadline and duplicate-publication quiet window.

### Script and golden checks

Use for tooling, fixtures, and numerical-reference flows.

- `bash scripts/check.sh golden-tests`
- `bash scripts/check.sh mdbook`
- `cargo nextest run golden_log_mel_matches_python`
- `cargo nextest run golden_embedding_matches_python`

### Public mdBook documentation

Use this lane when changing `book.toml` or files under `docs/book/src/`.

- build:
  - `bash scripts/check.sh mdbook`
  - or `mdbook build`
- preview:
  - `mdbook serve --hostname 127.0.0.1 --port 3011`
- generated local output:
  - `target/mdbook/yield-docs/`
- public publishing target:
  - `https://portalsurfer.org/yield/docs/`

### Yield UI projection compatibility tests

Use for Yield-owned projection snapshot and behavior changes. Default GUI
work should prefer focused tests around `src/native_app.rs` and the relevant
Radiant public API surface.

- targeted snapshot suites:
  - `startup_shot_matches_fixture`
  - `browser_dense_shot_matches_fixture`
  - `waveform_selection_shot_matches_fixture`
  - update fixtures with
    `cargo test -p yield --lib update_shot_fixtures -- --ignored`

### Sample navigation responsiveness

Use this focused harness when changing sample selection, deferred loading,
waveform cache promotion, decode scheduling, or stale sample-load completion
handling:

- `cargo test -p yield rapid_navigation_harness --no-default-features`

The harness exercises rapid keyboard navigation while sample-load business work
is queued, verifies visible selection updates before completions, checks stale
deferred/sample-load completions cannot overwrite the current selection, and
asserts Radiant runtime diagnostics report no slow UI update handlers.

Manual cold-cache smoke for large folders:

1. Run Yield against a source folder with many uncached audio files.
2. Navigate quickly through the sample browser with keyboard arrows and mouse
   selection while cache/decode work is still cold.
3. Confirm row selection and status feedback move immediately.
4. Confirm late loads from earlier selections do not steal playback, selection,
   or visible error state from the current row.

### Radiant compatibility tests

Use for generic Radiant behavior and compatibility coverage in the standalone
sibling runtime dependency. Provision it first with `scripts/radiant.sh
provision` when starting from a fresh clone.

- `cargo nextest run --manifest-path ../radiant/Cargo.toml`

### GUI test platform

Use for semantic GUI contracts and CLI scenarios.

- Automated GUI runs default to the isolated `isolated-startup` fixture, which
  exercises the production `NativeAppState`/Radiant composition against a
  temporary config base using the dedicated `automated-tests` persistence
  profile.
- Product-native source-system runs may select `empty`, `small-multi-source`,
  or `large-source`. `small-multi-source` is the routine source scanning,
  readiness, watcher, progress, source-switching, and visual-QA baseline;
  `large-source` is opt-in for performance and high-cardinality coverage only.
- Provision and launch the routine native fixture with
  `bash scripts/run.sh sandbox --name source-qa --fixture small-multi-source`
  or `powershell -ExecutionPolicy Bypass -File scripts/run.ps1 sandbox -Name source-qa -Fixture small-multi-source`.
  Provisioning resets by default; pass `--fixture-preserve` or
  `-FixturePreserve` only when intentionally continuing the same mutation run.
- The shared `yield-fixture` tool emits
  `.yield/fixtures/<name>/fixture-manifest.json` with stable source ids,
  relative paths, hashes, file counts, and expected readiness-target counts.
  Its deterministic `create`, `same-size-change`, `move`, `delete`,
  `root-offline`, `root-online`, and `reset` scenarios replace ambient-folder
  mutations in source-system validation.
- Use the `live` GUI fixture only for deliberate manual validation against the
  real persisted startup profile. It uses the same native composition as the
  product executable and is the only GUI fixture allowed to resolve live state.
- Treat `default` only as a legacy alias for `isolated-startup`; it is not a
  separate live-profile mode.
- Named fixtures such as `browser`, `waveform`, and `map` remain explicitly
  legacy-controller compatibility coverage. They do not certify native startup
  or background-worker ownership.
- Native startup artifacts identify `fixture_runtime: native-app`, report native
  watcher/readiness-supervisor counts, report zero legacy analysis pools, and
  include the native shutdown artifact.

- Browser preview/commit flicker regressions:
  - targeted controller checks: `cargo test browser_focus_transition`
  - semantic GUI proof lane: `powershell -ExecutionPolicy Bypass -File scripts/gui.ps1 contract`
  - contract scenario anchor: `browser_focus_transition_stability`
- contract loop:
  - `powershell -ExecutionPolicy Bypass -File scripts/gui.ps1 contract`
  - this lane explicitly runs ignored fixture-backed GUI and bridge-runtime
    checks, including action parity and `contract_smoke_pack_runs_cleanly`; the
    default library test lane keeps only the cheaper runner and assertion
    checks.
- broader GUI suite:
  - `powershell -ExecutionPolicy Bypass -File scripts/gui.ps1 suite`

See `docs/TARGET.md` for the GUI artifact and automation contract details.

### Persistence profile boundary

Use these checks when the change could affect startup config loading/saving,
sample-source persistence, or GUI fixture profile selection.

- profile model:
  - live runs use `<config-base>/.yield/`
  - sandbox/manual QA runs use `<config-base>/.yield/profiles/sandbox/`
  - automated validation runs use `<config-base>/.yield/profiles/automated-tests/`
- regression anchors:
  - `cargo test app_core::controller::tests::persistence_boundary::`
  - `cargo test gui_test::`
  - `cargo test native_app::automation::tests::startup_capture_uses_native_workers_and_shutdown_hook`
- wrapper coverage:
  - `powershell -ExecutionPolicy Bypass -File scripts/gui.ps1 contract`
    includes the native-startup regression that snapshots live configuration
    bytes before and after an isolated native-runtime run, plus the retained
    controller `library.db` boundary checks
  - `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1 agent`
    still covers the same regression through `cargo test -p yield --lib`

### Windows manual confirmation

Use this after boundary/profile work when you want one explicit PowerShell proof
that validation did not leak fixture sources into the real startup profile.

1. Run `powershell -ExecutionPolicy Bypass -File scripts/ci.ps1 agent`.
2. Run `powershell -ExecutionPolicy Bypass -File scripts/gui.ps1 contract`.
3. Start one normal live runtime without `YIELD_CONFIG_HOME` or
   `YIELD_CONFIG_PROFILE` overrides:
   `cargo run --release`
4. Confirm the app opens with the intended real source list instead of temp or
   fixture paths.
5. If you need a sandbox/manual QA run instead, use
   `powershell -ExecutionPolicy Bypass -File scripts/run.ps1 sandbox --`
   so the session writes to the dedicated `sandbox` profile instead of the live
   profile.

### Windows ZIP Distribution Lifecycle

Windows releases are distributed as downloadable ZIP artifacts. A supported
Windows release package contains the `yield/` archive root, the
`yield/yield.exe` runtime binary, and `yield/update-manifest.json`.
Yield does not currently provide OS-managed app registration or automatic
system cleanup for the supported release path.

Windows users install Yield by extracting the ZIP to a folder they manage.
To update a manual install, close Yield, download a newer ZIP, and replace
the extracted `yield/` folder or its files with the newer release contents.
To remove Yield, delete the extracted folder. App settings, source metadata,
logs, and rebuildable caches live under the user's `.yield` app data paths
and can be removed separately when the user deliberately wants to clear local
Yield state.

Focused validation for Windows release distribution changes:

- `cargo test --test release_contract`
- `python3 -m py_compile scripts/internal/release/verify_published_release.py`
  when archive verification behavior changes
- Inspect the release ZIP and confirm it preserves the archive root and required
  runtime files declared by `release_contract.toml`

### Database migration and compatibility

Use this lane when changing `.yield.db`, `library.db`, source metadata
queries, schema DDL, schema version stamps, or migration code.

- source DB contract and migration fixtures:
  - `cargo test -p yield-library source_db_migration_tests`
- read-only source query compatibility:
  - `cargo test -p yield-library sample_sources::db::read`
- global library DB contract and migrations:
  - `cargo test -p yield-library sample_sources::library`
- compile check for the storage crate:
  - `cargo check -p yield-library`

See `docs/DATABASE_MIGRATIONS.md` for the schema-change contract. Source DB
additive repairs must run even when `PRAGMA user_version` already matches the
current schema, and read-only source opens must tolerate pre-migration database
shapes by projecting safe defaults or avoiding missing optional tables.

### Debug log workflow

On macOS, build manual-test binaries with `bash scripts/build.sh --release`.
The helper emits a branch-specific, ad-hoc-signed app under
`target/app-bundles/` and prints both its Launch Services identity and launch
command. Use that app name for UI automation; launching `target/release/yield`
directly does not register a distinct macOS application and can cause automation
to fall back to an installed Yield bundle.

Use this when reproducing a release/manual bug that needs the richer
per-launch diagnostics trail.

1. Start a repro run with the Yield-owned debug flag:
   `cargo run --release -- --log`
   For a live run that also shows debug layout overlays, use either spelling:
   `powershell -ExecutionPolicy Bypass -File scripts/run.ps1 logs debug-overlays`
   or `.\run.ps1 logs debug-layout`.
2. For isolated manual QA, use the sandbox profile instead:
   `powershell -ExecutionPolicy Bypass -File scripts/run.ps1 sandbox -- --log`
3. Retrieve the newest log quickly:
   `powershell -ExecutionPolicy Bypass -File scripts/run.ps1 logs`
4. Retrieve the sandbox run log the same way:
   `powershell -ExecutionPolicy Bypass -File scripts/run.ps1 logs -Sandbox`
5. Remember the boundary:
   the log captures startup metadata plus structured action/DB diagnostics for
   the current reproduced run only; earlier sessions still require a fresh repro
   with `--log` enabled.
6. Extra-verbose hot-path traces are separate:
   default `--log` keeps routine loop-cycle and similar hot-path chatter out of
   the file, but it still records warning-level slow starmap audition hot phases.
   Only enable `YIELD_HOTPATH_TELEMETRY=1` when you deliberately need
   developer-level loop/hot-path tracing during a focused repro.

### Benchmarks and perf checks

- `cargo bench`
- `cargo bench --bench ann_index`
- `cargo bench --bench tagging`
- local perf guard:
  - `bash scripts/perf.sh guard`
  - `powershell -ExecutionPolicy Bypass -File scripts/perf.ps1 guard`

Startup threshold calibration is not a release lane by itself. The Bash
`scripts/perf.sh calibrate-startup` command is optional Linux developer tooling
for refreshing startup threshold lock files on compositor-backed hosts. Current
release-risk startup perf evidence should be collected with `scripts/perf.* guard`
on supported app platforms, using `YIELD_PERF_GUARD_STARTUP_PROFILE=1` when
first-paint startup timing evidence is required.

## CI reference

GitHub CI and local wrappers together cover:

- formatting
- clippy
- required nextest
- doc tests
- selected guardrail scripts
- advisory quarantined tests and hygiene checks
- local-only perf and GUI contract lanes where appropriate

When in doubt, run the wrapper script instead of assembling the command list by
hand.


## Controlled release metadata matrix

Run `python3 scripts/internal/release/test_release_metadata.py` on Linux to
exercise every channel and target declared in `release_contract.toml`. It runs
the actual release ZIP packer in its existing skip-build mode in a temporary
directory, then validates those ZIPs with `verify_published_release.py`.
Cross-channel/version/target substitution, missing or changed identity fields,
non-object/duplicate-field JSON, and oversized metadata must be rejected.

The executable contents are explicitly marked placeholder fixture bytes.
These checks prove the metadata contract, not native binary support, signatures,
private CI artifacts, installation, physical qualification, or active updater UX.
Keep OPT-1769, native artifact checks OPT-1865/1994, and the broader GPUI release
and updater acceptance open until their own evidence exists.
