# GPUI retained update downloads (OPT-2228)

The actual general settings panel now offers **Download and Verify Update** and
**Cancel Download / Discard Download**, alongside the existing read-only update
check. These controls use `PayloadStagingService`, whose single application
admission and background owner select, download, verify and retain original ZIP
bytes. The existing core-only metadata outcome does not authorize staging.
Installation, extraction, cleanup, installed swaps and relaunch remain unavailable.

## Compiled context and live publication

The app registers a private service only from valid immutable compiled full
version/build number/commit/channel/target. Missing metadata or unsupported targets,
including ordinary Linux development builds, keep downloads unavailable. Both the
UI context and payload owner independently consume the same compiled identity.
There is no normal-build fixture, key, URL or installed-context override.

The UI holds only small original-owner controls and immutable observations. A
strong service/context pointer binds the original compiled identity and selected
lane. Each request also has a private strong identity, in addition to its numeric
generation, so replacing a state holder and reusing an integer cannot make an old
async task clear or publish into a new request. Original owner custody/cancellation,
requested lane and target, live panel, service and request are rechecked before
Staged publication. Direct rendering applies the same checks before presentation.
Actual full stable/RC/nightly versions come from the authenticated retained
selection; they are never reconstructed from a core-only version.

A small async observer reads the owner every 50 ms. Network, crypto, ZIP parsing,
archive retention and deallocation occur only on the standing background owner.
Closing the panel, retiring the download, replacing the service/lane, a stale
request or dropping its actual state holder requests original-attempt retirement.
The UI reports Stopping until the owner confirms resources have retired. Retrying
in another view/facade cannot bypass application admission. Staged stays volatile
until retirement; restart has no staged path to discover or synthetic recovery.
The panel distinguishes Selecting, Downloading, Verifying, Staged, Stopping,
Unstaged, Offline, Failed and Unavailable. Staged text explicitly says installation
and relaunch are unavailable.

## Controls and accessibility

`verify-update-download` and `retire-update-download` use shared fresh enablement
and guarded dispatch. The macOS settings accessibility adapter registers the same
IDs, keyboard order, status/frame marker and handlers. Download is disabled while
this view has active, retained or retiring custody; retirement remains available.
Missing global context cannot fabricate a download. The native adapter is compiled
only on macOS; Linux's existing accessibility stub does not qualify OS delivery.
The reference action register keeps `OpenReleaseDownloadPage` a gap: retaining
verified bytes is a different action from opening that external page.

## Source verification and native limits

Relevant Linux commands:

- `cargo test --locked -p yield-ui --lib update_payload`
- `cargo test --locked -p yield-ui --lib update_check::tests`
- `cargo test --locked -p yield-ui --lib update_catalog_service::tests`
- `cargo test --locked -p yield-ui --lib general_settings::tests`
- `cargo test --locked -p yield-platform --lib`
- `cargo test --locked -p yield-platform --doc`
- `bash scripts/agent.sh preflight`

The actual GPUI tests run five real loopback requests through complete selection,
real signed checksum/SHA-256/CRC/ZIP verification and retained owner. They cover
full stable/RC/nightly identity and retained hash/length, corrupt and signed-wrong
archives, offline/failure/retry, close/lane/target/retirement, a blocked old socket
across service replacement and a second view, an actual retained old async task
across numeric generation reuse, state-holder Drop, shared action enablement,
rendered control anchors and unchanged installed/profile/source sentinels.

Dependent tests enable the default-off platform `test-support` feature. Its owner
fixture constructor accepts only loopback transport and a fixed nonproduction
fixture public key; the compiled production entry still uses its original pin.
A read-only fixture socket-entry observation makes blocked-I/O tests deterministic,
without granting verified-byte or retirement authority. Fixture binaries/keys and
controlled metadata do not qualify genuine production artifacts or physical
installed apps. The existing platform suite separately exercises the actual
release packer for all nine lane/target combinations.

OPT-2229 records the exact unavailable native step: compile/run
`browser/accessibility.rs` and actual macOS pointer/keyboard/AX delivery. Run
`cargo test --locked -p yield-ui --lib actual_settings_ax_action_download_and_retire_share_guarded_payload_owner`,
`cargo test --locked -p yield-ui --lib accessibility`, and
`scripts/gpui-local-ci.sh full` on macOS, then genuine signed artifact/preservation
cases. OPT-2230 retains Windows runtime, actual compiled identity/signing and
installed preservation. Both depend on OPT-1769-qualified fixed-pin artifacts;
OPT-2123 retains deployed-route access. Compiled number/date and native signing /
notarization are separate from authenticated catalog/manifest claims.
Tracking OPT-1776, OPT-1498 and OPT-2035 remain open until full acceptance.
