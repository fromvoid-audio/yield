# Scoped updater renumbering (OPT-2201)

The production read-only updater follows `docs/TARGET.md` and the archived
`reference/radiant/src/updater/check.rs` policy:

* Installed `19.1.x` may discover major-zero candidates with `(minor, patch)`
  at least `(19, 1)`, including validated candidates in the selected RC/nightly lane.
* Installed major-zero versions at or above that first new-line core ignore
  historical `19.1.x` candidates **before** highest-build catalog selection.
* Every other line retains ordinary SemVer precedence. Real future `19.2.x`
  upgrades, and ordinary `18.9.0` to `19.1.0`, are not filtered. Earlier
  major-zero installations retain the archived policy's ordinary ordering.

Shared bounded core predicates live in `yield-platform::update_catalog`.
`CatalogService::check_for_installed` filters only the historical collision;
it does not authorize installation or silently fall back from an incomplete
latest allowed identity. Existing byte/count/depth/HTTP/time/inflight bounds
and lane/target/full-commit validation remain in force.

The production adapter passes its retained compiled installed version only
**after** rejecting substitutions of installed version, build or platform.
Exact Current still requires full version text, build number and complete
commit equality. Newer still requires a strictly higher positive build number,
a different full nonzero commit, the correct lane/target and canonical full
version/RC/nightly identity. Full prerelease precedence remains checked;
only the exact legacy core transition bypasses ordinary downgrade rejection.
The browser worker applies the same core policy to its fenced result before
projecting Current/Newer, preserving cancellation/service/generation checks.

Catalog versions are never rewritten. The browser's existing core-version
projection uses the actual candidate core; historical signed artifacts, full
version metadata and payload identities are not relabeled or modified.
Downloader/install/relaunch and publisher/trust formats are unchanged.

Two actual loopback HTTP → production adapter → background browser worker
regressions failed before the fix: legacy `19.1.9` rejected `0.19.1`, and a
`0.19.1` installation selected historical `19.1.99` build999 instead of allowed
`0.20.0` build45. The fixed suites additionally exercise exact Current,
ordinary/future/unrecognized lines, build rollback/reset, complete commits with
identical prefixes, wrong targets/lanes, RC precedence, nightly date/hash
identity, and incomplete latest allowed entries. Fixture installed/core versions
are parsed consistently from full version text; no contradictory test context
is used to authorize a transition.

```sh
cargo test -p yield-platform --lib update_catalog --locked
cargo test -p yield-ui --lib update_ --locked
cargo test -p yield-ui --lib --locked
scripts/agent.sh preflight
```

Loopback/headless Linux checks prove this portable production route, not native
packaged app startup, external release-server acceptance, install/relaunch or
physical release qualification. Those acceptance scopes and tracking parents
remain open, including OPT-2123 and OPT-2104.
