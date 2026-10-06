//! Pinned win32 file/SHM bindings. Readonly observations, never allocation authority.
use super::*;

// sqlite3.c 3.50.2: complete unconditional prefixes through the fields we read.
// WinCE/unknown pointer width and changed SQLite source IDs refuse in io.rs.
#[repr(C)]
pub(in crate::sqlite_budget) struct WinFilePrefix {
    pub(in crate::sqlite_budget) methods: *const ffi::sqlite3_io_methods,
    pub(in crate::sqlite_budget) vfs: *mut ffi::sqlite3_vfs,
    handle: *mut c_void,
    lock_type: u8,
    shared_lock_byte: i16,
    control_flags: u8,
    last_errno: u32,
    shm: *mut WinShm,
}
impl WinFilePrefix {
    pub(in crate::sqlite_budget) fn valid_handle(&self) -> bool {
        valid_handle(self.handle)
    }
}
fn valid_handle(handle: *mut c_void) -> bool {
    !handle.is_null() && handle as usize != usize::MAX
}
#[repr(C)]
struct WinShm {
    node: *mut WinShmNodePrefix,
    shared_mask: u16,
    exclusive_mask: u16,
    handle: *mut c_void,
    readonly: c_int,
}
#[repr(C)]
struct WinShmNodePrefix {
    mutex: *mut ffi::sqlite3_mutex,
    filename: *mut c_char,
    handle: *mut c_void,
    unlocked: c_int,
    readonly: c_int,
    region_size: c_int,
    region_count: c_int,
    regions: *mut ShmRegion,
}
#[repr(C)]
struct ShmRegion {
    handle: *mut c_void,
    mapping: *mut c_void,
}

struct MappingShape {
    base: usize,
    allocation_base: usize,
    bytes: usize,
    protect: u32,
    allocation_protect: u32,
    state: u32,
    kind: u32,
}
impl MappingShape {
    fn matches(&self, address: usize) -> bool {
        address != 0
            && self.base == address
            && self.allocation_base == address
            && self.bytes >= 32768
            && address.checked_add(self.bytes).is_some()
            && self.protect == 4 // PAGE_READWRITE, no guard/private/execute flags
            && self.allocation_protect == 4
            && self.state == 0x1000 // MEM_COMMIT
            && self.kind == 0x40000 // MEM_MAPPED
    }
}

#[cfg(windows)]
fn handle_facts(handle: *mut c_void) -> Option<WalObjectFacts> {
    use std::{mem::ManuallyDrop, os::windows::io::FromRawHandle};
    if !valid_handle(handle) {
        return None;
    }
    // SAFETY: the original SQLite connection (or locked shared node) retains this
    // borrowed handle. ManuallyDrop prevents taking ownership/closing it. Only
    // readonly descriptor queries occur before the borrow ends.
    let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_handle(handle) });
    facts(&file)
}

#[cfg(windows)]
fn associated_file(
    connection: &OwnedConnection,
    context: &Context,
    operation: c_int,
    role: usize,
) -> Option<*mut WinFilePrefix> {
    let mut file = ptr::null_mut();
    // SAFETY: SQLite supplies its actual associated wrapper; the original private
    // header/context/role/native source are checked before any native cast.
    unsafe {
        (ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            operation,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        ) == ffi::SQLITE_OK)
            .then_some(())?;
        io::known_windows_file(file, context, role)
    }
}

#[cfg(windows)]
unsafe fn original_shm_matches(
    main: *mut WinFilePrefix,
    address: usize,
    expected: &WalObjectFacts,
) -> Option<()> {
    use ::windows::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    struct LockedNode(*mut ffi::sqlite3_mutex);
    impl Drop for LockedNode {
        fn drop(&mut self) {
            // SAFETY: exactly the mutex acquired below, while original connection is live.
            unsafe { ffi::sqlite3_mutex_leave(self.0) };
        }
    }
    // SAFETY: checked original live winFile owns winShm; it retains this shared
    // node through close. Other connections can extend its region array, so take
    // the actual node mutex before reading any mutable shared-node field.
    unsafe {
        let shm = (*main).shm.as_ref()?;
        if shm.readonly != 0 || !valid_handle(shm.handle) || shm.node.is_null() {
            return None;
        }
        let mutex = (*shm.node).mutex;
        if mutex.is_null() {
            return None;
        }
        ffi::sqlite3_mutex_enter(mutex);
        let _locked = LockedNode(mutex);
        let node = &*shm.node;
        if node.unlocked != 0
            || node.readonly != 0
            || node.region_size != 32768
            || !(1..=4096).contains(&node.region_count)
            || node.regions.is_null()
            || !valid_handle(node.handle)
        {
            return None;
        }
        let region = &*node.regions;
        if region.mapping as usize != address || !valid_handle(region.handle) {
            return None;
        }
        let mut memory = MEMORY_BASIC_INFORMATION::default();
        let returned = VirtualQuery(
            Some(region.mapping.cast_const()),
            &mut memory,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        );
        if returned != std::mem::size_of::<MEMORY_BASIC_INFORMATION>()
            || !(MappingShape {
                base: memory.BaseAddress as usize,
                allocation_base: memory.AllocationBase as usize,
                bytes: memory.RegionSize,
                protect: memory.Protect.0,
                allocation_protect: memory.AllocationProtect.0,
                state: memory.State.0,
                kind: memory.Type.0,
            })
            .matches(address)
            || handle_facts(node.handle)? != *expected
            || handle_facts(shm.handle)? != *expected
        {
            return None;
        }
        Some(())
    }
}

