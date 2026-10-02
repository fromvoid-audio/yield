# Compiled release identity (OPT-2110)

`yield_core::ReleaseBuildIdentity::compiled()` reads only compiler-captured
release inputs. The production packer already supplies `YIELD_RELEASE_CHANNEL`,
`YIELD_RELEASE_VERSION`, `YIELD_RELEASE_TARGET_VERSION`,
`YIELD_RELEASE_BUILD_DATE`, `YIELD_GIT_SHA` and optional `YIELD_BUILD_NUMBER`.
The core build script records Cargo's actual `TARGET`; a runtime target
environment variable cannot select a supported platform.

The API requires complete bounded ASCII metadata, a contract-listed target,
an exact nonzero 20-byte commit, a positive u64 build number, a valid calendar
date and matching full version/channel/target-version identity. Stable uses
exact target-version equality. RC preserves a positive finite ordinal.
Nightly preserves its full date/commit suffix and binds both to the captured
build date and full commit. No monotonic build number is invented when the
packer omits it.

The Copy identity exposes `channel()`, full `version()`,
`target_version()` as three numeric components, `target()`, `commit()`,
`build_number()` and `build_date()`. Missing inputs return
`ReleaseIdentityError::Unavailable`; invalid/conflicting inputs return
`InvalidMetadata`. Neither result means Current. Runtime environment,
editable external manifests and Cargo package version cannot fill missing
inputs. The full RC/nightly version must remain available to metadata matching;
the three numeric target components alone do not identify a prerelease.

Portable commands:

```sh
cargo test --locked -p yield-core --lib release_identity
cargo test --locked -p yield-core --test compiled_release_inputs
python3 scripts/internal/release/test_release_metadata.py
```

The unit matrix uses explicit synthetic packer contexts for contracted targets.
The integration test runs the real compiled API in a child process with
conflicting runtime release inputs and asserts the result is unchanged.
This Linux executor is absent from the current release target contract, so
even complete compiler-captured metadata cannot claim a supported release here.
These tests do not qualify a native binary or signing identity.

OPT-2104 owns actual transport and GPUI typed adapter/bootstrap registration
using this context and the finalized OPT-1775 port. That production integration,
download/staging, install/relaunch and native artifact qualification remain
open under their original issues. This API issues no filesystem authority.
