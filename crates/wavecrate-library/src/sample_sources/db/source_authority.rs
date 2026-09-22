//! Private source-owner handoff values for external capacity registration.
//!
//! A [`SourceAuthorityRequest`] is the only value that may cross from a source owner to the
//! journal owner.  It contains duplicated descriptor-bound root capabilities and immutable lease
//! identity evidence, but never the source lease, its control connection, or a publication fence.
//! The [`SourceAuthorityGuard`] remains with the source owner and is the only value that can mint
//! a fresh one-shot confirmation or complete retirement.

#![allow(dead_code)]

use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use uuid::Uuid;

use super::file_ops_journal::BoundDatabaseRoot;
use super::source_lease::{SourceLease, SourceLeaseError};
use super::sqlite_identity::DatabaseFileBinding;
use super::{SourceDatabase, SourceDbError};

const PREPARED: u8 = 0;
const REGISTERED: u8 = 1;
const DRAINING: u8 = 2;
const RETIRED: u8 = 3;
const ABANDONED: u8 = 4;

fn unix_now() -> Result<i64, SourceAuthorityError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SourceAuthorityError::ConfirmationStale)?
        .as_secs();
    i64::try_from(seconds).map_err(|_| SourceAuthorityError::ConfirmationStale)
}

/// A stable owner and epoch captured from one retained source lease.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceLeaseIdentity {
    owner_identity: String,
    lease_epoch: i64,
}

impl SourceLeaseIdentity {
    fn from_lease(lease: &SourceLease) -> Self {
        Self {
            owner_identity: lease.owner_identity().to_owned(),
            lease_epoch: lease.epoch(),
        }
    }
}

/// Opaque identity for one source-volume registration handshake.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct SourceRegistrationId(Uuid);

#[derive(Debug)]
struct AuthorityState {
    registration_id: SourceRegistrationId,
    lease_identity: SourceLeaseIdentity,
    lease_liveness: Arc<AtomicBool>,
    phase: std::sync::atomic::AtomicU8,
    pending_confirmation: Mutex<Option<SourceAuthorityConfirmation>>,
}

impl AuthorityState {
    fn new(
        registration_id: SourceRegistrationId,
        lease_identity: SourceLeaseIdentity,
        lease_liveness: Arc<AtomicBool>,
    ) -> Self {
        Self {
            registration_id,
            lease_identity,
            lease_liveness,
            phase: std::sync::atomic::AtomicU8::new(PREPARED),
            pending_confirmation: Mutex::new(None),
        }
    }

