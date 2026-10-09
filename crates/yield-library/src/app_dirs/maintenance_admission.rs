//! Process-lifetime admission shared with the reversible local-state tool.

use std::io;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(target_os = "macos")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "macos")]
use std::sync::{Mutex, OnceLock};

/// Retained shared admission to one compiled Yield persistence namespace.
///
/// The underlying descriptor remains open for the process lifetime. A clean/reset tool holds
/// the same lock exclusively and therefore cannot rename the app root while any updated app or
/// helper retains this value.
#[derive(Clone, Debug)]
pub struct ProcessMaintenanceAdmission {
    #[cfg(target_os = "macos")]
    file: Arc<std::fs::File>,
    #[cfg(target_os = "macos")]
    path: PathBuf,
}

impl ProcessMaintenanceAdmission {
    /// The stable external lock path retained by this admission.
    #[cfg(target_os = "macos")]
    pub fn lock_path(&self) -> &std::path::Path {
        &self.path
    }

    /// The retained descriptor's filesystem identity, for verified process handoff.
    #[cfg(target_os = "macos")]
    pub fn identity(&self) -> io::Result<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let metadata = self.file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
}

#[cfg(target_os = "macos")]
static PROCESS_ADMISSION: OnceLock<Mutex<Option<Arc<ProcessMaintenanceAdmission>>>> =
    OnceLock::new();
#[cfg(target_os = "macos")]
static PROCESS_ADMISSION_ESTABLISHED: AtomicBool = AtomicBool::new(false);

/// Acquire shared admission before app logging, profile bootstrap, or canonical registry access.
///
/// Failures are retryable: the process cache is populated only after the stable lock is opened,
/// validated and held. The local CI launcher can pass an inherited descriptor while it transfers
/// an exclusive reset lock to a clean application; that descriptor is accepted only after its
/// inode, owner, namespace path, and an independently observed exclusive lock are verified.
pub fn acquire_process_maintenance_admission() -> Result<Arc<ProcessMaintenanceAdmission>, io::Error>
{
    #[cfg(target_os = "macos")]
    {
        let cache = PROCESS_ADMISSION.get_or_init(|| Mutex::new(None));
        let mut cached = cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(admission) = cached.as_ref() {
            return Ok(Arc::clone(admission));
        }
        let admission = Arc::new(unix::acquire()?);
        *cached = Some(Arc::clone(&admission));
        PROCESS_ADMISSION_ESTABLISHED.store(true, Ordering::Release);
        Ok(admission)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Arc::new(ProcessMaintenanceAdmission {}))
    }
}

