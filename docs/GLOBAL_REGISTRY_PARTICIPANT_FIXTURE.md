# Normal-build global registry process fixture

OPT-2146 adds `crates/yield-library/tests/global_registry_participant_process.rs`.
Run without `test-support`:

```sh
cargo test -p yield-library --test global_registry_participant_process --locked
cargo test -p yield-library --test profile_admission --locked
cargo test -p yield-library --test versioned_profile_migration --locked
scripts/agent.sh preflight
```

The parent dispatches one ignored helper in the same normal-build integration
binary. The helper opens a private temporary profile with a real
`WritableProfileGuard` and `GlobalLibraryWriter`, submits one bounded production
`SourceRegistryParticipantRequest`, receives its actual committed receipt and
reads back the registry through the owner. Only then does it publish a bounded
receipt observation file outside the profile. This file contains values obtained
from public receipt getters; it cannot construct a production receipt or grant.

The helper remains alive with its guard/writer, without calling shutdown or
dropping them. The parent proves another profile acquisition is denied, kills
only its owned `Child`, reaps it, and inspects the actual SQLite store read-only.
It reacquires real profile ownership and verifies:

- Original registry and operation/work/payload/sequence/disposition/owner receipt
  fields survived process death.
- A request carrying the old acquisition is rejected without changing registry
  or receipt evidence.
- The same frozen operation/work/payload, refenced to the new acquisition,
  returns the original immutable receipt, including its historical owner.
- Changed payload or work key conflicts fail without changing persisted evidence.
- Exact replay after a later ordinary registry edit returns the same receipt,
  preserves the later content and retains exactly one ledger row.

There is one operation and two source descriptors (original and later); no source
database or audio data is created. Owner readiness/replies and read-only SQLite
busy waits are bounded to five seconds. Child readiness/death and unwind cleanup
have thirty-second deadlines. An owned child guard kills/reaps on assertion
failure; the helper also exits without destructors after thirty seconds if its
parent fails to kill it. Evidence reads are capped at 4096 bytes; retained SQL
reads limit row counts and text/blob lengths before materializing them.

The helper's ignored line is intentional: the ordinary parent test explicitly
executes it. This fixture tests an unclean process exit after a committed SQLite
receipt on the tested host. It does not inject an in-flight commit failure or
power loss, qualify native storage durability, issue journal/coordinator/private
physical checkpoint authority, or cover source/Harvest participants and the
combined crash matrix. OPT-1666/1345, OPT-1665/1645 and native verification retain
their separate acceptance requirements. No production hooks, schema or ownership
policy are changed.
