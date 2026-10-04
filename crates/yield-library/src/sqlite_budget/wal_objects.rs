//! Read-only retained WAL-object identity. Never physical capacity qualification or a permit.
use super::*;
use crate::sample_sources::db::BoundDatabaseRoot;
use std::{path::Path, time::Instant};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WalObjectFacts {
    pub(crate) identity: String,
    pub(crate) device: u64,
    pub(crate) allocation_unit: u64,
    pub(crate) bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WalObjects {
    pub(crate) main: WalObjectFacts,
    pub(crate) wal: WalObjectFacts,
    pub(crate) shm: WalObjectFacts,
}

impl OwnedConnection {
    /// Capture only objects proven against this actual private forwarding VFS. The borrowed
    /// sole-owner connection keeps callback/mapping lifetimes fixed; filenames alone never
    /// prove SQLite sidecar binding. Unknown hosts/VFS/mappings return no observation.
    pub(crate) fn retained_wal_objects(
        &self,
        root: &BoundDatabaseRoot,
        name: &Path,
        deadline: Instant,
    ) -> Option<WalObjects> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (root, name, deadline);
            None
        }
        #[cfg(target_os = "linux")]
        {
            use std::{ffi::OsString, os::unix::fs::MetadataExt};
            if Instant::now() >= deadline
                || rusqlite::version_number() != 3_050_002
                || name.components().count() != 1
                || !matches!(
                    name.components().next(),
                    Some(std::path::Component::Normal(_))
                )
            {
                return None;
            }
            let registration = self.registration.as_ref()?;
            // SAFETY: the adapter has process lifetime; this private registration owns the
            // forwarding VFS and every file callback for the borrowed live connection.
            let native = unsafe { ffi::sqlite3_vfs_find(c"yield-linux-retained-root-v1".as_ptr()) };
            if native.is_null() || registration.context.native != native {
                return None;
            }
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
            let shm_metadata = shm.metadata().ok()?;
            if !mapped_identity(mapping, shm_metadata.dev(), shm_metadata.ino(), deadline)?
                || !native_identity(
                    self,
                    &registration.context,
                    ffi::SQLITE_FCNTL_FILE_POINTER,
                    0,
                    &main,
                )
                || !native_identity(
                    self,
                    &registration.context,
                    ffi::SQLITE_FCNTL_JOURNAL_POINTER,
                    1,
                    &wal,
                )
            {
                return None;
            }
            // Reopen all three current direct children and compare while original handles stay
            // retained. A mapped inode cannot be reused while SQLite's live mapping retains it.
            if facts(&root.open_file_read_nofollow(name).ok()?)? != result.main
                || facts(&root.open_file_read_nofollow(Path::new(&wal_name)).ok()?)? != result.wal
                || facts(&root.open_file_read_nofollow(Path::new(&shm_name)).ok()?)? != result.shm
                || Instant::now() >= deadline
            {
                return None;
            }
            Some(result)
        }
    }
}

#[cfg(target_os = "linux")]
fn facts(file: &std::fs::File) -> Option<WalObjectFacts> {
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let volume = crate::durable_capacity::descriptor_capacity_facts(file).ok()?;
    if volume.allocation_unit == 0
        || !volume.allocation_unit.is_power_of_two()
        || volume.allocation_unit > 1024 * 1024
    {
        return None;
    }
    Some(WalObjectFacts {
        identity: crate::filesystem_identity::stable_filesystem_identity_from_open_file(file)?,
        device: volume.identity.device,
        allocation_unit: volume.allocation_unit,
        bytes: metadata.len(),
    })
}

#[cfg(target_os = "linux")]
fn native_identity(
    connection: &Connection,
    context: &Context,
    operation: c_int,
    role: usize,
    expected: &std::fs::File,
) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = expected.metadata() else {
        return false;
    };
    let mut file: *mut ffi::sqlite3_file = ptr::null_mut();
    // SAFETY: SQLite initializes a borrowed exact associated wrapper file pointer. Its private
    // context and pinned native Unix prefix are checked before borrowing the native descriptor.
    unsafe {
        if ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            operation,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        ) != ffi::SQLITE_OK
            || file.is_null()
        {
            return false;
        }
        let Some(descriptor) = io::known_unix_descriptor(file, context, role) else {
            return false;
        };
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if libc::fstat(descriptor, stat.as_mut_ptr()) != 0 {
            return false;
        }
        let stat = stat.assume_init();
        stat.st_mode & libc::S_IFMT == libc::S_IFREG
            && stat.st_dev as u64 == metadata.dev()
            && stat.st_ino as u64 == metadata.ino()
            && u64::try_from(stat.st_size).ok() == Some(metadata.len())
    }
}

#[cfg(target_os = "linux")]
fn mapped_identity(address: usize, device: u64, inode: u64, deadline: Instant) -> Option<bool> {
    use std::io::Read;
    const MAX_MAP_BYTES: u64 = 2 * 1024 * 1024;
    let mut bytes = Vec::new();
    // Kernel observation only. Bounded whole read; never interpret a displayed pathname as
    // object identity and never open the mapping's target or inspect arbitrary process memory.
    std::fs::File::open("/proc/self/maps")
        .ok()?
        .take(MAX_MAP_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_MAP_BYTES || Instant::now() >= deadline {
        return None;
    }
    let maps = std::str::from_utf8(&bytes).ok()?;
    let mut found = false;
    for line in maps.lines() {
        if Instant::now() >= deadline {
            return None;
        }
        let mut fields = line.split_ascii_whitespace();
        let range = fields.next()?;
        let permissions = fields.next()?;
        let offset = u64::from_str_radix(fields.next()?, 16).ok()?;
        let (major, minor) = fields.next()?.split_once(':')?;
        let mapped_inode: u64 = fields.next()?.parse().ok()?;
        let (start, end) = range.split_once('-')?;
        let start = usize::from_str_radix(start, 16).ok()?;
        let end = usize::from_str_radix(end, 16).ok()?;
        if !(start..end).contains(&address) {
            continue;
        }
        if found
            || permissions != "rw-s"
            || address.checked_add(32768)? > end
            || offset.checked_add(u64::try_from(address.checked_sub(start)?).ok()?)? != 0
        {
            return None;
        }
        let major = u32::from_str_radix(major, 16).ok()?;
        let minor = u32::from_str_radix(minor, 16).ok()?;
        if libc::makedev(major, minor) as u64 != device || mapped_inode != inode {
            return Some(false);
        }
        found = true;
    }
    Some(found)
}

#[cfg(test)]
mod tests;
