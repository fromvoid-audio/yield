//! Complete, private update selection from compiled context and the fixed catalog/gate.
//!
//! A selection retains metadata, not signed bytes or installation authority. Actual bounded
//! staging/verification and application-wide payload custody belong to the staging owner.

use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use crate::update_catalog::{Channel, Platform};

mod catalog;
mod protocol;
mod verification;
use catalog::{Installed, Release};
pub use verification::VerifiedPayload;

const DEADLINE: Duration = Duration::from_secs(10);

/// Fixed error codes contain no remote text, credentials or token-bearing URLs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SelectionError {
    /// Invalid, conflicting, incomplete or over-budget metadata.
    Invalid,
    /// No matching complete release or an occupied/retired service.
    Unavailable,
    /// Connection or name resolution is unavailable.
    Offline,
    /// Cancellation, supersession or retirement invalidated the original issuance.
    Retired,
}

/// Monotonic cancellation identity; callers cannot reset it or substitute its backing.
#[derive(Clone, Default)]
pub struct SelectionCancellation(Arc<AtomicBool>);
impl SelectionCancellation {
    /// Permanently cancel this identity.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

struct Issuer {
    retired: AtomicBool,
    generation: AtomicU64,
    busy: AtomicBool,
}
struct Lease {
    issuer: Weak<Issuer>,
    generation: u64,
    cancellation: SelectionCancellation,
    closed: AtomicBool,
}
impl Lease {
    fn live(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
            && !self.cancellation.cancelled()
            && self.issuer.upgrade().is_some_and(|i| {
                !i.retired.load(Ordering::Acquire)
                    && i.generation.load(Ordering::Acquire) == self.generation
            })
    }
}

/// Factory whose installed version/build/commit/target come only from compiled packer inputs.
/// There is no production runtime context, endpoint, TLS or trust-key override.
pub struct PayloadSelectionService {
    installed: Installed,
    issuer: Arc<Issuer>,
    transport: protocol::Transport,
}
impl PayloadSelectionService {
    /// Development or unsupported builds without a complete compiled identity stay unavailable.
    pub fn compiled() -> Option<Self> {
        let installed = Installed::compiled()?;
        Some(Self::new(installed, protocol::Transport::Production))
    }
    fn new(installed: Installed, transport: protocol::Transport) -> Self {
        Self {
            installed,
            transport,
            issuer: Arc::new(Issuer {
                retired: AtomicBool::new(false),
                generation: AtomicU64::new(0),
                busy: AtomicBool::new(false),
            }),
        }
    }

