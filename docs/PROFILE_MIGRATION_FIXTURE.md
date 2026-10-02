# Isolated profile migration evidence (OPT-2095)

The checked-in `crates/yield-library/tests/fixtures/profile-v11` SQL is a
synthetic fixture of known legacy shapes, not a captured historical user
profile. The source shape and version 11 come from the existing
`source_db_migration_tests::contract` fixture. Current source schema version is
13 in `sample_sources/db/schema/mod.rs`. Global library migration uses
column inspection, not a global `user_version` contract. The preference JSON
is opaque retained payload here; this test does not validate session preference
semantics or claim to migrate that document.

Run the normal-build integration test:

```sh
cargo test --locked -p yield-library --test versioned_profile_migration
cargo test --locked -p yield-library --test profile_admission
bash scripts/agent.sh preflight
```

The test creates temporary profile, source and unrelated sibling roots. Raw
SQLite writes seed only those disposable fixtures before production owners
start. Evidence inspection connections are read-only. Migration/rejection runs
through actual `WritableProfileGuard`, profile-owned catalog open and
`GlobalLibraryWriter`, without `test-support` or a portable reserve fixture.

| State | Before | Actual owner result and reopen |
| --- | --- | --- |
| Source database | `user_version=11`, revision `7`, `legacy/one.wav`, size `42`, mtime `99` | `SourceLease(MigrationRequired { user_version: 11, .. })`; bytes and exact rows unchanged; no durable `source_lease` table created |
| Global sources | `id`, `root`, `sort_order`; one stable ID `legacy-source`, order `0` | Additive role/storage/inbox columns; defaults `normal`, `source_folder`, `_Yield Inbox`; ID/path/order unchanged through owner query and reopen |
| Preferences | Exact `ui_preferences_v1` payload | Byte-identical payload returned by global owner before and after restart |
| Profile config | Exact sentinel bytes | Unchanged after both owner lifetimes |
| Unrelated sibling | Exact sentinel bytes | Unchanged |
| Ownership | One profile capability acquired | Competing acquisition rejected; after writer shutdown and capability drop a fresh acquisition succeeds |

Source rejection is recovery evidence, not successful source migration. Lease
bootstrap intentionally accepts only a new empty or current known schema.
The existing test-only migration opener does not supply production adoption.
OPT-2098 tracks the portable owner-fenced legacy adoption contract and
implementation. Preserve the database and lease sidecar; do not delete the
sidecar, stamp the database current, or call a path-only/test-support writer to
bypass this rejection. The structured error identifies the legacy version and
database; repeat open is not a migration procedure.

OPT-1745 and OPT-1495 remain open for original profile migration/reference and
native startup acceptance. This fixture supplies no historical reference
profile, physical publication authority, crash/power-loss qualification, or
native GUI startup evidence. Missing source adoption is portable work, not a
Linux platform blocker.