    fn phase(&self) -> u8 {
        self.phase.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// A source-owner error that leaves registration state unchanged unless explicitly stated.
#[derive(Debug, Error)]
pub enum SourceAuthorityError {
    /// The source database failed its retained profile/lease validation.
    #[error(transparent)]
    Database(#[from] SourceDbError),
    /// The retained source lease could not validate or retire.
    #[error(transparent)]
    Lease(#[from] SourceLeaseError),
    /// The source owner already has a registration for this source authority.
    #[error("source authority registration is already prepared")]
    AlreadyPrepared,
    /// The request has no retained source-owner confirmation to consume.
    #[error("source authority confirmation is missing or stale")]
    ConfirmationStale,
    /// The confirmation was already consumed by an earlier installation attempt.
    #[error("source authority confirmation was already consumed")]
    ConfirmationSpent,
    /// The source-owner confirmation cannot be minted after draining or retirement begins.
    #[error("source authority is no longer accepting installation confirmations")]
    ConfirmationClosed,
    /// The request and confirmation identify different source registrations.
    #[error("source authority registration identity mismatch")]
    RegistrationMismatch,
    /// The registration cannot enter the requested lifecycle state.
    #[error("source authority lifecycle state does not permit this operation")]
    InvalidLifecycle,
    /// The guard does not belong to the source database being retired.
    #[error("source authority guard does not belong to this source database")]
    GuardMismatch,
    /// A poisoned handshake mutex cannot safely make an authority decision.
    #[error("source authority handshake state is unavailable")]
    StateUnavailable,
}

/// A sealed request carrying only owned, descriptor-bound source capabilities.
///
/// This type is crate-private and has no caller-supplied constructor. Its root accessors are
/// crate-private so the future journal owner can derive live volume facts from the retained
/// descriptors without accepting a pathname or serialized identity as authority.
pub(crate) struct SourceAuthorityRequest {
    registration_id: SourceRegistrationId,
    lease_identity: SourceLeaseIdentity,
    media_root: BoundDatabaseRoot,
    database_root: BoundDatabaseRoot,
    database_file_binding: Arc<DatabaseFileBinding>,
    database_file_identity: String,
    state: Arc<AuthorityState>,
}

impl fmt::Debug for SourceAuthorityRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceAuthorityRequest")
            .field("registration_id", &self.registration_id)
            .field("lease_identity", &self.lease_identity)
            .field("media_root_identity", &self.media_root.identity())
            .field("database_root_identity", &self.database_root.identity())
            .field("database_file_identity", &self.database_file_identity)
            .finish_non_exhaustive()
    }
}

impl SourceAuthorityRequest {
    fn new(
        registration_id: SourceRegistrationId,
        lease_identity: SourceLeaseIdentity,
        media_root: BoundDatabaseRoot,
        database_root: BoundDatabaseRoot,
        database_file_binding: Arc<DatabaseFileBinding>,
        database_file_identity: String,
        state: Arc<AuthorityState>,
    ) -> Self {
        Self {
            registration_id,
            lease_identity,
            media_root,
            database_root,
            database_file_binding,
            database_file_identity,
            state,
        }
    }

    /// Return the opaque registration identity used to pair source-owner messages.
    pub(crate) const fn registration_id(&self) -> SourceRegistrationId {
        self.registration_id
    }

    /// Return the owner and epoch captured when the source owner prepared this request.
    pub(crate) fn lease_identity(&self) -> &SourceLeaseIdentity {
        &self.lease_identity
    }

    /// Return the retained media-root descriptor capsule for live fact derivation.
    pub(crate) fn media_root(&self) -> &BoundDatabaseRoot {
        &self.media_root
    }

    /// Return the retained database-root descriptor capsule for live fact derivation.
    pub(crate) fn database_root(&self) -> &BoundDatabaseRoot {
        &self.database_root
    }

    /// Return the captured main database identity as evidence, never as standalone authority.
    pub(crate) fn database_file_identity(&self) -> &str {
        &self.database_file_identity
    }

    pub(crate) fn matches_registration(&self, other: &Self) -> bool {
        self.registration_id == other.registration_id
            && self.lease_identity == other.lease_identity
            && self.media_root.identity() == other.media_root.identity()
            && self.database_root.identity() == other.database_root.identity()
            && self.database_file_identity == other.database_file_identity
    }

    /// Revalidate the descriptor-bound request at the journal-owner consuming boundary.
    pub(crate) fn validate_consuming_boundary(
        &self,
        profile_guard: &crate::app_dirs::WritableProfileGuard,
    ) -> Result<(), SourceAuthorityError> {
        self.media_root
            .validate_profile_guard(profile_guard)
            .map_err(|error| SourceAuthorityError::Database(SourceDbError::from(error)))?;
        self.database_root
            .validate_profile_guard(profile_guard)
            .map_err(|error| SourceAuthorityError::Database(SourceDbError::from(error)))?;
        self.database_file_binding
            .validate_path()
            .map_err(|error| {
                SourceAuthorityError::Database(SourceDbError::DatabaseIdentity(
                    super::source_database_identity_error(error),
                ))
            })?;
        if self.database_file_binding.identity() != self.database_file_identity {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        Ok(())
    }

    /// Consume a fresh source-owner confirmation for one installation/use proof.
    pub(crate) fn accept_confirmation(
        &self,
        confirmation: SourceAuthorityConfirmation,
    ) -> Result<SourceAuthorityUseProof, SourceAuthorityError> {
        if confirmation.registration_id != self.registration_id
            || confirmation.lease_identity != self.lease_identity
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        if !self.state.lease_liveness.load(Ordering::Acquire) {
            return Err(SourceAuthorityError::ConfirmationStale);
        }
        if confirmation.expires_at <= unix_now()? {
            return Err(SourceAuthorityError::ConfirmationStale);
        }

        let mut pending = self
            .state
            .pending_confirmation
            .lock()
            .map_err(|_| SourceAuthorityError::StateUnavailable)?;
        match self.state.phase() {
            PREPARED => {}
            REGISTERED => return Err(SourceAuthorityError::ConfirmationSpent),
            DRAINING | RETIRED | ABANDONED => {
                return Err(SourceAuthorityError::ConfirmationClosed);
            }
            _ => return Err(SourceAuthorityError::InvalidLifecycle),
        }
        let Some(expected) = pending.take() else {
            return Err(SourceAuthorityError::ConfirmationStale);
        };
        if expected != confirmation {
            *pending = Some(expected);
            return Err(SourceAuthorityError::ConfirmationStale);
        }
        self.state
            .phase
            .compare_exchange(
                PREPARED,
                REGISTERED,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| SourceAuthorityError::InvalidLifecycle)?;
        Ok(SourceAuthorityUseProof {
            registration_id: self.registration_id,
            lease_identity: self.lease_identity.clone(),
            expires_at: confirmation.expires_at,
            lease_liveness: Arc::clone(&self.state.lease_liveness),
            state: Arc::clone(&self.state),
        })
    }

    /// Begin a pre-release drain after the journal owner has accepted the source proof.
    ///
    /// The marker proves the long-lived registration relationship; it intentionally does not
    /// require the expiry-bound physical-use proof to remain fresh because draining revokes
    /// authority and only prevents new capacity transitions. Draining retains the registration,
    /// reserve, and volume ownership until the source owner returns a durable retirement
    /// acknowledgement.
    pub(crate) fn begin_drain(
        &self,
        marker: SourceAuthorityDrainMarker,
    ) -> Result<(), SourceAuthorityError> {
        if marker.registration_id != self.registration_id
            || marker.lease_identity != self.lease_identity
            || !Arc::ptr_eq(&marker.state, &self.state)
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        if self.state.phase() != REGISTERED {
            return Err(SourceAuthorityError::ConfirmationClosed);
        }
        // A drain revokes authority and retains the registration, reserve, and locks until the
        // source owner can acknowledge durable retirement. It therefore remains valid after the
        // expiry-bound installation proof has gone stale. A dropped guard changes the lifecycle
        // to ABANDONED and is still rejected by the phase check above.
        self.state
            .phase
            .compare_exchange(
                REGISTERED,
                DRAINING,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .map_err(|_| SourceAuthorityError::InvalidLifecycle)
            .map(|_| ())
    }

    /// Consume a sealed acknowledgement after the source lease has durably retired.
    pub(crate) fn finalize_retirement(
        &self,
        acknowledgement: SourceAuthorityRetirementAck,
    ) -> Result<(), SourceAuthorityError> {
        if acknowledgement.registration_id != self.registration_id
            || acknowledgement.lease_identity != self.lease_identity
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        match self.state.phase() {
            DRAINING => self
                .state
                .phase
                .compare_exchange(
                    DRAINING,
                    RETIRED,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .map_err(|_| SourceAuthorityError::InvalidLifecycle)
                .map(|_| ()),
            RETIRED => Ok(()),
            _ => Err(SourceAuthorityError::InvalidLifecycle),
        }
    }

    #[cfg(test)]
    pub(crate) fn phase_for_tests(&self) -> u8 {
        self.state.phase()
    }
}

/// The source-owner guard retained while a request is registered or draining.
///
/// This value intentionally contains the retained `Arc<SourceLease>` and is explicitly marked
/// non-`Send` so it cannot cross into the journal owner. The source owner uses it to validate the
/// live lease and mint confirmations; the journal receives only the request and confirmation.
pub(crate) struct SourceAuthorityGuard {
    lease: Option<Arc<SourceLease>>,
    state: Arc<AuthorityState>,
    retirement_acknowledged: bool,
    _source_owner_thread: PhantomData<Rc<()>>,
}

impl fmt::Debug for SourceAuthorityGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceAuthorityGuard")
            .field("registration_id", &self.state.registration_id)
            .field("lease_identity", &self.state.lease_identity)
            .finish_non_exhaustive()
    }
}

impl SourceAuthorityGuard {
    fn new(lease: Arc<SourceLease>, state: Arc<AuthorityState>) -> Self {
        Self {
            lease: Some(lease),
            state,
            retirement_acknowledged: false,
            _source_owner_thread: PhantomData,
        }
    }

    /// Mint a fresh, one-shot confirmation after validating the retained control connection.
    pub(crate) fn confirm(&self) -> Result<SourceAuthorityConfirmation, SourceAuthorityError> {
        match self.state.phase() {
            PREPARED => {}
            REGISTERED | DRAINING | RETIRED | ABANDONED => {
                return Err(SourceAuthorityError::ConfirmationClosed);
            }
            _ => return Err(SourceAuthorityError::InvalidLifecycle),
        }
        let mut pending = self
            .state
            .pending_confirmation
            .lock()
            .map_err(|_| SourceAuthorityError::StateUnavailable)?;
        pending.take();
        drop(pending);
        if !self.state.lease_liveness.load(Ordering::Acquire) {
            return Err(SourceAuthorityError::ConfirmationStale);
        }
        let lease = self
            .lease
            .as_ref()
            .ok_or(SourceAuthorityError::ConfirmationClosed)?;
        let expires_at = lease.validate_current_owner()?;
        let current = SourceLeaseIdentity::from_lease(lease);
        if current != self.state.lease_identity {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        let confirmation = SourceAuthorityConfirmation {
            registration_id: self.state.registration_id,
            lease_identity: current,
            token: Uuid::new_v4(),
            expires_at,
        };
        let mut pending = self
            .state
            .pending_confirmation
            .lock()
            .map_err(|_| SourceAuthorityError::StateUnavailable)?;
        *pending = Some(confirmation.clone());
        Ok(confirmation)
    }

    fn retirement_acknowledgement(
        &mut self,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        if self.state.phase() != DRAINING {
            return Err(SourceAuthorityError::InvalidLifecycle);
        }
        let lease = self
            .lease
            .as_ref()
            .ok_or(SourceAuthorityError::ConfirmationClosed)?;
        let current = SourceLeaseIdentity::from_lease(lease);
        if current != self.state.lease_identity {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        lease.clear_authority_registration(self.state.registration_id.0)?;
        self.retirement_acknowledged = true;
        Ok(SourceAuthorityRetirementAck {
            registration_id: self.state.registration_id,
            lease_identity: current,
        })
    }

    fn belongs_to(&self, source_lease: &Arc<SourceLease>) -> bool {
        self.lease
            .as_ref()
            .is_some_and(|lease| Arc::ptr_eq(lease, source_lease))
    }
}

impl Drop for SourceAuthorityGuard {
    fn drop(&mut self) {
        if self.retirement_acknowledged {
            return;
        }
        if let Ok(mut pending) = self.state.pending_confirmation.lock() {
            pending.take();
        }
        self.state
            .phase
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |phase| match phase {
                    RETIRED | ABANDONED => None,
                    _ => Some(ABANDONED),
                },
            )
            .ok();
    }
}

/// A confirmation minted by the source owner for exactly one install/use attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SourceAuthorityConfirmation {
    registration_id: SourceRegistrationId,
    lease_identity: SourceLeaseIdentity,
    token: Uuid,
    expires_at: i64,
}

#[cfg(test)]
impl SourceAuthorityConfirmation {
    pub(crate) fn with_expired_deadline_for_tests(mut self) -> Self {
        self.expires_at = 0;
        self
    }
}

/// A sealed, expiry-bound proof for one installation or physical-use transition.
///
/// Milestone 1 has no physical transition consumer. The proof can be converted into a
/// [`SourceAuthorityDrainMarker`] after installation; that marker carries only the durable
/// registration relationship and remains usable for the restrictive drain transition after this
/// proof expires.
pub(crate) struct SourceAuthorityUseProof {
    registration_id: SourceRegistrationId,
    lease_identity: SourceLeaseIdentity,
    expires_at: i64,
    lease_liveness: Arc<AtomicBool>,
    state: Arc<AuthorityState>,
}

#[cfg(test)]
impl SourceAuthorityUseProof {
    pub(crate) fn with_expired_deadline_for_tests(mut self) -> Self {
        self.expires_at = 0;
        self
    }
}

impl SourceAuthorityUseProof {
    /// Validate the one-shot proof immediately before a physical installation or use effect.
    /// The proof never extends its deadline and cannot be refreshed by the journal owner.
    pub(crate) fn validate_for_physical_use(&self) -> Result<(), SourceAuthorityError> {
        if self.state.phase() != REGISTERED {
            return Err(SourceAuthorityError::ConfirmationClosed);
        }
        if !self.lease_liveness.load(Ordering::Acquire) {
            return Err(SourceAuthorityError::ConfirmationStale);
        }
        if self.expires_at <= unix_now()? {
            return Err(SourceAuthorityError::ConfirmationStale);
        }
        Ok(())
    }

    pub(crate) fn into_drain_marker(self) -> SourceAuthorityDrainMarker {
        SourceAuthorityDrainMarker {
            registration_id: self.registration_id,
            lease_identity: self.lease_identity,
            lease_liveness: self.lease_liveness,
            state: self.state,
        }
    }
}

/// A sealed long-lived registration marker for the restrictive pre-release drain transition.
///
/// This marker carries no expiry-bound physical-use authority. The source-owner liveness token
/// still prevents a retargeted, released, or dropped lease from being drained through stale
/// registration state.
#[derive(Clone)]
pub(crate) struct SourceAuthorityDrainMarker {
    registration_id: SourceRegistrationId,
    lease_identity: SourceLeaseIdentity,
    lease_liveness: Arc<AtomicBool>,
    state: Arc<AuthorityState>,
}

/// A sealed proof emitted only after the retained source lease row is durably released.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceAuthorityRetirementAck {
    registration_id: SourceRegistrationId,
    lease_identity: SourceLeaseIdentity,
}

/// The source-owner guard and send-safe request produced by one validated source database.
pub struct SourceAuthorityRegistration {
    request: Arc<SourceAuthorityRequest>,
    guard: SourceAuthorityGuard,
}

impl SourceAuthorityRegistration {
    /// Return the source-owner request envelope for one bounded journal registration attempt.
    pub fn journal_request(
        &self,
    ) -> Result<SourceAuthorityRegistrationRequest, SourceAuthorityError> {
        Ok(SourceAuthorityRegistrationRequest {
            request: Arc::clone(&self.request),
            confirmation: self.guard.confirm()?,
        })
    }

    /// Close the source after the journal owner has durably entered the draining phase.
    pub fn close_database(
        &mut self,
        database: SourceDatabase,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        database.close_with_source_authority(&mut self.guard)
    }

    /// Detach the non-Send source lease guard after a journal drain, before a committed catalog
    /// retarget. The returned context carries no source lease or physical authority; it can emit
    /// the old-generation acknowledgement only after the caller has observed retarget success.
    pub fn detach_for_retarget(
        self,
    ) -> Result<SourceAuthorityRetargetContext, SourceAuthorityError> {
        if self.guard.state.phase() != DRAINING {
            return Err(SourceAuthorityError::InvalidLifecycle);
        }
        let SourceAuthorityRegistration { request, mut guard } = self;
        let state = Arc::clone(&guard.state);
        let lease_identity = request.lease_identity.clone();
        let media_root_identity = request.media_root.identity().to_owned();
        let database_root_identity = request.database_root.identity().to_owned();
        let database_file_identity = request.database_file_identity.clone();
        guard.lease.take();
        guard.retirement_acknowledged = true;
        Ok(SourceAuthorityRetargetContext {
            state,
            lease_identity,
            media_root_identity,
            database_root_identity,
            database_file_identity,
            retarget_committed: false,
        })
    }

    /// Return the private request and source-owner guard for library-local lifecycle tests.
    pub(crate) fn split(self) -> (Arc<SourceAuthorityRequest>, SourceAuthorityGuard) {
        (self.request, self.guard)
    }
}

/// Opaque old-generation state retained across a successful profile-child retarget transaction.
pub struct SourceAuthorityRetargetContext {
    state: Arc<AuthorityState>,
    lease_identity: SourceLeaseIdentity,
    media_root_identity: String,
    database_root_identity: String,
    database_file_identity: String,
    retarget_committed: bool,
}

impl SourceAuthorityRetargetContext {
    /// Emit the superseded-generation acknowledgement after the retarget control transaction
    /// has returned success. The journal owner still must consume this ack before removal.
    pub fn acknowledgement_after_retarget_commit(
        self,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        if self.state.phase() != DRAINING || !self.retarget_committed {
            return Err(SourceAuthorityError::InvalidLifecycle);
        }
        Ok(SourceAuthorityRetirementAck {
            registration_id: self.state.registration_id,
            lease_identity: self.state.lease_identity.clone(),
        })
    }

    pub(crate) fn mark_retarget_committed(&mut self) {
        self.retarget_committed = true;
    }

    /// Validate that this drained generation belongs to the exact source database that is about
    /// to be retargeted. This check is intentionally performed before the source lease mutates
    /// its liveness or durable owner row, so a context from another source cannot retire it.
    pub(crate) fn validate_retarget_subject(
        &self,
        source_lease: &SourceLease,
        database_file: &DatabaseFileBinding,
    ) -> Result<(), SourceAuthorityError> {
        let current_media_root = source_lease.media_root_binding();
        if source_lease.owner_identity() != self.lease_identity.owner_identity
            || source_lease.epoch() != self.lease_identity.lease_epoch
            || current_media_root.identity() != self.media_root_identity
            || source_lease.database_root_binding().identity() != self.database_root_identity
            || database_file.identity() != self.database_file_identity
        {
            return Err(SourceAuthorityError::RegistrationMismatch);
        }
        Ok(())
    }
}

/// Opaque source-owner input accepted by the profile-owned journal worker.
pub struct SourceAuthorityRegistrationRequest {
    pub(crate) request: Arc<SourceAuthorityRequest>,
    pub(crate) confirmation: SourceAuthorityConfirmation,
}

impl fmt::Debug for SourceAuthorityRegistrationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SourceAuthorityRegistrationRequest")
            .field("registration_id", &self.request.registration_id)
            .field("lease_identity", &self.request.lease_identity)
            .finish_non_exhaustive()
    }
}

