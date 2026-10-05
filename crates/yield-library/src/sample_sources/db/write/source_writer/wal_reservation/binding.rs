//! Genuine original reservation scope; never a journal decision or allocation policy.
use super::{SourceWalPreparation, SourceWalPreparationError};
use crate::{
    lifecycle_allocation::{self, LifecycleAllocationError},
    sample_sources::db::{
        SourceAuthorityRegistration, SourceRegistryAuditObservation,
        SourceRegistryReferenceSubject, SourceRegistryRegistrationPhase, schema,
        source_registry_reference,
    },
    sqlite_budget::wal_objects::{WalObjectFacts, WalObjects},
};
use serde::Serialize;
use std::{
    io::{self, Write},
    sync::{Arc, Weak},
    time::Instant,
};
use uuid::Uuid;

/// Per-source descriptor ceiling. Coordinators must additionally charge their entire handshake.
pub const SOURCE_WAL_BINDING_MAX_BYTES: usize = 16 * 1024;

/// Native object observations from the original forwarding VFS, never physical qualification.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct SourceWalObjectObservation {
    identity: String,
    device: u64,
    allocation_unit: u64,
    observed_bytes: u64,
}
impl SourceWalObjectObservation {
    /// Descriptor-derived object identity, not a pathname or reopen permission.
    pub fn identity(&self) -> &str {
        &self.identity
    }
    /// Observed native database device, not the media volume.
    pub fn device(&self) -> u64 {
        self.device
    }
    /// Observed native allocation unit, not an allocation allowance.
    pub fn allocation_unit(&self) -> u64 {
        self.allocation_unit
    }
    /// Captured object length; this does not establish remaining capacity.
    pub fn observed_bytes(&self) -> u64 {
        self.observed_bytes
    }
}
impl From<WalObjectFacts> for SourceWalObjectObservation {
    fn from(f: WalObjectFacts) -> Self {
        Self {
            identity: f.identity,
            device: f.device,
            allocation_unit: f.allocation_unit,
            observed_bytes: f.bytes,
        }
    }
}

#[derive(Debug, Serialize)]
struct Descriptor {
    version: u8,
    preparation_nonce: Uuid,
    subject: SourceRegistryReferenceSubject,
    main: SourceWalObjectObservation,
    wal: SourceWalObjectObservation,
    shm: SourceWalObjectObservation,
}

