# Durable original publication issuance (OPT-2394)

Journal schema 13 adds `operation_journal_publication_issuance`. The original
private staging issuer inserts one immutable data-only association in the same
SQLite transaction that changes its exact fresh checkpoint's `grant_reserved`
from false to true. Neither a replay nor stored bytes can construct a live grant.

The association retains the original publication command's complete authority-free
projection, distinct publication effect, staging output/effect/fingerprint and
acknowledged sequence, source fence and both root observations. It also retains the
selected ordinary claim, optional issued chunk and issue identity, original charge
generation/accounting observation, and original registry incarnation/slot generation.
The registry only reports the identity of a pointer-equal slot it still owns.
The issuer captures these observations from the actual sealed target and selected
charge; it does not accept a caller-supplied association or decoded authority.

Reservation validates the actual unreleased ordinary row and, when selected, the
exact issued chunk row. A unique output and unique publication effect prevent
another association. Boolean reservation and binding commit or roll back together.
Exact durable readback compares the whole original association before returning the
original private grant. Lost commit acknowledgement or failed post-commit readback
retains the reservation/evidence and closes original issuer admission. No charge is
refunded or marked settled.

The original 64 KiB input debit precedes capture and remains with the original
grant. Canonical command capture is capped at 12 KiB; the complete association is
capped at 16 KiB, with at most 8,192 retained associations. Exhaustion refuses before
issuance without evicting historical rows. Direct typed decoding caps source fences
at one and rejects unknown/duplicate fields. Streaming command hashing avoids a
second owned projection; readback borrows the bounded SQLite blob before decoding.
These caps add conservative backpressure within the existing input allowance.
The existing staging reader now validates borrowed SQLite cells before decoding.
It preserves the historical64KiB wire allowance, whitespace and key order, while
an allocation-free token scan caps escaped string tokens at6KiB and bounded
string visitors validate text before owned copying. It constructs no JSON Value
graph. A typed issuance projection fits8KiB (header<=2KiB, at most4352 string
bytes and one source element<=256 bytes); a typed staging projection fits4KiB.
Two issuance projections and three staging projections use at most28KiB.
Encoding and parsing/error formatting occur in separate phases; a conservative
32KiB temporary allowance for either phase keeps the total within60KiB. Parser
scratch drops before successful canonical encoding. Insert encoding drops before
commit/readback, and original command bytes drop before checkpoint work. The
original target and native capsule retain their separate existing backing.


Guarded schema-12 migration creates an empty relation. Existing reserved checkpoints
remain reserved without a fabricated association and cannot issue again. Clean
handoff counts the new relation, including unresolved or orphan history. Restart
readback is an observation; it cannot mint a permit, authorize retry, free staging,
refund a claim, or establish genuine settlement. Active readback currently requires
the selected charge to remain unreleased and any selected chunk to remain issued.

Linux tests exercise real reservation transactions, restart, commit acknowledgement
loss, rollback, foreign associations, selected chunk identity, corrupt readback and
fixed bounds using authority-free durable fixtures. The shared original native
handoff harness additionally compares the persisted association with the original
moved grant after accepted reply loss and restart. Its Unix compilation is not
native execution. Genuine macOS resources and execution remain OPT-2325.

This leaf does not implement completion settlement or a positive namespace backend.
OPT-2215/2216/2217/2218, native qualification and database/physical settlement parents
remain open. See [publication lifetime](PUBLICATION_LIFETIME_GATE.md) and
[database-attempt limits](WAL_ATTEMPT_LEDGER.md).
