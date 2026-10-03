use super::{
    SelectionError,
    catalog::{Release, Text},
};
use crate::update_catalog::{Channel, Platform, asset_name, bounded_json, channel_name};
use serde::Deserialize;
use std::{io::Read, time::Instant};
mod guarded;

pub(super) const CATALOG: &str = "https://portalsurfer.org/yield/api/v1/releases";
const ORIGIN: &str = "https://portalsurfer.org";
const MAX_BODY: usize = 512 * 1024;

/// Test substitution affects transport only; stored asset requests keep the real fixed origin.
#[derive(Clone)]
pub(super) enum Transport {
    Production,
    #[cfg(test)]
    Loopback(std::net::SocketAddr),
}
impl Transport {
    pub(super) fn get(&self, original: &str, deadline: Instant) -> Result<Vec<u8>, SelectionError> {
        let invalid = SelectionError::Invalid;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(SelectionError::Retired)?;
        let original_url = url::Url::parse(original).map_err(|_| invalid)?;
        let endpoint = match self {
            Self::Production => original.to_owned(),
            #[cfg(test)]
            Self::Loopback(address) => {
                if !address.ip().is_loopback() {
                    return Err(invalid);
                }
                let query = original_url
                    .query()
                    .map(|q| format!("?{q}"))
                    .unwrap_or_default();
                format!("https://{address}{}{query}", original_url.path())
            }
        };
        // The production origin is fixed before any HTTP operation, not caller controlled.
        if original_url.origin().ascii_serialization() != ORIGIN
            || original_url.fragment().is_some()
            || !original_url.username().is_empty()
            || original_url.password().is_some()
        {
            return Err(invalid);
        }
        let builder = ureq::AgentBuilder::new().redirects(0).timeout(remaining);
        let agent = match self {
            Self::Production => builder
                .tls_connector(std::sync::Arc::new(guarded::ValidatedTls::new()))
                .build(),
            #[cfg(test)]
            Self::Loopback(_) => builder
                .tls_connector(std::sync::Arc::new(guarded::LoopbackTls))
                .build(),
        };
        let response = match agent
            .get(&endpoint)
            .set("Accept", "application/json")
            .set("Accept-Encoding", "identity")
            .set("User-Agent", "yield-payload-selection")
            .call()
        {
            Ok(r) if r.status() == 200 => r,
            Err(ureq::Error::Transport(e))
                if matches!(
                    e.kind(),
                    ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed
                ) =>
            {
                return Err(SelectionError::Offline);
            }
            _ => return Err(invalid),
        };
        let length = response
            .header("Content-Length")
            .map(|s| s.parse::<usize>().map_err(|_| invalid))
            .transpose()?;
        if response
            .header("Content-Type")
            .and_then(|s| s.split(';').next())
            != Some("application/json")
            || response
                .header("Content-Encoding")
                .is_some_and(|s| s != "identity")
            || length.is_some_and(|n| n > MAX_BODY)
            || (length.is_some() && response.header("Transfer-Encoding").is_some())
        {
            return Err(invalid);
        }
        let mut reader = response.into_reader();
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if Instant::now() >= deadline {
                return Err(SelectionError::Retired);
            }
            let n = reader.read(&mut chunk).map_err(|_| invalid)?;
            if n == 0 {
                return if length.is_none_or(|n| n == bytes.len()) {
                    Ok(bytes)
                } else {
                    Err(invalid)
                };
            }
            let len = bytes
                .len()
                .checked_add(n)
                .filter(|n| *n <= MAX_BODY)
                .ok_or(invalid)?;
            bytes
                .try_reserve_exact(len - bytes.len())
                .map_err(|_| invalid)?;
            bytes.extend_from_slice(&chunk[..n]);
        }
    }
}

pub(super) fn gate_url(build_id: &str) -> Result<String, SelectionError> {
    if build_id.is_empty() || build_id.len() > 160 || !build_id.is_ascii() {
        return Err(SelectionError::Invalid);
    }
    let encoded: String = url::form_urlencoded::byte_serialize(build_id.as_bytes()).collect();
    Ok(format!("{CATALOG}/{encoded}/gate?donation_amount=0.00"))
}

