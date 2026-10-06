//! Genuine pre-audit input and original native custody on the shared source FIFO.
use super::*;
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
        let scratch = input
            .preparation_scratch_bytes()
            .ok_or(SourceWalTransportRejection::Full);
        let native = match scratch.and_then(|bytes| staging.resize(bytes)) {
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
            current: Mutex::new(None),
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
            && self.0.binding.retirement_forward_policy()
                == other.0.binding.retirement_forward_policy()
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
    current: Mutex<Option<crate::operation_journal::wal_admission::CurrentCustody>>,
}
impl SourceLeaseWalTransportOwner {
    #[cfg(all(test, target_os = "linux"))]
    pub(crate) fn current_round_for_tests(
        &self,
    ) -> Option<Arc<crate::operation_journal::wal_admission::CurrentRound>> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .map(|guard| guard.original_round_for_tests())
    }

    /// Original journal decision currently prevents native cleanup. Historical replies cannot set it.
    pub fn settlement_held(&self) -> bool {
        self.current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|custody| custody.settlement_held())
    }
    fn begin_cleanup(&self) -> bool {
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        if current
            .as_ref()
            .is_some_and(|custody| !custody.begin_cleanup())
        {
            return false;
        }
        drop(current.take());
        true
    }
    fn invalidate_current(&self) {
        drop(
            self.current
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take(),
        );
    }
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
        let scratch = held
            .binding_scratch_bytes()
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<LeaseReport>() + 512))
            .ok_or(SourceWalTransportRejection::Full)?;
        credit.resize(scratch)?;
        let mut binding = held
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
        self.native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?
            .held()?
            .capture_retirement_forward_policy(
                &mut binding,
                registration.ok_or(SourceWalTransportRejection::Scope)?,
                &bytes,
            )
            .map_err(classify)?;
        // Keep the complete capture debit through Registered retention and policy sealing.
        // The private policy is inline in binding and funded by its concrete header size.
        credit.resize(size)?;
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
        if self.settlement_held() {
            // Cleanup custody is not current validation or effect authority.
            return Err(SourceWalTransportRejection::Busy);
        }
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
        }?;
        // The next genuine Registered validation uses the same parked scratch bound.
        drop(staging);
        let request = self
            .request
            .ticket
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(request) = request {
            self.validate_registered(registration.ok_or(SourceWalTransportRejection::Scope)?)?;
            if Instant::now() >= self.request.deadline {
                return Err(SourceWalTransportRejection::Deadline);
            }
            *self.current.lock().unwrap_or_else(|p| p.into_inner()) = Some(
                request.acknowledge_registered(
                    registration
                        .ok_or(SourceWalTransportRejection::Scope)?
                        .decision_boundary()
                        .ok_or(SourceWalTransportRejection::Full)?,
                ),
            );
        }
        Ok(())
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
        if !self.begin_cleanup() {
            return;
        }
        if self.rejection.is_none() {
            self.rejection = reason;
        }
        self.request.ticket.set(SourceWalTransportStatus::Aborting);
    }
    /// Explicitly preserve unconfirmed cleanup, never an automatic refund or reuse.
    pub fn mark_unavailable(&self, reason: SourceWalTransportRejection) {
        if !self.begin_cleanup() {
            return;
        }
        self.request
            .ticket
            .set(SourceWalTransportStatus::Unavailable(reason));
    }
    /// Recover only the identical original writer after confirmed rollback and policy restoration.
    pub fn into_writer(mut self) -> Result<SourceWriter, Self> {
        if !self.begin_cleanup() {
            return Err(self);
        }
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
        self.invalidate_current();
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
        self.invalidate_current();
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