/// Opaque journal-owned identity for one accepted source registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceAuthorityRegistrationReceipt {
    pub(crate) registration_id: SourceRegistrationId,
    pub(crate) lease_identity: SourceLeaseIdentity,
    pub(crate) volume_identities: Vec<crate::durable_capacity::VolumeIdentity>,
}

impl SourceAuthorityRegistrationReceipt {
    pub(crate) fn new(
        registration_id: SourceRegistrationId,
        lease_identity: SourceLeaseIdentity,
        volume_identities: Vec<crate::durable_capacity::VolumeIdentity>,
    ) -> Self {
        Self {
            registration_id,
            lease_identity,
            volume_identities,
        }
    }
}

impl SourceDatabase {
    /// Prepare one private source-authority request from the currently validated lease.
    ///
    /// The returned request owns cloned no-follow root descriptors. The paired guard retains the
    /// source lease on the source-owner thread and must remain there until retirement succeeds or
    /// the registration is abandoned with its evidence retained. This milestone deliberately does
    /// not route a journal command or install a capacity reserve.
    pub fn prepare_source_authority(
        &self,
    ) -> Result<SourceAuthorityRegistration, SourceAuthorityError> {
        let profile_guard = self
            .profile_guard
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        self.validate_profile_owned_open(profile_guard)?;
        let source_lease = self
            .source_lease
            .as_ref()
            .ok_or(SourceDbError::SourceLease(SourceLeaseError::Required))?;
        let database_file_binding = self
            .database_file_binding
            .as_ref()
            .ok_or(SourceDbError::ProfileOwnershipRequired)?;
        let lease_identity = SourceLeaseIdentity::from_lease(source_lease);
        let registration_id = SourceRegistrationId(Uuid::new_v4());
        if !source_lease.try_reserve_authority_registration(registration_id.0)? {
            return Err(SourceAuthorityError::AlreadyPrepared);
        }
        let state = Arc::new(AuthorityState::new(
            registration_id,
            lease_identity.clone(),
            source_lease.authority_liveness(),
        ));
        let media_root = match source_lease
            .media_root_binding()
            .try_clone_for_owned_capability()
        {
            Ok(root) => root,
            Err(source) => {
                let _ = source_lease.clear_authority_registration(registration_id.0);
                return Err(SourceAuthorityError::Lease(SourceLeaseError::Io {
                    path: source_lease.media_root_binding().path().to_path_buf(),
                    source,
                }));
            }
        };
        let database_root = match source_lease
            .database_root_binding()
            .try_clone_for_owned_capability()
        {
            Ok(root) => root,
            Err(source) => {
                let _ = source_lease.clear_authority_registration(registration_id.0);
                return Err(SourceAuthorityError::Lease(SourceLeaseError::Io {
                    path: source_lease.database_root_binding().path().to_path_buf(),
                    source,
                }));
            }
        };
        Ok(SourceAuthorityRegistration {
            request: Arc::new(SourceAuthorityRequest::new(
                registration_id,
                lease_identity,
                media_root,
                database_root,
                Arc::clone(database_file_binding),
                database_file_binding.identity().to_owned(),
                Arc::clone(&state),
            )),
            guard: SourceAuthorityGuard::new(Arc::clone(source_lease), state),
        })
    }