    /// Fetch complete catalog selection and its exact anonymous gate on a bounded worker.
    /// Call off the UI thread. Timed-out/cancelled workers retain admission until I/O quiesces.
    pub fn select(
        &self,
        channel: Channel,
        cancellation: SelectionCancellation,
    ) -> Result<SelectedPayload, SelectionError> {
        if cancellation.cancelled() || self.issuer.retired.load(Ordering::Acquire) {
            return Err(SelectionError::Retired);
        }
        if self
            .issuer
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SelectionError::Unavailable);
        }
        let generation =
            match self
                .issuer
                .generation
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1))
            {
                Ok(n) => n + 1,
                Err(_) => {
                    self.retire();
                    self.issuer.busy.store(false, Ordering::Release);
                    return Err(SelectionError::Retired);
                }
            };
        let lease = Arc::new(Lease {
            issuer: Arc::downgrade(&self.issuer),
            generation,
            cancellation,
            closed: AtomicBool::new(false),
        });
        let installed = self.installed.clone();
        let transport = self.transport.clone();
        let issuer = self.issuer.clone();
        let worker_lease = lease.clone();
        let deadline = Instant::now() + DEADLINE;
        let (sender, receiver) = mpsc::sync_channel(1);
        if std::thread::Builder::new()
            .name("yield-payload-selection".into())
            .spawn(move || {
                struct ReleaseAdmission(Arc<Issuer>);
                impl Drop for ReleaseAdmission {
                    fn drop(&mut self) {
                        self.0.busy.store(false, Ordering::Release);
                    }
                }
                let _admission = ReleaseAdmission(issuer);
                let result = select_worker(installed, channel, worker_lease, transport, deadline);
                let _ = sender.send(result);
            })
            .is_err()
        {
            lease.closed.store(true, Ordering::Release);
            self.issuer.busy.store(false, Ordering::Release);
            return Err(SelectionError::Unavailable);
        }
        loop {
            if !lease.live() || Instant::now() >= deadline {
                lease.closed.store(true, Ordering::Release);
                return Err(SelectionError::Retired);
            }
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(result) => {
                    return if lease.live() {
                        result
                    } else {
                        Err(SelectionError::Retired)
                    };
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    lease.closed.store(true, Ordering::Release);
                    return Err(SelectionError::Unavailable);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
    /// Close every issued selection; this never releases an unknown worker's admission.
    pub fn retire(&self) {
        self.issuer.retired.store(true, Ordering::Release);
    }
    /// Authenticate a selection against this exact factory and current request generation.
    pub fn owns(&self, selection: &SelectedPayload) -> bool {
        selection.lease.live() && selection.lease.issuer.ptr_eq(&Arc::downgrade(&self.issuer))
    }
}
impl Drop for PayloadSelectionService {
    fn drop(&mut self) {
        self.retire();
    }
}

/// One privately issued, non-Clone/non-serializable full selection. No caller constructor.
///
/// ```compile_fail
/// use yield_platform::payload_selection::SelectedPayload;
/// fn duplicate(selection: &SelectedPayload) { let _: SelectedPayload = selection.clone(); }
/// ```
/// ```compile_fail
/// use yield_platform::payload_selection::SelectedPayload;
/// fn forge() { let _ = SelectedPayload {}; }
/// ```
/// ```compile_fail
/// use yield_platform::payload_selection::SelectedPayload;
/// let _: SelectedPayload = serde_json::from_str("{}").unwrap();
/// ```
/// ```compile_fail
/// use yield_platform::payload_selection::PayloadSelectionService;
/// fn override_origin(service: &mut PayloadSelectionService) { service.transport = (); }
/// ```
pub struct SelectedPayload {
    release: Release,
    installed: Installed,
    channel: Channel,
    lease: Arc<Lease>,
    requests: Vec<protocol::TokenRequest>,
}
/// Metadata observation only, with no credentials or file/download capability.
pub struct PayloadIdentity<'a> {
    /// Exact RC/nightly/full version; never reconstructed from the UI core.
    pub version: &'a str,
    /// Exact build-specific identity used by the gate.
    pub build_id: &'a str,
    /// Positive monotonic packaged number.
    pub build_number: u64,
    /// Complete nonzero commit.
    pub commit: [u8; 20],
    /// Selected lane.
    pub channel: Channel,
    /// Compiled installed target.
    pub platform: Platform,
    /// Whether the full identity equals the installed artifact.
    pub current: bool,
}
/// One declared inventory observation; other target ZIPs are not downloaded/verified here.
pub struct PayloadAssetIdentity<'a> {
    /// Exact current-contract filename.
    pub name: &'a str,
    /// Positive bounded declared byte length.
    pub size_bytes: u64,
    /// Complete SHA-256 declaration, not proof of retained bytes.
    pub sha256: [u8; 32],
}
impl SelectedPayload {
    /// Verify downloaded bytes off the UI thread against this exact live selection and compiled
    /// trust pin. This grants immutable byte custody only, never staging paths or installation.
    pub fn verify_retained(
        self,
        archive: Vec<u8>,
        checksums: Vec<u8>,
        signature: Vec<u8>,
    ) -> Result<VerifiedPayload, SelectionError> {
        verification::verify(self, archive, checksums, signature)
    }
    /// Observe complete identity only while the original factory/request/cancellation remains live.
    pub fn identity(&self) -> Result<PayloadIdentity<'_>, SelectionError> {
        self.ensure_live()?;
        Ok(PayloadIdentity {
            version: &self.release.version,
            build_id: &self.release.build_id,
            build_number: self.release.build_number,
            commit: self.release.commit,
            channel: self.channel,
            platform: self.installed.platform,
            current: self.release.current(&self.installed),
        })
    }
    /// Observe the complete checksum/signature/three-target contract inventory.
    pub fn inventory(
        &self,
    ) -> Result<impl Iterator<Item = PayloadAssetIdentity<'_>>, SelectionError> {
        self.ensure_live()?;
        Ok(self.release.files.iter().map(|f| PayloadAssetIdentity {
            name: &f.name,
            size_bytes: f.size_bytes,
            sha256: f.sha256,
        }))
    }
    /// Recheck fresh metadata against the original immutable complete snapshot. Changed hashes,
    /// sizes, URLs, lane, full identity or selection cannot update or revive this issuance.
    pub fn matches_catalog(&self, bytes: &[u8]) -> Result<bool, SelectionError> {
        self.ensure_live()?;
        let fresh = catalog::select(bytes, &self.installed, self.channel)?;
        self.ensure_live()?;
        Ok(fresh == self.release)
    }
    /// Length observations for the selected ZIP/checksum/signature requests; tokens stay private.
    pub fn request_lengths(&self) -> Result<[usize; 3], SelectionError> {
        self.ensure_live()?;
        if self.requests.len() != 3 {
            return Err(SelectionError::Invalid);
        }
        let mut lengths = [0; 3];
        for (out, request) in lengths.iter_mut().zip(&self.requests) {
            *out = request.len();
        }
        Ok(lengths)
    }
    fn ensure_live(&self) -> Result<(), SelectionError> {
        if self.lease.live() {
            Ok(())
        } else {
            Err(SelectionError::Retired)
        }
    }
}

fn select_worker(
    installed: Installed,
    channel: Channel,
    lease: Arc<Lease>,
    transport: protocol::Transport,
    deadline: Instant,
) -> Result<SelectedPayload, SelectionError> {
    let check = || {
        if lease.live() && Instant::now() < deadline {
            Ok(())
        } else {
            Err(SelectionError::Retired)
        }
    };
    check()?;
    let bytes = transport.get(protocol::CATALOG, deadline)?;
    check()?;
    let release = catalog::select(&bytes, &installed, channel)?;
    drop(bytes);
    check()?;
    let gate_url = protocol::gate_url(&release.build_id)?;
    let gate = transport.get(&gate_url, deadline)?;
    check()?;
    let requests = protocol::gate_requests(&gate, &release, channel, installed.platform)?;
    check()?;
    Ok(SelectedPayload {
        release,
        installed,
        channel,
        lease,
        requests,
    })
}

#[cfg(test)]
mod tests;
