# Durable source-registry participant (OPT-1642)

`GlobalLibraryWriter::apply_source_registry_participant` submits one bounded registry
replacement to the existing profile-owned worker. `SourceRegistryParticipantRequest::new`
requires the caller's durable `OperationId`, frozen `GlobalLibrary` work key, current profile
acquisition identity, expected registry snapshot and replacement. It validates size and
identity before queue admission. The profile identity is an equality fence, not a monotonic
epoch or ownership proof; the worker retains and validates the actual profile capability.

The immutable command binding uses normalized registry JSON (the same stored policy as the
existing replacement route) and a versioned BLAKE3 domain covering expected and replacement
content. Expected content is a snapshot fence, not a physical source revision. Different
participant/work keys or payloads under the same operation conflict. This bounded command
permits one registry disposition per operation; a rejected retry needs a new operation to
attempt changed input.

The worker checks its current acquisition before reading even a duplicate. In one SQLite
transaction it checks existing evidence, compares the current snapshot, writes the registry
and retained source mappings when matched, inserts the result relation, and reads it back.
Only then does it commit and report the receipt. A stale snapshot commits `RegistryChanged`
without changing the registry. An exact retry returns that same disposition permanently,
even if expected content later matches again. SQL failure rolls back both registry and
receipt; response loss after commit is recovered by replaying the same immutable command.

`source_registry_participant_receipts` links `OperationId`, serialized work key (including
participant), payload identity, historical profile acquisition, ledger sequence and disposition.
The sequence orders this ledger, not physical source revisions. A replay after restart supplies
fresh live acquisition identity but preserves the historical receipt. It never reapplies the
old snapshot over subsequent ordinary registry changes. Receipt reads bound retained fields
before allocation; malformed evidence fails closed. At 8192 receipts new operations are
rejected without evicting evidence, while exact replay still works. Receipt reclamation needs
a separately proven journal retention contract; it is not implicit deletion on success.

The existing ordinary `replace_source_registry` route retains its semantics. Neither queue
acceptance nor this receipt is journal admission or a private `VerifiedStepCompletion`.
Future family adapters must retain the stable proposal, establish the exact journal/work-plan
binding and required source checkpoints, and consume only an `Applied` receipt as an owner
observation. `RegistryChanged`, SQL uncertainty, stale ownership, capacity exhaustion and
conflicting replay must not advance the global participant. Cross-source references,
operation-link commands, Harvest, projection, source retirement and UI/session wiring remain
their separate consumers under OPT-1341/1357.

Portable validation uses the actual global worker and SQLite database, including lost response,
duplicate/reopen/reacquisition, stale snapshot, changed payload, transaction abort and bounded
retention. Synthetic ledger-budget rows prove budget behavior only. These tests do not execute
or qualify physical source operations, filesystem publication, power-loss durability, native
path/handle behavior or cross-database atomicity. Native acceptance should rerun the focused
fixtures on macOS/Windows without treating them as physical qualification.
