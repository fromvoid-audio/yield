//! Private genuine source custody. No caller-provided state can publish Prepared or cleanup.
use super::*;

enum Native {
    Held(SourceWalPreparation),
    Failed(SourceWalPreparationFailure),
}
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
}
/// Read-only owner outcome witness. It cannot publish readiness, rollback or retirement.
#[derive(Clone)]
pub struct SourceWalOwnerObservation(Arc<Ticket>);
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
    let request = command.request.take().expect("original source command");
    let expected = request.expected.as_ref().expect("pending revision");
    #[cfg(feature = "test-support")]
    let prepared = if command.shared.unknown_wal.load(Ordering::Acquire) {
        writer.prepare_wal_reservation_unknown_for_tests(expected, request.deadline)
    } else {
        writer.prepare_wal_reservation(expected, request.deadline)
    };
    #[cfg(not(feature = "test-support"))]
    let prepared = writer.prepare_wal_reservation(expected, request.deadline);
    let (native, rejection) = match prepared {
        Ok(held) => (Native::Held(held), None),
        Err(failure) => {
            let reason = classify(failure.error());
            (Native::Failed(failure), Some(reason))
        }
    };
    let owner = SourceWalTransportOwner {
        native: Some(native),
        scope: None,
        request,
        shared: Arc::clone(&command.shared),
        rejection,
        resolved: false,
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
    /// Retain a read-only outcome witness across native cleanup/owner loss.
    pub fn outcome_observation(&self) -> SourceWalOwnerObservation {
        SourceWalOwnerObservation(Arc::clone(&self.request.ticket))
    }
    /// Genuine successful preparation is distinct from a retained failed native owner.
    pub fn is_prepared(&self) -> bool {
        matches!(self.native, Some(Native::Held(_)))
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
        match self.native.as_ref()? {
            Native::Held(held) => Some(held.revision().database_identity()),
            Native::Failed(failure) => failure.retained_database_identity(),
        }
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
        let Some(Native::Held(held)) = self.native.as_ref() else {
            return Err(SourceWalTransportRejection::OwnerLost);
        };
        held.revalidate_owner().map_err(|e| classify(&e))?;
        if self.request.require_binding {
            self.scope = Some(scope::capture_scope(
                &self.shared,
                held,
                registration.ok_or(SourceWalTransportRejection::Scope)?,
            )?);
        }
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
        let mut credit = ByteCredit::new(&self.shared);
        credit.resize(heap)?;
        if Instant::now() >= self.request.deadline {
            return Err(SourceWalTransportRejection::Deadline);
        }
        let observation = SourceWalTransportObservation {
            data: Arc::new(ObservationData {
                revision: self.request.expected.take().expect("original revision"),
                correlation: self.request.correlation.clone(),
                nonce: held.nonce().to_string(),
                wal_bytes: held.wal_bytes(),
                scope: self.scope.clone(),
                _credit: credit,
            }),
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
        match self.native.as_ref() {
            Some(Native::Held(held)) => match self.scope.as_ref() {
                Some(scope) => held
                    .validate_wal_binding(
                        scope.binding(),
                        registration.ok_or(SourceWalTransportRejection::Scope)?,
                    )
                    .map_err(|e| classify(&e)),
                None => held.revalidate_owner().map_err(|e| classify(&e)),
            },
            _ => Err(self
                .rejection
                .unwrap_or(SourceWalTransportRejection::OwnerLost)),
        }
    }
    /// Begin pre-intent abort; the original writer remains exclusively held until real cleanup.
    pub fn request_abort(&mut self, reason: Option<SourceWalTransportRejection>) {
        if self.rejection.is_none() {
            self.rejection = reason;
        }
        self.request.ticket.set(SourceWalTransportStatus::Aborting);
    }
    /// Keep a cleanup failure explicitly unavailable, without claiming rollback or close.
    pub fn mark_unavailable(&self, reason: SourceWalTransportRejection) {
        self.request
            .ticket
            .set(SourceWalTransportStatus::Unavailable(reason));
    }
    /// Return only the exact original writer after confirmed rollback and timeout restoration.
    pub fn into_writer(mut self) -> Result<SourceWriter, Self> {
        self.request.ticket.set(SourceWalTransportStatus::Aborting);
        let result = match self.native.take().expect("original native custody") {
            Native::Held(held) => held.abort(),
            Native::Failed(failed) => failed.into_writer(),
        };
        match result {
            Ok(writer) => {
                self.resolved = true;
                drop(self.scope.take());
                self.request.ticket.finish(self.rejection.map_or(
                    SourceWalTransportStatus::Aborted,
                    SourceWalTransportStatus::Rejected,
                ));
                Ok(writer)
            }
            Err(failed) => {
                self.native = Some(Native::Failed(failed));
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
        matches!(self.native.as_ref(), Some(Native::Failed(failure)) if failure.source_authority_close_ready())
    }
    /// Consume only actual failed custody through original draining source authority.
    /// Retired is published only after the genuine close/release acknowledgement returns.
    pub fn close_for_retirement(
        mut self,
        authority: &mut SourceAuthorityRegistration,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        let failure = match self.native.take() {
            Some(Native::Failed(failure)) => failure,
            native => {
                self.native = native;
                return Err(SourceAuthorityError::InvalidLifecycle);
            }
        };
        let ack = failure.close_for_retirement(authority)?;
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
        | SourceWalPreparationError::RollbackUnconfirmed => SourceWalTransportRejection::OwnerLost,
    }
}