/// Check whether eager process admission already exists without opening files or creating state.
pub(crate) fn process_maintenance_admission_is_established() -> bool {
    #[cfg(target_os = "macos")]
    {
        PROCESS_ADMISSION_ESTABLISHED.load(Ordering::Acquire)
    }
    #[cfg(not(target_os = "macos"))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
mod unix {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::Path;

    use super::ProcessMaintenanceAdmission;

    const CONTROL_DIRECTORY: &str = ".yield-local-ci";
    const INHERITED_NAMESPACE_FD_ENV: &str = "YIELD_MAINTENANCE_ADMISSION_FD";
    const INHERITED_TRANSITION_FD_ENV: &str = "YIELD_MAINTENANCE_TRANSITION_FD";

    pub(super) fn acquire() -> Result<ProcessMaintenanceAdmission, io::Error> {
        let base = super::super::resolution::maintenance_namespace_base()
            .map_err(|error| {
                io::Error::other(format!(
                    "canonical macOS ~/Library/Application Support must exist as a current-user-owned real directory before Yield can start: {error}"
                ))
            })?;
        validate_owned_directory(&base)?;
        let control = base.join(CONTROL_DIRECTORY);
        create_owned_directory(&control)?;
        let namespace = super::super::APP_DIR_NAME;
        let path = control.join(format!("{namespace}.admission.lock"));
        let transition_path = control.join(format!("{namespace}.transition.lock"));
        let transition_file = open_lock_file(&transition_path)?;
        validate_lock_file(&transition_path, &transition_file)?;
        let canonical_file = open_lock_file(&path)?;
        validate_lock_file(&path, &canonical_file)?;
        match (
            std::env::var_os(INHERITED_NAMESPACE_FD_ENV),
            std::env::var_os(INHERITED_TRANSITION_FD_ENV),
        ) {
            (Some(namespace_fd), Some(transition_fd)) => {
                let namespace_fd = parse_fd(namespace_fd)?;
                let transition_fd = parse_fd(transition_fd)?;
                let admission = adopt_exclusive_handoff(
                    &base,
                    namespace,
                    namespace_fd,
                    &path,
                    canonical_file,
                    transition_fd,
                    &transition_path,
                    transition_file,
                )?;
                // Successful validation establishes that these are the exact handoff FDs.
                // Close the raw inherited descriptors now; the retained duplicate is the
                // sole namespace lease and the transition fence has already been released.
                unsafe {
                    libc::close(namespace_fd);
                    libc::close(transition_fd);
                }
                return Ok(admission);
            }
            (None, None) => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "incomplete inherited maintenance admission handoff",
                ));
            }
        }
        lock_shared(&transition_file, &transition_path)?;
        lock_shared(&canonical_file, &path)?;
        validate_path_identity(&path, &canonical_file)?;
        ensure_no_pending_recovery(&base, namespace)?;
        unsafe { libc::flock(transition_file.as_raw_fd(), libc::LOCK_UN) };
        Ok(ProcessMaintenanceAdmission {
            file: std::sync::Arc::new(canonical_file),
            path,
        })
    }

    fn ensure_no_pending_recovery(base: &Path, namespace: &str) -> Result<(), io::Error> {
        let namespace_label = match namespace {
            ".yield" => "production",
            ".yield-development" => "development",
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unknown compiled maintenance namespace",
                ));
            }
        };
        let control = base.join(CONTROL_DIRECTORY);
        validate_owned_directory(&control)?;
        let backups = control.join("backups");
        match fs::symlink_metadata(&backups) {
            Ok(_) => validate_owned_directory(&backups)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        let backup_root = backups.join(namespace);
        match fs::symlink_metadata(&backup_root) {
            Ok(_) => validate_owned_directory(&backup_root)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
        let entries = match fs::read_dir(&backup_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let entry = entry?;
            let entry_type = entry.file_type()?;
            let entry_metadata = entry.metadata()?;
            if entry_type.is_symlink()
                || !entry_type.is_dir()
                || entry_metadata.uid() != unsafe { libc::geteuid() }
                || !entry
                    .file_name()
                    .to_string_lossy()
                    .chars()
                    .all(|character| {
                        character.is_ascii_hexdigit() && !character.is_ascii_uppercase()
                    })
                || entry.file_name().to_string_lossy().len() != 32
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "maintenance backup entry is not a real directory",
                ));
            }
            let record_path = entry.path().join("transaction.json");
            let record_metadata = match fs::symlink_metadata(&record_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "uncommitted maintenance preparation requires explicit recovery before startup",
                    ));
                }
                Err(error) => return Err(error),
            };
            if record_metadata.file_type().is_symlink()
                || !record_metadata.is_file()
                || record_metadata.uid() != unsafe { libc::geteuid() }
                || record_metadata.nlink() != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "maintenance record is not a regular current-user file",
                ));
            }
            let record = match fs::read(&record_path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let record: serde_json::Value = serde_json::from_slice(&record).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "maintenance transaction record is malformed",
                )
            })?;
            let phase = record
                .get("phase")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if record.get("schema").and_then(serde_json::Value::as_u64) != Some(1)
                || record
                    .get("transaction")
                    .and_then(serde_json::Value::as_str)
                    != entry.file_name().to_str()
                || record.get("namespace").and_then(serde_json::Value::as_str)
                    != Some(namespace_label)
                || record.get("root").and_then(serde_json::Value::as_str)
                    != Some(base.join(namespace).to_str().unwrap_or(""))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "maintenance transaction identity is malformed",
                ));
            }
            if phase == "original_parked" {
                let original = entry.path().join("original-root");
                let original_metadata = fs::symlink_metadata(original)?;
                let identity = record.get("original_identity");
                if original_metadata.file_type().is_symlink()
                    || !original_metadata.is_dir()
                    || original_metadata.uid() != unsafe { libc::geteuid() }
                    || identity
                        .and_then(|value| value.get("device"))
                        .and_then(serde_json::Value::as_u64)
                        != Some(original_metadata.dev())
                    || identity
                        .and_then(|value| value.get("inode"))
                        .and_then(serde_json::Value::as_u64)
                        != Some(original_metadata.ino())
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "retained original root identity is ambiguous",
                    ));
                }
            }
            if !matches!(phase, "original_parked" | "restored" | "aborted") {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "pending Yield state recovery must complete before startup",
                ));
            }
        }
        Ok(())
    }

    fn adopt_exclusive_handoff(
        base: &Path,
        namespace: &str,
        namespace_fd: i32,
        namespace_path: &Path,
        canonical_file: File,
        transition_fd: i32,
        transition_path: &Path,
        transition_file: File,
    ) -> Result<ProcessMaintenanceAdmission, io::Error> {
        // SAFETY: fstat initializes this value on success; checked below before use.
        let mut inherited_stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(namespace_fd, &mut inherited_stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let canonical_metadata = canonical_file.metadata()?;
        if inherited_stat.st_dev as u64 != canonical_metadata.dev()
            || inherited_stat.st_ino as u64 != canonical_metadata.ino()
            || inherited_stat.st_uid as u32 != unsafe { libc::geteuid() } as u32
            || (inherited_stat.st_mode & libc::S_IFMT) != libc::S_IFREG
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "inherited admission descriptor does not identify the canonical owned lock",
            ));
        }
        validate_path_identity(namespace_path, &canonical_file)?;
        validate_handoff_fd(transition_fd, transition_path, &transition_file)?;

        // A separate shared probe conflicts with an exclusive reset holder but is compatible
        // with ordinary shared admission. This proves that a lock is currently exclusive before
        // converting the inherited open-file description to shared app admission.
        verify_exclusive_lock(&canonical_file, "namespace")?;
        verify_exclusive_lock(&transition_file, "transition")?;
        verify_inherited_exclusive(namespace_fd, "namespace")?;
        verify_inherited_exclusive(transition_fd, "transition")?;
        set_cloexec(namespace_fd)?;
        set_cloexec(transition_fd)?;
        if unsafe { libc::flock(namespace_fd, libc::LOCK_SH | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        ensure_no_pending_recovery(base, namespace)?;
        // Keep the transition fence exclusive until namespace shared ownership is established.
        if unsafe { libc::flock(transition_fd, libc::LOCK_UN) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let owned_fd = unsafe { libc::dup(namespace_fd) };
        if owned_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: dup returned a new owned descriptor referring to the validated lock file.
        let file = unsafe { File::from_raw_fd(owned_fd) };
        set_cloexec(file.as_raw_fd())?;
        validate_path_identity(namespace_path, &file)?;
        Ok(ProcessMaintenanceAdmission {
            file: std::sync::Arc::new(file),
            path: namespace_path.to_path_buf(),
        })
    }

    fn set_cloexec(fd: i32) -> Result<(), io::Error> {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn parse_fd(raw: std::ffi::OsString) -> Result<i32, io::Error> {
        raw.to_str()
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid inherited lock fd")
            })?
            .parse()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid inherited lock fd"))
    }

    fn validate_handoff_fd(fd: i32, path: &Path, canonical_file: &File) -> Result<(), io::Error> {
        // SAFETY: fstat initializes this value on success; checked below before use.
        let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let metadata = canonical_file.metadata()?;
        if stat.st_dev as u64 != metadata.dev()
            || stat.st_ino as u64 != metadata.ino()
            || stat.st_uid as u32 != unsafe { libc::geteuid() } as u32
            || (stat.st_mode & libc::S_IFMT) != libc::S_IFREG
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid transition lock handoff",
            ));
        }
        validate_path_identity(path, canonical_file)
    }

    fn verify_exclusive_lock(file: &File, kind: &str) -> Result<(), io::Error> {
        match unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } {
            0 => {
                unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("inherited {kind} lock is not exclusive"),
                ))
            }
            _ if io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }

    fn verify_inherited_exclusive(fd: i32, kind: &str) -> Result<(), io::Error> {
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("inherited {kind} descriptor does not itself hold the exclusive lock"),
            ))
        } else {
            Err(error)
        }
    }

    fn lock_shared(file: &File, path: &Path) -> Result<(), io::Error> {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "Yield namespace is under clean/reset maintenance: {}",
                    path.display()
                ),
            ))
        } else {
            Err(error)
        }
    }

    fn open_lock_file(path: &Path) -> Result<File, io::Error> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let file = options.open(path)?;
        validate_lock_file(path, &file)?;
        Ok(file)
    }

    fn validate_lock_file(path: &Path, file: &File) -> Result<(), io::Error> {
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.nlink() != 1
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "maintenance lock entry is foreign, linked, or not a regular file",
            ));
        }
        validate_path_identity(path, file)
    }

    fn validate_path_identity(path: &Path, file: &File) -> Result<(), io::Error> {
        let entry = fs::symlink_metadata(path)?;
        let opened = file.metadata()?;
        if entry.file_type().is_symlink()
            || !entry.is_file()
            || entry.dev() != opened.dev()
            || entry.ino() != opened.ino()
            || entry.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "maintenance lock path identity changed",
            ));
        }
        Ok(())
    }

    fn create_owned_directory(path: &Path) -> Result<(), io::Error> {
        match fs::create_dir(path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        validate_owned_directory(path)
    }

    fn validate_owned_directory(path: &Path) -> Result<(), io::Error> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "namespace path is not a current-user-owned real directory: {}",
                    path.display()
                ),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        fn paths() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
            let base = tempfile::tempdir().expect("temporary namespace base");
            let control = base.path().join(CONTROL_DIRECTORY);
            fs::create_dir(&control).expect("control directory");
            let namespace_path = control.join(".yield.admission.lock");
            let transition_path = control.join(".yield.transition.lock");
            let _namespace = open_lock_file(&namespace_path).expect("namespace lock");
            let _transition = open_lock_file(&transition_path).expect("transition lock");
            (base, control, namespace_path, transition_path)
        }

        #[test]
        fn transition_fence_closes_ex_to_shared_downgrade_window() {
            let (base, _control, namespace_path, transition_path) = paths();
            let namespace_owner = open_lock_file(&namespace_path).expect("namespace owner");
            let transition_owner = open_lock_file(&transition_path).expect("transition owner");
            unsafe { libc::flock(transition_owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            unsafe { libc::flock(namespace_owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };

            let namespace_canonical = open_lock_file(&namespace_path).expect("namespace verifier");
            let transition_canonical =
                open_lock_file(&transition_path).expect("transition verifier");
            let admission = adopt_exclusive_handoff(
                base.path(),
                ".yield",
                namespace_owner.as_raw_fd(),
                &namespace_path,
                namespace_canonical,
                transition_owner.as_raw_fd(),
                &transition_path,
                transition_canonical,
            )
            .expect("verified reset handoff");

            // A competing reset may acquire transition after adoption, but cannot get EX on
            // namespace while the app's shared proof remains retained.
            let competing_transition =
                open_lock_file(&transition_path).expect("transition contender");
            assert_eq!(
                unsafe {
                    libc::flock(
                        competing_transition.as_raw_fd(),
                        libc::LOCK_EX | libc::LOCK_NB,
                    )
                },
                0
            );
            let competing_namespace = open_lock_file(&namespace_path).expect("namespace contender");
            assert_ne!(
                unsafe {
                    libc::flock(
                        competing_namespace.as_raw_fd(),
                        libc::LOCK_EX | libc::LOCK_NB,
                    )
                },
                0
            );
            unsafe { libc::flock(competing_transition.as_raw_fd(), libc::LOCK_UN) };
            drop(admission);
            drop(namespace_owner);

            let succeeding_namespace =
                open_lock_file(&namespace_path).expect("namespace after drop");
            assert_eq!(
                unsafe {
                    libc::flock(
                        succeeding_namespace.as_raw_fd(),
                        libc::LOCK_EX | libc::LOCK_NB,
                    )
                },
                0
            );
        }

        #[test]
        fn ordinary_admission_blocks_reset_until_namespace_owner_drops() {
            let (_base, _control, namespace_path, transition_path) = paths();
            let transition = open_lock_file(&transition_path).expect("transition");
            let namespace = open_lock_file(&namespace_path).expect("namespace");
            lock_shared(&transition, &transition_path).expect("transition shared");
            lock_shared(&namespace, &namespace_path).expect("namespace shared");
            unsafe { libc::flock(transition.as_raw_fd(), libc::LOCK_UN) };

            let reset_transition = open_lock_file(&transition_path).expect("reset transition");
            assert_eq!(
                unsafe { libc::flock(reset_transition.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            let reset_namespace = open_lock_file(&namespace_path).expect("reset namespace");
            assert_ne!(
                unsafe { libc::flock(reset_namespace.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            unsafe { libc::flock(reset_transition.as_raw_fd(), libc::LOCK_UN) };
            drop(namespace);
            assert_eq!(
                unsafe { libc::flock(reset_transition.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            assert_eq!(
                unsafe { libc::flock(reset_namespace.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
        }

        #[test]
        fn inherited_handoff_requires_both_exact_exclusive_locks() {
            let (base, _control, namespace_path, transition_path) = paths();
            let namespace_owner = open_lock_file(&namespace_path).expect("namespace owner");
            let transition_owner = open_lock_file(&transition_path).expect("transition owner");
            unsafe { libc::flock(namespace_owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            let namespace_canonical = open_lock_file(&namespace_path).expect("namespace verifier");
            let transition_canonical =
                open_lock_file(&transition_path).expect("transition verifier");
            let error = adopt_exclusive_handoff(
                base.path(),
                ".yield",
                namespace_owner.as_raw_fd(),
                &namespace_path,
                namespace_canonical,
                transition_owner.as_raw_fd(),
                &transition_path,
                transition_canonical,
            )
            .expect_err("transition lock must also be held exclusively");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            let contender = open_lock_file(&namespace_path).expect("namespace contender");
            assert_ne!(
                unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
                0,
                "failed handoff must leave original exclusive authority intact"
            );
        }

        #[test]
        fn unlocked_transition_fd_cannot_borrow_another_owners_fence() {
            let (base, _control, namespace_path, transition_path) = paths();
            let namespace_owner = open_lock_file(&namespace_path).expect("namespace owner");
            let foreign_transition = open_lock_file(&transition_path).expect("foreign transition");
            assert_eq!(
                unsafe { libc::flock(namespace_owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            assert_eq!(
                unsafe {
                    libc::flock(
                        foreign_transition.as_raw_fd(),
                        libc::LOCK_EX | libc::LOCK_NB,
                    )
                },
                0
            );
            let unlocked_transition = open_lock_file(&transition_path).expect("unlocked fd");
            let namespace_canonical = open_lock_file(&namespace_path).expect("namespace verifier");
            let transition_canonical =
                open_lock_file(&transition_path).expect("transition verifier");
            let error = adopt_exclusive_handoff(
                base.path(),
                ".yield",
                namespace_owner.as_raw_fd(),
                &namespace_path,
                namespace_canonical,
                unlocked_transition.as_raw_fd(),
                &transition_path,
                transition_canonical,
            )
            .expect_err("unlocked transition descriptor cannot borrow a foreign fence");
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
            let contender = open_lock_file(&transition_path).expect("transition probe");
            assert_ne!(
                unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
                0,
                "failed adoption must not release the other owner's exclusive fence"
            );
        }

        #[test]
        fn pending_recovery_blocks_startup_but_committed_backup_is_accepted() {
            use std::os::unix::fs::MetadataExt;

            let (base, _control, _namespace_path, _transition_path) = paths();
            let tx_id = "0123456789abcdef0123456789abcdef";
            let tx = base
                .path()
                .join(CONTROL_DIRECTORY)
                .join("backups")
                .join(".yield")
                .join(tx_id);
            fs::create_dir_all(&tx).expect("transaction path");
            let original = tx.join("original-root");
            fs::create_dir(&original).expect("parked root");
            let metadata = fs::metadata(&original).expect("parked identity");
            let record = serde_json::json!({
                "schema": 1,
                "transaction": tx_id,
                "namespace": "production",
                "phase": "restore_prepared",
                "root": base.path().join(".yield").to_string_lossy(),
                "original_identity": {"device": metadata.dev(), "inode": metadata.ino()},
            });
            let record_path = tx.join("transaction.json");
            fs::write(
                &record_path,
                serde_json::to_vec(&record).expect("record bytes"),
            )
            .expect("transaction record");
            assert!(ensure_no_pending_recovery(base.path(), ".yield").is_err());
            let committed = record
                .to_string()
                .replace("restore_prepared", "original_parked");
            fs::write(&record_path, committed).expect("committed record");
            ensure_no_pending_recovery(base.path(), ".yield").expect("committed backup is stable");
        }

        #[test]
        fn python_state_records_interoperate_with_native_admission() {
            use std::process::Command;

            let base = tempfile::tempdir().expect("temporary support directory");
            let root = base.path().join(".yield-development");
            fs::create_dir(&root).expect("development root");
            fs::write(root.join("dirty-evidence"), b"retain").expect("marker");
            let script_dir =
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/internal/ci");
            let python = format!(
                "import pathlib,sys; sys.path.insert(0, {script_dir:?}); import local_state; local_state.clean('development', pathlib.Path({support:?}))",
                script_dir = script_dir.to_string_lossy(),
                support = base.path().to_string_lossy(),
            );
            let result = Command::new("python3")
                .arg("-c")
                .arg(python)
                .status()
                .expect("run local state tool");
            assert!(result.success(), "Python clean should commit a backup");
            ensure_no_pending_recovery(base.path(), ".yield-development")
                .expect("native startup accepts Python committed-clean schema");

            fs::create_dir(&root).expect("recreate development root");
            let crash_script = format!(
                "import pathlib,sys; sys.path.insert(0, {script_dir:?}); import local_state; local_state.clean('development', pathlib.Path({support:?}), _test_crash_at='clean_after_rename')",
                script_dir = script_dir.to_string_lossy(),
                support = base.path().to_string_lossy(),
            );
            let result = Command::new("python3")
                .arg("-c")
                .arg(crash_script)
                .status()
                .expect("run interrupted local state tool");
            assert_eq!(
                result.code(),
                Some(86),
                "fault point should terminate process"
            );
            assert!(
                ensure_no_pending_recovery(base.path(), ".yield-development").is_err(),
                "native startup must block on Python's pending recovery schema"
            );

            let recover_script = format!(
                "import json,pathlib,sys; support=pathlib.Path({support:?}); sys.path.insert(0, {script_dir:?}); import local_state; tx=next(p for p in (support/'.yield-local-ci/backups/.yield-development').iterdir() if json.loads((p/'transaction.json').read_text())['phase']=='prepared'); local_state.recover(tx.name, support)",
                script_dir = script_dir.to_string_lossy(),
                support = base.path().to_string_lossy(),
            );
            let result = Command::new("python3")
                .arg("-c")
                .arg(recover_script)
                .status()
                .expect("recover Python transaction");
            assert!(
                result.success(),
                "Python recovery should retain a committed backup"
            );
            ensure_no_pending_recovery(base.path(), ".yield-development")
                .expect("native startup accepts recovered Python transaction");

            let backup_root = base
                .path()
                .join(CONTROL_DIRECTORY)
                .join("backups")
                .join(".yield-development");
            let transaction_dir = fs::read_dir(backup_root)
                .expect("Python backup namespace")
                .next()
                .expect("one Python transaction")
                .expect("transaction entry")
                .path();
            let record_path = transaction_dir.join("transaction.json");
            let mut record: serde_json::Value =
                serde_json::from_slice(&fs::read(&record_path).expect("Python record"))
                    .expect("Python transaction JSON");
            record["namespace"] = serde_json::Value::String("production".into());
            fs::write(
                &record_path,
                serde_json::to_vec(&record).expect("foreign record bytes"),
            )
            .expect("foreign transaction marker");
            assert!(
                ensure_no_pending_recovery(base.path(), ".yield-development").is_err(),
                "native startup must refuse a foreign namespace in Python record schema"
            );
        }

        #[test]
        fn native_pending_scan_refuses_linked_backup_namespace() {
            let (base, _control, _namespace_path, _transition_path) = paths();
            let elsewhere = tempfile::tempdir().expect("foreign backup directory");
            let backups = base.path().join(CONTROL_DIRECTORY).join("backups");
            fs::create_dir(&backups).expect("backups directory");
            std::os::unix::fs::symlink(elsewhere.path(), backups.join(".yield-development"))
                .expect("linked namespace directory");
            assert!(ensure_no_pending_recovery(base.path(), ".yield-development").is_err());
        }

        #[test]
        fn native_pending_scan_refuses_transaction_without_committed_record() {
            let (base, _control, _namespace_path, _transition_path) = paths();
            let tx = base
                .path()
                .join(CONTROL_DIRECTORY)
                .join("backups")
                .join(".yield-development")
                .join("0123456789abcdef0123456789abcdef");
            fs::create_dir_all(&tx).expect("transaction path");
            fs::write(
                tx.join("transaction.json.aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa.tmp"),
                b"partial",
            )
            .expect("partial initial record");
            assert!(ensure_no_pending_recovery(base.path(), ".yield-development").is_err());
        }

        #[test]
        #[ignore = "launched as an inherited-descriptor child by its parent regression"]
        fn inherited_handoff_child_probe() {
            let base = std::env::var_os("YIELD_TEST_HANDOFF_BASE")
                .expect("parent must provide temporary namespace");
            let namespace_fd = std::env::var("YIELD_TEST_HANDOFF_NAMESPACE_FD")
                .expect("namespace fd")
                .parse::<i32>()
                .expect("numeric namespace fd");
            let transition_fd = std::env::var("YIELD_TEST_HANDOFF_TRANSITION_FD")
                .expect("transition fd")
                .parse::<i32>()
                .expect("numeric transition fd");
            let base = PathBuf::from(base);
            let control = base.join(CONTROL_DIRECTORY);
            let namespace_path = control.join(".yield-development.admission.lock");
            let transition_path = control.join(".yield-development.transition.lock");
            let canonical = open_lock_file(&namespace_path).expect("canonical namespace lock");
            let transition = open_lock_file(&transition_path).expect("canonical transition lock");
            let admission = adopt_exclusive_handoff(
                &base,
                ".yield-development",
                namespace_fd,
                &namespace_path,
                canonical,
                transition_fd,
                &transition_path,
                transition,
            )
            .expect("child adopts both exclusive locks");
            unsafe {
                libc::close(namespace_fd);
                libc::close(transition_fd);
            }
            std::thread::sleep(std::time::Duration::from_millis(120));
            drop(admission);
            // Exercise the same pre-thread cleanup performed by the real main function.
            // SAFETY: this isolated child test has not started worker threads.
            unsafe {
                std::env::remove_var(INHERITED_NAMESPACE_FD_ENV);
                std::env::remove_var(INHERITED_TRANSITION_FD_ENV);
            }

            // A grandchild exec must not inherit the raw handoff descriptors and prolong
            // admission after the process-owned Arc is dropped.
            let probe = format!(
                "import fcntl,os,sys; assert 'YIELD_MAINTENANCE_ADMISSION_FD' not in os.environ; assert 'YIELD_MAINTENANCE_TRANSITION_FD' not in os.environ; f=os.open(sys.argv[1], os.O_RDWR); fcntl.flock(f, fcntl.LOCK_EX|fcntl.LOCK_NB); fcntl.flock(f, fcntl.LOCK_UN)",
            );
            let result = std::process::Command::new("python3")
                .arg("-c")
                .arg(probe)
                .arg(namespace_path)
                .status()
                .expect("exec independent lock probe");
            assert!(
                result.success(),
                "exec child must not retain raw lock descriptors"
            );
        }

        #[test]
        fn inherited_handoff_survives_process_boundary_and_closes_raw_fds_on_exec() {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;
            use std::process::Command;

            let base = tempfile::tempdir().expect("temporary handoff namespace");
            let control = base.path().join(CONTROL_DIRECTORY);
            fs::create_dir(&control).expect("control directory");
            let namespace_path = control.join(".yield-development.admission.lock");
            let transition_path = control.join(".yield-development.transition.lock");
            let namespace = open_lock_file(&namespace_path).expect("namespace owner");
            let transition = open_lock_file(&transition_path).expect("transition owner");
            assert_eq!(
                unsafe { libc::flock(transition.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            assert_eq!(
                unsafe { libc::flock(namespace.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0
            );
            let namespace_fd = namespace.as_raw_fd();
            let transition_fd = transition.as_raw_fd();
            let mut command = Command::new(std::env::current_exe().expect("test executable"));
            command
                .arg("--exact")
                .arg("app_dirs::maintenance_admission::unix::tests::inherited_handoff_child_probe")
                .arg("--ignored")
                .arg("--nocapture")
                .env("YIELD_TEST_HANDOFF_BASE", base.path())
                .env("YIELD_TEST_HANDOFF_NAMESPACE_FD", namespace_fd.to_string())
                .env(
                    "YIELD_TEST_HANDOFF_TRANSITION_FD",
                    transition_fd.to_string(),
                )
                .env(INHERITED_NAMESPACE_FD_ENV, namespace_fd.to_string())
                .env(INHERITED_TRANSITION_FD_ENV, transition_fd.to_string());
            unsafe {
                command.pre_exec(move || {
                    for fd in [namespace_fd, transition_fd] {
                        let flags = libc::fcntl(fd, libc::F_GETFD);
                        if flags < 0
                            || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().expect("spawn handoff adopter process");
            drop(namespace);
            drop(transition);

            let contender = open_lock_file(&namespace_path).expect("namespace contender");
            assert_ne!(
                unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0,
                "child must retain shared admission after downgrade"
            );
            assert!(child.wait().expect("wait for adopter").success());
            assert_eq!(
                unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                0,
                "all admission descriptors must close when child exits"
            );
        }
    }
}