/// Validate the unnormalized spelling first: URL parsers remove dot segments themselves.
pub(super) fn asset_url(input: &str) -> Result<url::Url, SelectionError> {
    let invalid = SelectionError::Invalid;
    if input.is_empty()
        || input.len() > 4096
        || !input.is_ascii()
        || input.bytes().any(|b| b <= b' ' || b == 127 || b == b'\\')
    {
        return Err(invalid);
    }
    if input.contains(':') && !input.starts_with("https://") {
        return Err(invalid);
    }
    let without_query = input.split(['?', '#']).next().ok_or(invalid)?;
    let raw_path = if let Some((_, authority)) = without_query.split_once("://") {
        authority.find('/').map(|i| &authority[i..]).unwrap_or("")
    } else {
        without_query
    };
    let decoded = decode_path(raw_path)?;
    if decoded.split('/').any(|p| matches!(p, "." | "..")) {
        return Err(invalid);
    }
    let base = url::Url::parse(ORIGIN).map_err(|_| invalid)?;
    let parsed = base.join(input).map_err(|_| invalid)?;
    if parsed.origin().ascii_serialization() != ORIGIN
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || !parsed.path().starts_with("/yield/")
        || parsed.as_str().len() > 4096
    {
        return Err(invalid);
    }
    let path = decode_path(parsed.path())?;
    if !path.starts_with("/yield/") || path.split('/').any(|p| matches!(p, "." | "..")) {
        return Err(invalid);
    }
    let pairs: Vec<_> = parsed
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    if pairs.iter().any(|(key, _)| {
        key.contains('%')
            || key.contains('\\')
            || key.bytes().any(|b| b <= b' ' || b >= 127)
            || (key.eq_ignore_ascii_case("download_token") && key != "download_token")
    }) {
        return Err(invalid);
    }
    Ok(parsed)
}

fn decode_path(path: &str) -> Result<String, SelectionError> {
    let b = path.as_bytes();
    let mut out = String::new();
    out.try_reserve_exact(b.len())
        .map_err(|_| SelectionError::Invalid)?;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = path.get(i + 1..i + 3).ok_or(SelectionError::Invalid)?;
            let decoded = u8::from_str_radix(hex, 16).map_err(|_| SelectionError::Invalid)?;
            if decoded <= b' '
                || decoded >= 127
                || matches!(decoded, b'/' | b'\\' | b'%' | b'?' | b'#')
            {
                return Err(SelectionError::Invalid);
            }
            out.push(char::from(decoded));
            i += 3;
        } else {
            out.push(char::from(b[i]));
            i += 1;
        }
    }
    Ok(out)
}

pub(super) fn same_resource(a: &url::Url, b: &url::Url) -> bool {
    let mut a_pairs = a
        .query_pairs()
        .filter(|(k, _)| k != "download_token")
        .collect::<Vec<_>>();
    let mut b_pairs = b
        .query_pairs()
        .filter(|(k, _)| k != "download_token")
        .collect::<Vec<_>>();
    a_pairs.sort();
    b_pairs.sort();
    decode_path(a.path()).ok() == decode_path(b.path()).ok() && a_pairs == b_pairs
}

#[derive(Deserialize)]
struct Gate {
    build_id: Text<160>,
    download_token: Text<4096>,
}
/// No Debug/Display/Clone or public token/URL accessor. The staging owner consumes this custody.
pub(super) struct TokenRequest {
    url: url::Url,
}
impl TokenRequest {
    pub(super) fn len(&self) -> usize {
        self.url.as_str().len()
    }
}

pub(super) fn gate_requests(
    bytes: &[u8],
    release: &Release,
    channel: Channel,
    platform: Platform,
) -> Result<Vec<TokenRequest>, SelectionError> {
    let invalid = SelectionError::Invalid;
    if !bounded_json(bytes) {
        return Err(invalid);
    }
    let gate: Gate = serde_json::from_slice(bytes).map_err(|_| invalid)?;
    if gate.build_id.0 != release.build_id || gate.download_token.0.is_empty() {
        return Err(invalid);
    }
    let version = semver::Version::parse(&release.version).map_err(|_| invalid)?;
    let zip = asset_name(&version, channel, platform).ok_or(invalid)?;
    let core = format!("{}.{}.{}", version.major, version.minor, version.patch);
    let contract: toml::Value =
        toml::from_str(include_str!("../../../../release_contract.toml")).map_err(|_| invalid)?;
    let mut names = vec![zip];
    for kind in ["checksums", "checksums_sig"] {
        let key = format!("{}_{}", channel_name(channel), kind);
        let template = contract["templates"]
            .get(&key)
            .and_then(|v| v.as_str())
            .ok_or(invalid)?;
        names.push(template.replace("{version}", &core).replace(
            "{rc_number}",
            version.pre.as_str().strip_prefix("rc.").unwrap_or(""),
        ));
    }
    let mut requests = Vec::new();
    requests.try_reserve_exact(3).map_err(|_| invalid)?;
    for name in names {
        let file = release
            .files
            .iter()
            .find(|f| f.name == name)
            .ok_or(invalid)?;
        let mut url = file.url.clone();
        let pairs: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k != "download_token")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        url.query_pairs_mut().clear().extend_pairs(pairs);
        url.query_pairs_mut()
            .append_pair("download_token", &gate.download_token.0);
        if url.as_str().len() > 20 * 1024 {
            return Err(invalid);
        }
        requests.push(TokenRequest { url });
    }
    Ok(requests)
}

#[cfg(test)]
pub(super) fn test_request(request: &TokenRequest) -> &str {
    request.url.as_str()
}
