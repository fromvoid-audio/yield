# Retained publication native observations (OPT-2272)

The private original journal runtime can capture a nonserializable, non-Clone reader
only from its complete retained volume owner, protected reserve, bound production root
and current profile. Capture revalidates native ownership, actual UUID on macOS, current
marker and root, single-linked regular reserve object, allocation unit and the complete
512 MiB protected physical floor. Device-only volume fixtures cannot mint this reader.
Linux native capture remains unsupported; portable root observations grant no Native
permission.

A reader retains readonly reserve and directory observations and a readonly native
volume descriptor, never the original flock descriptor, Rc capacity ledger, profile guard
or child lease. Independent readonly/nonblocking marker opens avoid both ownership-lock
transfer and shared file offsets. The original actor retains mutation-capable resources.
The native volume descriptor must itself be readonly. The atomic observation fence
closes before actual owner handoff changes the marker and before the last original
volume-owner context alias releases its ownership lock.

Every sample requires a unique live ResourceUse from the exact original runtime Arc,
with a single observation per use. Sampling checks original admission before and after
native IO, marker/UUID/root/child identity and descriptor allocation before and after
fresh facts. A separately derived readonly profile-root descriptor and original
profile-lock identity are rechecked as well: transplanting an unchanged journal child
under a replacement profile root, or byte-identical lock replacement, refuses. Neither
witness retains profile/lock authority. IO holds no admission or slot mutex. Unknown native evidence invalidates
original resource admission; a known closure remains closed. Sealed facts are associated
with their original capsule and counted-use generation; IDs and VolumeFacts cannot
construct them. They authorize no namespace action, retry, refund or settlement.

At most 32 live original capsules are cached by bounded weak references; duplicates
perform fresh native proof before returning the original backing. A conflicting original
scope refuses, and only expired weak backing can be replaced. Duplicate proof also
compares the private original volume-owner fence Arc: same-profile dirty reacquisition
can retain marker generation and object identities but is a different owner incarnation. Each backing reserves
64 KiB from the existing journal ordinary 4 MiB input pool through its last alias or fact
holder. The weak registry reserves 32 KiB until original runtime teardown. The complete
retained backing is measured against the debit before installation, including all copied
path/string capacities and 32 KiB scratch allowance. Paths are limited to 4096 encoded
bytes and 64 components, identities to 256 bytes. One native sample per capsule at a time
bounds transient scratch; competing samples refuse without consuming their use. At most 32 retained fact headers per capsule fit this
allowance, even after their
counted uses drop. Header exhaustion refuses before consuming a use; last fact Drop
returns only header memory credit. Kernel calls already in progress cannot be
interrupted by closing admission; counted custody keeps original resources alive until
they return and the use is released.

Runtime closure prevents new original reads. Retained observation descriptors after
runtime teardown retain their memory debit but cannot resurrect ownership or a read use.
The full OPT-2215 issuer must account for these capsules, registry, custody header,
mirrors, slots and bindings inside its shared 2 MiB publication ceiling. This leaf uses
the existing 4 MiB input pool and does not claim that final composition is implemented.
Source/target/claim/capacity associations, all mutation fences, effect slots, completion
and verified settlement remain open in OPT-2215; backend and qualification remain open.

Portable tests exercise actual journal/profile/root custody, refusal/refunds, original
counted origin, replay/closure, real root/child/symlink/FIFO substitution, bounded deep
paths and absence of hidden child lease or profile-lock transfer. The macOS harness uses
actual native UUID, protected allocation and original owned profile/journal resources
with a disposable test registry. It covers fresh duplicate proofs, full facts, reserve,
root, marker and physical-floor failure, sticky unknown, expired reuse, memory retention
and original teardown. This test registry does not qualify the production per-user root.
OPT-2273 records exact SDK/runtime/production-root, pressure, UUID/unit change and
multi-volume verification requirements. These native cases have not run on Linux.