    /// Close this source after a pre-release drain and return a sealed retirement acknowledgement.
    ///
    /// The SQLite connection is dropped before the retained lease's control connection marks its
    /// row released. The guard is borrowed on the source-owner thread; it is never sent with the
    /// request. Snapshot fences, identity mismatches, and lease failures return without an ack and
    /// leave the guard's draining evidence available to its owner.
    pub(crate) fn close_with_source_authority(
        self,
        guard: &mut SourceAuthorityGuard,
    ) -> Result<SourceAuthorityRetirementAck, SourceAuthorityError> {
        let source_lease = self
            .source_lease
            .as_ref()
            .ok_or(SourceDbError::SourceLease(SourceLeaseError::Required))?;
        if !guard.belongs_to(source_lease) {
            return Err(SourceAuthorityError::GuardMismatch);
        }
        if guard.state.phase() != DRAINING {
            return Err(SourceAuthorityError::InvalidLifecycle);
        }

        let SourceDatabase {
            connection,
            source_lease,
            ..
        } = self;
        let snapshots_active = source_lease.as_ref().is_some_and(|lease| {
            // One strong reference belongs to this SourceDatabase and one to the source-owner
            // guard. Any additional owner is a live snapshot/write fence.
            Arc::strong_count(lease) > 2
        });
        drop(connection);
        if snapshots_active {
            drop(source_lease);
            return Err(SourceAuthorityError::Lease(
                SourceLeaseError::SnapshotsActive,
            ));
        }
        let source_lease =
            source_lease.ok_or(SourceDbError::SourceLease(SourceLeaseError::Required))?;
        source_lease.release()?;
        drop(source_lease);
        guard.retirement_acknowledgement()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn request_and_send_safe_proofs_do_not_carry_source_lease() {
        assert_send_sync::<SourceAuthorityRequest>();
        assert_send_sync::<SourceAuthorityConfirmation>();
        assert_send_sync::<SourceAuthorityUseProof>();
        assert_send_sync::<SourceAuthorityRetirementAck>();
    }
}
