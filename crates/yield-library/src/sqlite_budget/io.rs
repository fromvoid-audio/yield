//! FFI adapters never hold the accounting mutex across a native callback.
use super::*;
#[repr(C)]
struct Header {
    base: ffi::sqlite3_file,
    context: *const Context,
    role: usize,
}
pub(super) const HEADER_BYTES: usize = (std::mem::size_of::<Header>() + 7) & !7;
// SAFETY for callback bodies: SQLite supplies live wrapper files/VFS objects, sized buffers
// and its documented arguments. Registration outlives every native callback. We never patch
// the native file's methods; it occupies its own aligned szOsFile bytes after our header.
unsafe fn header<'a>(f: *mut ffi::sqlite3_file) -> &'a Header {
    unsafe { &*f.cast::<Header>() }
}
unsafe fn inner(f: *mut ffi::sqlite3_file) -> *mut ffi::sqlite3_file {
    unsafe { f.cast::<u8>().add(HEADER_BYTES).cast() }
}
unsafe fn context<'a>(v: *mut ffi::sqlite3_vfs) -> &'a Context {
    unsafe { &*(*v).pAppData.cast::<Context>() }
}
unsafe fn methods<'a>(f: *mut ffi::sqlite3_file) -> &'a ffi::sqlite3_io_methods {
    unsafe { &*(*inner(f)).pMethods }
}
unsafe extern "C" fn open(
    v: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    f: *mut ffi::sqlite3_file,
    flags: c_int,
    out: *mut c_int,
) -> c_int {
    unsafe {
        let ctx = context(v);
        let role = if flags & ffi::SQLITE_OPEN_MAIN_DB != 0 {
            0
        } else if flags & ffi::SQLITE_OPEN_WAL != 0 {
            1
        } else if flags & ffi::SQLITE_OPEN_MAIN_JOURNAL != 0 {
            3
        } else {
            4
        };
        if name.is_null() && role != 4 {
            return ffi::SQLITE_CANTOPEN;
        }
        if ctx.no_effect.load(Ordering::Acquire)
            && (role != 0
                || flags & (ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_DELETEONCLOSE) != 0
                || flags & ffi::SQLITE_OPEN_READWRITE == 0)
        {
            return ffi::SQLITE_READONLY;
        }
        if role == 0 {
            if ctx
                .main
                .compare_exchange(ptr::null_mut(), f, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return ffi::SQLITE_CANTOPEN;
            }
            let Ok(mut s) = ctx.state.lock() else {
                return ffi::SQLITE_CANTOPEN;
            };
            s.main_name = CStr::from_ptr(name).to_bytes().to_vec();
        } else if role == 1 || role == 3 {
            // SQLite documents this call only for its own WAL/main-journal xOpen filenames.
            // The returned object, not a matching pathname suffix, proves exact parent association.
            let main = ffi::sqlite3_database_file_object(name);
            if main.is_null()
                || main != ctx.main.load(Ordering::SeqCst)
                || header(main).context != ctx
            {
                return ffi::SQLITE_CANTOPEN;
            }
        }
        if role > 1 && ctx.restricted() {
            return ctx.charge(role, 1);
        }
        if role >= 3 {
            let Ok(mut s) = ctx.state.lock() else {
                return ffi::SQLITE_CANTOPEN;
            };
            let Some(n) = s.open_auxiliary.checked_add(1) else {
                return ffi::SQLITE_CANTOPEN;
            };
            s.open_auxiliary = n;
        }
        let native = ctx.native;
        let Some(native_open) = (*native).xOpen else {
            return ffi::SQLITE_CANTOPEN;
        };
        #[cfg(test)]
        if role != 0 || flags & (ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_DELETEONCLOSE) != 0 {
            ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
        }
        let rc = native_open(native, name, inner(f), flags, out);
        if rc != ffi::SQLITE_OK {
            if role == 0 {
                ctx.main.store(ptr::null_mut(), Ordering::SeqCst);
            }
            if role >= 3 {
                if let Ok(mut s) = ctx.state.lock() {
                    s.open_auxiliary = s.open_auxiliary.saturating_sub(1);
                }
            }
            return ctx.native_result(rc);
        }
        let m = (*inner(f)).pMethods;
        if m.is_null() {
            return ffi::SQLITE_IOERR;
        }
        let version = (*m).iVersion;
        let table = match version {
            1 => &V1,
            2 => &V2,
            3 => &V3,
            _ => {
                if let Some(close) = (*m).xClose {
                    close(inner(f));
                }
                return ffi::SQLITE_IOERR;
            }
        };
        ptr::write(
            f.cast::<Header>(),
            Header {
                base: ffi::sqlite3_file { pMethods: table },
                context: ctx,
                role,
            },
        );
        ffi::SQLITE_OK
    }
}
unsafe extern "C" fn close(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        let h = header(f);
        let ctx = &*h.context;
        let role = h.role;
        let rc = methods(f)
            .xClose
            .map_or(ffi::SQLITE_IOERR_CLOSE, |call| call(inner(f)));
        if role == 0 {
            ctx.main.store(ptr::null_mut(), Ordering::SeqCst);
        }
        if role >= 3 {
            if let Ok(mut s) = ctx.state.lock() {
                s.open_auxiliary = s.open_auxiliary.saturating_sub(1);
            }
        }
        (*f).pMethods = ptr::null();
        ctx.native_result(rc)
    }
}
unsafe extern "C" fn write(
    f: *mut ffi::sqlite3_file,
    data: *const c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    unsafe {
        let h = header(f);
        let ctx = &*h.context;
        if ctx.no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        if amount < 0 || offset < 0 || offset.checked_add(i64::from(amount)).is_none() {
            return ffi::SQLITE_IOERR_WRITE;
        }
        if ctx.restricted() {
            let mut old = -1;
            if methods(f)
                .xFileSize
                .map_or(ffi::SQLITE_IOERR_FSTAT, |call| call(inner(f), &mut old))
                != ffi::SQLITE_OK
                || old < 0
            {
                return ctx.native_result(ffi::SQLITE_IOERR_FSTAT);
            }
            let end = offset + i64::from(amount);
            if end > old {
                // Charge holes and growth separately from touched overwrite units.
                let rc = ctx.charge_extent(h.role, old as u64, (end - old) as u64);
                if rc != ffi::SQLITE_OK {
                    return rc;
                }
            }
        }
        let rc = ctx.charge_extent(h.role, offset as u64, amount as u64);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        #[cfg(test)]
        if let Some(prefix) = ctx
            .partial_write
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            // Actual native prefix I/O then an uncertain failure, with no method-table patch.
            #[cfg(test)]
            ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
            let rc = methods(f).xWrite.map_or(ffi::SQLITE_IOERR_WRITE, |call| {
                call(inner(f), data, prefix.min(amount), offset)
            });
            return ctx.native_result(if rc == ffi::SQLITE_OK {
                ffi::SQLITE_IOERR_WRITE
            } else {
                rc
            });
        }
        #[cfg(test)]
        ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
        let rc = methods(f).xWrite.map_or(ffi::SQLITE_IOERR_WRITE, |call| {
            call(inner(f), data, amount, offset)
        });
        ctx.native_result(rc)
    }
}
unsafe extern "C" fn truncate(f: *mut ffi::sqlite3_file, size: i64) -> c_int {
    unsafe {
        if size < 0 {
            return ffi::SQLITE_IOERR_TRUNCATE;
        }
        let h = header(f);
        let ctx = &*h.context;
        if ctx.no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        let mut old = -1;
        if methods(f)
            .xFileSize
            .map_or(ffi::SQLITE_IOERR_FSTAT, |call| call(inner(f), &mut old))
            != ffi::SQLITE_OK
            || old < 0
        {
            return ctx.native_result(ffi::SQLITE_IOERR_FSTAT);
        }
        if size > old {
            let rc = ctx.charge_extent(h.role, old as u64, (size - old) as u64);
            if rc != ffi::SQLITE_OK {
                return rc;
            }
        }
        // Non-growing rollback truncation remains possible after failure; it never refunds charge.
        #[cfg(test)]
        ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
        let rc = methods(f)
            .xTruncate
            .map_or(ffi::SQLITE_IOERR_TRUNCATE, |call| call(inner(f), size));
        ctx.native_result(rc)
    }
}
unsafe extern "C" fn control(f: *mut ffi::sqlite3_file, op: c_int, arg: *mut c_void) -> c_int {
    unsafe {
        let h = header(f);
        let ctx = &*h.context;
        if ctx.no_effect.load(Ordering::Acquire) {
            // SQLite uses this native control for both mmap readback and zero-size setup.
            // Returning NOTFOUND for the query yields no PRAGMA row and prevents the
            // genuine admitted owner from verifying its policy. Never forward a request
            // that enables mappings; native queries report actual policy, not a forged zero.
            if op == ffi::SQLITE_FCNTL_MMAP_SIZE {
                if arg.is_null() || *arg.cast::<i64>() > 0 {
                    return ffi::SQLITE_NOTFOUND;
                }
                return methods(f)
                    .xFileControl
                    .map_or(ffi::SQLITE_NOTFOUND, |call| call(inner(f), op, arg));
            }
            if let Some(rc) = no_effect::control_result(op) {
                return rc;
            }
        }
        if ctx.restricted() {
            match op {
                ffi::SQLITE_FCNTL_SIZE_HINT => {
                    if arg.is_null() {
                        return ctx.charge(4, 1);
                    }
                    let requested = *arg.cast::<i64>();
                    let mut old = -1;
                    if requested < 0
                        || methods(f)
                            .xFileSize
                            .map_or(ffi::SQLITE_IOERR_FSTAT, |call| call(inner(f), &mut old))
                            != ffi::SQLITE_OK
                        || old < 0
                    {
                        return ctx.charge(4, 1);
                    }
                    if requested > old {
                        let rc = ctx.charge_extent(h.role, old as u64, (requested - old) as u64);
                        if rc != ffi::SQLITE_OK {
                            return rc;
                        }
                    }
                }
                ffi::SQLITE_FCNTL_MMAP_SIZE | ffi::SQLITE_FCNTL_CHUNK_SIZE => {
                    if arg.is_null() {
                        return ctx.charge(4, 1);
                    }
                    let value = if op == ffi::SQLITE_FCNTL_MMAP_SIZE {
                        *arg.cast::<i64>()
                    } else {
                        i64::from(*arg.cast::<c_int>())
                    };
                    if value > 0 {
                        return ctx.charge(4, 1);
                    }
                }
                // Native no-op pragma handling lets SQLite core retain its ordinary validation.
                ffi::SQLITE_FCNTL_PRAGMA => return ffi::SQLITE_NOTFOUND,
                ffi::SQLITE_FCNTL_HAS_MOVED
                | ffi::SQLITE_FCNTL_WIN32_GET_HANDLE
                | ffi::SQLITE_FCNTL_LOCKSTATE
                | ffi::SQLITE_FCNTL_LAST_ERRNO
                | ffi::SQLITE_FCNTL_VFSNAME
                | ffi::SQLITE_FCNTL_VFS_POINTER
                | ffi::SQLITE_FCNTL_FILE_POINTER
                | ffi::SQLITE_FCNTL_JOURNAL_POINTER
                | ffi::SQLITE_FCNTL_DATA_VERSION
                | ffi::SQLITE_FCNTL_SYNC
                | ffi::SQLITE_FCNTL_COMMIT_PHASETWO
                | ffi::SQLITE_FCNTL_CKPT_START
                | ffi::SQLITE_FCNTL_CKPT_DONE
                | ffi::SQLITE_FCNTL_BUSYHANDLER
                | ffi::SQLITE_FCNTL_TRACE => {}
                _ => return ctx.charge(4, 1),
            }
        }
        methods(f)
            .xFileControl
            .map_or(ffi::SQLITE_NOTFOUND, |call| call(inner(f), op, arg))
    }
}
unsafe extern "C" fn shm_map(
    f: *mut ffi::sqlite3_file,
    page: c_int,
    size: c_int,
    extend: c_int,
    output: *mut *mut c_void,
) -> c_int {
    unsafe {
        let h = header(f);
        let ctx = &*h.context;
        if output.is_null() || page < 0 || size <= 0 {
            return ffi::SQLITE_IOERR_SHMMAP;
        }
        *output = ptr::null_mut();
        if ctx.no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        if ctx.restricted() && (page >= 4096 || size != 32768) {
            return ctx.charge(4, 1);
        }
        let Some(end) = (page as u64 + 1).checked_mul(size as u64) else {
            return ffi::SQLITE_IOERR_SHMMAP;
        };
        let old = match ctx.state.lock() {
            Ok(s) => s.mapped_shm_bytes,
            Err(_) => return ffi::SQLITE_IOERR_SHMMAP,
        };
        let rc = ctx.charge(2, end.saturating_sub(old));
        if rc != ffi::SQLITE_OK {
            *output = ptr::null_mut();
            return rc;
        }
        #[cfg(test)]
        ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
        let rc = methods(f).xShmMap.map_or(ffi::SQLITE_IOERR_SHMMAP, |call| {
            call(inner(f), page, size, extend, output)
        });
        if rc == ffi::SQLITE_OK && !(*output).is_null() {
            if let Ok(mut s) = ctx.state.lock() {
                s.mapped_shm_bytes = s.mapped_shm_bytes.max(end);
                if page == 0 && size == 32768 {
                    s.mapped_shm_region_zero = Some(*output as usize);
                }
            }
        }
        ctx.native_result(rc)
    }
}
unsafe extern "C" fn shm_unmap(f: *mut ffi::sqlite3_file, delete: c_int) -> c_int {
    unsafe {
        let ctx = &*header(f).context;
        if ctx.no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        #[cfg(test)]
        ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
        let rc = methods(f)
            .xShmUnmap
            .map_or(ffi::SQLITE_IOERR_SHMOPEN, |call| call(inner(f), delete));
        if rc == ffi::SQLITE_OK {
            if let Ok(mut s) = ctx.state.lock() {
                s.mapped_shm_bytes = 0;
                s.mapped_shm_region_zero = None;
            }
        }
        ctx.native_result(rc)
    }
}
unsafe extern "C" fn fetch(
    f: *mut ffi::sqlite3_file,
    offset: i64,
    amount: c_int,
    output: *mut *mut c_void,
) -> c_int {
    unsafe {
        if output.is_null() {
            return ffi::SQLITE_IOERR;
        }
        *output = ptr::null_mut();
        let ctx = &*header(f).context;
        if ctx.no_effect.load(Ordering::Acquire) || ctx.restricted() {
            *output = ptr::null_mut();
            return ffi::SQLITE_OK;
        }
        let rc = methods(f).xFetch.map_or(ffi::SQLITE_OK, |call| {
            call(inner(f), offset, amount, output)
        });
        if rc == ffi::SQLITE_OK && !(*output).is_null() {
            if let Ok(mut s) = ctx.state.lock() {
                s.mapped_database = s.mapped_database.saturating_add(1);
            }
        }
        rc
    }
}
unsafe extern "C" fn unfetch(f: *mut ffi::sqlite3_file, offset: i64, data: *mut c_void) -> c_int {
    unsafe {
        let ctx = &*header(f).context;
        let rc = methods(f)
            .xUnfetch
            .map_or(ffi::SQLITE_OK, |call| call(inner(f), offset, data));
        if rc == ffi::SQLITE_OK {
            if let Ok(mut s) = ctx.state.lock() {
                if data.is_null() {
                    s.mapped_database = 0;
                } else {
                    s.mapped_database = s.mapped_database.saturating_sub(1);
                }
            }
        }
        rc
    }
}
unsafe extern "C" fn delete(v: *mut ffi::sqlite3_vfs, name: *const c_char, sync: c_int) -> c_int {
    unsafe {
        let ctx = context(v);
        if ctx.no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        if ctx.restricted() {
            return ctx.charge(4, 1);
        }
        #[cfg(test)]
        ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
        (*ctx.native)
            .xDelete
            .map_or(ffi::SQLITE_IOERR_DELETE, |call| {
                call(ctx.native, name, sync)
            })
    }
}
unsafe extern "C" fn read(
    f: *mut ffi::sqlite3_file,
    data: *mut c_void,
    amount: c_int,
    offset: i64,
) -> c_int {
    unsafe {
        methods(f).xRead.map_or(ffi::SQLITE_IOERR_READ, |call| {
            call(inner(f), data, amount, offset)
        })
    }
}
unsafe extern "C" fn sync(f: *mut ffi::sqlite3_file, flags: c_int) -> c_int {
    unsafe {
        let h = header(f);
        let ctx = &*h.context;
        if ctx.no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        // Sync can allocate filesystem metadata even without growing the file.
        let rc = ctx.charge(h.role, 1);
        if rc != ffi::SQLITE_OK {
            return rc;
        }
        #[cfg(test)]
        if ctx.fail_sync.swap(false, Ordering::SeqCst) {
            return ctx.native_result(ffi::SQLITE_IOERR_FSYNC);
        }
        #[cfg(test)]
        ctx.native_effect_calls.fetch_add(1, Ordering::SeqCst);
        let rc = methods(f)
            .xSync
            .map_or(ffi::SQLITE_IOERR_FSYNC, |call| call(inner(f), flags));
        ctx.native_result(rc)
    }
}
unsafe extern "C" fn file_size(f: *mut ffi::sqlite3_file, size: *mut i64) -> c_int {
    unsafe {
        methods(f)
            .xFileSize
            .map_or(ffi::SQLITE_IOERR_FSTAT, |call| call(inner(f), size))
    }
}
unsafe extern "C" fn lock(f: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe {
        methods(f)
            .xLock
            .map_or(ffi::SQLITE_IOERR_LOCK, |call| call(inner(f), level))
    }
}
unsafe extern "C" fn unlock(f: *mut ffi::sqlite3_file, level: c_int) -> c_int {
    unsafe {
        methods(f)
            .xUnlock
            .map_or(ffi::SQLITE_IOERR_UNLOCK, |call| call(inner(f), level))
    }
}
unsafe extern "C" fn reserved(f: *mut ffi::sqlite3_file, value: *mut c_int) -> c_int {
    unsafe {
        methods(f)
            .xCheckReservedLock
            .map_or(ffi::SQLITE_IOERR_CHECKRESERVEDLOCK, |call| {
                call(inner(f), value)
            })
    }
}
unsafe extern "C" fn sector(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe { methods(f).xSectorSize.map_or(0, |call| call(inner(f))) }
}
unsafe extern "C" fn characteristics(f: *mut ffi::sqlite3_file) -> c_int {
    unsafe {
        methods(f)
            .xDeviceCharacteristics
            .map_or(0, |call| call(inner(f)))
    }
}
unsafe extern "C" fn shm_lock(
    f: *mut ffi::sqlite3_file,
    offset: c_int,
    count: c_int,
    flags: c_int,
) -> c_int {
    unsafe {
        if (&*header(f).context).no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        methods(f)
            .xShmLock
            .map_or(ffi::SQLITE_IOERR_SHMLOCK, |call| {
                call(inner(f), offset, count, flags)
            })
    }
}
unsafe extern "C" fn shm_barrier(f: *mut ffi::sqlite3_file) {
    unsafe {
        if let Some(call) = methods(f).xShmBarrier {
            call(inner(f));
        }
    }
}
unsafe extern "C" fn access(
    v: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    flags: c_int,
    result: *mut c_int,
) -> c_int {
    unsafe {
        let native = context(v).native;
        (*native).xAccess.map_or(ffi::SQLITE_IOERR_ACCESS, |call| {
            call(native, name, flags, result)
        })
    }
}
unsafe extern "C" fn full_path(
    v: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    capacity: c_int,
    result: *mut c_char,
) -> c_int {
    unsafe {
        let native = context(v).native;
        (*native)
            .xFullPathname
            .map_or(ffi::SQLITE_CANTOPEN, |call| {
                call(native, name, capacity, result)
            })
    }
}
unsafe extern "C" fn dl_open(v: *mut ffi::sqlite3_vfs, name: *const c_char) -> *mut c_void {
    unsafe {
        if context(v).no_effect.load(Ordering::Acquire) {
            return ptr::null_mut();
        }
        let native = context(v).native;
        (*native)
            .xDlOpen
            .map_or(ptr::null_mut(), |call| call(native, name))
    }
}
unsafe extern "C" fn dl_sym(
    v: *mut ffi::sqlite3_vfs,
    handle: *mut c_void,
    name: *const c_char,
) -> Option<unsafe extern "C" fn(*mut ffi::sqlite3_vfs, *mut c_void, *const c_char)> {
    unsafe {
        let native = context(v).native;
        (*native)
            .xDlSym
            .map_or(None, |call| call(native, handle, name))
    }
}
unsafe extern "C" fn randomness(
    v: *mut ffi::sqlite3_vfs,
    size: c_int,
    output: *mut c_char,
) -> c_int {
    unsafe {
        let native = context(v).native;
        (*native)
            .xRandomness
            .map_or(0, |call| call(native, size, output))
    }
}
unsafe extern "C" fn sleep(v: *mut ffi::sqlite3_vfs, microseconds: c_int) -> c_int {
    unsafe {
        let native = context(v).native;
        (*native)
            .xSleep
            .map_or(0, |call| call(native, microseconds))
    }
}
unsafe extern "C" fn time(v: *mut ffi::sqlite3_vfs, output: *mut f64) -> c_int {
    unsafe {
        let native = context(v).native;
        (*native)
            .xCurrentTime
            .map_or(ffi::SQLITE_ERROR, |call| call(native, output))
    }
}
unsafe extern "C" fn last_error(
    v: *mut ffi::sqlite3_vfs,
    size: c_int,
    output: *mut c_char,
) -> c_int {
    unsafe {
        let native = context(v).native;
        (*native)
            .xGetLastError
            .map_or(0, |call| call(native, size, output))
    }
}
unsafe extern "C" fn time64(v: *mut ffi::sqlite3_vfs, output: *mut i64) -> c_int {
    unsafe {
        let native = context(v).native;
        (*native)
            .xCurrentTimeInt64
            .map_or(ffi::SQLITE_ERROR, |call| call(native, output))
    }
}
unsafe extern "C" fn set_syscall(
    v: *mut ffi::sqlite3_vfs,
    name: *const c_char,
    value: ffi::sqlite3_syscall_ptr,
) -> c_int {
    unsafe {
        if context(v).no_effect.load(Ordering::Acquire) {
            return ffi::SQLITE_READONLY;
        }
        let native = context(v).native;
        (*native)
            .xSetSystemCall
            .map_or(ffi::SQLITE_NOTFOUND, |call| call(native, name, value))
    }
}
unsafe extern "C" fn get_syscall(
    v: *mut ffi::sqlite3_vfs,
    name: *const c_char,
) -> ffi::sqlite3_syscall_ptr {
    unsafe {
        let native = context(v).native;
        (*native)
            .xGetSystemCall
            .map_or(None, |call| call(native, name))
    }
}
unsafe extern "C" fn next_syscall(v: *mut ffi::sqlite3_vfs, name: *const c_char) -> *const c_char {
    unsafe {
        let native = context(v).native;
        (*native)
            .xNextSystemCall
            .map_or(ptr::null(), |call| call(native, name))
    }
}
unsafe extern "C" fn dl_error(v: *mut ffi::sqlite3_vfs, size: c_int, output: *mut c_char) {
    unsafe {
        let native = context(v).native;
        if let Some(call) = (*native).xDlError {
            call(native, size, output);
        }
    }
}
unsafe extern "C" fn dl_close(v: *mut ffi::sqlite3_vfs, handle: *mut c_void) {
    unsafe {
        let native = context(v).native;
        if let Some(call) = (*native).xDlClose {
            call(native, handle);
        }
    }
}
pub(super) fn install_vfs(v: &mut ffi::sqlite3_vfs) {
    if v.xAccess.is_some() {
        v.xAccess = Some(access);
    }
    if v.xFullPathname.is_some() {
        v.xFullPathname = Some(full_path);
    }
    if v.xDlOpen.is_some() {
        v.xDlOpen = Some(dl_open);
    }
    if v.xDlSym.is_some() {
        v.xDlSym = Some(dl_sym);
    }
    if v.xRandomness.is_some() {
        v.xRandomness = Some(randomness);
    }
    if v.xSleep.is_some() {
        v.xSleep = Some(sleep);
    }
    if v.xCurrentTime.is_some() {
        v.xCurrentTime = Some(time);
    }
    if v.xGetLastError.is_some() {
        v.xGetLastError = Some(last_error);
    }
    if v.xCurrentTimeInt64.is_some() {
        v.xCurrentTimeInt64 = Some(time64);
    }
    if v.xSetSystemCall.is_some() {
        v.xSetSystemCall = Some(set_syscall);
    }
    if v.xGetSystemCall.is_some() {
        v.xGetSystemCall = Some(get_syscall);
    }
    if v.xNextSystemCall.is_some() {
        v.xNextSystemCall = Some(next_syscall);
    }
    if v.xDlError.is_some() {
        v.xDlError = Some(dl_error);
    }
    if v.xDlClose.is_some() {
        v.xDlClose = Some(dl_close);
    }
    if v.xOpen.is_some() {
        v.xOpen = Some(open);
    }
    if v.xDelete.is_some() {
        v.xDelete = Some(delete);
    }
}
static V1: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 1,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(file_size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(reserved),
    xFileControl: Some(control),
    xSectorSize: Some(sector),
    xDeviceCharacteristics: Some(characteristics),
    xShmMap: None,
    xShmLock: None,
    xShmBarrier: None,
    xShmUnmap: None,
    xFetch: None,
    xUnfetch: None,
};
static V2: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 2,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(file_size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(reserved),
    xFileControl: Some(control),
    xSectorSize: Some(sector),
    xDeviceCharacteristics: Some(characteristics),
    xShmMap: Some(shm_map),
    xShmLock: Some(shm_lock),
    xShmBarrier: Some(shm_barrier),
    xShmUnmap: Some(shm_unmap),
    xFetch: None,
    xUnfetch: None,
};
static V3: ffi::sqlite3_io_methods = ffi::sqlite3_io_methods {
    iVersion: 3,
    xClose: Some(close),
    xRead: Some(read),
    xWrite: Some(write),
    xTruncate: Some(truncate),
    xSync: Some(sync),
    xFileSize: Some(file_size),
    xLock: Some(lock),
    xUnlock: Some(unlock),
    xCheckReservedLock: Some(reserved),
    xFileControl: Some(control),
    xSectorSize: Some(sector),
    xDeviceCharacteristics: Some(characteristics),
    xShmMap: Some(shm_map),
    xShmLock: Some(shm_lock),
    xShmBarrier: Some(shm_barrier),
    xShmUnmap: Some(shm_unmap),
    xFetch: Some(fetch),
    xUnfetch: Some(unfetch),
};

