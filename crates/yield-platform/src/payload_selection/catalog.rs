use super::{SelectionError, protocol};
use crate::update_catalog::{
    Channel, Platform, asset_name, bounded_json, channel_name, full_commit,
    is_historical_legacy_candidate, is_legacy_renumbering_transition, release_core_is_admissible,
};
use serde::{
    Deserialize, Deserializer,
    de::{self, SeqAccess, Visitor},
};
use std::{fmt, marker::PhantomData};

#[derive(Clone)]
pub(super) struct Installed {
    pub(super) version: semver::Version,
    pub(super) full_version: String,
    pub(super) core: [u32; 3],
    pub(super) number: u64,
    pub(super) commit: [u8; 20],
    pub(super) platform: Platform,
}
impl Installed {
    pub(super) fn compiled() -> Option<Self> {
        let identity = yield_core::ReleaseBuildIdentity::compiled().ok()?;
        let platform = match identity.target() {
            "aarch64-apple-darwin" => Platform::MacArm64,
            "x86_64-apple-darwin" => Platform::MacX64,
            "x86_64-pc-windows-msvc" => Platform::WindowsX64,
            _ => return None,
        };
        Some(Self {
            version: semver::Version::parse(identity.version()).ok()?,
            full_version: identity.version().into(),
            core: identity.target_version(),
            number: identity.build_number(),
            commit: identity.commit(),
            platform,
        })
    }
}

/// Bounded strings allocate only after their field-specific length check.
pub(super) struct Text<const N: usize>(pub(super) String);
impl<'de, const N: usize> Deserialize<'de> for Text<N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<const N: usize>;
        impl<'de, const N: usize> Visitor<'de> for V<N> {
            type Value = Text<N>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bounded text")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value.len() > N {
                    return Err(E::custom("text limit"));
                }
                let mut out = String::new();
                out.try_reserve_exact(value.len())
                    .map_err(|_| E::custom("allocation"))?;
                out.push_str(value);
                Ok(Text(out))
            }
        }
        d.deserialize_str(V::<N>)
    }
}
pub(super) struct List<T, const N: usize>(pub(super) Vec<T>);
impl<'de, T: Deserialize<'de>, const N: usize> Deserialize<'de> for List<T, N> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<T, const N: usize>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for V<T, N> {
            type Value = List<T, N>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bounded list")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                if seq.size_hint().is_some_and(|n| n > N) {
                    return Err(de::Error::custom("list limit"));
                }
                let mut out = Vec::new();
                loop {
                    if out.len() == N {
                        if seq.next_element::<de::IgnoredAny>()?.is_some() {
                            return Err(de::Error::custom("list limit"));
                        }
                        return Ok(List(out));
                    }
                    let Some(item) = seq.next_element()? else {
                        return Ok(List(out));
                    };
                    out.try_reserve_exact(1)
                        .map_err(|_| de::Error::custom("allocation"))?;
                    out.push(item);
                }
            }
        }
        d.deserialize_seq(V::<T, N>(PhantomData))
    }
}
#[derive(Deserialize)]
struct Catalog {
    releases: List<WireRelease, 512>,
}
#[derive(Deserialize)]
struct WireRelease {
    build_id: Text<160>,
    build_number: u64,
    version: Text<100>,
    #[serde(default)]
    commit: Option<Text<40>>,
    #[serde(default)]
    channel: Option<Text<16>>,
    files: List<WireFile, 16>,
}
#[derive(Deserialize)]
struct WireFile {
    name: Text<160>,
    #[serde(default)]
    sha256: Option<Text<64>>,
    #[serde(default)]
    size_bytes: Option<u64>,
    #[serde(default)]
    url: Option<Text<4096>>,
    #[serde(default)]
    download_url: Option<Text<4096>>,
    #[serde(default)]
    href: Option<Text<4096>>,
}

