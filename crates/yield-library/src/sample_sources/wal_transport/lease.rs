//! Genuine pre-audit input and original native custody on the shared source FIFO.
use super::*;
use crate::operation_journal::source_lease_binding::MAX_SOURCE_LEASE_SCOPE_BYTES;
use crate::sample_sources::db::write::{
    SourceLeaseMaintenanceAllocation, SourceLeaseMaintenanceBinding,
    SourceLeaseMaintenancePreparation, SourceLeaseMaintenancePreparationError,
    SourceLeaseMaintenancePreparationFailure,
};
// The original SourceWriter/database already exists in RegisteredSource and moves
// without duplication. Charge the concrete lease family, not unrelated audited native
// headers or another copy of that original owner. Include complete shared bookkeeping.
const LEASE_CONTEXT_FIXED_BYTES: usize = std::mem::size_of::<PrepareRequest>()
    + std::mem::size_of::<SourceLeaseWalTransportOwner>()
        .saturating_sub(std::mem::size_of::<SourceWriter>())
    + std::mem::size_of::<Ticket>()
    + std::mem::size_of::<Credit>()
    + std::mem::size_of::<SourceLeaseMaintenancePreparation>()
        .saturating_sub(std::mem::size_of::<SourceWriter>())
    + 160;
mod native;
use native::LeaseNative;

/// Original typed lease command. No caller can construct it or obtain an audited revision.
/// ```compile_fail
/// use yield_library::sample_sources::wal_transport::SourceLeaseWalTransportCommand;
/// let forged = SourceLeaseWalTransportCommand {};
/// ```
/// ```compile_fail
/// fn audited(c: &yield_library::sample_sources::wal_transport::SourceLeaseWalTransportCommand) {
///     c.expected_revision();
/// }
/// ```
pub struct SourceLeaseWalTransportCommand(pub(super) SourceWalTransportCommand);
impl SourceLeaseWalTransportCommand {
    /// Original genuine source subject; never an authoritative audit revision.
    pub fn subject(&self) -> &crate::sample_sources::db::SourceRegistryReferenceSubject {
        self.0
            .request()
            .lease
            .as_ref()
            .expect("original lease input")
            .subject()
    }
    /// Original minimum deadline, including time queued.
    pub fn deadline(&self) -> Instant {
        self.0.deadline()
    }
    /// Caller cancellation requires actual native cleanup.
    pub fn is_cancelled(&self) -> bool {
        self.0.is_cancelled()
    }
    /// Reject only before acquiring the original native owner.
    pub fn reject(self, reason: SourceWalTransportRejection) {
        self.0.reject(reason);
    }
    /// Consume the original source writer into genuine lease preparation or retained failure.
    pub fn acquire(mut self, writer: SourceWriter) -> SourceLeaseWalTransportOwner {
        let mut request = self.0.request.take().expect("original lease command");
        let input = request.lease.take().expect("original lease input");
        // Bound initial scope/native capture temporaries before native preparation allocates.
        let mut staging = ByteCredit::in_handshake(&self.0.shared, request.handshake.as_ref());
        let native = match staging.resize(3 * MAX_SOURCE_LEASE_SCOPE_BYTES + 512) {
            Ok(()) => LeaseNative::acquire(
                writer,
                input,
                request.deadline,
                #[cfg(feature = "test-support")]
                self.0.shared.unknown_wal.load(Ordering::Acquire),
            ),
            Err(reason) => LeaseNative::Unprepared(writer, input, reason),
        };
        drop(staging);
        let rejection = native.rejection();
        let owner = SourceLeaseWalTransportOwner {
            native: Some(native),
            scope: None,
            request,
            shared: Arc::clone(&self.0.shared),
            rejection,
            resolved: false,
        };
        if let Some(reason) = rejection {
            owner.mark_unavailable(reason);
        }
        owner
    }
}
impl SourceWalTransport {
    /// Consume genuine pre-audit allocation into the same bounded original source-owner inbox.
    /// Does not renew a lease, issue a required permit, or authorize mutation or settlement.
    pub fn submit_lease_with_binding(
        &self,
        input: SourceLeaseMaintenanceAllocation,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        self.submit_lease_scoped(input, correlation, deadline, None)
    }
    pub(super) fn submit_lease_scoped(
        &self,
        input: SourceLeaseMaintenanceAllocation,
        correlation: SourceWalTransportCorrelation,
        deadline: Instant,
        handshake: Option<Arc<crate::operation_journal::wal_admission::HandshakeBudget>>,
    ) -> Result<SourceWalTransportRequest, SourceWalTransportRejection> {
        let deadline = deadline.min(input.deadline());
        let now = Instant::now();
        if deadline <= now || deadline.duration_since(now) > MAX_DEADLINE {
            return Err(SourceWalTransportRejection::Deadline);
        }
        let size = input
            .retained_input_bytes()
            .and_then(|n| n.checked_add(LEASE_CONTEXT_FIXED_BYTES))
            .ok_or(SourceWalTransportRejection::InvalidContext)?;
        self.enqueue(
            None,
            correlation,
            deadline,
            true,
            None,
            Some(input),
            size,
            handshake,
        )
    }
}

