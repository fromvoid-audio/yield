# Original recording destination correlation (OPT-2389)

The original pre-start custody factory and chunk-retention consumer compare the
held Source registration with the exact Required SourceDatabase participant named
by the recording destination parent. A matching unrelated participant cannot
supply the destination's root, database or lease-epoch correlation.

The shared borrowed matcher lives in the recording owner. It uses existing
proposal getters, requires destination participant ID/kind/requirement/SourceId,
and compares the fence with the held request's root/database and registered epoch.
It allocates no backing and changes no stored encoding or historical proposal.
The proposal model remains independent of the held Native request type.

The surrounding worker still checks the exact original receipt, Registered
lifecycle, current profile and SourceEffectPin, retained proposal, capacity/provider
and operation state. StoredSourceFence has no registration UUID, profile-owner or
private lease-owner field; existing receipt/request/pin checks enforce those facts.
The namespace validator still correlates destination SourceId/revision/root with
its selected descriptor. The held request has no SourceId or revision scalar that
could replace that validation.

The regression holds two genuine Source requests and confirmations. Destination B
matches held B and refuses held A. Valid foreign root/database/epoch proposals
retain unrelated matching fences and remain codec round-trippable. The old
any-participant predicate compiled but failed this regression; the new matcher
passed. These comparisons install no journal receipt, provider or physical grant.
Native positive production custody execution remains OPT-2330, with OPT-2326 open.

This leaf supplies no recording Start, required-WAL permit, terminal capability,
Finalizing/Frozen/seal SQL, physical effect, publication or settlement. Full
OPT-2145/2058 acceptance remains open.