#[derive(PartialEq, Eq)]
pub(super) struct File {
    pub(super) name: String,
    pub(super) sha256: [u8; 32],
    pub(super) size_bytes: u64,
    pub(super) url: url::Url,
}
#[derive(PartialEq, Eq)]
pub(super) struct Release {
    pub(super) build_id: String,
    pub(super) build_number: u64,
    pub(super) version: String,
    pub(super) commit: [u8; 20],
    pub(super) files: Vec<File>,
}
impl Release {
    pub(super) fn current(&self, installed: &Installed) -> bool {
        self.version == installed.full_version
            && self.build_number == installed.number
            && self.commit == installed.commit
    }
}

pub(super) fn select(
    bytes: &[u8],
    installed: &Installed,
    channel: Channel,
) -> Result<Release, SelectionError> {
    let invalid = SelectionError::Invalid;
    if !bounded_json(bytes) {
        return Err(invalid);
    }
    let catalog: Catalog = serde_json::from_slice(bytes).map_err(|_| invalid)?;
    let mut allowed = Vec::new();
    let mut retired_nightly = 0;
    for wire in catalog.releases.0 {
        // An identity-less retired nightly row cannot authorize a payload or a fallback.
        if wire.version.0 == "nightly" {
            if channel == Channel::Nightly
                && wire.files.0.iter().any(|f| {
                    asset_name(&semver::Version::new(0, 0, 0), channel, installed.platform).as_ref()
                        == Some(&f.name.0)
                })
            {
                retired_nightly = retired_nightly.max(wire.build_number);
            }
            continue;
        }
        let version = semver::Version::parse(&wire.version.0).map_err(|_| invalid)?;
        let core = core(&version).ok_or(invalid)?;
        if is_historical_legacy_candidate(installed.core, core) {
            continue;
        }
        let lane = lane(&version).ok_or(invalid)?;
        if wire
            .channel
            .as_ref()
            .is_some_and(|c| c.0 != channel_name(lane))
        {
            return Err(invalid);
        }
        if lane != channel {
            continue;
        }
        let commit = full_commit(&wire.commit.ok_or(invalid)?.0).ok_or(invalid)?;
        if !canonical_version(&version, channel, commit)
            || wire.build_number == 0
            || !build_id(
                &wire.build_id.0,
                &wire.version.0,
                channel,
                wire.build_number,
                commit,
            )
        {
            return Err(invalid);
        }
        let expected = inventory(&version, channel).ok_or(invalid)?;
        if wire.files.0.len() != expected.len() {
            return Err(invalid);
        }
        let mut files = Vec::new();
        files
            .try_reserve_exact(expected.len())
            .map_err(|_| invalid)?;
        for file in wire.files.0 {
            if !expected.contains(&file.name.0)
                || files.iter().any(|f: &File| f.name == file.name.0)
            {
                return Err(invalid);
            }
            let size = file.size_bytes.filter(|n| *n > 0).ok_or(invalid)?;
            let ceiling = if file.name.0.ends_with(".zip") {
                1024 * 1024 * 1024
            } else if file.name.0.ends_with(".sig") {
                8192
            } else {
                1024 * 1024
            };
            if size > ceiling {
                return Err(invalid);
            }
            let sha256 = digest(&file.sha256.ok_or(invalid)?.0).ok_or(invalid)?;
            let mut selected_url = None;
            for raw in [file.url, file.download_url, file.href]
                .into_iter()
                .flatten()
            {
                let parsed = protocol::asset_url(&raw.0)?;
                if selected_url
                    .as_ref()
                    .is_some_and(|previous| previous != &parsed)
                {
                    return Err(invalid);
                }
                selected_url = Some(parsed);
            }
            let url = selected_url.ok_or(invalid)?;
            if files.iter().any(|f| protocol::same_resource(&f.url, &url)) {
                return Err(invalid);
            }
            files.push(File {
                name: file.name.0,
                sha256,
                size_bytes: size,
                url,
            });
        }
        files.sort_by(|a, b| a.name.cmp(&b.name));
        let release = Release {
            build_id: wire.build_id.0,
            build_number: wire.build_number,
            version: wire.version.0,
            commit,
            files,
        };
        if allowed.iter().any(|r: &Release| {
            r.build_id == release.build_id || r.build_number == release.build_number
        }) {
            return Err(invalid);
        }
        allowed.try_reserve_exact(1).map_err(|_| invalid)?;
        allowed.push(release);
    }
    let release = allowed
        .into_iter()
        .max_by_key(|r| r.build_number)
        .ok_or(SelectionError::Unavailable)?;
    if channel == Channel::Nightly && retired_nightly >= release.build_number {
        return Err(SelectionError::Unavailable);
    }
    let version = semver::Version::parse(&release.version).map_err(|_| invalid)?;
    let candidate_core = core(&version).ok_or(invalid)?;
    if !release.current(installed)
        && !(release.build_number > installed.number
            && release.commit != installed.commit
            && release_core_is_admissible(installed.core, candidate_core)
            && (!version.cmp_precedence(&installed.version).is_lt()
                || is_legacy_renumbering_transition(installed.core, candidate_core)))
    {
        return Err(invalid);
    }
    Ok(release)
}

