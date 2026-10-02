# Archived Radiant dependency and command map (OPT-1988)

Reviewed active GPUI at `f5dd5f13ae18666e850048d0e29835bf99f325f1`.
The last change to the checked-in `reference/radiant` tree is
`07c57b961995aaae4cfffe6c274a1ad687285fe6`, the domain-crate migration.
That tree is excluded by the active Cargo workspace and contains no root
Cargo manifest. It is comparison source, not an executable Radiant sibling.

The migration also removed `radiant-dependency.toml`. Its last accepted
metadata, retrieved from `07c57b9^:radiant-dependency.toml`, named repository
`https://github.com/PORTALSURFER/radiant.git`, full pin
`fa707ff5cba809d334968416f9bcd1b702083db1`, and path `../radiant`.
This is a historical sibling pin, not the revision of the archived Yield
source or an active GPUI runtime dependency. No sibling was cloned, repinned
or built for this source inventory.

| Command/dependency | Current boundary | Portable outcome / retained gap |
| --- | --- | --- |
| Active `Cargo.toml` and lockfile | `yield-*` domain crates; exact GPUI 0.2.2 with Metal/runtime shaders and patched vendor; `reference/radiant` excluded | GPUI app check passes on Linux; no active Radiant dependency |
| `scripts/radiant.sh locate` → `internal/radiant/sibling.sh` | Requires removed root `radiant-dependency.toml`; validates sibling manifest, remote and configured path | Actual checkout rejects with `missing radiant-dependency.toml`; historical helper is not an active build prerequisite |
| `scripts/radiant.sh provision --clean` | Historical exact detached sibling provisioning; preserves development trees and refuses dirty targets | Disposable offline fixture verifies refusal/preservation; no real clone/fetch or release provisioning attempted |
| `scripts/internal/release/provision_radiant_sibling.sh` | Historical private deployment-key provisioning wrapper | Inapplicable to the active GPUI dependency graph; no key sought or artifact published |
| `cargo test --lib --locked` in historical `../radiant` | Original OPT-1816 Radiant library qualification | Not run: no provisioned accepted sibling. Archived tree lacks manifest. Full library evidence remains independent/obsolete for active GPUI and must not be reported as a pass |
| `bash scripts/ci.sh agent` → `internal/ci/ci_agent.sh` | Calls Radiant locate before Cargo; then legacy `controller_browser_integration`, legacy-controller feature and old library tests | Source inspection plus actual locator rejection proves the obsolete dependency boundary; this lane is not active GPUI preflight |
| `scripts/agent.sh checks` | Retained agent wrapper / readiness checks | Distinguish wrapper readiness from actual native GPUI test lane; no helper success implies native parity |
| `bash scripts/gpui-development-checks.sh preflight` | Active portable branch/script/owner guards, formatting and app check; repaired in PR #170 / OPT-1980 | PASS on the Linux executor at this review SHA |
| `scripts/gpui-local-ci.sh full` | Active native macOS compilation/session CI | Explicit macOS boundary; not run on Linux |
| Isolated/overlapping waveform bands, low/high amplitude states | Original visual evidence requirements from OPT-1816 / OPT-1288 | Remain native verification OPT-1989; CPU geometry or helper fixtures cannot certify colors/painted output |

Portable validation uses only disposable fixture repositories via:

```sh
python3 scripts/internal/check/test_radiant_archived_locator.py
bash -n scripts/radiant.sh scripts/internal/radiant/sibling.sh scripts/internal/ci/ci_agent.sh
bash scripts/gpui-development-checks.sh preflight
```

The locator fixtures verify a clean full-SHA match, dirty locate without
mutation, wrong paired-path rejection, and dirty clean-provision refusal
before networking. The active missing-metadata case is explicit. Fixture
commits are synthetic and do not qualify the historical accepted pin.

This completes the Linux source/dependency/command inventory. Original
OPT-1816 remains open for its retained native/legacy evidence; the active
application uses GPUI. Native review may retire obsolete requirements with
evidence, but this audit does not silently rewrite them or resurrect Radiant.
