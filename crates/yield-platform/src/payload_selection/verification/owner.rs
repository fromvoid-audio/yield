//! One application admission; immutable UI observations and cancellation carry no ZIP ownership.
use super::Key;
use crate::payload_selection::{
    Installed, Issuer, Lease, SelectionCancellation, SelectionError,
    protocol::{self, staging::Traffic},
};
use crate::update_catalog::{Channel, Platform};
use std::{
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
mod engine;
mod pipeline;

/// Honest attempt/staging observation; no variant is an installation or native qualification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadStagePhase {
    /// Selecting complete release metadata and its exact anonymous gate.
    Selecting,
    /// Reading only the selected checksum/signature/target archive.
    Downloading,
    /// Verifying the immutable original bytes on the background owner.
    Verifying,
    /// Verified bytes remain privately retained by the live background owner.
    Staged,
    /// Cancellation/retirement requested; actual I/O or byte custody remains unconfirmed.
    Stopping,
    /// The owner confirms there are no retained bytes or active I/O for this attempt.
    Unstaged,
    /// A connection could not be established; no accepted payload remains.
    Offline,
    /// Corrupt/substituted/malformed/over-budget input; no accepted payload remains.
    Failed,
    /// No complete matching selection or an unavailable resource; no accepted payload remains.
    Unavailable,
}
/// Copied full identity; this observation cannot mint a selection or verified-byte witness.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayloadStageIdentity {
    /// Exact selected stable/RC/nightly version.
    pub version: String,
    /// Exact gate/catalog build identifier.
    pub build_id: String,
    /// Positive catalog build number; native binary attestation remains separate.
    pub build_number: u64,
    /// Complete selected source commit.
    pub commit: [u8; 20],
    /// Original requested lane.
    pub channel: Channel,
    /// Immutable compiled installed target.
    pub platform: Platform,
}
/// Small immutable snapshot; fields contain no tokens, URLs, paths or archive allocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PayloadStageObservation {
    /// Current honest phase, with cancellation/service retirement rechecked on observation.
    pub phase: PayloadStagePhase,
    /// Exact selected identity once selection succeeds.
    pub identity: Option<PayloadStageIdentity>,
    /// Retained byte count only while Staged; not a request/staging/install grant.
    pub archive_len: Option<usize>,
    /// Verified retained hash only while Staged.
    pub archive_sha256: Option<[u8; 32]>,
    /// Authenticated manifest date only while Staged; not compiled/native attestation.
    pub build_date: Option<String>,
}
impl PayloadStageObservation {
    fn initial() -> Self {
        Self {
            phase: PayloadStagePhase::Selecting,
            identity: None,
            archive_len: None,
            archive_sha256: None,
            build_date: None,
        }
    }
    fn clear_bytes(&mut self) {
        self.archive_len = None;
        self.archive_sha256 = None;
        self.build_date = None;
    }
}
struct Facade {
    installed: Installed,
    transport: protocol::Transport,
    issuer: Arc<Issuer>,
    engine: Arc<engine::Engine>,
    key: Key,
    #[cfg(test)]
    witness: Option<Arc<TestWitness>>,
}
impl Drop for Facade {
    fn drop(&mut self) {
        self.issuer.retired.store(true, Ordering::Release);
        self.engine.wake();
    }
}
/// A facade of the single process-wide staging owner, constructed only from compiled identity.
/// Clone shares the same service lifetime; independently constructed replacements share admission.
#[derive(Clone)]
pub struct PayloadStagingService {
    facade: Arc<Facade>,
}
impl PayloadStagingService {
    /// Missing/unsupported compiled metadata stays unavailable and starts no network operation.
    pub fn compiled() -> Option<Self> {
        Some(Self::new(
            Installed::compiled()?,
            protocol::Transport::Production,
            engine::Engine::global()?,
            Key::Compiled,
        ))
    }
    fn new(
        installed: Installed,
        transport: protocol::Transport,
        engine: Arc<engine::Engine>,
        key: Key,
    ) -> Self {
        Self {
            facade: Arc::new(Facade {
                installed,
                transport,
                engine,
                key,
                issuer: Arc::new(Issuer {
                    retired: AtomicBool::new(false),
                    generation: AtomicU64::new(0),
                    busy: AtomicBool::new(false),
                }),
                #[cfg(test)]
                witness: None,
            }),
        }
    }
    /// Start bounded background selection/download/verification. The single admission remains held
    /// while Active, Staged or Stopping, including across replacement facades and caller Drop.
    pub fn start(
        &self,
        channel: Channel,
        cancellation: SelectionCancellation,
    ) -> Result<PayloadStageAttempt, SelectionError> {
        self.start_before(
            channel,
            cancellation,
            Instant::now() + Duration::from_secs(300),
        )
    }
    fn start_before(
        &self,
        channel: Channel,
        cancellation: SelectionCancellation,
        deadline: Instant,
    ) -> Result<PayloadStageAttempt, SelectionError> {
        let facade = &self.facade;
        if cancellation.cancelled() || facade.issuer.retired.load(Ordering::Acquire) {
            return Err(SelectionError::Retired);
        }
        if facade
            .engine
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SelectionError::Unavailable);
        }
        let generation =
            match facade
                .issuer
                .generation
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            {
                Ok(n) => n + 1,
                Err(_) => {
                    self.retire();
                    facade.engine.busy.store(false, Ordering::Release);
                    return Err(SelectionError::Retired);
                }
            };
        let lease = Arc::new(Lease {
            issuer: Arc::downgrade(&facade.issuer),
            generation,
            cancellation,
            closed: AtomicBool::new(false),
        });
        let job = Arc::new(Job {
            installed: facade.installed.clone(),
            transport: facade.transport.clone(),
            key: facade.key.clone(),
            channel,
            traffic: Traffic::new(lease.clone(), deadline),
            lease,
            deadline,
            engine: Arc::downgrade(&facade.engine),
            state: Mutex::new(PayloadStageObservation::initial()),
            confirmed: AtomicBool::new(false),
            #[cfg(test)]
            witness: facade.witness.clone(),
        });
        let attempt = PayloadStageAttempt {
            client: Arc::new(Client { job: job.clone() }),
        };
        if facade.engine.submit(job).is_err() {
            // No accepted execution was submitted. A dead/inconsistent owner stays unavailable;
            // failure to submit never clears a potentially unknown engine's admission.
            return Err(SelectionError::Unavailable);
        }
        Ok(attempt)
    }
    /// Close this exact facade/service lifetime. The background owner confirms actual retirement.
    pub fn retire(&self) {
        self.facade.issuer.retired.store(true, Ordering::Release);
        self.facade.engine.wake();
    }
    /// Authenticate the original current attempt against this exact facade, not copied fields.
    pub fn owns(&self, attempt: &PayloadStageAttempt) -> bool {
        let job = &attempt.client.job;
        !job.confirmed.load(Ordering::Acquire)
            && job.lease.live()
            && job
                .lease
                .issuer
                .ptr_eq(&Arc::downgrade(&self.facade.issuer))
    }
}
struct Client {
    job: Arc<Job>,
}
impl Drop for Client {
    fn drop(&mut self) {
        self.job.cancel();
    }
}
/// Small opaque observation/retirement control. Clones share caller lifetime; they never clone ZIP
/// bytes. Dropping the last caller requests retirement, without releasing unknown resource custody.
///
/// ```compile_fail
/// use yield_platform::payload_selection::PayloadStageAttempt;
/// fn forge() { let _: PayloadStageAttempt=serde_json::from_slice(b"{}").unwrap(); }
/// ```
#[derive(Clone)]
pub struct PayloadStageAttempt {
    client: Arc<Client>,
}
impl PayloadStageAttempt {
    /// Read a small snapshot; it performs no network/verification/deallocation on the caller.
    pub fn observe(&self) -> Result<PayloadStageObservation, SelectionError> {
        let job = &self.client.job;
        let guard = job.state.lock().map_err(|_| SelectionError::Unavailable)?;
        let mut state = guard.clone();
        if !job.confirmed.load(Ordering::Acquire)
            && (!job.lease.live()
                || (state.phase != PayloadStagePhase::Staged && Instant::now() >= job.deadline))
        {
            state.phase = PayloadStagePhase::Stopping;
            state.clear_bytes();
        }
        Ok(state)
    }
    /// Request retirement. Observe Stopping until the owner confirms actual I/O/buffer release.
    pub fn retire(&self) {
        self.client.job.cancel();
    }
}
struct Job {
    installed: Installed,
    transport: protocol::Transport,
    key: Key,
    channel: Channel,
    lease: Arc<Lease>,
    traffic: Arc<Traffic>,
    deadline: Instant,
    engine: Weak<engine::Engine>,
    state: Mutex<PayloadStageObservation>,
    confirmed: AtomicBool,
    #[cfg(test)]
    witness: Option<Arc<TestWitness>>,
}
impl Job {
    fn cancel(&self) {
        self.lease.cancellation.cancel();
        if let Some(engine) = self.engine.upgrade() {
            engine.wake();
        }
    }
    fn phase(&self, phase: PayloadStagePhase) -> Result<(), SelectionError> {
        let mut state = self.state.lock().map_err(|_| SelectionError::Unavailable)?;
        state.phase = phase;
        Ok(())
    }
}
#[cfg(test)]
struct TestWitness {
    expected: Vec<u8>,
    retained: AtomicBool,
    retired_thread: Mutex<Option<std::thread::ThreadId>>,
}
#[cfg(test)]
mod tests;

