//! Original kernel vnode-backed SHM observation; path text never establishes identity.
use super::ObservationDeadline;

#[cfg(target_os = "macos")]
#[link(name = "proc")]
unsafe extern "C" {}

// Apple's bsd/sys/proc_info.h, proc_regioninfo. Its fixed public C prefix is
// reviewed against xnu f6217f891ac0bb64f3d375211650a4c1ff8ca1ea. Unknown record
// sizes refuse. The original kernel resolves/pins the mapped vnode by its vid;
// a failed/non-vnode query leaves zero vnode fields and cannot pass validation.
#[repr(C)]
#[derive(Default)]
struct RegionInfo {
    protection: u32,
    max_protection: u32,
    inheritance: u32,
    flags: u32,
    offset: u64,
    behavior: u32,
    user_wired_count: u32,
    user_tag: u32,
    pages_resident: u32,
    pages_shared_now_private: u32,
    pages_swapped_out: u32,
    pages_dirtied: u32,
    ref_count: u32,
    shadow_depth: u32,
    share_mode: u32,
    private_pages_resident: u32,
    shared_pages_resident: u32,
    object_id: u32,
    depth: u32,
    address: u64,
    size: u64,
}
// Derived only from the actual kernel vnode_info, never an opened pathname.
struct VnodeIdentity {
    device: u64,
    inode: u64,
    bytes: i64,
    mode: u16,
    kind: i32,
}
fn matches(
    region: &RegionInfo,
    vnode: &VnodeIdentity,
    address: usize,
    device: u64,
    inode: u64,
    bytes: u64,
    deadline: &mut ObservationDeadline,
) -> Option<bool> {
    let address = u64::try_from(address).ok()?;
    let end = region.address.checked_add(region.size)?;
    let required_end = address.checked_add(32768)?;
    // Known public constants: VM_PROT_READ|WRITE, PROC_REGION_SHARED (no submap),
    // SM_SHARED/TRUESHARED/SHARED_ALIASED, VREG and S_IFREG. Unknown shapes refuse.
    if address == 0
        || inode == 0
        || bytes < 32768
        || bytes % 32768 != 0
        || region.address != address
        || required_end > end
        || region.offset != 0
        || region.protection != 3
        || region.max_protection & 3 != 3
        || region.flags != 2
        || !matches!(region.share_mode, 4 | 5 | 7)
        || vnode.kind != 1
        || vnode.mode & 0o170000 != 0o100000
        || vnode.device != device
        || vnode.inode != inode
        || u64::try_from(vnode.bytes).ok() != Some(bytes)
        || deadline.expired()
    {
        return None;
    }
    Some(true)
}

#[cfg(target_os = "macos")]
pub(super) fn mapped_identity(
    address: usize,
    device: u64,
    inode: u64,
    bytes: u64,
    deadline: &mut ObservationDeadline,
) -> Option<bool> {
    // proc_regionwithpathinfo: use libc's reviewed native vnode layout. The path
    // buffer is unavoidable fixed API backing; it is never parsed or reopened.
    #[repr(C)]
    struct RegionWithPath {
        region: RegionInfo,
        vnode: libc::vnode_info_path,
    }
    // Existing global parked scratch includes three 512-byte native prefixes.
    // Keep this actual query backing within that same ceiling, never a new pool.
    const _: () = assert!(
        std::mem::size_of::<RegionWithPath>() + std::mem::size_of::<VnodeIdentity>() <= 3 * 512
    );
    const _: () = assert!(std::mem::size_of::<RegionInfo>() == 96);
    if deadline.expired() || address == 0 {
        return None;
    }
    let mut record = std::mem::MaybeUninit::<RegionWithPath>::zeroed();
    let size = i32::try_from(std::mem::size_of::<RegionWithPath>()).ok()?;
    // SAFETY: exact fixed C ABI record and capacity are supplied for this process's
    // original live region-zero mapping. A successful exact-size reply initializes
    // the full record; kernel errors, short/changed ABI and deadline all refuse.
    let returned = unsafe {
        libc::proc_pidinfo(
            libc::getpid(),
            8, // PROC_PIDREGIONPATHINFO
            u64::try_from(address).ok()?,
            record.as_mut_ptr().cast(),
            size,
        )
    };
    if returned != size || deadline.expired() {
        return None;
    }
    // SAFETY: the zero-initialized exact fixed record was filled by the kernel.
    let record = unsafe { record.assume_init_ref() };
    let stat = &record.vnode.vip_vi.vi_stat;
    matches(
        &record.region,
        &VnodeIdentity {
            // Match MetadataExt/fstat's signed native dev_t conversion exactly.
            device: stat.vst_dev as libc::dev_t as u64,
            inode: stat.vst_ino,
            bytes: stat.vst_size,
            mode: stat.vst_mode,
            kind: record.vnode.vip_vi.vi_type,
        },
        address,
        device,
        inode,
        bytes,
        deadline,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn darwin_mapping_record_rejects_aliases_private_regions_and_unknown_identity() {
        // Contract records exercise parsing only. They cannot call the original
        // native producer, create a WAL observation or qualify any host/filesystem.
        let mut region = RegionInfo {
            protection: 3,
            max_protection: 7,
            flags: 2,
            share_mode: 4,
            address: 0x10000,
            size: 32768,
            ..RegionInfo::default()
        };
        let mut vnode = VnodeIdentity {
            device: 7,
            inode: 42,
            bytes: 32768,
            mode: 0o100600,
            kind: 1,
        };
        let check = |r: &RegionInfo, v: &VnodeIdentity| {
            matches(
                r,
                v,
                0x10000,
                7,
                42,
                32768,
                &mut ObservationDeadline::new(Instant::now() + Duration::from_secs(2)),
            )
        };
        assert_eq!(std::mem::size_of::<RegionInfo>(), 96);
        assert_eq!(check(&region, &vnode), Some(true));
        region.offset = 4096;
        assert_eq!(check(&region, &vnode), None);
        region.offset = 0;
        for mode in [1, 2, 3, 6, 8, 9] {
            region.share_mode = mode;
            assert_eq!(check(&region, &vnode), None);
        }
        region.share_mode = 4;
        for flags in [0, 1, 3, 4] {
            region.flags = flags;
            assert_eq!(check(&region, &vnode), None);
        }
        region.flags = 2;
        region.protection = 7;
        assert_eq!(check(&region, &vnode), None);
        region.protection = 3;
        region.address -= 1;
        assert_eq!(check(&region, &vnode), None);
        region.address += 1;
        region.size -= 1;
        assert_eq!(check(&region, &vnode), None);
        region.size += 1;
        vnode.inode += 1;
        assert_eq!(check(&region, &vnode), None);
        vnode.inode -= 1;
        vnode.device += 1;
        assert_eq!(check(&region, &vnode), None);
        vnode.device -= 1;
        vnode.bytes = -1;
        assert_eq!(check(&region, &vnode), None);
        vnode.bytes = 32768;
        vnode.kind = 0;
        assert_eq!(check(&region, &vnode), None);
        vnode.kind = 1;
        vnode.mode = 0o040600;
        assert_eq!(check(&region, &vnode), None);
        vnode.mode = 0o100600;
        assert_eq!(
            matches(
                &region,
                &vnode,
                0x10000,
                7,
                42,
                32768,
                &mut ObservationDeadline::new(Instant::now())
            ),
            None
        );
        region.address = u64::MAX - 10;
        assert_eq!(check(&region, &vnode), None);
    }
}
