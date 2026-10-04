//! Original global worker scope and conservative local handoff memory custody.
use super::*;
use crate::sqlite_budget::wal_objects::{WalObjectFacts, WalObjects};
use serde::Serialize;
use std::io::{self, Write};

const DESCRIPTOR_MAX_BYTES: usize = 4096;
// Pays for capture/encoding, original queued request, observation data, channels and Arc
// headers. Local views share all variable data. This is not the coordinator's full budget.
pub(super) const HANDOFF_BYTES: usize = 16 * 1024;

// Keep fixed request/reply headers within the conservative staging allowance as these
// private types evolve. Variable identity buffers and encoding have separate hard caps.
const _: () = assert!(
    std::mem::size_of::<Command>()
        + std::mem::size_of::<GlobalPreparationRequest>()
        + std::mem::size_of::<GlobalPreparationPending>()
        + std::mem::size_of::<GlobalPreparationReservation>()
        + std::mem::size_of::<ObservationData>()
        + std::mem::size_of::<GlobalWalScope>()
        + 8 * std::mem::size_of::<usize>()
        < 2048
);

#[derive(Debug, Default)]
pub(in super::super) struct Budget(Mutex<usize>);
impl Budget {
    pub(super) fn reserve(self: &Arc<Self>) -> Result<Arc<Credit>, GlobalLibraryWriterQueueError> {
        let mut bytes = self
            .0
            .lock()
            .map_err(|_| GlobalLibraryWriterQueueError::Full)?;
        let next = bytes
            .checked_add(HANDOFF_BYTES)
            .filter(|n| *n <= GLOBAL_PREPARATION_MAX_BYTES)
            .ok_or(GlobalLibraryWriterQueueError::Full)?;
        *bytes = next;
        Ok(Arc::new(Credit(Arc::clone(self))))
    }
}
#[derive(Debug)]
pub(super) struct Credit(Arc<Budget>);
impl Drop for Credit {
    fn drop(&mut self) {
        if let Ok(mut bytes) = self.0.0.lock() {
            // Every credit has one original reservation, shared until its last owner drops.
            *bytes -= HANDOFF_BYTES;
        }
    }
}

/// Borrowed native object facts from an original genuine global preparation.
/// This view carries no writable connection, allocation allowance or qualification.
#[derive(Clone, Copy, Debug)]
pub struct GlobalWalObjectObservation<'a>(&'a WalObjectFacts);
impl GlobalWalObjectObservation<'_> {
    /// Descriptor-proven physical object identity, never a pathname.
    pub fn identity(&self) -> &str {
        &self.0.identity
    }
    /// Native database device observed by the retained descriptor.
    pub fn device(&self) -> u64 {
        self.0.device
    }
    /// Observed native allocation unit, not a write allowance.
    pub fn allocation_unit(&self) -> u64 {
        self.0.allocation_unit
    }
    /// Captured object length, not available physical capacity.
    pub fn observed_bytes(&self) -> u64 {
        self.0.bytes
    }
}

/// Privately issued historical scope from a genuine global empty writer transaction.
/// No constructor or decoder exists; borrowed/encoded observations cannot issue a latch.
///
/// ```compile_fail
/// use yield_library::sample_sources::library::GlobalWalScope;
/// let fabricated = GlobalWalScope {};
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct GlobalWalScope {
    objects: WalObjects,
    bytes: Box<[u8; DESCRIPTOR_MAX_BYTES]>,
    encoded_len: usize,
}
impl GlobalWalScope {
    /// Original retained MAIN object.
    pub fn main(&self) -> GlobalWalObjectObservation<'_> {
        GlobalWalObjectObservation(&self.objects.main)
    }
    /// Original forwarding-VFS WAL object.
    pub fn wal(&self) -> GlobalWalObjectObservation<'_> {
        GlobalWalObjectObservation(&self.objects.wal)
    }
    /// Original SQLite mapped SHM object.
    pub fn shm(&self) -> GlobalWalObjectObservation<'_> {
        GlobalWalObjectObservation(&self.objects.shm)
    }
    /// Versioned bounded historical bytes. These are not a durable claim or intent format.
    pub fn descriptor_bytes(&self) -> &[u8] {
        &self.bytes[..self.encoded_len]
    }
}