/// Historical original lease scope. Copies cannot recreate native custody or settle a journal.
/// ```compile_fail
/// use yield_library::sample_sources::wal_transport::SourceLeaseWalTransportObservation;
/// let forged = SourceLeaseWalTransportObservation {};
/// ```
#[derive(Clone, Debug)]
pub struct SourceLeaseWalTransportObservation(Arc<LeaseReport>);
#[derive(Debug)]
struct LeaseReport {
    binding: SourceLeaseMaintenanceBinding,
    bytes: Vec<u8>,
    correlation: SourceWalTransportCorrelation,
    wal_bytes: u64,
    registered_custody_retained: bool,
    _credit: ByteCredit,
}
impl PartialEq for SourceLeaseWalTransportObservation {
    fn eq(&self, other: &Self) -> bool {
        self.0.registered_custody_retained == other.0.registered_custody_retained
            && self.0.bytes == other.0.bytes
            && self.0.correlation == other.0.correlation
            && self.0.wal_bytes == other.0.wal_bytes
    }
}
impl Eq for SourceLeaseWalTransportObservation {}
impl SourceLeaseWalTransportObservation {
    // Historical original-producer marker only; no current claim or decision authority.
    pub(crate) fn registered_custody_was_retained(&self) -> bool {
        self.0.registered_custody_retained
    }

    /// Genuine original binding, retaining only historical issuer observations.
    pub fn original_binding(&self) -> &SourceLeaseMaintenanceBinding {
        &self.0.binding
    }
    /// Canonical historical descriptor; decoding it cannot construct this native observation.
    pub fn scope_descriptor_bytes(&self) -> &[u8] {
        &self.0.bytes
    }
    /// Historical correlation, never allocated claims or a decision.
    pub fn correlation(&self) -> &SourceWalTransportCorrelation {
        &self.0.correlation
    }
    /// Actual original WAL observation, never watermark permission.
    pub fn wal_bytes(&self) -> u64 {
        self.0.wal_bytes
    }
}

