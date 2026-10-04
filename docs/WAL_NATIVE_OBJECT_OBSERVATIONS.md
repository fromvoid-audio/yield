# Original native WAL-object observations

OPT-2264 extends the existing private forwarding-VFS observation route to macOS.
It captures the original retained MAIN/WAL descriptors and current region-zero
SHM mapping. It grants no physical allocation, required permit, admission,
Registered acknowledgement, retry/refund, lease release or settlement.

The original `OwnedConnection` keeps SQLite/native callback lifetimes fixed.
Capture validates the private wrapper method, context and role before reading
the pinned bundled SQLite 3.50.2 Unix file prefix. Both version and exact
SQLite source ID are checked. Linux requires its existing retained-root VFS;
macOS requires the original bundled `unix` parent. Native descriptor `fstat`
must match each no-follow retained file's device, inode and length. The final
root-relative reopen must still identify the same three distinct objects.
Unknown VFS, wrapper, root, source version or replaced objects refuse.

Linux continues scanning the complete bounded `/proc/self/maps` stream with
fixed backing. macOS queries the current process's original mapping address
with libproc `PROC_PIDREGIONPATHINFO`. The kernel returns region data and a
vnode identity pinned by its vnode ID. The adapter does not parse, trust or
reopen the accompanying pathname buffer. Non-vnode/failed queries, unknown
record sizes, private/copy-on-write/submap regions, incorrect protection,
offset, address/span, type, device, inode or length remain unknown. The original
deadline is checked before and after the query; it is cooperative and cannot
forcibly interrupt a stalled operating-system call.

The fixed public `proc_regioninfo` prefix is 96 bytes. The native record uses
the pinned libc 0.2.189 `vnode_info_path`; compile-time assertions keep the
record plus scalar vnode comparison within the existing three 512-byte global
native scratch allowance. The initialized record is borrowed, without another
record-sized copy or heap buffer. Existing source/global and combined handshake
ceilings remain unchanged.

The reviewed kernel contract is Apple's XNU
`f6217f891ac0bb64f3d375211650a4c1ff8ca1ea`:

- `bsd/sys/proc_info.h`: region/vnode record ABI and known flags/share modes.
- `bsd/kern/proc_info.c`: exact returned record size and `vnode_getwithvid`.
- `osfmk/vm/bsd_vm.c`: region address, file offset, shared/submap flags and vnode lookup.

The source review and parsing fixtures are contract evidence. They do not qualify
the executing kernel or filesystem. Actual Linux tests exercise original SQLite
objects and same-size replacement rejection. The actual private macOS module
cross-checks on both x86_64 and aarch64 Darwin against the pinned libc, including
its ABI/buffer assertions. Full Darwin library checking on this Linux host stops
at bundled SQLite C compilation because the host compiler lacks Darwin SDK/target
support. Native runtime and full native build verification are tracked by
OPT-2265; no forced positive or zero-executed-test result substitutes for them.

Windows original object/mapping support remains unimplemented and refuses. Full
OPT-2194 qualified physical write policy/exact direction slices and OPT-2136
current Registered quorum/sealed decision/atomic intent/uncertainty acceptance
remain open. Native identity observation is a prerequisite to those contracts.
