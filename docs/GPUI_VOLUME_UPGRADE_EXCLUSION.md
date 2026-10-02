# Volume ownership upgrade/exclusion decision (OPT-1934)

Baseline `5e9b2cabc74c43f65ebe4514dc48770856509a80`.
**Retain device-keyed v1 lock authority. UUID namespace activation remains
disabled.** Mac descriptor UUID binding from OPT-1933 is retained identity
validation, not a new lock namespace. This is the supported source decision
until an enforceable old-binary exclusion boundary is qualified.

## Counterexample and actual boundary

A shipped v1 binary opens only `volume-{device}.lock`. If new UUID A at d1
exits dirty and A remounts at unseen d2, the old binary can lock a blank d2
file without consulting A's dirty UUID primary. Known-device generic aliases,
UUID-qualified aliases, a later sweep and a cooperating bridge version cannot
change that shipped executable's lookup or prevent its admission.

The portable fixture
`old_v1_unseen_device_ignores_uuid_dirty_owner_and_active_policy_keeps_shared_key`
creates hypothetical A UUID dirty bytes and preserves the known d1 dirty
marker, then emulates v1 lookup/lock/write at unseen d2. Admission succeeds
in that emulator, demonstrating why a UUID primary alone is insufficient.
The **actual current** lock-name function still chooses d2's v1 device key;
the cooperating current owner is rejected while that old lock is held and
after its dirty exit, with all original bytes preserved. The hypothetical
UUID file never grants production admission.

This fixture does not claim a production UUID migration was enabled and
blocked, emulate physical APFS remounting, or retroactively protect shipped
v1 device ownership across device reuse. It checks the chosen retention
boundary and prevents accidental new/old authority namespaces. The old
device scheme retains its documented ambiguity/reconciliation limitations.

## Supported versions and migration prerequisites

Today, cooperating versions use v1 device locks. Current macOS versions also
retain/revalidate descriptor UUID binding; historical foreign dirty v1 bytes
are ambiguous and require preserved, audited reconciliation. No UUID is
inferred for a historical marker, no clean handoff is fabricated, and no
foreign takeover removes the evidence. UUID A(d2)/B(reused d1) independent
concurrency is **not** qualified while device authority remains in force.

A future UUID-authority deployment needs an execution boundary outside an
ordinary cooperative application update: stop/drain old processes and enforce
that no pre-UUID executable can launch against the migrated profile/volume,
including user-retained app copies and alternate entry points. A version-aware
broker is sufficient only if independently enforced OS/deployment policy
prevents bypass; putting a check in the new launcher does not constrain an old
binary launched directly. No such enforceable deployment boundary is available
or installed by this change, so UUID activation remains unsupported.

Before activation, the qualified boundary must reject unsupported binary
versions **before** opening/admitting migrated writable state, issue an
auditable migration receipt only after old owners drain, and preserve dirty
markers for explicit reconciliation. Missing/unknown exclusion proof rejects
migration; it cannot fall back to a post-admission scan. Historical aliases
must not become retained locks that incorrectly exclude a different volume
reusing d1. Validate independent A/B concurrency and all remount/restart/dirty
cases under that enforced boundary, not just cooperating fixture binaries.

## Evidence and retained acceptance

The Linux fixture uses actual local lock/marker operations in disposable
storage, synthetic device/UUID observations and the production v1 lock-name
and marker admission logic. Existing ownership contention, dirty preservation,
clean/uncertain handoff, namespace replacement and child-process fixtures run
serially. They qualify portable retention logic, not native UUID migration.

Actual APFS UUID observation/remount/device reuse, old/new executable launch
exclusion and independent volume concurrency remain OPT-1995 under OPT-1953.
No old app copy, live marker or production profile is removed or changed.
This closes the bounded source decision/fixture slice; UUID migration and
native upgrade acceptance remain unavailable until the external boundary is
qualified and a separately reviewed activation change is accepted.
