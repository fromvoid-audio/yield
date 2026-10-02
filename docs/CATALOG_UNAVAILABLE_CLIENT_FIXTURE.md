# Ended journal client catalog fixture (OPT-2134)

The existing catalog unavailable-client test exercises actual registration,
removal, reopening at the next lifecycle generation, and a second removal using
`CatalogService::start_with_journal`. Its journal worker has ended before any of
those operations. It does not issue positive source or capacity authority.

The setup waits at most five seconds for journal initialization. An Available
worker must shut down cleanly, become Closed, and reject a real point-lookup
submission with Closed. An actually Unavailable worker must report unsuccessful
shutdown, preserve its exact diagnostic after joining, and reject submission with
that same Unavailable diagnostic. Other lifecycle states fail the fixture.

Previously setup unconditionally required successful shutdown. Linux's actual
protected-reserve bootstrap failure made that assertion fail before the catalog
contract was exercised. The corrected setup preserves the documented failed-owner
semantics and runs the same two-generation catalog assertions. A second run uses
the existing `start_without_capacity_for_test` constructor: the actual store,
profile-child lease, recovery and worker must become Available, then close cleanly.
It installs no volume owner, protected reserve or live capacity/source authority.
It adds no ignore, platform skip, substitute reserve provider, or synthetic success
capability.

```sh
cargo test --locked -p yield-catalog --lib \
  tests::unavailable_journal_client_does_not_block_catalog_reopen_or_removal -- --exact
```

Linux exercises both the real Unavailable bootstrap case and the real
Available-to-Closed journal worker without a capacity provider. A supported native
reserve backend is still required to execute the original capacity-enabled clean
close and the separate registration-authority and finalization-retry tests.
OPT-2079/2026 and physical qualification remain open; this fixture does not qualify
physical reservation, publication or power-loss behavior.
