# Packaged update-check registration (OPT-2140)

Application startup calls `update_catalog_service::register` before creating its
window. Registration consumes only `ReleaseBuildIdentity::compiled()`. Complete
channel, target, version, positive build number and full commit are required;
missing/unsupported metadata leaves the global checker absent. Ordinary Linux
development builds remain truthfully Unavailable because the active release
contract has no Linux artifact target. Neither Cargo versions, runtime environment
variables nor editable manifests can fill missing installed identity.

The registered checker invokes the merged OPT-2122 production catalog service
from the existing OPT-1775 background settings worker. Startup and the render
callback do no HTTP I/O. The service's retained single-inflight slot, ten-second
caller deadline, response limits, TLS origin and no-redirect policy remain in
force. Late cancellation and request identity substitution return no candidate.

The adapter retains the complete installed version text and semantic version.
Current requires exact version text, number and full commit; a shared numeric
version core cannot establish RC/nightly parity. Newer requires a strictly larger
build number, different commit and nonolder semantic version including prerelease
precedence. Unknown/reused/older identities fail before the UI's core-only result
projection. Target and installed identity are immutable; selecting a different
lane queries that lane without changing the installed build.

Controlled HTTP integration uses an explicit platform `test-support` feature.
Its constructor accepts only a loopback socket address and bounded deadline, not
an arbitrary production URL. Normal application builds expose no such constructor.
The actual settings-worker fixture connects this production transport to Current,
same-core Newer, Offline, Failed and Unavailable states, repeated activation and
retry. Focused tests separately preserve complete RC/nightly identity and reject
substituted request fields before I/O; existing worker close/cancellation/lane
replacement tests retain those delivery fences.

```sh
cargo test --locked -p yield-ui --lib update_catalog
cargo test --locked -p yield-ui --lib update_check
cargo test --locked -p yield-platform --lib
cargo check --locked -p yield
bash scripts/internal/agent/run_agent_preflight.sh
```

Fixture installed contexts exercise the adapter/worker contract; they are not
compiled packaged identity or physical release qualification. OPT-2110 supplies
the compiled identity implementation and its separate exact-input evidence.
OPT-2123 retains actual public catalog full-identity publication/roundtrip;
incomplete catalog records remain Unavailable. OPT-2104/1498 remain open until
their original service, verified download, installation and native relaunch gates
are met. No installer, live catalog success or macOS/Windows qualification follows
from these portable checks.