/// Borrow the actual Unix descriptor only from this exact wrapper and locked bundled layout.
/// sqlite3.c 3.50.2 unixFile begins with these four fields before any conditional fields.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) unsafe fn known_unix_descriptor(
    file: *mut ffi::sqlite3_file,
    ctx: &Context,
    expected_role: usize,
) -> Option<c_int> {
    #[repr(C)]
    struct UnixPrefix {
        methods: *const ffi::sqlite3_io_methods,
        vfs: *mut ffi::sqlite3_vfs,
        inode: *mut c_void,
        descriptor: c_int,
    }
    // SAFETY: the caller borrows the live SQLite owner. Check the wrapper method identity
    // before interpreting our own header. Its validated context/role proves native parentage.
    unsafe {
        let wrapper = file.as_ref()?;
        if ![&V1 as *const _, &V2 as *const _, &V3 as *const _].contains(&wrapper.pMethods) {
            return None;
        }
        let h = header(file);
        if !ptr::eq(h.context, ctx) || h.role != expected_role {
            return None;
        }
        if rusqlite::version_number() != 3_050_002
            || CStr::from_ptr(ffi::sqlite3_sourceid()).to_bytes()
                != b"2025-06-28 14:00:48 2af157d77fb1304a74176eaee7fbc7c7e932d946bf25325e9c26c91db19e3079"
            || (*ctx.native).szOsFile < std::mem::size_of::<UnixPrefix>() as c_int {
            return None;
        }
        #[cfg(target_os = "linux")]
        let expected = ffi::sqlite3_vfs_find(c"yield-linux-retained-root-v1".as_ptr());
        #[cfg(target_os = "macos")]
        let expected = ffi::sqlite3_vfs_find(c"unix".as_ptr());
        if expected.is_null() || ctx.native != expected {
            return None;
        }
        // The Linux adapter delegates xOpen unchanged; macOS uses the pinned Unix VFS. No
        // conditional field precedes descriptor; alignment is the wrapper's native alignment.
        let native = &*inner(file).cast::<UnixPrefix>();
        if native.vfs != ctx.native || native.methods.is_null() || native.descriptor < 0 {
            return None;
        }
        Some(native.descriptor)
    }
}

