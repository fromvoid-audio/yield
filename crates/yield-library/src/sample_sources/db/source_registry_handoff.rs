//! Authenticated metadata-only custody. Observations cannot reconstruct this handoff.
use super::source_authority::{SourceEffectGate, SourceEffectPermit, SourceEffectPin};
use super::{
    SourceAuthorityError, SourceAuthorityRegistration, SourceAuthorityRegistrationReceipt,
    SourceAuthorityRequest, SourceRegistryReferenceError, SourceRegistryReferenceObservation,
};
use crate::app_dirs::WritableProfileGuard;
use std::sync::{
    Arc, Weak,
    atomic::{AtomicU8, AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};
use thiserror::Error;

pub(crate) const MAX_SOURCE_REGISTRY_CARRIERS: usize = 4096;
static CARRIERS: AtomicUsize = AtomicUsize::new(0);

pub(crate) struct Credit;
impl Credit {
    fn acquire() -> Result<Self, SourceRegistryMetadataHandoffError> {
        CARRIERS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_SOURCE_REGISTRY_CARRIERS).then(|| n + 1)
            })
            .map_err(|_| SourceRegistryMetadataHandoffError::Full)?;
        Ok(Self)
    }
}
impl Drop for Credit {
    fn drop(&mut self) {
        CARRIERS.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(crate) fn capture_credit(
    deadline: Instant,
) -> Result<Credit, SourceRegistryMetadataHandoffError> {
    check_deadline(deadline)?;
    Credit::acquire()
}

// Only the original journal record owns this liveness state. Opaque metadata
// handoffs borrow a weak token; owner shutdown, partial state or drain revokes it.
pub(crate) struct MetadataRegistrationCustody(Arc<AtomicU8>);
impl MetadataRegistrationCustody {
    pub(crate) fn new() -> Self {
        Self(Arc::new(AtomicU8::new(0)))
    }
    pub(crate) fn activate(&self) -> Result<(), SourceRegistryMetadataHandoffError> {
        self.0
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| SourceRegistryMetadataHandoffError::RegistrationLost)
    }
    pub(crate) fn revoke(&self) {
        self.0.store(2, Ordering::Release);
    }
    pub(crate) fn token(&self) -> Weak<AtomicU8> {
        Arc::downgrade(&self.0)
    }
}
impl Drop for MetadataRegistrationCustody {
    fn drop(&mut self) {
        self.revoke();
    }
}

pub(crate) struct WriterCustody {
    gate: Arc<SourceEffectGate>,
}
impl WriterCustody {
    pub(crate) fn new() -> Self {
        Self {
            gate: Arc::new(SourceEffectGate::new()),
        }
    }
    pub(crate) fn gate(&self) -> Arc<SourceEffectGate> {
        Arc::clone(&self.gate)
    }
}
impl Drop for WriterCustody {
    fn drop(&mut self) {
        self.gate.close_and_wait_for_custody();
    }
}

/// A handoff error grants no registration, successful SQL or physical capacity.
#[derive(Debug, Error)]
pub enum SourceRegistryMetadataHandoffError {
    /// Actual source capture failed.
    #[error(transparent)]
    Observation(#[from] SourceRegistryReferenceError),
    /// Original source ownership or private registration validation failed.
    #[error(transparent)]
    Authority(#[from] SourceAuthorityError),
    /// Carrier count is exhausted; nothing is evicted.
    #[error("source metadata handoff carrier limit reached")]
    Full,
    /// Original two-second handoff deadline expired or was invalid.
    #[error("source metadata handoff deadline expired or exceeds two seconds")]
    Deadline,
    /// The original writer was handed off, dropped or replaced.
    #[error("source metadata handoff writer lifetime changed")]
    WriterLost,
    /// Original journal registration was drained, became partial or lost its owner.
    #[error("source metadata handoff original journal registration unavailable")]
    RegistrationLost,
    /// Current consumer profile or immutable proposal subject differs.
    #[error("source metadata handoff context mismatch")]
    Context,
}

/// Non-cloneable, non-serializable envelope issued only by the actual source writer.
pub struct SourceRegistryMetadataCandidate {
    pub(crate) observation: SourceRegistryReferenceObservation,
    pub(crate) request: Arc<SourceAuthorityRequest>,
    pub(crate) receipt: SourceAuthorityRegistrationReceipt,
    pub(crate) pin: SourceEffectPin,
    pub(crate) writer: Weak<()>,
    writer_gate: Arc<SourceEffectGate>,
    deadline: Instant,
    _credit: Credit,
}

impl SourceRegistryMetadataCandidate {
    pub(crate) fn capture(
        observation: SourceRegistryReferenceObservation,
        registration: &SourceAuthorityRegistration,
        receipt: SourceAuthorityRegistrationReceipt,
        writer: Weak<()>,
        writer_gate: Arc<SourceEffectGate>,
        deadline: Instant,
        credit: Credit,
    ) -> Result<Self, SourceRegistryMetadataHandoffError> {
        check_deadline(deadline)?;
        // Credit was reserved before the actual source observation/capture.
        let (request, pin) = registration.metadata_handoff_parts(&receipt)?;
        let result = Self {
            observation,
            request,
            receipt,
            pin,
            writer,
            writer_gate,
            deadline,
            _credit: credit,
        };
        result.validate_writer()?;
        Ok(result)
    }
    /// Historical metadata only; this borrow supplies no mutation permission.
    pub fn observation(&self) -> &SourceRegistryReferenceObservation {
        &self.observation
    }
    pub(crate) fn input_bytes(&self) -> usize {
        // Capture bounded the complete escaped JSON to16KiB; fixed private headers and
        // the receipt's at-most-two fixed-sized volume identities are charged as well.
        super::SOURCE_REGISTRY_REFERENCE_MAX_BYTES
            + std::mem::size_of::<Self>()
            + std::mem::size_of::<SourceRegistryMetadataPermit>()
            + 128
            + 2 * std::mem::size_of::<crate::durable_capacity::VolumeIdentity>()
    }
    fn validate_writer(&self) -> Result<(), SourceRegistryMetadataHandoffError> {
        if self.writer.strong_count() == 0 {
            return Err(SourceRegistryMetadataHandoffError::WriterLost);
        }
        Ok(())
    }
    pub(crate) fn into_permit(
        self,
        profile: &WritableProfileGuard,
        registration: Weak<AtomicU8>,
    ) -> Result<SourceRegistryMetadataPermit, SourceRegistryMetadataHandoffError> {
        check_deadline(self.deadline)?;
        self.pin.validate_for_issuance(&self.request, profile)?;
        self.validate_writer()?;
        let result = SourceRegistryMetadataPermit {
            candidate: self,
            registration,
        };
        result.validate_metadata_boundary(profile, result.observation().subject())?;
        Ok(result)
    }
}

fn check_deadline(deadline: Instant) -> Result<(), SourceRegistryMetadataHandoffError> {
    match deadline.checked_duration_since(Instant::now()) {
        Some(left) if !left.is_zero() && left <= Duration::from_secs(2) => Ok(()),
        _ => Err(SourceRegistryMetadataHandoffError::Deadline),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn poisoned_writer_gate_still_waits_for_original_custody_and_never_reopens() {
        let custody = WriterCustody::new();
        let gate = custody.gate();
        let active = gate.begin(|| Ok(())).unwrap();
        assert!(
            std::panic::catch_unwind(|| {
                let _ = gate.begin(|| panic!("deliberately poison metadata validation gate"));
            })
            .is_err()
        );
        let (tx, rx) = std::sync::mpsc::channel();
        let joined = std::thread::spawn(move || {
            drop(custody);
            tx.send(()).unwrap();
        });
        let stop = Instant::now() + Duration::from_secs(2);
        while !gate.is_closed_for_tests() {
            assert!(Instant::now() < stop);
            std::thread::yield_now();
        }
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        assert!(gate.begin(|| Ok(())).is_err());
        drop(active);
        rx.recv_timeout(Duration::from_secs(2)).unwrap();
        joined.join().unwrap();
    }
    #[test]
    fn carrier_credit_is_bounded_and_drop_refunds_without_eviction() {
        let _runtime = crate::test_runtime::TestRuntimeGuard::acquire();
        assert_eq!(CARRIERS.load(Ordering::Acquire), 0);
        let mut held: Vec<_> = (0..MAX_SOURCE_REGISTRY_CARRIERS)
            .map(|_| Credit::acquire().unwrap())
            .collect();
        assert!(matches!(
            Credit::acquire(),
            Err(SourceRegistryMetadataHandoffError::Full)
        ));
        assert_eq!(held.len(), MAX_SOURCE_REGISTRY_CARRIERS);
        held.pop();
        let replacement = Credit::acquire().unwrap();
        drop(held);
        assert_eq!(CARRIERS.load(Ordering::Acquire), 1);
        drop(replacement);
        assert_eq!(CARRIERS.load(Ordering::Acquire), 0);
    }
}

/// Restricted custody for a future admitted global metadata transaction.
/// It is neither a file-effect grant nor a required-commit/capacity permission.
/// Its private SQL-entry custody holds both original gates through actual SQL quiescence.
/// Merely retaining a delivered handoff does not start a SQL effect.
pub struct SourceRegistryMetadataPermit {
    candidate: SourceRegistryMetadataCandidate,
    registration: Weak<AtomicU8>,
}

/// Quiescence custody entered by the existing global worker immediately before admitted SQL.
/// Borrowing the permit keeps its original lease pin alive through both effect counts.
#[allow(dead_code)] // The production admitted SQL consumer is OPT-2195.
pub(crate) struct SourceRegistryMetadataSqlCustody<'a> {
    _source_effect: SourceEffectPermit,
    _writer_effect: SourceEffectPermit,
    _permit: &'a SourceRegistryMetadataPermit,
}

impl SourceRegistryMetadataPermit {
    /// Retained historical subject, not an enrollment receipt or current source fence.
    pub fn observation(&self) -> &SourceRegistryReferenceObservation {
        self.candidate.observation()
    }
    #[allow(dead_code)] // The production admitted SQL consumer is OPT-2195.
    pub(crate) fn enter_metadata_sql(
        &self,
        profile: &WritableProfileGuard,
        subject: &super::SourceRegistryReferenceSubject,
    ) -> Result<SourceRegistryMetadataSqlCustody<'_>, SourceRegistryMetadataHandoffError> {
        self.validate_metadata_boundary(profile, subject)?;
        let writer_effect = self.candidate.writer_gate.begin(|| {
            self.candidate
                .validate_writer()
                .map_err(|_| SourceAuthorityError::ConfirmationClosed)
        })?;
        let source_effect = self.candidate.pin.begin_effect()?;
        // Close/drop racing either begin is observed at the same original boundaries.
        self.validate_metadata_boundary(profile, subject)?;
        Ok(SourceRegistryMetadataSqlCustody {
            _source_effect: source_effect,
            _writer_effect: writer_effect,
            _permit: self,
        })
    }

    #[allow(dead_code)] // Actual admitted global consumer is OPT-2195, not ordinary SQL.
    pub(crate) fn validate_metadata_boundary(
        &self,
        profile: &WritableProfileGuard,
        subject: &super::SourceRegistryReferenceSubject,
    ) -> Result<(), SourceRegistryMetadataHandoffError> {
        check_deadline(self.candidate.deadline)?;
        if self
            .registration
            .upgrade()
            .is_none_or(|live| live.load(Ordering::Acquire) != 1)
        {
            return Err(SourceRegistryMetadataHandoffError::RegistrationLost);
        }
        self.candidate.validate_writer()?;
        if subject != self.observation().subject()
            || profile.owner_identity() != subject.profile_acquisition().to_string()
        {
            return Err(SourceRegistryMetadataHandoffError::Context);
        }
        self.candidate
            .pin
            .validate_for_issuance(&self.candidate.request, profile)?;
        Ok(())
    }
}
