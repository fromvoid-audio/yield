//! Retained pre-lease read custody. Inspection never authorizes adoption or allocation.
use super::source_lease::SourceLeaseReservation;
use super::{BoundDatabaseRoot, SourceDatabase, sqlite_identity};
use crate::app_dirs::WritableProfileGuard;
use rusqlite::OpenFlags;
use std::{
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use thiserror::Error;

mod image;
mod rows;

/// Refusal to inspect an unsupported, stale or unbounded legacy source.
#[derive(Debug, Error)]
#[error("legacy source observation refused: {0}")]
pub struct LegacyObservationError(pub(crate) &'static str);
type Result<T> = std::result::Result<T, LegacyObservationError>;
fn refused(_: impl std::fmt::Debug) -> LegacyObservationError {
    LegacyObservationError("ownership, SQLite or descriptor validation failed")
}

/// Bounded original image, privately captured by a retained read owner; never a grant.
#[derive(Debug, Eq, PartialEq)]
pub struct LegacySourceImage {
    revision: u64,
    metadata_rows: usize,
    audio_rows: usize,
    digest: [u8; 32],
    main_bytes: u64,
}
impl LegacySourceImage {
    /// Canonical original metadata revision, not a current source lease revision.
    pub fn revision(&self) -> u64 {
        self.revision
    }
    /// Number of preserved original metadata rows.
    pub fn metadata_rows(&self) -> usize {
        self.metadata_rows
    }
    /// Number of preserved original audio rows.
    pub fn audio_rows(&self) -> usize {
        self.audio_rows
    }
    /// Digest of bounded schema/rows and original physical bindings.
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    /// Actual main-file length measured through the retained SQLite VFS.
    pub fn main_bytes(&self) -> u64 {
        self.main_bytes
    }
}

/// Read-only owner retaining the fresh reservation and original profile/file/root bindings.
/// No SQLite connection, lease, mutable row or grant constructor is exposed.
pub struct LegacySourceReadOwner {
    // Close SQLite before releasing its original file/root/reservation/profile custody.
    connection: crate::sqlite_budget::OwnedConnection,
    binding: Arc<sqlite_identity::DatabaseFileBinding>,
    reservation: SourceLeaseReservation,
    media_root: Arc<BoundDatabaseRoot>,
    profile: WritableProfileGuard,
    original_digest: Option<[u8; 32]>,
}
impl SourceDatabase {
    /// Open the explicit first legacy-inspection route. Ordinary opens still reject migration.
    /// A failed request preserves created sidecar evidence; retries do not reset that evidence.
    pub fn open_legacy_read_owner(
        root: impl AsRef<Path>,
        profile: &WritableProfileGuard,
        child: impl AsRef<Path>,
    ) -> Result<LegacySourceReadOwner> {
        LegacySourceReadOwner::open(root.as_ref(), profile, child.as_ref())
    }
}
impl LegacySourceReadOwner {
    fn open(root: &Path, profile: &WritableProfileGuard, child: &Path) -> Result<Self> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LegacyObservationError("SQLite extension runtime unknown"))?;
        profile.validate_current().map_err(refused)?;
        let database_root =
            Arc::new(BoundDatabaseRoot::for_profile_child(profile, child).map_err(refused)?);
        let media_root =
            Arc::new(BoundDatabaseRoot::for_external_source_root(profile, root).map_err(refused)?);
        absent_sidecars(&database_root)?;
        // Reject existing evidence before acquiring it. The fresh flag also closes the race.
        absent(
            &database_root,
            Path::new(super::source_lease::SOURCE_LEASE_LOCK_FILE_NAME),
        )?;
        let name = Path::new(super::DB_FILE_NAME);
        let path = database_root.path().join(name);
        let original = sqlite_identity::open_existing_database_file(&database_root, name, &path)
            .map_err(refused)?;
        if original.file.metadata().map_err(refused)?.len() > 16 * 1024 * 1024 {
            return Err(LegacyObservationError("legacy main-file bound"));
        }
        let binding = Arc::new(sqlite_identity::DatabaseFileBinding::new(
            Arc::clone(&database_root),
            name,
            &path,
            original.identity,
        ));
        let reservation = SourceLeaseReservation::acquire(
            Arc::clone(&database_root),
            Arc::clone(&media_root),
            profile,
        )
        .map_err(refused)?;
        if !reservation.is_fresh() {
            return Err(LegacyObservationError("existing reservation evidence"));
        }
        reservation
            .validate_before_open(profile, &binding)
            .map_err(refused)?;
        absent_sidecars(&database_root)?;
        let connection = database_root
            .open_owned_sqlite(name, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(refused)?;
        binding.validate(&connection).map_err(refused)?;
        connection.busy_timeout(Duration::ZERO).map_err(refused)?;
        connection
            .execute_batch(
                "PRAGMA query_only=ON; PRAGMA temp_store=MEMORY; PRAGMA cache_spill=OFF;",
            )
            .map_err(refused)?;
        let mut owner = Self {
            connection,
            binding,
            reservation,
            media_root,
            profile: profile.try_clone().map_err(refused)?,
            original_digest: None,
        };
        owner.validate(profile)?;
        // No unsupported schema survives a successful open; capture remains separately callable.
        owner.original_digest = Some(*owner.capture(profile)?.digest());
        Ok(owner)
    }
    /// Revalidate the original acquisition, roots, main-file and fresh locked sidecar.
    pub fn validate(&self, profile: &WritableProfileGuard) -> Result<()> {
        if profile.owner_identity() != self.profile.owner_identity() {
            return Err(LegacyObservationError("foreign profile acquisition"));
        }
        self.profile.validate_current().map_err(refused)?;
        self.media_root
            .validate_profile_guard(profile)
            .map_err(refused)?;
        self.reservation
            .validate_before_open(profile, &self.binding)
            .map_err(refused)?;
        self.binding.validate(&self.connection).map_err(refused)?;
        absent_sidecars(self.binding.root())?;
        if !self
            .connection
            .is_readonly(rusqlite::MAIN_DB)
            .map_err(refused)?
        {
            return Err(LegacyObservationError("read-only custody required"));
        }
        Ok(())
    }
    /// Capture one coherent bounded image under the actual retained read connection.
    /// The result has no allocation estimate, current lease, audit or adoption permission.
    pub fn capture(&self, profile: &WritableProfileGuard) -> Result<LegacySourceImage> {
        let _extensions = crate::sqlite_ext::try_extension_free_metadata_scope()
            .ok_or(LegacyObservationError("SQLite extension runtime unknown"))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let _progress = Progress::install(&self.connection, deadline);
        self.validate(profile)?;
        let main_bytes =
            crate::sqlite_wal::retained_database_bytes(&self.connection).map_err(refused)?;
        if main_bytes == 0 || main_bytes > 16 * 1024 * 1024 {
            return Err(LegacyObservationError("legacy main-file bound"));
        }
        let before: u64 = self
            .connection
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .map_err(refused)?;
        let transaction = self.connection.unchecked_transaction().map_err(refused)?;
        let schema = image::capture(&transaction)?;
        let rows = rows::capture(&transaction)?;
        transaction.commit().map_err(refused)?;
        let after: u64 = self
            .connection
            .pragma_query_value(None, "data_version", |r| r.get(0))
            .map_err(refused)?;
        if before != after || Instant::now() >= deadline {
            return Err(LegacyObservationError(
                "legacy image changed or deadline elapsed",
            ));
        }
        self.validate(profile)?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"yield-legacy-source-read-image-v1");
        for identity in [
            profile.owner_identity(),
            self.media_root.identity(),
            self.binding.root().identity(),
            self.binding.identity(),
            self.reservation.observation_lock_identity(),
        ] {
            hash.update(&(identity.len() as u64).to_le_bytes());
            hash.update(identity.as_bytes());
        }
        hash.update(&main_bytes.to_le_bytes());
        hash.update(&schema);
        hash.update(&rows.digest);
        if Instant::now() >= deadline {
            return Err(LegacyObservationError("legacy image deadline elapsed"));
        }
        let digest = *hash.finalize().as_bytes();
        if self
            .original_digest
            .is_some_and(|original| original != digest)
        {
            return Err(LegacyObservationError("original legacy image changed"));
        }
        Ok(LegacySourceImage {
            revision: rows.revision,
            metadata_rows: rows.metadata_rows,
            audio_rows: rows.audio_rows,
            digest,
            main_bytes,
        })
    }
}
fn absent(root: &BoundDatabaseRoot, name: &Path) -> Result<()> {
    match root.symlink_metadata(name) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(LegacyObservationError(
            "existing or uninspectable sidecar evidence",
        )),
    }
}
fn absent_sidecars(root: &BoundDatabaseRoot) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        absent(
            root,
            Path::new(&format!("{}{}", super::DB_FILE_NAME, suffix)),
        )?;
    }
    Ok(())
}
struct Progress<'a>(&'a rusqlite::Connection);
impl<'a> Progress<'a> {
    fn install(connection: &'a rusqlite::Connection, deadline: Instant) -> Self {
        connection.progress_handler(1000, Some(move || Instant::now() >= deadline));
        Self(connection)
    }
}
impl Drop for Progress<'_> {
    fn drop(&mut self) {
        self.0.progress_handler(0, None::<fn() -> bool>);
    }
}
