# Original WAL factory prepayment

The three `WalPreparationInput` factories require a `WalPreparationFunding`
token issued by `OperationJournalClient::reserve_wal_preparation` before they
serialize, validate or derive topology. The token prepays the existing complete
64 KiB handshake envelope from that journal's ordinary input pool. The existing
4 MiB total and 64 KiB control margin are unchanged.

A token cannot be constructed, cloned or deserialized by callers. It is memory
custody, not physical capacity or authority to write. Queueing checks the exact
original owner and rechecks admission under its gate, then transfers the same
debit. It does not reserve a second envelope. Queue/result saturation, closure,
foreign-owner rejection, factory error and unused-input Drop destroy their
backing before the final debit returns.

The original factory budget retains the prepaid debit from its creation,
including diagnostic aliases held before queueing. At collection, the same
private `JournalInput` travels through the existing collector, reports, current
rounds and terminal handles. Private wrappers free their final Arc allocation
before returning credit. No raw Arc or Weak escapes. Local scratch accounting
still enforces the complete 64 KiB bound; prepayment does not enlarge it or
relax validation.

The public call order is:

```rust,ignore
let funding = journal.client().reserve_wal_preparation()?;
let input = WalPreparationInput::from_universal(
    funding, &args, &participants, &plan, &claims, sources, global, deadline,
)?;
let request = journal.client().prepare_wal_owners(input)?;
```

Original worker tests cover pool-pressure refusal before factory invocation,
factory rejection and prequeue alias retention, queue transfer with zero
remaining ordinary bytes, drain and exact-owner rejection, and the existing
lost-reply/terminal/report custody cases. Public factory SQLite integration
tests now explicitly require `test-support` memory-only journal startup: Linux
production bootstrap cannot supply a qualified reserve. This seam does not
supply native capacity, source registration, intent, permits or settlement.
Normal-build startup refusal is tested separately; production methods compile
without test support. Actual native production bootstrap/positive qualification
remain in OPT-2393/2405 and the existing platform gates. Full OPT-2194/2136 remain
open.
