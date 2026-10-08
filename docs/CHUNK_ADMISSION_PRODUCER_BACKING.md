# Original post-intent chunk admission backing

ClaimChunk queues the actual request header and owned output key, with the original
historical reply carrier. Duplicate reads use the original scalar codec through a
borrowed SQLite statement. A successful owned row retains its own input-pool loan
until its data is destroyed, including after the request/reply handle is consumed.
The wrapper has no Clone, Deref or funded-record extraction API.

Before provider admission, the owner funds complete operation/state decoding,
restart reconciliation, one-row plan/claim construction, retained descriptor facts
and provenance, source-boundary/volume/root validation scratch, provider comparison,
transaction work, and both duplicate and committed-row lookups. The private write
scope binds the full request and an owned prepaid operation observation to the
original borrowed cell's exact digest and sequence. Fresh verification before lease
pinning and inside the original transaction rejects changed bytes or sequence.
These observations and memory loans supply no live authority.

The same connection, provider and provisional lease execute the original write.
The compatibility route retains its original driver codec and diagnostics. Funded
controls and DML use the existing private borrowed statement bridge. Namespace
checks borrow an adequate loan from the same original pool; variable native SQL
and namespace messages are still measured and funded before ownership. Selected
errors retain transaction and observation loans in the existing reply carrier.
Only confirmed rollback releases the pinned charge. Unknown commit, unconfirmed
rollback and committed-but-unreadable acknowledgement retain it.

Real SQLite and arithmetic provider tests cover exact committed rows, changed
bytes/sequence refusal, foreign/undersized loans, pressure before admission,
success with no unreserved fixed working memory, confirmed/unconfirmed rollback,
unknown commit and result/error lifetimes. Actual queue tests cover pressure and
read-only duplicates during an entered publication fence. Arithmetic fixtures
confer no native physical qualification. The System historical allocation tests
measure their original historical and Legacy transport paths, not the entire
new chunk producer. GNU Windows compilation is not native Windows acceptance.

IssueChunk uses the same original pool, reply carrier, original point codec,
complete operation visitor, restart image, provider checks and genuine live witness.
Its private scope prepays the before-transition, after-transition and committed
readbacks. The actual Claimed-to-Issued SQL compares the complete original chunk
row and exact operation digest/sequence before mutation; a sealed receipt escapes
only after commit and exact readback. Unknown acknowledgement returns no permit
and retains the existing durable charge.

The returned permit and retained owner ticket independently pay their owned
identity data before cloning it. Permit consumption moves its keeper into the
private entry. Ticket removal releases its data keeper, while the original
HashMap bucket growth keeps a separate accumulated loan through owner teardown.
The next complete table is charged from the observed capacity before try_reserve,
including overlapping old/new tables; bucket retention after removal or failure
cannot refund that debit. No new pool, queue, registry or authority is created.

Real SQL, actual queue, pressure and lifetime tests cover the issuance transition,
stale operation/chunk refusal, lost commit acknowledgement, fence refusal, receipt
ownership, permit-to-entry transfer and empty retained ticket buckets. Logical
fixtures and lifetime projections supply no native effect qualification.
Live-effect/source-pin entry, staging checkpoint, release and recording producer
funding and whole-owner finite policy remain separate work.

The new macOS positive harness shares the genuine production recording admission
setup: original Owner::start, real registered Source and FileOwner root, original
reserve descriptor facts and admit_prepared. It then queues an actual chunk claim
and one-shot issuance, inspects the original permit identity and retained debit,
refuses reissue, and returns the never-consumed permit through the existing release
path before joined shutdown and durable reopen inspection. Initial intent/source
registration evidence remains unsettled, so this case runs on its own fresh
native account and preserves the original dirty volume marker. No capture or staging
file is created. Its native execution is pending; the accompanying script requires
exactly one executed passing test with zero failures/ignores, independently of the
two original recording custody tests. Linux compiles the same helper and executes
only its genuine refusal tests; this is no replacement for native execution.