/// Original thread-local lease custody; only the genuine producer can publish outcomes.
/// ```compile_fail
/// use yield_library::sample_sources::wal_transport::SourceLeaseWalTransportOwner;
/// let forged = SourceLeaseWalTransportOwner {};
/// ```
pub struct SourceLeaseWalTransportOwner {
    native: Option<LeaseNative>,
    scope: Option<SourceLeaseWalTransportObservation>,
    request: PrepareRequest,
    shared: Arc<Shared>,
    rejection: Option<SourceWalTransportRejection>,
    resolved: bool,
}
impl SourceLeaseWalTransportOwner {
    /// Read-only outcome witness retained across cleanup or owner loss.
    pub fn outcome_observation(&self) -> SourceWalOwnerObservation {
        SourceWalOwnerObservation(Arc::clone(&self.request.ticket))
    }
    /// Only original successful native preparation is prepared.
    pub fn is_prepared(&self) -> bool {
        matches!(self.native, Some(LeaseNative::Held(_)))
    }
    /// Bounded initial rejection, never retry or capacity permission.
    pub fn rejection(&self) -> Option<SourceWalTransportRejection> {
        self.rejection
    }
    /// Original minimum deadline.
    pub fn deadline(&self) -> Instant {
        self.request.deadline
    }
    /// Cancellation is a request for real cleanup.
    pub fn is_cancelled(&self) -> bool {
        self.request.ticket.cancelled.load(Ordering::Acquire)
    }
    /// Original native MAIN identity for alias exclusion.
    pub fn database_identity(&self) -> Option<&str> {
        self.native.as_ref()?.database_identity()
    }
    /// Publish only genuine original scope under actual native custody and source registration.
    pub fn publish_prepared(
        &mut self,
        registration: Option<&SourceAuthorityRegistration>,
    ) -> Result<(), SourceWalTransportRejection> {
        if let Some(reason) = self.rejection {
            return Err(reason);
        }
        if self.is_cancelled() {
            return Err(SourceWalTransportRejection::Cancelled);
        }
        let held = self
            .native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?
            .held()?;
        let mut staging = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        staging.resize(scope::PARKED_BYTES)?;
        held.revalidate_owner().map_err(classify)?;
        drop(staging);
        let mut credit = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        credit
            .resize(3 * MAX_SOURCE_LEASE_SCOPE_BYTES + std::mem::size_of::<LeaseReport>() + 512)?;
        let binding = held
            .capture_lease_binding(registration.ok_or(SourceWalTransportRejection::Scope)?)
            .map_err(classify)?;
        let bytes = binding
            .stored_fence()
            .canonical_bytes()
            .map_err(|_| SourceWalTransportRejection::Scope)?;
        let size = binding
            .retained_memory_bytes()
            .and_then(|n| n.checked_add(bytes.capacity()))
            .and_then(|n| n.checked_add(std::mem::size_of::<LeaseReport>() + 64))
            .ok_or(SourceWalTransportRejection::InvalidContext)?;
        credit.resize(size)?;
        if Instant::now() >= self.deadline() {
            return Err(SourceWalTransportRejection::Deadline);
        }
        let wal_bytes = held.wal_bytes();
        if self.request.handshake.is_some() {
            self.native
                .as_mut()
                .ok_or(SourceWalTransportRejection::OwnerLost)?
                .held_mut()?
                .retain_registered_lease_binding(
                    &binding,
                    registration.ok_or(SourceWalTransportRejection::Scope)?,
                )
                .map_err(classify)?;
        }
        // Retention and original scope checks can consume time or race caller cancellation.
        // Failed publication leaves the native count held for original-owner cleanup.
        if self.is_cancelled() {
            return Err(SourceWalTransportRejection::Cancelled);
        }
        if Instant::now() >= self.deadline() {
            return Err(SourceWalTransportRejection::Deadline);
        }
        let scope = SourceLeaseWalTransportObservation(Arc::new(LeaseReport {
            binding,
            bytes,
            correlation: self.request.correlation.clone(),
            wal_bytes,
            registered_custody_retained: self.request.handshake.is_some(),
            _credit: credit,
        }));
        self.scope = Some(scope.clone());
        self.request
            .ticket
            .set(SourceWalTransportStatus::PreparedLease(scope));
        Ok(())
    }
    /// Parked original CONTROL/native validation; no ordinary lease reads or renewal.
    pub fn revalidate(
        &self,
        registration: Option<&SourceAuthorityRegistration>,
    ) -> Result<(), SourceWalTransportRejection> {
        let mut staging = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        staging.resize(scope::PARKED_BYTES)?;
        let held = self
            .native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?
            .held()?;
        match self.scope.as_ref() {
            Some(scope) => {
                let registration = registration.ok_or(SourceWalTransportRejection::Scope)?;
                if self.request.handshake.is_some() {
                    held.validate_registered_lease_binding(scope.original_binding(), registration)
                } else {
                    held.validate_lease_binding(scope.original_binding(), registration)
                }
                .map_err(classify)
            }
            None => held.revalidate_owner().map_err(classify),
        }
    }

