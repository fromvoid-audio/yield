# Linux mount observation for capacity qualification

`scripts/qa/linux_mount_probe.c` is a read-only diagnostic for the first Linux
capacity authority investigation. It obtains a metadata-only descriptor with
`O_PATH`, rejects a final symlink and compares descriptor metadata with the
matching current mount row. Linux 6.8 introduced a mount ID unique until reboot.
The older mount ID matches the mount table and may be reused after unmount.
The probe uses both and checks the unique ID again after reading the mount table.
See the [Linux statx ABI](https://github.com/torvalds/linux/blob/v6.8/include/uapi/linux/stat.h),
[kernel mountinfo documentation](https://www.kernel.org/doc/html/v6.8/filesystems/proc.html)
and [Linux O_PATH contract](https://github.com/mkerrisk/man-pages/blob/master/man2/open.2).

The first candidate requires a writable ext4 root mount, consistent major and
minor numbers, the ext-family magic reported by the held descriptor, and a
device-shaped mount source. The source field is descriptive kernel mount
metadata. A candidate still needs independent block-device and durable volume
identity evidence. Mount IDs and device numbers cannot bind a durable registry
record across remounts and reboot.

Build on Linux with the system C compiler.

```sh
cc -std=c17 -Wall -Wextra -Werror scripts/qa/linux_mount_probe.c -o /tmp/yield-linux-mount-probe
/tmp/yield-linux-mount-probe --self-test
/tmp/yield-linux-mount-probe /path/to/disposable/ext4/root
```

Run the descriptor probe against a file and its directory on the same disposable
ext4 mount. Compare their mount IDs and device numbers. Use a different mount
to check distinct IDs, an overlay and a read-only mount to check refusal, and a
fresh unmount and mount of the disposable volume to check that the unique mount
ID changes. A missing kernel field or inconsistent mount row must also refuse.
Only use resources prepared for the test. Exit 0 identifies a candidate for
further qualification, exit 2 records refusal, and exit 1 indicates a usage
error or failed parser fixture.

Separate work must qualify durable volume identity, account and quota accounting,
protected reserve allocation, and the registry and recovery owner before Yield
can issue Linux write authority.