/// Non-Clone, non-deserializable scope issued only by a genuine retained preparation.
/// Stored bytes and borrowed observations cannot substitute the original writer/nonce/deadline.
#[derive(Debug)]
pub struct SourceWalBinding {
    descriptor: Descriptor,
    bytes: Vec<u8>,
    issuer: Weak<()>,
    deadline: Instant,
}
impl SourceWalBinding {
    /// Actual separately owned scalar/encoding buffers plus this binding's inline headers.
    /// Coordinators must additionally charge shared wrapper, queue and reply ownership.
    /// None means accounting is unavailable and must reject; this is never a capacity grant.
    pub fn retained_memory_bytes(&self) -> Option<usize> {
        [
            std::mem::size_of::<Self>(),
            self.bytes.capacity(),
            self.subject().retained_heap_bytes()?,
            self.main().identity.capacity(),
            self.wal().identity.capacity(),
            self.shm().identity.capacity(),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
    }
    /// Historical complete source/lease/registration scope; phase is not journal readiness.
    pub fn subject(&self) -> &SourceRegistryReferenceSubject {
        &self.descriptor.subject
    }
    /// Original native MAIN observation.
    pub fn main(&self) -> &SourceWalObjectObservation {
        &self.descriptor.main
    }
    /// Original forwarding-VFS WAL observation.
    pub fn wal(&self) -> &SourceWalObjectObservation {
        &self.descriptor.wal
    }
    /// Original SQLite mapped SHM observation.
    pub fn shm(&self) -> &SourceWalObjectObservation {
        &self.descriptor.shm
    }
    /// Bounded historical descriptor bytes. Decoding these cannot recreate this binding.
    /// This is not a durable journal inventory or claim format.
    pub fn descriptor_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl SourceWalPreparation {
    /// Capture finite read-only scope inside the original empty transaction before parking.
    /// Prepared registration is observable, but does not authorize admission or publication.
    /// No selected pager cap, logical write allowance or physical reserve is established here.
    pub fn capture_wal_binding(
        &self,
        registration: &SourceAuthorityRegistration,
    ) -> Result<SourceWalBinding, SourceWalPreparationError> {
        use SourceWalPreparationError::BindingUnavailable;
        self.revalidate_owner()?;
        let writer = self.writer();
        let _extensions =
            crate::sqlite_ext::try_extension_free_metadata_scope().ok_or(BindingUnavailable)?;
        let subject =
            lifecycle_allocation::bounded_read(&writer.database.connection, self.deadline, || {
                schema::require_known_metadata_schema(&writer.database.connection)
                    .map_err(|_| LifecycleAllocationError::Unknown)?;
                let scan = writer
                    .current_scan_fence()
                    .map_err(|_| LifecycleAllocationError::Unknown)?;
                let observation = source_registry_reference::capture_registered_in_transaction(
                    &writer.database,
                    &scan,
                    registration,
                )
                .map_err(|_| LifecycleAllocationError::Unknown)?;
                Ok(observation.subject().clone())
            })
            .map_err(|_| BindingUnavailable)?;
        let expected = self.revision();
        if subject.source_id() != expected.source_id().as_str()
            || subject.lifecycle_generation() != expected.lifecycle_generation()
            || subject.lease_epoch() != expected.lease_epoch()
            || subject.scan_revision() != expected.revision()
            || subject.media_root_identity() != expected.root_identity().as_bytes()
            || subject.database_identity() != expected.database_identity()
            || subject.root_binding_identity() != expected.root_binding_identity()
            || !matches!(subject.audit(), SourceRegistryAuditObservation::Captured { revision, .. }
                if *revision == expected.revision())
            || !matches!(
                subject.registration().map(|r| r.phase()),
                Some(
                    SourceRegistryRegistrationPhase::Prepared
                        | SourceRegistryRegistrationPhase::RegisteredPhaseObserved
                )
            )
        {
            return Err(BindingUnavailable);
        }
        source_registry_reference::validate_subject(&subject).map_err(|_| BindingUnavailable)?;
        let objects = self.binding_objects()?;
        if objects.main.identity != subject.database_identity() {
            return Err(BindingUnavailable);
        }
        let descriptor = Descriptor {
            version: 1,
            preparation_nonce: self.nonce(),
            subject,
            main: objects.main.into(),
            wal: objects.wal.into(),
            shm: objects.shm.into(),
        };
        let mut output = BoundedOutput(Vec::new());
        serde_json::to_writer(&mut output, &descriptor).map_err(|_| BindingUnavailable)?;
        let binding = SourceWalBinding {
            descriptor,
            bytes: output.0,
            issuer: Arc::downgrade(&writer.reference_liveness),
            deadline: self.deadline,
        };
        self.validate_wal_binding(&binding, registration)?;
        Ok(binding)
    }

    /// Revalidate only original custody, read-only CONTROL, registration and native objects.
    /// Performs no ordinary SQL, hidden renewal or deadline extension while parked.
    /// Failure retains the original reservation until explicit confirmed abort or drop.
    pub fn validate_wal_binding(
        &self,
        binding: &SourceWalBinding,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceWalPreparationError> {
        use SourceWalPreparationError::BindingUnavailable;
        if binding.descriptor.preparation_nonce != self.nonce()
            || binding.deadline != self.deadline
            || !Weak::ptr_eq(
                &binding.issuer,
                &Arc::downgrade(&self.writer().reference_liveness),
            )
        {
            return Err(BindingUnavailable);
        }
        self.revalidate_owner()?;
        let actual = registration
            .registry_reference_observation(&self.writer().database)
            .map_err(|_| BindingUnavailable)?;
        if binding.subject().registration() != Some(&actual) {
            return Err(BindingUnavailable);
        }
        let objects = self.binding_objects()?;
        if binding.main() != &objects.main.into()
            || binding.wal() != &objects.wal.into()
            || binding.shm() != &objects.shm.into()
        {
            return Err(BindingUnavailable);
        }
        self.revalidate_owner()
    }

    /// Validate original parked scope against the genuine currently Registered source.
    /// Prepared diagnostics cannot pass. No ordinary SQL, renewal, deadline extension,
    /// release, decision latch or capacity/effect permission results from this check.
    pub fn validate_registered_wal_binding(
        &self,
        binding: &SourceWalBinding,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceWalPreparationError> {
        self.validate_wal_binding(binding, registration)?;
        if let Some(custody) = self.held.registered.as_ref() {
            custody
                .validate(&self.writer().database, registration)
                .map_err(|_| SourceWalPreparationError::BindingUnavailable)?;
        }
        registration
            .validate_registered_database_boundary(&self.writer().database)
            .map_err(|_| SourceWalPreparationError::BindingUnavailable)?;
        self.validate_wal_binding(binding, registration)
    }

    /// Retain one genuine currently Registered database use on this original empty reservation.
    /// The fixed owner-local custody survives failed rollback/restoration until actual close.
    /// It grants no decision, write, deadline extension, capacity or settlement authority.
    /// Repeated acquisition refuses; existing parked validation remains read-only.
    pub fn retain_registered_wal_binding(
        &mut self,
        binding: &SourceWalBinding,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceWalPreparationError> {
        use SourceWalPreparationError::BindingUnavailable;
        if self.held.registered.is_some() {
            return Err(BindingUnavailable);
        }
        self.validate_registered_wal_binding(binding, registration)?;
        let custody = registration
            .retain_registered_database_boundary(&self.writer().database)
            .map_err(|_| BindingUnavailable)?;
        self.validate_wal_binding(binding, registration)?;
        custody
            .validate(&self.writer().database, registration)
            .map_err(|_| BindingUnavailable)?;
        self.held.registered = Some(custody);
        Ok(())
    }

    fn binding_objects(&self) -> Result<WalObjects, SourceWalPreparationError> {
        use SourceWalPreparationError::BindingUnavailable;
        let database = &self.writer().database;
        if !self.held.reservation || database.connection.is_autocommit() {
            return Err(BindingUnavailable);
        }
        let file = database
            .database_file_binding
            .as_deref()
            .ok_or(BindingUnavailable)?;
        let objects = database
            .connection
            .retained_wal_objects(file.root(), file.name(), self.deadline)
            .ok_or(BindingUnavailable)?;
        for object in [&objects.main, &objects.wal, &objects.shm] {
            if object.identity.is_empty()
                || object.identity.len() > 256
                || object.identity.chars().any(char::is_control)
                || object.bytes > i64::MAX as u64
                || object.device != objects.main.device
                || object.allocation_unit != objects.main.allocation_unit
            {
                return Err(BindingUnavailable);
            }
        }
        Ok(objects)
    }
}

struct BoundedOutput(Vec<u8>);
impl Write for BoundedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > SOURCE_WAL_BINDING_MAX_BYTES)
        {
            return Err(io::Error::other("bounded source WAL descriptor"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
