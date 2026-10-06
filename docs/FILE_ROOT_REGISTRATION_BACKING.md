# Original FileOwner root backing (OPT-2398)

The original file worker uses the existing 128-root policy and input pool. It
prepays its exact fixed slot array before allocating it. Removing a registration
destroys its capabilities, labels and fences before returning their original
credit. Worker teardown destroys the slot allocation before refunding its debit.
No tree-node growth or new memory pool is introduced.

Profile/source registration prepays cloned Rust metadata before making additional
root/profile capabilities. Each original FileOwner Source capability conservatively
funds the bounded request graph that it retains, including shared Arc allocations;
a journal Current keeper need not survive after journal actor teardown. Credit-last
construction custody destroys that graph before refund on every early clone error.
Profile-root namespace conversion uses the existing finite native bounds;
actual retained metadata is checked against the construction debit before shrinking
temporary margin. This accounts for owned Rust heap allocations and fixed headers.
The Windows query's fixed 8,194-byte stack buffer is original callback/thread stack,
outside this heap accounting; its 16,388-byte heap debit covers namespace conversion
and shrink overlap. Kernel descriptor and native allocator storage are separate.

The worker retains the original queued command debit in each accepted registration.
Definite refusal destroys original root payloads before sending a bounded funded
diagnostic. That diagnostic remains funded through queued or abandoned replies; the
caller receives its own copy rather than the original owner payload. A definite
success-sender rejection removes its fresh registration. Success queued before the
caller abandons its receiver is unknown: original registration and credit remain
until explicit retirement or actual worker teardown. Lost replies supply no refund
or settlement evidence.

Publication aliases retain privately wrapped root liveness. All strong aliases use
`Arc::into_inner` on drop, and no raw Arc or Weak escapes. The last alias frees the
actual Arc allocation and implicit Weak header before dropping its embedded debit.
Retirement closes liveness immediately but cannot refund backing held by another
original publication alias. Liveness is an observation, never effect authority.

Portable acceptance exercises original actor registration, duplicate/refusal and
exhaustion, abandoned replies, retirement, genuine publication alias retention,
concurrent last-alias drops and worker teardown. The existing production native
source registration harness remains separate runtime verification on macOS/Windows.
Cross-compilation does not execute native callbacks or qualify allocation policy.

This supplies no cleanup/restore grant, verified absence/adoption, namespace effect,
durable filesystem checkpoint, capacity refund/retry, settlement or physical
qualification. OPT-1633/1635/1665/1670/2215 retain their full acceptance.
