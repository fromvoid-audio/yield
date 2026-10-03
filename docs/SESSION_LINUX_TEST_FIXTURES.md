# Session Linux fixtures (OPT-2234)

Portable session tests use the existing `cfg(test)`
`start_portable_registry_for_test` seam. It starts the real profile guard,
operation journal, global writer, catalog and SQLite workers, using the explicit
non-capacity journal fixture. It supplies no physical reserve authority. Normal
startup and its unsupported-reserve negative test retain their original behavior.

The twenty previously failing session tests fall into these scopes:

| Scope | Tests moved to the portable owner fixture |
| --- | --- |
| Preferences and registry persistence | `collection_slot_name_survives_profile_restart_without_leaking_to_another_profile`, `malformed_preferences_remain_untouched_until_explicit_reset`, `preference_save_is_not_starved_by_retained_lookup_backlog`, `existing_remove_is_durable_across_reopen`, `retained_lookup_preserves_descriptor_policy_on_readd` |
| Profile ownership and journal lifetime | `session_journal_reopens_after_orderly_shutdown`, `catalog_shares_session_profile_owner_and_releases_after_shutdown`, `profile_ownership_conflict_is_reported_without_live_fallback`, `cross_process_profile_owner_conflict_leaves_session_slots_uninstalled`, `idle_profile_replacement_fails_admitted_requests_once` |
| Catalog forwarding and shutdown | `catalog_audit_does_not_block_preference_persistence`, `immediate_shutdown_closes_catalog_admission`, `session_retains_compound_catalog_outcome_after_catalog_shutdown_admission_closes`, `shutdown_retains_catalog_curation_receipt_and_reopens_persisted_revision`, `shutdown_drains_without_an_event_consumer` |
| Initialization rejection | `maintenance_admission_serializes_and_blocks_protected_admissions` |

The cross-process helper uses the same actual portable startup in a child
process. The parent still verifies OS profile exclusion, unavailable owner slots
and byte-for-byte journal preservation. Profile substitution, SQLite restart,
durable curation receipts and accepted command draining retain all assertions.
The gated initialization test expects `NotReady`; it does not qualify admitted
physical maintenance. Its now-unused native gated helper is removed.

Four tests retain normal startup and every original physical assertion. Native
qualification is tracked by [OPT-2235](https://linear.app/boostnlvp/issue/OPT-2235):

- `loaded_cache_clear_rejects_source_and_trash_namespace_conflicts`
- `loaded_cache_clear_succeeds_and_releases_admission_for_storage_query`
- `dropping_admitted_cache_clear_is_joined_during_shutdown`
- `competing_profile_restarts_only_after_orderly_volume_handoff`

Run each on macOS with a genuinely qualified protected namespace/reserve backend
and local volume, using `cargo test --locked -p yield-session --lib tests::NAME --
--exact --test-threads=1`. Require one executed passing test, actual effects and
original error/count/handoff assertions. Unsupported backend or missing effects
are failures. Catalog source-authority qualification remains separately
[OPT-2026](https://linear.app/boostnlvp/issue/OPT-2026); these fixtures do not finish
post-latch journal admission OPT-2136 or physical qualification.

The complete suite still executes those native tests on Linux and truthfully
reports their failures. For the explicitly scoped portable run:

```sh
cargo test --locked -p yield-session --lib -- --test-threads=1 \
  --skip tests::loaded_cache_clear_rejects_source_and_trash_namespace_conflicts \
  --skip tests::loaded_cache_clear_succeeds_and_releases_admission_for_storage_query \
  --skip tests::dropping_admitted_cache_clear_is_joined_during_shutdown \
  --skip tests::competing_profile_restarts_only_after_orderly_volume_handoff
```

There are no new ignored tests or production fixture overrides. The existing
ignored child helpers are executed by their parent tests.
