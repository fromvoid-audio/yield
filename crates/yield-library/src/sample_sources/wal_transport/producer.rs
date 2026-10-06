//! Private genuine source custody. No caller-provided state can publish Prepared or cleanup.
use super::*;

/// Original source actor's genuine native preparation/failure custody.
/// No public constructor, Clone, decoder or arbitrary positive outcome setter exists.
///
/// ```compile_fail
/// use yield_library::sample_sources::wal_transport::SourceWalTransportOwner;
/// let forged = SourceWalTransportOwner {};
/// ```
pub struct SourceWalTransportOwner {
    native: Option<Native>,
    scope: Option<ScopeView>,
    request: PrepareRequest,
    shared: Arc<Shared>,
    rejection: Option<SourceWalTransportRejection>,
    resolved: bool,
    current: Mutex<Option<crate::operation_journal::wal_admission::CurrentCustody>>,
}
/// Read-only owner outcome witness. It cannot publish readiness, rollback or retirement.
#[derive(Clone)]
pub struct SourceWalOwnerObservation(pub(super) Arc<Ticket>);
impl SourceWalOwnerObservation {
    /// Observe actual producer status without SQL or effect authority.
    pub fn status(&self) -> SourceWalTransportStatus {
        self.0.status()
    }
}

pub(super) fn acquire(
    mut command: SourceWalTransportCommand,
    writer: SourceWriter,
) -> SourceWalTransportOwner {
    let mut request = command.request.take().expect("original source command");
    let expected = request.expected.as_ref().expect("pending revision");
    let mut staging = ByteCredit::in_handshake(&command.shared, request.handshake.as_ref());
    // Full current owner/schema/native validation shares the original bounded capture pool.
    // Reserve its conservative staging BEFORE native preparation executes or allocates.
    let staging_rejection = staging.resize(scope::CAPTURE_BYTES).err();
    let native = if staging_rejection.is_some() {
        Native::Unprepared(writer, request.policy.take())
    } else {
        #[cfg(feature = "test-support")]
        let native = if command.shared.unknown_wal.load(Ordering::Acquire) {
            // Negative I/O fault only: no family policy or positive preparation is published.
            match writer.prepare_wal_reservation_unknown_for_tests(expected, request.deadline) {
                Ok(held) => Native::Held(held),
                Err(failed) => Native::Failed(failed),
            }
        } else {
            Native::acquire(writer, expected, request.deadline, request.policy.take())
        };
        #[cfg(not(feature = "test-support"))]
        let native = Native::acquire(writer, expected, request.deadline, request.policy.take());
        native
    };
    drop(staging);
    let rejection = staging_rejection.or_else(|| native.error().map(classify));
    let owner = SourceWalTransportOwner {
        native: Some(native),
        scope: None,
        request,
        shared: Arc::clone(&command.shared),
        rejection,
        resolved: false,
        current: Mutex::new(None),
    };
    if let Some(reason) = owner.rejection {
        owner
            .request
            .ticket
            .set(SourceWalTransportStatus::Unavailable(reason));
    }
    owner
}
impl SourceWalTransportOwner {
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
    /// Retain a read-only outcome witness across native cleanup/owner loss.
    pub fn outcome_observation(&self) -> SourceWalOwnerObservation {
        SourceWalOwnerObservation(Arc::clone(&self.request.ticket))
    }
    /// Genuine successful preparation is distinct from a retained failed native owner.
    pub fn is_prepared(&self) -> bool {
        self.native.as_ref().is_some_and(Native::is_prepared)
    }
    /// Initial or later pre-intent rejection, never a grant or retry permission.
    pub fn rejection(&self) -> Option<SourceWalTransportRejection> {
        self.rejection
    }
    /// Original common deadline; no caller timer proves rollback.
    pub fn deadline(&self) -> Instant {
        self.request.deadline
    }
    /// Original caller cancellation, which must still be cleaned up on this actor.
    pub fn is_cancelled(&self) -> bool {
        self.request.ticket.cancelled.load(Ordering::Acquire)
    }
    /// Strict original native MAIN identity for alias exclusion, not reopen authority.
    pub fn database_identity(&self) -> Option<&str> {
        self.native.as_ref()?.database_identity()
    }
    /// Publish only after this exact native owner is held and original scope is validated.
    /// Strict requests require the genuine registration; there is no caller-authored acknowledgement.
    pub fn publish_prepared(
        &mut self,
        registration: Option<&SourceAuthorityRegistration>,
    ) -> Result<(), SourceWalTransportRejection> {
        if self.request.expected.is_none() {
            return Err(SourceWalTransportRejection::InvalidContext);
        }
        if let Some(reason) = self.rejection {
            return Err(reason);
        }
        if self.is_cancelled() {
            return Err(SourceWalTransportRejection::Cancelled);
        }
        let native = self
            .native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?;
        let held = native.held().map_err(|e| classify(&e))?;
        let mut staging = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        staging.resize(scope::PARKED_BYTES)?;
        held.revalidate_owner().map_err(|e| classify(&e))?;
        drop(staging);
        if self.request.require_binding {
            self.scope = Some(scope::capture_scope(
                &self.shared,
                held,
                registration.ok_or(SourceWalTransportRejection::Scope)?,
                self.request.handshake.as_ref(),
            )?);
        }
        // Only the private original journal collection route requires retained Registered use.
        // Standalone diagnostic capture continues to observe genuine Prepared registrations.
        if self.request.handshake.is_some() {
            let binding = self
                .scope
                .as_ref()
                .ok_or(SourceWalTransportRejection::Scope)?
                .binding();
            self.native
                .as_mut()
                .ok_or(SourceWalTransportRejection::OwnerLost)?
                .retain_registered(
                    binding,
                    registration.ok_or(SourceWalTransportRejection::Scope)?,
                )
                .map_err(|e| classify(&e))?;
        }
        let native = self
            .native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?;
        let held = native.held().map_err(|e| classify(&e))?;
        // Charge complete immutable view before moving the original expected revision into it.
        let expected = self
            .request
            .expected
            .as_ref()
            .ok_or(SourceWalTransportRejection::InvalidContext)?;
        let heap = [
            expected.source_id().as_str().len(),
            expected.root_identity().as_bytes().len(),
            expected.root_binding_identity().len(),
            expected.database_identity().len(),
        ]
        .into_iter()
        .try_fold(0usize, usize::checked_add)
        .and_then(|n| n.checked_mul(3))
        .and_then(|n| n.checked_add(std::mem::size_of::<ObservationData>() + 128))
        .ok_or(SourceWalTransportRejection::InvalidContext)?;
        let mut credit = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        credit.resize(heap)?;
        if self.is_cancelled() {
            return Err(SourceWalTransportRejection::Cancelled);
        }
        if Instant::now() >= self.request.deadline {
            return Err(SourceWalTransportRejection::Deadline);
        }
        let observation = SourceWalTransportObservation {
            data: Some(Arc::new(ObservationData {
                revision: self.request.expected.take().expect("original revision"),
                correlation: self.request.correlation.clone(),
                nonce: held.nonce().to_string(),
                wal_bytes: held.wal_bytes(),
                scope: self.scope.clone(),
                policy_family: native.family(),
                registered_custody_retained: self.request.handshake.is_some(),
                _credit: credit,
            })),
        };
        self.request
            .ticket
            .set(SourceWalTransportStatus::Prepared(observation));
        Ok(())
    }
    /// Original CONTROL/native-only validation while parked; no renewal or ordinary SQL.
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
        let native = self
            .native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?;
        let held = native.held().map_err(|e| classify(&e))?;
        match self.scope.as_ref() {
            Some(scope) => {
                let registration = registration.ok_or(SourceWalTransportRejection::Scope)?;
                if self.request.handshake.is_some() {
                    held.validate_registered_wal_binding(scope.binding(), registration)
                } else {
                    held.validate_wal_binding(scope.binding(), registration)
                }
                .map_err(|e| classify(&e))
            }
            None => held.revalidate_owner().map_err(|e| classify(&e)),
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
            SourceWalTransportStatus::Prepared(_)
        ) {
            return Err(SourceWalTransportRejection::Scope);
        }
        let mut staging = ByteCredit::in_handshake(&self.shared, self.request.handshake.as_ref());
        staging.resize(scope::PARKED_BYTES)?;
        let native = self
            .native
            .as_ref()
            .ok_or(SourceWalTransportRejection::OwnerLost)?;
        let held = native.held().map_err(|e| classify(&e))?;
        let scope = self
            .scope
            .as_ref()
            .ok_or(SourceWalTransportRejection::Scope)?;
        held.validate_registered_wal_binding(scope.binding(), registration)
            .map_err(|e| classify(&e))?;
        if self.is_cancelled() {
            return Err(SourceWalTransportRejection::Cancelled);
        }
        if !matches!(
            self.request.ticket.status(),
            SourceWalTransportStatus::Prepared(_)
        ) {
            return Err(SourceWalTransportRejection::Scope);
        }
        Ok(())
    }

    /// Begin pre-intent abort; the original writer remains exclusively held until real cleanup.
    pub fn request_abort(&mut self, reason: Option<SourceWalTransportRejection>) {
        if !self.begin_cleanup() {
            return;
        }
        if self.rejection.is_none() {
            self.rejection = reason;
        }
        self.request.ticket.set(SourceWalTransportStatus::Aborting);
    }
    /// Keep a cleanup failure explicitly unavailable, without claiming rollback or close.
    pub fn mark_unavailable(&self, reason: SourceWalTransportRejection) {
        if !self.begin_cleanup() {
            return;
        }
        self.request
            .ticket
            .set(SourceWalTransportStatus::Unavailable(reason));
    }
    /// Return only the exact original writer after confirmed rollback and timeout restoration.
    pub fn into_writer(mut self) -> Result<SourceWriter, Self> {
        if !self.begin_cleanup() {
            return Err(self);
        }
        self.request.ticket.set(SourceWalTransportStatus::Aborting);
        let result = self
            .native
            .take()
            .expect("original native custody")
            .into_writer();
        match result {
            Ok(writer) => {
                self.resolved = true;
                drop(self.scope.take());
                drop(self.request.expected.take());
                drop(self.request.policy.take());
                self.request.ticket.finish(self.rejection.map_or(
                    SourceWalTransportStatus::Aborted,
                    SourceWalTransportStatus::Rejected,
                ));
                Ok(writer)
            }
            Err(failed) => {
                self.native = Some(failed);
                self.mark_unavailable(
                    self.rejection
                        .unwrap_or(SourceWalTransportRejection::OwnerLost),
                );
                Err(self)
            }
        }
    }
    /// Failed original owner is close-ready only under its original source authority.
    pub fn source_authority_close_ready(&self) -> bool {
        self.native.as_ref().is_some_and(Native::close_ready)
    }
    /// Consume only actual failed custody through original draining source authority.
    /// Retired is published only after the genuine close/release acknowledgement returns.
    pub fn close_for_retirement(
        mut self,
        authority: &mut SourceAuthorityRegistration,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        self.invalidate_current();
        if !self.source_authority_close_ready() {
            return Err(SourceAuthorityError::InvalidLifecycle);
        }
        let native = self.native.take().expect("original failed custody");
        let ack = native.close(authority)?;
        self.resolved = true;
        drop(self.scope.take());
        self.request
            .ticket
            .finish(SourceWalTransportStatus::Retired);
        Ok(ack)
    }
}
impl Drop for SourceWalTransportOwner {
    fn drop(&mut self) {
        self.invalidate_current();
        if !self.resolved {
            // Publish loss BEFORE private native custody drops; never report automatic reuse.
            self.request
                .ticket
                .set(SourceWalTransportStatus::Unavailable(
                    SourceWalTransportRejection::OwnerLost,
                ));
        }
    }
}
pub(super) fn classify(error: &SourceWalPreparationError) -> SourceWalTransportRejection {
    match error {
        SourceWalPreparationError::Deadline => SourceWalTransportRejection::Deadline,
        SourceWalPreparationError::LeaseRenewalRequired => {
            SourceWalTransportRejection::LeaseRenewalRequired
        }
        SourceWalPreparationError::Owner(_) | SourceWalPreparationError::BindingUnavailable => {
            SourceWalTransportRejection::Scope
        }
        SourceWalPreparationError::Wal(_) | SourceWalPreparationError::Sqlite(_) => {
            SourceWalTransportRejection::WalUnavailable
        }
        SourceWalPreparationError::TransactionActive
        | SourceWalPreparationError::EmptyReservationChanged
        | SourceWalPreparationError::RollbackUnconfirmed => SourceWalTransportRejection::OwnerLost,
    }
}