fn core(v: &semver::Version) -> Option<[u32; 3]> {
    Some([
        v.major.try_into().ok()?,
        v.minor.try_into().ok()?,
        v.patch.try_into().ok()?,
    ])
}
fn lane(v: &semver::Version) -> Option<Channel> {
    if v.pre.is_empty() {
        Some(Channel::Stable)
    } else if v.pre.as_str().starts_with("rc.") {
        Some(Channel::Rc)
    } else if v.pre.as_str().starts_with("nightly.") {
        Some(Channel::Nightly)
    } else {
        None
    }
}
fn canonical_version(v: &semver::Version, channel: Channel, commit: [u8; 20]) -> bool {
    if v.to_string().len() > 100 {
        return false;
    }
    match channel {
        Channel::Stable => v.pre.is_empty() && v.build.is_empty(),
        Channel::Rc => {
            v.build.is_empty()
                && v.pre
                    .as_str()
                    .strip_prefix("rc.")
                    .and_then(|s| s.parse::<u32>().ok().map(|n| n > 0 && n.to_string() == s))
                    .unwrap_or(false)
        }
        Channel::Nightly => {
            let Some(date) = v.pre.as_str().strip_prefix("nightly.") else {
                return false;
            };
            let hash = v.build.as_str();
            valid_date(date)
                && hash.len() == 8
                && u32::from_str_radix(hash, 16).ok()
                    == Some(u32::from_be_bytes(commit[..4].try_into().unwrap()))
        }
    }
}
fn valid_date(date: &str) -> bool {
    let b = date.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || b.iter()
            .enumerate()
            .any(|(i, c)| i != 4 && i != 7 && !c.is_ascii_digit())
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
fn build_id(id: &str, version: &str, channel: Channel, number: u64, commit: [u8; 20]) -> bool {
    if id == format!("yield-{version}") {
        return true;
    }
    if channel != Channel::Nightly {
        return false;
    }
    let Some(tail) = id.strip_prefix("yield-nightly-b") else {
        return false;
    };
    let Some((build, hash)) = tail.split_once('-') else {
        return false;
    };
    let full = commit
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    build == number.to_string()
        && (hash.len() == 8 || hash.len() == 40)
        && full[..hash.len()].eq_ignore_ascii_case(hash)
}
fn digest(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}
pub(super) fn inventory(version: &semver::Version, channel: Channel) -> Option<Vec<String>> {
    let contract: toml::Value =
        toml::from_str(include_str!("../../../../release_contract.toml")).ok()?;
    let mut names = Vec::new();
    for platform in [Platform::MacArm64, Platform::MacX64, Platform::WindowsX64] {
        names.push(asset_name(version, channel, platform)?);
    }
    let core = format!("{}.{}.{}", version.major, version.minor, version.patch);
    for kind in ["checksums", "checksums_sig"] {
        let template = contract["templates"]
            .get(format!("{}_{}", channel_name(channel), kind))?
            .as_str()?;
        names.push(template.replace("{version}", &core).replace(
            "{rc_number}",
            version.pre.as_str().strip_prefix("rc.").unwrap_or(""),
        ));
    }
    Some(names)
}
