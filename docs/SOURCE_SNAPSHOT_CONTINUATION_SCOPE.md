# Retained snapshot continuation scope (OPT-1690)

A source-owner metadata continuation carries the exact `SourceRevision` that
produced it: source ID, physical media/database/root identities, lifecycle,
lease epoch and numeric revision. The private, nonserializable binding conveys
read provenance only. It grants no write authority and cannot reopen an owner.

Previously `BrowserMetadataCursor` carried only a path. Pairing a cursor from
revision A with a refreshed revision B passed the owner checks and returned
`FreshAsOf(B)` after the old path, skipping any changed prefix. A remap to a
new source/database at the same numeric revision had the same problem.

The retained writer now rejects a mismatched or unbound continuation as `Stale`
before reading browser rows. The result supplies the current owner observation;
restart with that revision and no cursor. New successful pages bind their next
cursor to the complete observation. Low-level database pages retain their
existing path cursor API; their unbound cursors cannot enter the retained-owner
route. The worker still retains custody and the same bounded admission/deadline
and no-write behavior from OPT-1689.

## Actual portable verification

```sh
cargo test --locked -p yield-library --lib snapshot_cursor
cargo test --locked -p yield-library --lib source_owner_page_is_bounded_read_only
cargo test --locked -p yield-library --lib snapshot_read_only
cargo test --locked -p yield-library --lib db::read::tests
bash scripts/agent.sh preflight
```

Real profile-bound SQLite tests cover current continuation, changed revision and
restart at the first row, a new lifecycle on the retained database, actual close
and lease reacquisition with an unchanged numeric revision/lifecycle, a physical
source/database rebind at the same source ID/revision/epoch, and an unbound
low-level cursor. The physical rebind test forbids browser-row SELECTs while
checking that the old cursor returns Stale, then verifies the replacement's
first row. Reads/rejections leave SQLite change counts unchanged.

This synchronizes owner read metadata with the retained source binding. It does
not implement the GPUI missing-source remap action, cross-source move ordering,
projection publication, native namespace qualification or OPT-1348's full WAL,
checkpoint and capacity acceptance. Those original tracking scopes remain open.
