# Harvest copy receipt maintenance (OPT-2351)

The original `LibraryDatabase::apply_harvest_copy` now marks the existing
connection-local ordinary maintenance bit immediately after a successful **new**
immutable receipt COMMIT. The original Global worker consumes that bit through
its existing retained-WAL observation, throttle and PASSIVE checkpoint policy.
This also applies to new GraphChanged and IdentityConflict receipts: those
transactions persist evidence even though they retain the requested edit.

The marker precedes postcommit live source/journal validation. Source loss can
therefore keep `HarvestCopyCommitUncertain` and its exact immutable receipt while
still scheduling the original best-effort maintenance. Maintenance failure or a
pinned reader cannot turn a confirmed metadata commit into failure or completion.
Global profile loss closes the original worker through its existing ownership
checks, with no fabricated confirmation.

Reference snapshots and exact receipt replay remain maintenance-free. Foreign
owner/source/journal evidence, receipt retention exhaustion, failed SQL and
unconfirmed COMMIT preserve their original errors without marking success.
An error is never proof of rollback. No copy transaction, replay key, payload,
timestamp, source fence, receipt or outcome is changed. The copy path has no new
four-route ordinary WAL admission gate; this leaf adds none and does not claim
full-family admission.

Actual Source/Global/SQLite tests observe the original pending bit so throttling
cannot hide a false marker. They cover new applied and retained receipts, replay,
precommit/commit-hook refusals, count/byte retention bounds, stale evidence, actual
source changes at and after COMMIT, pinned-reader and denied/unknown maintenance,
profile loss before/during maintenance, and fresh profile/source/journal
reacquisition preserving the historical edge and later authored state.

Run `cargo test --locked -p yield-library --lib harvest_copy_wal`, then the existing
Harvest-copy/maintenance suites, full library and canonical checks. These fixtures
use genuine retained Source and Global owners and an actual journal-issued weak
copy observation. Their stored copy intent and capacity-free test journal startup
are logical metadata fixtures, not production protected-reserve bootstrap,
qualified Copy publication, initial settlement, or physical qualification.

No new connection or worker is introduced in production. The private marker uses
the existing bit; the numeric test-only accessor supplies no authority. Original
OPT-1687/1686/1685/1348 acceptance for other families, idle/hard/shutdown scheduling
and physical qualification remains open, as does the full Harvest workflow.
