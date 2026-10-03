//! Private five-request transport, actual transfer/capacity/deadline accounting and token secrecy.
use super::{SelectionError, TokenRequest, Transport, guarded, wire};
use crate::payload_selection::{Lease, catalog::File};
use std::{
    io::Read,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
pub(in crate::payload_selection) const BODY_LIMIT: u64 =
    1024 * 1024 * 1024 + 2 * 1024 * 1024 + 8192;
pub(in crate::payload_selection) const PLAINTEXT_LIMIT: u64 =
    BODY_LIMIT + 5 * (16 * 1024 * 1024 + 32 * 1024);
// Conservative phase credit, not an allocation: one non-pooled TLS/HTTP parser at a time,
// 64 KiB rustls handshake/send buffers, 16 KiB received plaintext, 32 KiB bounded headers,
// 20 KiB request rewrite, 64 KiB read chunk, and bounded catalog/selection/observation work.
// The verifier applies the same credit after all HTTP agents/readers have been dropped.
pub(in crate::payload_selection) const OWNER_WORKING_CREDIT: usize = 8 * 1024 * 1024;

pub(in crate::payload_selection) struct Traffic {
    lease: Arc<Lease>,
    deadline: Instant,
    pub(in crate::payload_selection) body: AtomicU64,
    pub(in crate::payload_selection) plaintext: AtomicU64,
    pub(in crate::payload_selection) requests: AtomicU8,
    #[cfg(test)]
    pub(in crate::payload_selection) reading: std::sync::atomic::AtomicBool,
}
impl Traffic {
    pub(in crate::payload_selection) fn new(lease: Arc<Lease>, deadline: Instant) -> Arc<Self> {
        Arc::new(Self {
            lease,
            deadline,
            body: AtomicU64::new(0),
            plaintext: AtomicU64::new(0),
            requests: AtomicU8::new(0),
            #[cfg(test)]
            reading: std::sync::atomic::AtomicBool::new(false),
        })
    }
    pub(in crate::payload_selection) fn check(&self) -> Result<(), SelectionError> {
        if self.lease.live() && Instant::now() < self.deadline {
            Ok(())
        } else {
            Err(SelectionError::Retired)
        }
    }
    pub(in crate::payload_selection) fn consume_body(
        &self,
        n: usize,
    ) -> Result<(), SelectionError> {
        Self::charge(&self.body, n, BODY_LIMIT)
    }
    pub(in crate::payload_selection) fn consume_plaintext(
        &self,
        n: usize,
    ) -> Result<(), SelectionError> {
        Self::charge(&self.plaintext, n, PLAINTEXT_LIMIT)
    }
    fn charge(counter: &AtomicU64, n: usize, limit: u64) -> Result<(), SelectionError> {
        let n = u64::try_from(n).map_err(|_| SelectionError::Invalid)?;
        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                old.checked_add(n).filter(|v| *v <= limit)
            })
            .map(|_| ())
            .map_err(|_| SelectionError::Invalid)
    }
    fn request(&self) -> Result<(), SelectionError> {
        self.check()?;
        self.requests
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                n.checked_add(1).filter(|v| *v <= 5)
            })
            .map(|_| ())
            .map_err(|_| SelectionError::Invalid)
    }
}
#[derive(Clone, Copy)]
enum Body {
    Metadata,
    Asset { size: usize },
}
impl Body {
    fn ceiling(&self) -> usize {
        match self {
            Self::Metadata => 512 * 1024,
            Self::Asset { size, .. } => *size,
        }
    }
    fn mime(&self, mime: Option<&str>) -> bool {
        // Asset authenticity is the exact selected hash/size and mandatory signed inventory,
        // not a server's MIME spelling (which differs for .sig among actual download stacks).
        match self {
            Self::Metadata => mime == Some("application/json"),
            Self::Asset { .. } => true,
        }
    }
}
pub(in crate::payload_selection) fn metadata(
    transport: &Transport,
    url: &str,
    traffic: Arc<Traffic>,
) -> Result<Vec<u8>, SelectionError> {
    fetch(transport, url, None, Body::Metadata, traffic)
}
pub(in crate::payload_selection) fn asset(
    transport: &Transport,
    request: &TokenRequest,
    file: &File,
    traffic: Arc<Traffic>,
) -> Result<Vec<u8>, SelectionError> {
    let invalid = SelectionError::Invalid;
    if request.name() != file.name || file.size_bytes == 0 {
        return Err(invalid);
    }
    let ceiling = if file.name.ends_with(".zip") {
        1024 * 1024 * 1024
    } else if file.name.ends_with(".sig") {
        8192
    } else {
        1024 * 1024
    };
    let size = usize::try_from(file.size_bytes)
        .ok()
        .filter(|n| *n <= ceiling)
        .ok_or(invalid)?;
    let (redacted, line) = wire::RequestLine::new(&request.url)?;
    fetch(
        transport,
        redacted.as_str(),
        Some(Arc::new(line)),
        Body::Asset { size },
        traffic,
    )
}
fn fetch(
    transport: &Transport,
    original: &str,
    line: Option<Arc<wire::RequestLine>>,
    body: Body,
    traffic: Arc<Traffic>,
) -> Result<Vec<u8>, SelectionError> {
    let invalid = SelectionError::Invalid;
    traffic.request()?;
    let original_url = url::Url::parse(original).map_err(|_| invalid)?;
    if original_url.origin().ascii_serialization() != super::ORIGIN
        || !original_url.username().is_empty()
        || original_url.password().is_some()
        || original_url.fragment().is_some()
    {
        return Err(invalid);
    }
    let endpoint = match transport {
        Transport::Production => original.to_owned(),
        #[cfg(test)]
        Transport::Loopback(address) => {
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
    let remaining = traffic
        .deadline
        .checked_duration_since(Instant::now())
        .ok_or(SelectionError::Retired)?;
    let builder = ureq::AgentBuilder::new()
        .redirects(0)
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .timeout(remaining)
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(10))
        .timeout_write(Duration::from_secs(10));
    let agent = match transport {
        Transport::Production => builder
            .tls_connector(Arc::new(guarded::ValidatedTls::staging(
                body.ceiling(),
                traffic.clone(),
                line,
            )))
            .build(),
        #[cfg(test)]
        Transport::Loopback(_) => builder
            .tls_connector(Arc::new(guarded::LoopbackStagingTls::new(
                body.ceiling(),
                traffic.clone(),
                line,
            )))
            .build(),
    };
    traffic.check()?;
    let result = agent
        .get(&endpoint)
        .set("Accept", "*/*")
        .set("Accept-Encoding", "identity")
        .set("Connection", "close")
        .set("User-Agent", "yield-payload-staging")
        .call();
    traffic.check()?;
    let response = match result {
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
    let mime = response
        .header("Content-Type")
        .and_then(|s| s.split(';').next());
    if !body.mime(mime)
        || response
            .header("Content-Encoding")
            .is_some_and(|s| s != "identity")
        || length.is_some_and(|n| n > body.ceiling())
        || (length.is_some() && response.header("Transfer-Encoding").is_some())
    {
        return Err(invalid);
    }
    if let Body::Asset { size, .. } = body {
        if length.is_some_and(|n| n != size) {
            return Err(invalid);
        }
    }
    let mut bytes = Vec::new();
    if let Body::Asset { size, .. } = body {
        traffic.check()?;
        bytes.try_reserve_exact(size).map_err(|_| invalid)?;
        if bytes.capacity() > size {
            return Err(invalid);
        }
    }
    let mut reader = response.into_reader();
    let mut chunk = [0; 64 * 1024];
    loop {
        traffic.check()?;
        let result = reader.read(&mut chunk);
        traffic.check()?;
        let n = result.map_err(|_| invalid)?;
        if n == 0 {
            if length.is_some_and(|n| n != bytes.len())
                || matches!(body,Body::Asset{size,..} if size!=bytes.len())
            {
                return Err(invalid);
            }
            return Ok(bytes);
        }
        let new = bytes
            .len()
            .checked_add(n)
            .filter(|n| *n <= body.ceiling())
            .ok_or(invalid)?;
        traffic.check()?;
        if new > bytes.capacity() {
            bytes
                .try_reserve_exact(new - bytes.len())
                .map_err(|_| invalid)?;
        }
        if bytes.capacity() > body.ceiling() {
            return Err(invalid);
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
}
