//! Bounded read-only checks against the existing public Yield release catalog.
//!
//! This service grants no download, installation or artifact verification authority.

use std::io::Read;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use serde::Deserialize;

const ENDPOINT: &str = "https://portalsurfer.org/yield/api/v1/releases";
const MAX_BYTES: u64 = 512 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);

/// Whether the colliding legacy 19.1.x line may move to the new pre-1.0 line.
/// This changes version ordering only, never build, commit, lane or target checks.
pub fn is_legacy_renumbering_transition(installed: [u32; 3], candidate: [u32; 3]) -> bool {
    installed[0] == 19
        && installed[1] == 1
        && candidate[0] == 0
        && (candidate[1], candidate[2]) >= (19, 1)
}

/// Whether a new-line installation must ignore a historical colliding release.
pub fn is_historical_legacy_candidate(installed: [u32; 3], candidate: [u32; 3]) -> bool {
    installed[0] == 0
        && (installed[1], installed[2]) >= (19, 1)
        && candidate[0] == 19
        && candidate[1] == 1
}

/// Core version eligibility shared by the catalog adapter and UI projection.
/// Full prerelease precedence and immutable packaged identity still need checking.
pub fn release_core_is_admissible(installed: [u32; 3], candidate: [u32; 3]) -> bool {
    !is_historical_legacy_candidate(installed, candidate)
        && (candidate >= installed || is_legacy_renumbering_transition(installed, candidate))
}

/// Release lane. Catalog versions must agree with this lane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// Stable releases only.
    Stable,
    /// Release candidates only.
    Rc,
    /// Nightly releases only.
    Nightly,
}

/// Targets supported by the current release contract, independent of test host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    /// Apple Silicon macOS.
    MacArm64,
    /// Intel macOS.
    MacX64,
    /// 64-bit Windows.
    WindowsX64,
}

impl Platform {
    fn labels(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::MacArm64 => ("macos", "aarch64", "aarch64-apple-darwin"),
            Self::MacX64 => ("macos", "x86_64", "x86_64-apple-darwin"),
            Self::WindowsX64 => ("windows", "x86_64", "x86_64-pc-windows-msvc"),
        }
    }
}

/// A candidate identity returned by the catalog, with no remote text or URLs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Semantic version core; lane identity is retained separately.
    pub version: [u32; 3],
    /// Validated complete version text (at most 100 bytes), preserving prerelease identity.
    pub release_version: String,
    /// Monotonic packaged build number.
    pub build_number: u64,
    /// Full commit identity, never a prefix match.
    pub commit: [u8; 20],
    /// Validated release lane.
    pub channel: Channel,
    /// Validated asset target.
    pub platform: Platform,
}

/// Bounded catalog outcome suitable for the UI worker's typed result port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The latest matching catalog entry has a complete identity.
    Candidate(Candidate),
    /// Connection or name resolution could not reach the catalog.
    Offline,
    /// HTTP, parsing, identity or resource-bound validation failed.
    Failed,
    /// No complete matching identity, cancellation, or an existing inflight check.
    Unavailable,
}

/// Single-inflight production HTTP service; a stalled DNS worker retains its slot.
///
/// A caller returns by the deadline even if the OS resolver cannot be interrupted.
/// Subsequent calls cannot accumulate more stalled threads. There are no retries,
/// redirects, credentials, cookies, runtime URL overrides or filesystem mutations.
pub struct CatalogService {
    busy: Arc<AtomicBool>,
    endpoint: String,
    deadline: Duration,
}

impl Default for CatalogService {
    fn default() -> Self {
        Self {
            busy: Arc::new(AtomicBool::new(false)),
            endpoint: ENDPOINT.into(),
            deadline: DEADLINE,
        }
    }
}

impl CatalogService {
    /// Construct a controlled loopback HTTP fixture with a bounded deadline.
    /// This API is absent unless the explicit `test-support` feature is enabled.
    #[cfg(feature = "test-support")]
    pub fn for_loopback_tests(address: std::net::SocketAddr, deadline: Duration) -> Option<Self> {
        if !address.ip().is_loopback() || deadline.is_zero() || deadline > DEADLINE {
            return None;
        }
        Some(Self {
            busy: Arc::new(AtomicBool::new(false)),
            endpoint: format!("http://{address}/releases"),
            deadline,
        })
    }

    /// Fetch the latest exact lane/target metadata without blocking the UI executor.
    /// Call this from the UI's background worker. Cancellation returns promptly;
    /// the bounded transport worker retains admission until its I/O ends.
    pub fn check(&self, channel: Channel, platform: Platform, cancelled: &AtomicBool) -> Outcome {
        self.check_context(channel, platform, None, cancelled)
    }

    /// Select metadata for the retained installed version, filtering the historical
    /// colliding line before highest-build selection. Installed context must come
    /// from the immutable packaged identity in the production adapter.
    pub fn check_for_installed(
        &self,
        channel: Channel,
        platform: Platform,
        installed: [u32; 3],
        cancelled: &AtomicBool,
    ) -> Outcome {
        self.check_context(channel, platform, Some(installed), cancelled)
    }

