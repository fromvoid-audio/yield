# Linux volume-owner restart fixtures

`flock` ownership belongs to an open-file description. A child created by `fork`
inherits that description, including its lock, until the inherited descriptor
closes. `FD_CLOEXEC` closes it at exec, not at fork. Closing every parent
descriptor therefore cannot guarantee immediate reacquisition while an unrelated
fork child is between fork and exec. A unique temporary path does not isolate
this descriptor inheritance.

OPT-2178 follows the parallel library failure in
`clean_handoff_fences_the_runtime_before_unlock`. The synchronized
`fork_retains_actual_owner_lock_until_exec` regression reproduces the same
restart rejection using the actual retained volume-owner capability: clean and
fence the owner, fork a pipe-gated child, close all parent contexts, observe the
typed busy rejection, let the child exec, wait for its exit, then acquire once
and verify the new generation. It also checks that one context's drop cannot
unlock a remaining cloned context. This establishes a concrete inheritance
mechanism; the original failed run did not record which concurrent child held
the descriptor.

Eight existing close/reacquire fixtures execute their original bodies in separate
Linux test processes, filtered to exactly one case with one test thread. The
parent checks the child exit status with a 30-second deadline; timeout kills and
reaps the child, including parent panic paths. A private entry marker proves the
exact original test body ran; a negative case rejects a successful zero-test
selection. No case is ignored and acquisition is not retried. The fork
regression has its own bounded child synchronization and cleanup. Other libtest
cases can launch subprocesses without inheriting these child-owned lock files.

OPT-2338 extends the same scheduling rule to the original native-reader alias
fence and old-v1 dirty-owner compatibility cases. The latter failed in a fresh
main default-parallel run: after the final old parent descriptor closed,
reacquisition returned `OwnedByAnotherProcess` on its unique temporary lock
instead of observing the retained dirty marker. The assertion now reports the
actual error. The synchronized fork regression establishes the descriptor
inheritance mechanism; the failed parallel run does not identify its concurrent
child. Both original bodies, including marker preservation and final alias
fencing, execute once in the exact marked child; no production policy changes.

Production ownership, clone lifetime, fencing, clean markers and lock release
remain unchanged. In particular, explicitly unlocking on each capability drop
would incorrectly release another retained context's shared lock. Runtime
acquisition still reports actual contention conservatively. These small
descriptor fixtures provide no protected-capacity, storage-durability or physical
qualification evidence; OPT-1851/1681 retain their full acceptance.

Run the focused cases with:

```sh
cargo test --locked -p yield-library --lib operation_journal::capacity_volume_owner::tests
```

The full default-parallel and serialized library suites must also pass, followed
by canonical `bash scripts/agent.sh preflight` on the final integrated commit.
