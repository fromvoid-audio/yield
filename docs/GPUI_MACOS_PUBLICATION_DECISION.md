# macOS staged publication decision (OPT-1474)

Decision, 2026-09-12: retain unsupported production staged publication and
staged cleanup on macOS. This resolves the platform contract decision; it does
not implement macOS destructive editing or certify the complete recovery owner.

## Threat boundary

The existing `IO_ARCHITECTURE_TARGET.md` invariants 19–22 remain authoritative.
A source or destination namespace may change between validation and mutation,
including through another process running as the same user. A held directory
descriptor anchors traversal, but does not freeze the entries inside it.
Ownership must derive from the transaction's verified object, not a filename,
matching bytes, a UUID suffix, or a successful reopen alone.

We do not adopt an exclusive staging namespace trust assumption. Mode 0700
directories and cooperative locks do not exclude uncooperating processes with
the same user authority. Accepting that assumption would change the contract,
rather than qualify the current implementation.

## Primitive assessment

Apple's [rename API](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/rename.2)
and [declarations](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/sys/stdio.h)
provide directory-relative names and exclusive destination creation through
`renameatx_np` with `RENAME_EXCL`. They do not take the held staged-file identity
as the source operand. Substituting that entry before the call can publish a
different object. A post-call check cannot undo the exposure safely.

Apple's [clone API](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/clonefile.2)
allows `fclonefileat` to bind the source descriptor and requires an absent
destination. It returns success or failure, not an opened destination handle.
Under this contract, a subsequent pathname reopen cannot establish that the
reopened object is the object created by this transaction. Identical content
does not establish provenance. These are contract assessments of the documented
interfaces, not claims that ordinary atomic file saves are impossible on macOS.

No inspected primitive qualifies identity-bound cleanup either. A check followed
by pathname unlink remains a substitution window. Publication and cleanup are
separate capabilities; neither is inferred from the other. Synchronization,
crash ordering, and durable journal ownership are additional requirements.

## Implementation and downstream boundary

The library exposes a side-effect-free platform finalization capability query
with independent publication and cleanup results. Production recovery consumes
the same policy. Primitive availability on Windows is not a promise that every
volume, transaction, ownership route, or durability requirement is qualified.

Unsupported operations return an error and retain staged data and journal
evidence. They must not report publication success, remove source evidence,
fall back to pathname cleanup, or create undo authority. Unpublished WAV
preparation remains independent and usable. Test-only finalizers are not
production primitive qualification.

OPT-1475, OPT-1479, and OPT-1500 must carry this explicit platform limitation
through service admission and UI outcomes. Full macOS mutation parity remains
unavailable until a separately reviewed contract or qualified primitive changes
this decision. OPT-1360 retains broad durability and SLO certification.

## Validation

On macOS, `cargo test --locked -p yield-library file_ops_journal --lib`
passed all 26 tests. Production-finalizer regressions cover repeated unsupported
publication and cleanup, missing/replaced target and source roots, and a late
unrelated destination collision. They assert retained staging identity/bytes,
journal state, source metadata, and unrelated destination evidence. The
`finalization` test filter passed 13 tests (overlapping the recovery suite).
The library check, workspace formatting check, and diff check passed.

Independent ownership review found no actionable issue. Windows primitives
were not changed; Windows-specific compilation and runtime qualification were
not rerun for this decision. These tests establish fail-closed retention, not
successful macOS publication or power-loss durability.