#[derive(Serialize)]
struct Object<'a> {
    identity: &'a str,
    device: u64,
    allocation_unit: u64,
    observed_bytes: u64,
}
impl<'a> From<&'a WalObjectFacts> for Object<'a> {
    fn from(f: &'a WalObjectFacts) -> Self {
        Self {
            identity: &f.identity,
            device: f.device,
            allocation_unit: f.allocation_unit,
            observed_bytes: f.bytes,
        }
    }
}
#[derive(Serialize)]
struct Descriptor<'a> {
    version: u8,
    acquisition: &'a str,
    registration: uuid::Uuid,
    preparation_nonce: uuid::Uuid,
    command_digest: &'a [u8; 32],
    mutation_revision: u64,
    main: Object<'a>,
    wal: Object<'a>,
    shm: Object<'a>,
}
struct Output {
    bytes: Box<[u8; DESCRIPTOR_MAX_BYTES]>,
    len: usize,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let end = self
            .len
            .checked_add(bytes.len())
            .filter(|n| *n <= DESCRIPTOR_MAX_BYTES)
            .ok_or_else(|| io::Error::other("global scope descriptor bound"))?;
        self.bytes[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn objects(
    database: &LibraryDatabase,
    deadline: Instant,
) -> Result<WalObjects, GlobalPreparationError> {
    let root = database
        .preparation_root()
        .ok_or(GlobalPreparationError::DatabaseBinding)?;
    database
        .connection
        .retained_wal_objects(
            root,
            Path::new(super::super::super::LIBRARY_DB_FILE_NAME),
            deadline,
        )
        .ok_or(GlobalPreparationError::DatabaseBinding)
}
pub(super) fn capture(
    database: &LibraryDatabase,
    request: &GlobalPreparationRequest,
    nonce: uuid::Uuid,
    mutation_revision: u64,
    main_identity: &str,
    lifecycle: Option<&crate::lifecycle_allocation::LifecycleDatabaseAllocation>,
) -> Result<GlobalWalScope, GlobalPreparationError> {
    let observed = objects(database, request.deadline)?;
    let acquisition = request.owner.acquisition.as_str();
    let facts = [&observed.main, &observed.wal, &observed.shm];
    if database.connection.is_autocommit()
        || acquisition.len() > 64
        || main_identity.len() > 256
        || observed.main.identity != main_identity
        || facts.iter().any(|f| {
            f.identity.len() > 256
                || f.identity.capacity() > 512
                || f.device != observed.main.device
                || f.allocation_unit != observed.main.allocation_unit
        })
    {
        return Err(GlobalPreparationError::DatabaseBinding);
    }
    if let Some(lifecycle) = lifecycle {
        for (current, prior) in [
            (&observed.main, lifecycle.main()),
            (&observed.wal, lifecycle.wal()),
            (&observed.shm, lifecycle.shm()),
        ] {
            if current.identity != prior.identity()
                || current.device != prior.device()
                || current.allocation_unit != prior.allocation_unit()
                || current.bytes != prior.observed_bytes()
            {
                return Err(GlobalPreparationError::DatabaseBinding);
            }
        }
    }
    // One fixed encoding buffer, with no grow/shrink copy and no separate cloned descriptor.
    let mut output = Output {
        bytes: Box::new([0; DESCRIPTOR_MAX_BYTES]),
        len: 0,
    };
    serde_json::to_writer(
        &mut output,
        &Descriptor {
            version: 1,
            acquisition,
            registration: request.owner.registration,
            preparation_nonce: nonce,
            command_digest: &request.command_digest,
            mutation_revision,
            main: (&observed.main).into(),
            wal: (&observed.wal).into(),
            shm: (&observed.shm).into(),
        },
    )
    .map_err(|_| GlobalPreparationError::InputBound)?;
    let scope = GlobalWalScope {
        objects: observed,
        bytes: output.bytes,
        encoded_len: output.len,
    };
    validate(database, &scope, request.deadline)?;
    Ok(scope)
}
pub(super) fn validate(
    database: &LibraryDatabase,
    scope: &GlobalWalScope,
    deadline: Instant,
) -> Result<(), GlobalPreparationError> {
    if Instant::now() >= deadline {
        return Err(GlobalPreparationError::Deadline);
    }
    if objects(database, deadline)? != scope.objects {
        return Err(GlobalPreparationError::DatabaseBinding);
    }
    if Instant::now() >= deadline {
        return Err(GlobalPreparationError::Deadline);
    }
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod binding_tests;
