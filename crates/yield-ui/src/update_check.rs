//! Read-only update-check port. Implementations check release metadata, never install an app.

use std::sync::{Arc, atomic::AtomicBool};

/// Release channel from the packaged artifact contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateChannel {
    /// Stable releases.
    Stable,
    /// Release candidates.
    Rc,
    /// Nightly releases.
    Nightly,
}

/// Declared release targets; this enum does not qualify a platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdatePlatform {
    /// Apple Silicon macOS.
    MacArm64,
    /// Intel macOS.
    MacX64,
    /// x64 Windows.
    WindowsX64,
}

/// Bounded semantic release version, excluding channel/build identity.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct UpdateVersion(pub u32, pub u32, pub u32);

impl std::fmt::Display for UpdateVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Exact packaged build identity; a zero number or empty commit is unknown.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateBuild {
    /// Monotonic release build number.
    pub number: u64,
    /// Full commit identity decoded from the packaged 40-digit hexadecimal SHA.
    pub commit: [u8; 20],
}

impl UpdateBuild {
    pub(crate) fn known(self) -> bool {
        self.number > 0 && self.commit != [0; 20]
    }
}

/// Exact release context supplied to the configured checker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UpdateCheckRequest {
    /// Installed application version.
    pub installed: UpdateVersion,
    /// Installed packaged build identity.
    pub build: UpdateBuild,
    /// Configured release channel.
    pub channel: UpdateChannel,
    /// Configured packaged target.
    pub platform: UpdatePlatform,
}

/// Metadata observation. A matching candidate is availability, never download/install success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateCheckOutcome {
    /// Latest candidate on the requested release lane.
    Candidate {
        /// Candidate version.
        version: UpdateVersion,
        /// Candidate packaged build identity.
        build: UpdateBuild,
        /// Candidate channel, checked again by the UI adapter.
        channel: UpdateChannel,
        /// Candidate target, checked again by the UI adapter.
        platform: UpdatePlatform,
    },
    /// Connectivity prevented a check; current/latest state is unknown.
    Offline,
    /// A check failed; installed app remains untouched.
    Failed,
    /// The checker or release lane is unavailable.
    Unavailable,
}

/// Read-only metadata checker executed on GPUI's background executor.
/// Implementations must bound their I/O and honor cancellation. This port grants no installer
/// authority. Candidate metadata must include the exact packaged build number and commit;
/// a semantic version alone cannot establish current/nightly parity.
pub trait UpdateChecker: Send + Sync + 'static {
    /// Check the exact release context without modifying the installed application.
    fn check(&self, request: UpdateCheckRequest, cancelled: &AtomicBool) -> UpdateCheckOutcome;
}

/// Optional app-global checker registration, supplied by a release-metadata service adapter.
/// No checker is registered by default. Registration must use the installed artifact's actual
/// channel and target. This is a UI port, not a production network implementation.
#[derive(Clone)]
pub struct UpdateCheckService {
    /// Actual packaged release channel.
    pub channel: UpdateChannel,
    /// Actual packaged target.
    pub platform: UpdatePlatform,
    /// Actual installed artifact release version, not the UI component crate version.
    pub installed_version: UpdateVersion,
    /// Actual installed artifact build identity, never inferred as zero on missing metadata.
    pub installed_build: UpdateBuild,
    /// Read-only checker.
    pub checker: Arc<dyn UpdateChecker>,
}

impl gpui::Global for UpdateCheckService {}