    /// Require the original parked scope and genuine current Registered authority.
    /// No historical reply, successful result or caller correlation can latch settlement.
    pub fn validate_registered(
        &self,
        registration: &SourceAuthorityRegistration,
    ) -> Result<(), SourceWalTransportRejection> {
        if self.rejection.is_some() || self.is_cancelled() {
            return Err(self
                .rejection
                .unwrap_or(SourceWalTransportRejection::Cancelled));
        }
        if !matches!(
            self.request.ticket.status(),
            SourceWalTransportStatus::PreparedLease(_)
        ) {
            return Err(SourceWalTransportRejection::Scope);
        }
        let mut staging = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        staging.resize(scope::PARKED_BYTES)?;
        let native = self
            .native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?;
        let held = native.held()?;
        let scope = self
            .scope
            .as_ref()
            .ok_or(SourceWalTransportRejection::Scope)?;
        held.validate_registered_lease_binding(scope.original_binding(), registration)
            .map_err(classify)?;
        if self.is_cancelled() {
            return Err(SourceWalTransportRejection::Cancelled);
        }
        if !matches!(
            self.request.ticket.status(),
            SourceWalTransportStatus::PreparedLease(_)
        ) {
            return Err(SourceWalTransportRejection::Scope);
        }
        Ok(())
    }

    /// Request pre-intent abort while preserving original native custody.
    pub fn request_abort(&mut self, reason: Option<SourceWalTransportRejection>) {
        if self.rejection.is_none() {
            self.rejection = reason;
        }
        self.request.ticket.set(SourceWalTransportStatus::Aborting);
    }
    /// Explicitly preserve unconfirmed cleanup, never an automatic refund or reuse.
    pub fn mark_unavailable(&self, reason: SourceWalTransportRejection) {
        self.request
            .ticket
            .set(SourceWalTransportStatus::Unavailable(reason));
    }
    /// Recover only the identical original writer after confirmed rollback and policy restoration.
    pub fn into_writer(mut self) -> Result<SourceWriter, Self> {
        self.request.ticket.set(SourceWalTransportStatus::Aborting);
        let mut staging = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        let bytes = self
            .native
            .as_ref()
            .expect("original lease custody")
            .cleanup_staging_bytes();
        if let Err(reason) = staging.resize_for_cleanup(bytes) {
            self.mark_unavailable(reason);
            return Err(self);
        }
        match self
            .native
            .take()
            .expect("original lease custody")
            .into_writer()
        {
            Ok(writer) => {
                self.resolved = true;
                drop(self.scope.take());
                drop(self.request.lease.take());
                self.request.ticket.finish(self.rejection.map_or(
                    SourceWalTransportStatus::Aborted,
                    SourceWalTransportStatus::Rejected,
                ));
                Ok(writer)
            }
            Err(native) => {
                self.native = Some(native);
                self.mark_unavailable(
                    self.rejection
                        .unwrap_or(SourceWalTransportRejection::OwnerLost),
                );
                Err(self)
            }
        }
    }
    /// Failed original empty custody alone may be eligible for genuine source-authority close.
    pub fn source_authority_close_ready(&self) -> bool {
        self.native.as_ref().is_some_and(LeaseNative::close_ready)
    }
    /// Publish Retired only after the original draining source authority acknowledges real close.
    pub fn close_for_retirement(
        mut self,
        authority: &mut SourceAuthorityRegistration,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        if !self.source_authority_close_ready() {
            return Err(SourceAuthorityError::InvalidLifecycle);
        }
        let ack = self
            .native
            .take()
            .expect("failed original lease custody")
            .close(authority)?;
        self.resolved = true;
        drop(self.scope.take());
        self.request
            .ticket
            .finish(SourceWalTransportStatus::Retired);
        Ok(ack)
    }
}
impl Drop for SourceLeaseWalTransportOwner {
    fn drop(&mut self) {
        if !self.resolved {
            self.mark_unavailable(SourceWalTransportRejection::OwnerLost);
        }
        if let Some(native) = self.native.take() {
            let mut staging =
                ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
            if staging
                .resize_for_cleanup(native.cleanup_staging_bytes())
                .is_ok()
            {
                drop(native);
            } else {
                native.close_without_reuse();
            }
        }
    }
}
fn classify(
    error: impl std::borrow::Borrow<SourceLeaseMaintenancePreparationError>,
) -> SourceWalTransportRejection {
    use SourceLeaseMaintenancePreparationError::*;
    match error.borrow() {
        Deadline => SourceWalTransportRejection::Deadline,
        TransactionActive | EmptyReservationChanged | RollbackUnconfirmed | PolicyUnconfirmed => {
            SourceWalTransportRejection::OwnerLost
        }
        Allocation(_) | Binding(_) | Owner(_) => SourceWalTransportRejection::Scope,
        Wal(_) | Sqlite(_) => SourceWalTransportRejection::WalUnavailable,
    }
}