#[cfg(windows)]
pub(super) fn retained_wal_objects(
    connection: &OwnedConnection,
    root: &BoundDatabaseRoot,
    name: &Path,
    deadline: &mut super::ObservationDeadline,
) -> Option<WalObjects> {
    use std::ffi::OsString;
    if deadline.expired()
        || name.components().count() != 1
        || !matches!(
            name.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return None;
    }
    let registration = connection.registration.as_ref()?;
    let mapping = registration
        .context
        .state
        .lock()
        .ok()?
        .mapped_shm_region_zero?;
    let mut wal_name = OsString::from(name);
    wal_name.push("-wal");
    let mut shm_name = OsString::from(name);
    shm_name.push("-shm");
    let main = root.open_file_read_nofollow(name).ok()?;
    let wal = root.open_file_read_nofollow(Path::new(&wal_name)).ok()?;
    let shm = root.open_file_read_nofollow(Path::new(&shm_name)).ok()?;
    let result = WalObjects {
        main: facts(&main)?,
        wal: facts(&wal)?,
        shm: facts(&shm)?,
    };
    if result.main.bytes == 0
        || result.shm.bytes < 32768
        || result.shm.bytes % 32768 != 0
        || result.main.identity == result.wal.identity
        || result.main.identity == result.shm.identity
        || result.wal.identity == result.shm.identity
    {
        return None;
    }
    let native_main = associated_file(
        connection,
        &registration.context,
        ffi::SQLITE_FCNTL_FILE_POINTER,
        0,
    )?;
    let native_wal = associated_file(
        connection,
        &registration.context,
        ffi::SQLITE_FCNTL_JOURNAL_POINTER,
        1,
    )?;
    // SAFETY: exact private wrapper/role/native source validation above; sole
    // borrowed owner prevents native close. Shared mutable SHM is locked separately.
    unsafe {
        if handle_facts((*native_main).handle)? != result.main
            || handle_facts((*native_wal).handle)? != result.wal
        {
            return None;
        }
        original_shm_matches(native_main, mapping, &result.shm)?;
    }
    if facts(&root.open_file_read_nofollow(name).ok()?)? != result.main
        || facts(&root.open_file_read_nofollow(Path::new(&wal_name)).ok()?)? != result.wal
        || facts(&root.open_file_read_nofollow(Path::new(&shm_name)).ok()?)? != result.shm
        || deadline.expired()
    {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_windows_prefix_offsets_match_reviewed_64_bit_layout() {
        if std::mem::size_of::<usize>() != 8 {
            return;
        }
        assert_eq!(std::mem::offset_of!(WinFilePrefix, handle), 16);
        assert_eq!(std::mem::offset_of!(WinFilePrefix, shm), 40);
        assert_eq!(std::mem::size_of::<WinFilePrefix>(), 48);
        assert_eq!(std::mem::offset_of!(WinShm, handle), 16);
        assert_eq!(std::mem::offset_of!(WinShmNodePrefix, handle), 16);
        assert_eq!(std::mem::offset_of!(WinShmNodePrefix, regions), 40);
        assert_eq!(std::mem::size_of::<ShmRegion>(), 16);
    }
    #[test]
    fn unknown_handles_and_private_guarded_or_short_mapping_refuse() {
        assert!(!valid_handle(ptr::null_mut()));
        assert!(!valid_handle(usize::MAX as *mut c_void));
        let address = 0x10000;
        let shape = || MappingShape {
            base: address,
            allocation_base: address,
            bytes: 32768,
            protect: 4,
            allocation_protect: 4,
            state: 0x1000,
            kind: 0x40000,
        };
        assert!(shape().matches(address));
        assert!(!shape().matches(0));
        for field in 0..8 {
            let mut bad = shape();
            match field {
                0 => bad.base += 1,
                1 => bad.allocation_base += 1,
                2 => bad.bytes = 32767,
                3 => bad.bytes = usize::MAX,
                4 => bad.protect |= 0x100,
                5 => bad.allocation_protect = 2,
                6 => bad.state = 0x2000,
                _ => bad.kind = 0x20000,
            }
            assert!(!bad.matches(address));
        }
    }
}