    fn check_context(
        &self,
        channel: Channel,
        platform: Platform,
        installed: Option<[u32; 3]>,
        cancelled: &AtomicBool,
    ) -> Outcome {
        if cancelled.load(Ordering::Acquire)
            || self
                .busy
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Outcome::Unavailable;
        }
        let busy = self.busy.clone();
        let endpoint = self.endpoint.clone();
        let deadline = self.deadline;
        let (sender, receiver) = mpsc::sync_channel(1);
        let spawned = std::thread::Builder::new()
            .name("yield-release-catalog".into())
            .spawn(move || {
                struct Release(Arc<AtomicBool>);
                impl Drop for Release {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _release = Release(busy);
                let _ = sender.send(fetch(&endpoint, deadline, channel, platform, installed));
            });
        if spawned.is_err() {
            self.busy.store(false, Ordering::Release);
            return Outcome::Failed;
        }
        let start = Instant::now();
        loop {
            if cancelled.load(Ordering::Acquire) {
                return Outcome::Unavailable;
            }
            let Some(remaining) = deadline.checked_sub(start.elapsed()) else {
                return Outcome::Failed;
            };
            match receiver.recv_timeout(remaining.min(Duration::from_millis(20))) {
                Ok(result) => {
                    return if cancelled.load(Ordering::Acquire) {
                        Outcome::Unavailable
                    } else {
                        result
                    };
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return Outcome::Failed,
            }
        }
    }
}

fn fetch(
    endpoint: &str,
    deadline: Duration,
    channel: Channel,
    platform: Platform,
    installed: Option<[u32; 3]>,
) -> Outcome {
    // TLS root validation is ureq/rustls's default. Redirects are rejected, so
    // catalog authority cannot move to another origin (including localhost).
    let agent = ureq::AgentBuilder::new()
        .redirects(0)
        .timeout(deadline)
        .build();
    let response = match agent
        .get(endpoint)
        .set("Accept", "application/json")
        .set("Accept-Encoding", "identity")
        .set("User-Agent", "yield-update-check")
        .call()
    {
        Ok(response) if response.status() == 200 => response,
        Err(ureq::Error::Transport(error)) => {
            return match error.kind() {
                ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => Outcome::Offline,
                _ => Outcome::Failed,
            };
        }
        _ => return Outcome::Failed,
    };
    if response
        .header("Content-Type")
        .and_then(|v| v.split(';').next())
        != Some("application/json")
        || response
            .header("Content-Encoding")
            .is_some_and(|v| v != "identity")
    {
        return Outcome::Failed;
    }
    if response
        .header("Content-Length")
        .is_some_and(|v| v.parse::<u64>().map_or(true, |n| n > MAX_BYTES))
    {
        return Outcome::Failed;
    }
    let mut bytes = Vec::new();
    if response
        .into_reader()
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_BYTES
    {
        return Outcome::Failed;
    }
    select_for_installed(&bytes, channel, platform, installed)
}

#[derive(Deserialize)]
struct Catalog {
    releases: Vec<Release>,
}
#[derive(Deserialize)]
struct Release {
    build_id: String,
    build_number: u64,
    version: String,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    files: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
}

#[cfg(test)]
fn select(bytes: &[u8], channel: Channel, platform: Platform) -> Outcome {
    select_for_installed(bytes, channel, platform, None)
}

fn select_for_installed(
    bytes: &[u8],
    channel: Channel,
    platform: Platform,
    installed: Option<[u32; 3]>,
) -> Outcome {
    // Bound nesting even for ignored server fields, before serde traverses them.
    if !bounded_json(bytes) {
        return Outcome::Failed;
    }
    let Ok(catalog) = serde_json::from_slice::<Catalog>(bytes) else {
        return Outcome::Failed;
    };
    if catalog.releases.len() > 512 {
        return Outcome::Failed;
    }
    let mut latest: Option<(u64, Option<Candidate>)> = None;
    let mut identities = Vec::new();
    for release in catalog.releases {
        if release.build_id.len() > 160
            || release.version.len() > 100
            || release.commit.as_ref().is_some_and(|s| s.len() > 40)
            || release.channel.as_ref().is_some_and(|s| s.len() > 16)
            || release.files.len() > 32
            || release.files.iter().any(|f| f.name.len() > 160)
        {
            return Outcome::Failed;
        }
        let Ok(version) = semver::Version::parse(&release.version) else {
            // Legacy `nightly` has no semantic identity. Never substitute it with
            // the installed version to produce a false Current observation.
            if release.version == "nightly" {
                if channel == Channel::Nightly {
                    let version = semver::Version::new(0, 0, 0);
                    let Some(asset) = asset_name(&version, channel, platform) else {
                        return Outcome::Failed;
                    };
                    if release.files.iter().any(|f| f.name == asset)
                        && latest
                            .as_ref()
                            .is_none_or(|(number, _)| *number < release.build_number)
                    {
                        latest = Some((release.build_number, None));
                    }
                }
                continue;
            }
            return Outcome::Failed;
        };
        let lane = if version.pre.is_empty() {
            Channel::Stable
        } else if version
            .pre
            .as_str()
            .strip_prefix("rc.")
            .is_some_and(|s| s.parse::<u32>().is_ok())
        {
            Channel::Rc
        } else if version.pre.as_str().starts_with("nightly.") {
            Channel::Nightly
        } else {
            return Outcome::Failed;
        };
        if release
            .channel
            .as_deref()
            .is_some_and(|s| s != channel_name(lane))
        {
            return Outcome::Failed;
        }
        if lane != channel {
            continue;
        }
        let Some(asset) = asset_name(&version, channel, platform) else {
            return Outcome::Failed;
        };
        if !release.files.iter().any(|f| f.name == asset) {
            continue;
        }
        if release.build_number == 0 {
            return Outcome::Failed;
        }
        let suffix = release.build_id.rsplit('-').next().unwrap_or("");
        let commit_text = release.commit.as_deref().unwrap_or(suffix);
        let commit = full_commit(commit_text);
        if release.commit.is_some() && commit.is_none() {
            return Outcome::Failed;
        }
        if release.commit.is_some()
            && release.build_id != format!("yield-{}", release.version)
            && (!suffix.bytes().all(|c| c.is_ascii_hexdigit())
                || suffix.len() < 7
                || suffix.len() > 40
                || !commit_text.starts_with(suffix))
        {
            return Outcome::Failed;
        }
        let core = [version.major, version.minor, version.patch].map(u32::try_from);
        let [Ok(major), Ok(minor), Ok(patch)] = core else {
            return Outcome::Failed;
        };
        if installed.is_some_and(|installed| {
            is_historical_legacy_candidate(installed, [major, minor, patch])
        }) {
            // Valid historical identities cannot mask a new-line candidate.
            // Parsing/identity bounds above still apply; no ordinary fallback
            // from an incomplete latest allowed entry is introduced.
            continue;
        }
        let candidate = commit.map(|commit| Candidate {
            version: [major, minor, patch],
            release_version: release.version,
            build_number: release.build_number,
            commit,
            channel,
            platform,
        });
        if identities
            .iter()
            .any(|(number, previous)| *number == release.build_number && previous != &candidate)
        {
            return Outcome::Failed;
        }
        identities.push((release.build_number, candidate.clone()));
        match latest.as_ref() {
            Some((number, previous))
                if *number == release.build_number && previous != &candidate =>
            {
                return Outcome::Failed;
            }
            Some((number, _)) if *number >= release.build_number => {}
            _ => latest = Some((release.build_number, candidate)),
        }
    }
    match latest.and_then(|(_, candidate)| candidate) {
        Some(candidate) => Outcome::Candidate(candidate),
        None => Outcome::Unavailable,
    }
}

fn channel_name(channel: Channel) -> &'static str {
    match channel {
        Channel::Stable => "stable",
        Channel::Rc => "rc",
        Channel::Nightly => "nightly",
    }
}