// Feature is default-off and enabled only by dependent test harnesses. Normal compiled entry
// always uses the original hardcoded pin and immutable compiled identity, even in fixture builds.
#[cfg(feature = "test-support")]
impl PayloadStagingService {
    /// Controlled loopback fixture owner; absent from normal builds and never native qualification.
    /// The fixed fixture key is deliberately distinct from the compiled production pin.
    pub fn for_loopback_tests(
        address: std::net::SocketAddr,
        full_version: &str,
        number: u64,
        commit: [u8; 20],
        platform: Platform,
    ) -> Option<Self> {
        if !address.ip().is_loopback()
            || number == 0
            || commit == [0; 20]
            || full_version.len() > 100
        {
            return None;
        }
        let version = semver::Version::parse(full_version).ok()?;
        let core = [
            version.major.try_into().ok()?,
            version.minor.try_into().ok()?,
            version.patch.try_into().ok()?,
        ];
        // Public fixture-key bytes only; no caller-supplied key can replace the production pin.
        let key = ed25519_dalek::VerifyingKey::from_bytes(&[
            190, 215, 210, 171, 102, 141, 163, 239, 173, 97, 57, 152, 240, 111, 122, 191, 120, 117,
            243, 166, 183, 103, 122, 159, 60, 233, 71, 215, 125, 119, 96, 166,
        ])
        .ok()?;
        Some(Self::new(
            Installed {
                version,
                full_version: full_version.into(),
                core,
                number,
                commit,
                platform,
            },
            protocol::Transport::Loopback(address),
            engine::Engine::global()?,
            Key::Fixture(key),
        ))
    }
}

#[cfg(feature = "test-support")]
impl PayloadStageAttempt {
    /// Read-only controlled-loopback socket-entry observation; absent from normal builds.
    /// Never reports production reads or grants resource-retirement/verified-byte authority.
    pub fn loopback_read_pending(&self) -> bool {
        let job = &self.client.job;
        matches!(job.transport, protocol::Transport::Loopback(_))
            && job.traffic.plaintext.load(Ordering::Acquire) > 0
            && job.traffic.reading.load(Ordering::Acquire)
    }
}
