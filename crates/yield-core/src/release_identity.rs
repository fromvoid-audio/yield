//! Immutable release identity supplied by the production packer at compile time.
//!
//! This is build metadata, not signature, installation or platform qualification. No runtime
//! environment, Cargo package version or external manifest can fill missing release inputs.

/// Release channel of one compiled artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseChannel {
    /// Final version without prerelease metadata.
    Stable,
    /// Numbered release candidate for the target version.
    Rc,
    /// Date- and commit-bound nightly build.
    Nightly,
}

/// Why the compiled artifact cannot supply a trustworthy update-check context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseIdentityError {
    /// One or more required packer inputs are absent.
    Unavailable,
    /// Inputs conflict, exceed bounds, lack a known identity or name an unsupported target.
    InvalidMetadata,
}

/// Complete immutable identity of the running artifact's compiled release context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReleaseBuildIdentity {
    channel: ReleaseChannel,
    version: &'static str,
    target_version: [u32; 3],
    target: &'static str,
    commit: [u8; 20],
    build_number: u64,
    build_date: &'static str,
}

impl ReleaseBuildIdentity {
    /// Read only compiler-captured inputs from the actual release packer.
    ///
    /// Local development builds and builds without a positive monotonic number stay unavailable
    /// or invalid. A successful result does not establish native release/signature qualification.
    pub fn compiled() -> Result<Self, ReleaseIdentityError> {
        validate([
            option_env!("YIELD_RELEASE_CHANNEL"),
            option_env!("YIELD_RELEASE_VERSION"),
            option_env!("YIELD_RELEASE_TARGET_VERSION"),
            Some(env!("YIELD_COMPILED_TARGET")),
            option_env!("YIELD_GIT_SHA"),
            option_env!("YIELD_BUILD_NUMBER"),
            option_env!("YIELD_RELEASE_BUILD_DATE"),
        ])
    }

    /// Exact channel supplied at build time.
    pub fn channel(self) -> ReleaseChannel {
        self.channel
    }

    /// Full release version, retaining the RC ordinal or nightly date/commit suffix.
    pub fn version(self) -> &'static str {
        self.version
    }

    /// Stable target version components; prerelease identity remains in the full version.
    pub fn target_version(self) -> [u32; 3] {
        self.target_version
    }

    /// Actual compiler target, checked against the current release contract.
    pub fn target(self) -> &'static str {
        self.target
    }

    /// Exact nonzero 20-byte source commit; short/unknown commits are rejected.
    pub fn commit(self) -> [u8; 20] {
        self.commit
    }

    /// Positive monotonic build identity; absent/zero values never imply Current.
    pub fn build_number(self) -> u64 {
        self.build_number
    }

    /// Validated calendar build date in YYYY-MM-DD form.
    pub fn build_date(self) -> &'static str {
        self.build_date
    }
}

fn validate(
    inputs: [Option<&'static str>; 7],
) -> Result<ReleaseBuildIdentity, ReleaseIdentityError> {
    let mut values = [""; 7];
    for (output, input) in values.iter_mut().zip(inputs) {
        *output = input
            .filter(|value| !value.is_empty())
            .ok_or(ReleaseIdentityError::Unavailable)?;
        if output.len() > 128 || !output.is_ascii() {
            return Err(ReleaseIdentityError::InvalidMetadata);
        }
    }
    let [
        channel,
        version,
        target_version,
        target,
        commit_text,
        build_number,
        build_date,
    ] = values;
    let invalid = ReleaseIdentityError::InvalidMetadata;
    let components = parse_version(target_version).ok_or(invalid)?;
    if !contract_target(target) || !valid_date(build_date) {
        return Err(invalid);
    }
    let commit = parse_commit(commit_text).ok_or(invalid)?;
    if !build_number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid);
    }
    let build_number = build_number
        .parse::<u64>()
        .ok()
        .filter(|number| *number > 0)
        .ok_or(invalid)?;
    let channel = match channel {
        "stable" if version == target_version => ReleaseChannel::Stable,
        "rc" => {
            let ordinal = version
                .strip_prefix(target_version)
                .and_then(|tail| tail.strip_prefix("-rc."))
                .ok_or(invalid)?;
            let number = ordinal
                .parse::<u32>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or(invalid)?;
            if number.to_string() != ordinal {
                return Err(invalid);
            }
            ReleaseChannel::Rc
        }
        "nightly" => {
            let suffix = version
                .strip_prefix(target_version)
                .and_then(|tail| tail.strip_prefix("-nightly."))
                .ok_or(invalid)?;
            let (date, abbreviated_commit) = suffix.split_once('+').ok_or(invalid)?;
            let expected_date: String = build_date
                .chars()
                .filter(|character| *character != '-')
                .collect();
            if date != expected_date
                || !(7..=40).contains(&abbreviated_commit.len())
                || !abbreviated_commit
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || !commit_text[..abbreviated_commit.len()].eq_ignore_ascii_case(abbreviated_commit)
            {
                return Err(invalid);
            }
            ReleaseChannel::Nightly
        }
        _ => return Err(invalid),
    };
    Ok(ReleaseBuildIdentity {
        channel,
        version,
        target_version: components,
        target,
        commit,
        build_number,
        build_date,
    })
}

fn parse_version(value: &str) -> Option<[u32; 3]> {
    let mut result = [0; 3];
    let mut parts = value.split('.');
    for component in &mut result {
        let part = parts.next()?;
        if part.is_empty()
            || !part.bytes().all(|byte| byte.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            return None;
        }
        *component = part.parse().ok()?;
    }
    parts.next().is_none().then_some(result)
}

fn parse_commit(value: &str) -> Option<[u8; 20]> {
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; 20];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok()?;
    }
    bytes.iter().any(|byte| *byte != 0).then_some(bytes)
}

fn valid_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return false;
    }
    let year = value[..4].parse::<u32>().unwrap_or(0);
    let month = value[5..7].parse::<u32>().unwrap_or(0);
    let day = value[8..].parse::<u32>().unwrap_or(0);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => 0,
    };
    year > 0 && day > 0 && day <= days
}

fn contract_target(target: &str) -> bool {
    // Read the authoritative checked-in target list, never a runtime target override.
    include_str!("../../../release_contract.toml")
        .lines()
        .skip_while(|line| line.trim() != "targets = [")
        .skip(1)
        .take_while(|line| line.trim() != "]")
        .any(|line| line.trim().trim_end_matches(',').trim_matches('"') == target)
}

#[cfg(test)]
mod tests;