fn full_commit(text: &str) -> Option<[u8; 20]> {
    if text.len() != 40 || !text.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut commit = [0; 20];
    for (index, byte) in commit.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    (commit != [0; 20]).then_some(commit)
}

fn asset_name(version: &semver::Version, channel: Channel, platform: Platform) -> Option<String> {
    // Consume the repository contract instead of admitting future/inactive targets.
    let contract: toml::Value =
        toml::from_str(include_str!("../../../release_contract.toml")).ok()?;
    let (os, arch, target) = platform.labels();
    if !contract["targets"]
        .as_array()?
        .iter()
        .any(|v| v.as_str() == Some(target))
    {
        return None;
    }
    let key = format!("{}_asset", channel_name(channel));
    let template = contract["templates"].get(&key)?.as_str()?;
    let core = format!("{}.{}.{}", version.major, version.minor, version.patch);
    Some(
        template
            .replace("{APP_NAME}", contract["app_name"].as_str()?)
            .replace("{platform}", os)
            .replace("{arch}", arch)
            .replace("{version}", &core)
            .replace(
                "{rc_number}",
                version.pre.as_str().strip_prefix("rc.").unwrap_or(""),
            ),
    )
}

fn bounded_json(bytes: &[u8]) -> bool {
    if bytes.len() as u64 > MAX_BYTES {
        return false;
    }
    let (mut depth, mut string, mut escape, mut text) = (0u32, false, false, 0usize);
    for &byte in bytes {
        if string {
            text += 1;
            if text > 16 * 1024 {
                return false;
            }
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                string = false;
            }
        } else {
            match byte {
                b'"' => {
                    string = true;
                    text = 0;
                }
                b'[' | b'{' => {
                    depth += 1;
                    if depth > 16 {
                        return false;
                    }
                }
                b']' | b'}' => {
                    let Some(next) = depth.checked_sub(1) else {
                        return false;
                    };
                    depth = next;
                }
                _ => {}
            }
        }
    }
    !string && depth == 0
}

#[cfg(test)]
mod tests;