/// Borrow only the original pinned win32 file behind this exact private wrapper.
#[cfg(windows)]
pub(super) unsafe fn known_windows_file(
    file: *mut ffi::sqlite3_file,
    ctx: &Context,
    expected_role: usize,
) -> Option<*mut super::wal_objects::windows::WinFilePrefix> {
    use super::wal_objects::windows::WinFilePrefix;
    // SAFETY: sole SQLite owner keeps wrapper/native registration alive throughout
    // this borrow. Validate our header before interpreting the bundled native prefix.
    unsafe {
        let wrapper = file.as_ref()?;
        if ![&V1 as *const _, &V2 as *const _, &V3 as *const _].contains(&wrapper.pMethods) {
            return None;
        }
        let h = header(file);
        let native = ffi::sqlite3_vfs_find(c"win32".as_ptr());
        if !ptr::eq(h.context, ctx)
            || h.role != expected_role
            || !matches!(expected_role, 0 | 1)
            || rusqlite::version_number() != 3_050_002
            || CStr::from_ptr(ffi::sqlite3_sourceid()).to_bytes()
                != b"2025-06-28 14:00:48 2af157d77fb1304a74176eaee7fbc7c7e932d946bf25325e9c26c91db19e3079"
            || native.is_null()
            || ctx.native != native
            || (*native).szOsFile < std::mem::size_of::<WinFilePrefix>() as c_int
            || std::mem::size_of::<usize>() != 8
        {
            return None;
        }
        let file = inner(file).cast::<WinFilePrefix>();
        let prefix = &*file;
        if prefix.vfs != native || prefix.methods.is_null() || !prefix.valid_handle() {
            return None;
        }
        Some(file)
    }
}
