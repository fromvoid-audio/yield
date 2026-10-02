# Read-only release catalog transport (OPT-2122)

`yield_platform::update_catalog::CatalogService` performs a production HTTP GET
against `https://portalsurfer.org/yield/api/v1/releases`, the catalog used by the
existing release publisher. It returns bounded candidate identities, Offline,
Failed or Unavailable. It neither downloads artifacts nor grants installation or
physical verification authority. GPUI registration and immutable installed-build
comparison remain OPT-2104, using the OPT-1775 port and OPT-2110 identity API.

Production authority is the fixed HTTPS origin with default rustls certificate
validation. There is no runtime endpoint override, authentication, redirect,
cookie store or retry. Tests alone instantiate loopback endpoints. HTTP errors,
redirects, non-JSON/encoded bodies, malformed identities and bounds failures
produce Failed; connection/DNS failures produce Offline. Missing matching/full
identity produces Unavailable, never Current.

Each service retains one transport slot. The caller deadline is ten seconds,
including a polling boundary around the OS resolver, which cannot be interrupted
by ureq. Cancellation returns promptly but leaves the slot retained until old I/O
exits. A stuck resolver can retain one thread/slot; retries cannot accumulate
additional threads. Timeout never implies transport cancellation or success.

Limits are 512 KiB response bytes (also for unknown-length bodies), 512 releases,
32 assets per release, 16 levels of JSON nesting and 16 KiB per encoded string.
Relevant version, identity, channel and asset strings have tighter limits.
The latest matching positive build number selects an exact asset name from the
current repository `release_contract.toml`. Version lanes must match; complete
RC/nightly text survives selection for the installed-build adapter. Contradictory
identities for a reused matching build number fail closed.

A complete commit is accepted from the public `commit` field or a 40-hex build ID
suffix. A supplied commit must agree with any build ID hash suffix. Stable/RC
publisher IDs `yield-{version}` need the explicit full commit field. Legacy
abbreviated IDs and bare `nightly` versions cannot establish Current/Newer.
OPT-2123 retains the external catalog deployment/roundtrip requirement; the
existing publisher verifier does not establish this complete identity today.

Portable verification commands:

```sh
cargo test --locked -p yield-platform --lib
cargo check --locked -p yield
cargo fmt --all --check
bash scripts/internal/agent/run_agent_preflight.sh
```

The real HTTP harness covers matching identities/assets, all three supported
release targets, complete RC identity, substitution/reused build numbers, missing
candidates, malformed/oversized/deep/encoded responses, redirects, HTTP failure,
offline connection, timeout, cancellation and retained single-inflight admission.
These checks exercise production transport with controlled local servers; they
do not assert that the public endpoint currently publishes complete metadata,
that macOS/Windows artifacts install, or that a native relaunch is qualified.
