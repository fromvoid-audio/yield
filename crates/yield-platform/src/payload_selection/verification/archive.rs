use super::{CHUNK, Check, Ledger, SelectionError};
use crate::payload_selection::{
    SelectedPayload,
    catalog::{List, Text},
};
use flate2::{Decompress, FlushDecompress, Status};
use serde::Deserialize;
mod paths;
mod zip;

pub(super) fn verify(
    bytes: &[u8],
    selection: &SelectedPayload,
    check: &Check<'_>,
    ledger: &mut Ledger,
) -> Result<String, SelectionError> {
    let invalid = SelectionError::Invalid;
    let entries = zip::index(bytes, check, ledger)?;
    paths::validate(&entries, check, ledger)?;
    let mut total = 0u64;
    for entry in &entries {
        check.live()?;
        total = total
            .checked_add(entry.expanded)
            .filter(|n| *n <= 2 * 1024 * 1024 * 1024)
            .ok_or(invalid)?;
    }
    let mut manifest = None;
    for entry in &entries {
        check.live()?;
        let is_manifest = entry.name == "yield/update-manifest.json";
        if is_manifest && (entry.directory || entry.expanded > 1024 * 1024) {
            return Err(invalid);
        }
        let mut raw_manifest = Vec::new();
        if is_manifest {
            raw_manifest
                .try_reserve_exact(usize::try_from(entry.expanded).map_err(|_| invalid)?)
                .map_err(|_| invalid)?;
            if raw_manifest.capacity() > 1024 * 1024 {
                return Err(invalid);
            }
        }
        let mut crc = crc32fast::Hasher::new();
        let mut produced = 0u64;
        let mut accept = |chunk: &[u8]| -> Result<(), SelectionError> {
            check.live()?;
            produced = produced
                .checked_add(chunk.len() as u64)
                .filter(|n| *n <= entry.expanded)
                .ok_or(invalid)?;
            crc.update(chunk);
            if is_manifest {
                raw_manifest.extend_from_slice(chunk);
            }
            Ok(())
        };
        let compressed = bytes.get(entry.data.clone()).ok_or(invalid)?;
        if entry.method == 0 {
            for chunk in compressed.chunks(CHUNK) {
                accept(chunk)?;
            }
        } else {
            let mut decoder = Decompress::new(false);
            let mut out = [0; CHUNK];
            let mut cursor = 0usize;
            loop {
                check.live()?;
                let end = cursor
                    .checked_add(CHUNK)
                    .ok_or(invalid)?
                    .min(compressed.len());
                let before_in = decoder.total_in();
                let before_out = decoder.total_out();
                let status = decoder
                    .decompress(&compressed[cursor..end], &mut out, FlushDecompress::None)
                    .map_err(|_| invalid)?;
                let consumed =
                    usize::try_from(decoder.total_in().checked_sub(before_in).ok_or(invalid)?)
                        .map_err(|_| invalid)?;
                let emitted =
                    usize::try_from(decoder.total_out().checked_sub(before_out).ok_or(invalid)?)
                        .map_err(|_| invalid)?;
                cursor = cursor
                    .checked_add(consumed)
                    .filter(|n| *n <= compressed.len())
                    .ok_or(invalid)?;
                accept(out.get(..emitted).ok_or(invalid)?)?;
                if status == Status::StreamEnd {
                    if cursor != compressed.len() {
                        return Err(invalid);
                    }
                    break;
                }
                if consumed == 0 && emitted == 0 {
                    return Err(invalid);
                }
            }
        }
        if produced != entry.expanded || crc.finalize() != entry.crc {
            return Err(invalid);
        }
        if is_manifest {
            if manifest.replace(raw_manifest).is_some() {
                return Err(invalid);
            }
        }
    }
    let raw = manifest.ok_or(invalid)?;
    check.live()?;
    if !crate::update_catalog::bounded_json_limit(&raw, 1024 * 1024) {
        return Err(invalid);
    }
    let manifest: Manifest = serde_json::from_slice(&raw).map_err(|_| invalid)?;
    manifest.validate(selection, &entries, check)?;
    Ok(manifest.build_date.0)
}

#[derive(Deserialize)]
struct Manifest {
    app: Text<32>,
    channel: Text<16>,
    target: Text<64>,
    platform: Text<16>,
    arch: Text<16>,
    version: Text<100>,
    target_version: Text<100>,
    commit: Text<40>,
    build_date: Text<10>,
    files: List<Text<1024>, 10000>,
}
impl Manifest {
    fn validate(
        &self,
        selection: &SelectedPayload,
        entries: &[zip::Entry<'_>],
        check: &Check<'_>,
    ) -> Result<(), SelectionError> {
        let invalid = SelectionError::Invalid;
        let (platform, arch, target) = selection.installed.platform.labels();
        let version = semver::Version::parse(&selection.release.version).map_err(|_| invalid)?;
        if self.app.0 != "yield"
            || self.channel.0 != crate::update_catalog::channel_name(selection.channel)
            || self.target.0 != target
            || self.platform.0 != platform
            || self.arch.0 != arch
            || self.version.0 != selection.release.version
            || self.target_version.0
                != format!("{}.{}.{}", version.major, version.minor, version.patch)
            || crate::update_catalog::full_commit(&self.commit.0) != Some(selection.release.commit)
            || !valid_date(&self.build_date.0)
        {
            return Err(invalid);
        }
        let contract: toml::Value =
            toml::from_str(include_str!("../../../../../release_contract.toml"))
                .map_err(|_| invalid)?;
        let layout = contract.get("archive_layout").ok_or(invalid)?;
        if layout.get("root_dir").and_then(toml::Value::as_str) != Some("{APP_NAME}") {
            return Err(invalid);
        }
        let required = layout
            .get("platform_files")
            .and_then(|p| p.get(platform))
            .and_then(toml::Value::as_array)
            .ok_or(invalid)?;
        for required in required {
            let name = format!(
                "yield/{}",
                required
                    .as_str()
                    .ok_or(invalid)?
                    .replace("{APP_NAME}", "yield")
            );
            if !entries.iter().any(|e| !e.directory && e.name == name) {
                return Err(invalid);
            }
        }
        let actual_count = entries.iter().filter(|e| !e.directory).count();
        if self.files.0.len() != actual_count {
            return Err(invalid);
        }
        // Manifest allocation/list overhead is covered by the fixed working credit. Sorting
        // borrowed references avoids cloning its strings or collecting expanded members.
        let mut files = Vec::new();
        files
            .try_reserve_exact(self.files.0.len())
            .map_err(|_| invalid)?;
        for name in &self.files.0 {
            check.live()?;
            paths::canonical(&name.0, false)?;
            files.push(name.0.as_str());
        }
        files.sort_unstable();
        if files.windows(2).any(|w| w[0] == w[1]) {
            return Err(invalid);
        }
        for entry in entries.iter().filter(|e| !e.directory) {
            check.live()?;
            let name = entry.name.strip_prefix("yield/").ok_or(invalid)?;
            if files.binary_search(&name).is_err() {
                return Err(invalid);
            }
        }
        Ok(())
    }
}
fn valid_date(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || b.iter()
            .enumerate()
            .any(|(i, c)| i != 4 && i != 7 && !c.is_ascii_digit())
    {
        return false;
    }
    let year = s[..4].parse::<u16>().unwrap_or(0);
    let month = s[5..7].parse::<u8>().unwrap_or(0);
    let day = s[8..].parse::<u8>().unwrap_or(0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}
