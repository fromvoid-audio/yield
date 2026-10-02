//! Production read-only update-check registration from immutable packaged build metadata.

use crate::update_check::{
    UpdateBuild, UpdateChannel, UpdateCheckOutcome, UpdateCheckRequest, UpdateCheckService,
    UpdateChecker, UpdatePlatform, UpdateVersion,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use yield_platform::update_catalog::{Candidate, CatalogService, Channel, Outcome, Platform};

struct InstalledContext {
    request: UpdateCheckRequest,
    full_version: &'static str,
    semantic_version: semver::Version,
}

impl InstalledContext {
    fn compiled() -> Option<Self> {
        use yield_core::{ReleaseBuildIdentity, ReleaseChannel};
        let identity = ReleaseBuildIdentity::compiled().ok()?;
        let [major, minor, patch] = identity.target_version();
        let semantic_version = semver::Version::parse(identity.version()).ok()?;
        if [
            semantic_version.major,
            semantic_version.minor,
            semantic_version.patch,
        ] != [u64::from(major), u64::from(minor), u64::from(patch)]
        {
            return None;
        }
        Some(Self {
            request: UpdateCheckRequest {
                installed: UpdateVersion(major, minor, patch),
                build: UpdateBuild {
                    number: identity.build_number(),
                    commit: identity.commit(),
                },
                channel: match identity.channel() {
                    ReleaseChannel::Stable => UpdateChannel::Stable,
                    ReleaseChannel::Rc => UpdateChannel::Rc,
                    ReleaseChannel::Nightly => UpdateChannel::Nightly,
                },
                platform: match identity.target() {
                    "aarch64-apple-darwin" => UpdatePlatform::MacArm64,
                    "x86_64-apple-darwin" => UpdatePlatform::MacX64,
                    "x86_64-pc-windows-msvc" => UpdatePlatform::WindowsX64,
                    _ => return None,
                },
            },
            full_version: identity.version(),
            semantic_version,
        })
    }
}

struct ProductionChecker {
    installed: InstalledContext,
    catalog: CatalogService,
}

impl UpdateChecker for ProductionChecker {
    fn check(&self, request: UpdateCheckRequest, cancelled: &AtomicBool) -> UpdateCheckOutcome {
        if cancelled.load(Ordering::Acquire) {
            return UpdateCheckOutcome::Unavailable;
        }
        // A selected lane may change; installed identity and packaged target may not.
        if request.installed != self.installed.request.installed
            || request.build != self.installed.request.build
            || request.platform != self.installed.request.platform
        {
            return UpdateCheckOutcome::Failed;
        }
        let result = match self.catalog.check(
            channel(request.channel),
            platform(request.platform),
            cancelled,
        ) {
            Outcome::Candidate(candidate) => self.classify(request, candidate),
            Outcome::Offline => UpdateCheckOutcome::Offline,
            Outcome::Failed => UpdateCheckOutcome::Failed,
            Outcome::Unavailable => UpdateCheckOutcome::Unavailable,
        };
        if cancelled.load(Ordering::Acquire) {
            UpdateCheckOutcome::Unavailable
        } else {
            result
        }
    }
}

impl ProductionChecker {
    fn classify(&self, request: UpdateCheckRequest, candidate: Candidate) -> UpdateCheckOutcome {
        let Ok(version) = semver::Version::parse(&candidate.release_version) else {
            return UpdateCheckOutcome::Failed;
        };
        let installed = &self.installed;
        if candidate.channel != channel(request.channel)
            || candidate.platform != platform(request.platform)
            || candidate.version.map(u64::from) != [version.major, version.minor, version.patch]
            || !canonical_candidate_version(&version, &candidate)
        {
            return UpdateCheckOutcome::Failed;
        }
        let exact = candidate.release_version == installed.full_version
            && candidate.build_number == request.build.number
            && candidate.commit == request.build.commit;
        let newer = candidate.build_number > request.build.number
            && candidate.commit != request.build.commit
            && !version.cmp_precedence(&installed.semantic_version).is_lt();
        if !exact && !newer {
            return UpdateCheckOutcome::Failed;
        }
        let [major, minor, patch] = candidate.version;
        UpdateCheckOutcome::Candidate {
            version: UpdateVersion(major, minor, patch),
            build: UpdateBuild {
                number: candidate.build_number,
                commit: candidate.commit,
            },
            channel: request.channel,
            platform: request.platform,
        }
    }
}

fn canonical_candidate_version(version: &semver::Version, candidate: &Candidate) -> bool {
    match candidate.channel {
        Channel::Stable => version.pre.is_empty() && version.build.is_empty(),
        Channel::Rc => {
            version.build.is_empty()
                && version
                    .pre
                    .as_str()
                    .strip_prefix("rc.")
                    .and_then(|s| s.parse::<u32>().ok())
                    .is_some_and(|number| number > 0)
        }
        Channel::Nightly => {
            let Some(date) = version.pre.as_str().strip_prefix("nightly.") else {
                return false;
            };
            let hash = version.build.as_str();
            valid_date(date)
                && hash.len() == 8
                && hash.bytes().all(|b| b.is_ascii_hexdigit())
                && u32::from_str_radix(hash, 16).ok()
                    == Some(u32::from_be_bytes(
                        candidate.commit[..4].try_into().unwrap(),
                    ))
        }
    }
}

fn valid_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes
            .iter()
            .enumerate()
            .any(|(i, b)| i != 4 && i != 7 && !b.is_ascii_digit())
    {
        return false;
    }
    let year = date[..4].parse::<u16>().unwrap_or(0);
    let month = date[5..7].parse::<u8>().unwrap_or(0);
    let day = date[8..].parse::<u8>().unwrap_or(0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return false,
    };
    year > 0 && day > 0 && day <= days
}

fn channel(channel: UpdateChannel) -> Channel {
    match channel {
        UpdateChannel::Stable => Channel::Stable,
        UpdateChannel::Rc => Channel::Rc,
        UpdateChannel::Nightly => Channel::Nightly,
    }
}
fn platform(platform: UpdatePlatform) -> Platform {
    match platform {
        UpdatePlatform::MacArm64 => Platform::MacArm64,
        UpdatePlatform::MacX64 => Platform::MacX64,
        UpdatePlatform::WindowsX64 => Platform::WindowsX64,
    }
}

pub(crate) fn register(cx: &mut gpui::App) {
    register_context(cx, InstalledContext::compiled(), CatalogService::default());
}

fn register_context(
    cx: &mut gpui::App,
    installed: Option<InstalledContext>,
    catalog: CatalogService,
) {
    let Some(installed) = installed else {
        if cx.try_global::<UpdateCheckService>().is_some() {
            cx.remove_global::<UpdateCheckService>();
        }
        return;
    };
    let request = installed.request;
    cx.set_global(UpdateCheckService {
        channel: request.channel,
        platform: request.platform,
        installed_version: request.installed,
        installed_build: request.build,
        checker: Arc::new(ProductionChecker { installed, catalog }),
    });
}

#[cfg(test)]
pub(crate) mod tests;
