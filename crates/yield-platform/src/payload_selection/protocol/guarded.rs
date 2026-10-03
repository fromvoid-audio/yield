//! Bound plaintext before ureq's allocating response/chunk parser accepts it.
use std::{
    fmt,
    io::{self, Read, Write},
    net::TcpStream,
    sync::Arc,
};
use ureq::{ReadWrite, TlsConnector};

#[derive(Clone)]
struct StagePolicy {
    ceiling: usize,
    traffic: Arc<super::staging::Traffic>,
    line: Option<Arc<super::wire::RequestLine>>,
}
pub(super) struct ValidatedTls(Arc<ureq::rustls::ClientConfig>, Option<StagePolicy>);
impl ValidatedTls {
    pub(super) fn new() -> Self {
        let config = ureq::rustls::ClientConfig::builder_with_provider(
            ureq::rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("ring protocol versions")
        .with_root_certificates(ureq::rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        })
        .with_no_client_auth();
        Self(Arc::new(config), None)
    }
    pub(super) fn staging(
        ceiling: usize,
        traffic: Arc<super::staging::Traffic>,
        line: Option<Arc<super::wire::RequestLine>>,
    ) -> Self {
        let mut tls = Self::new();
        tls.1 = Some(StagePolicy {
            ceiling,
            traffic,
            line,
        });
        tls
    }
}
impl TlsConnector for ValidatedTls {
    fn connect(
        &self,
        name: &str,
        stream: Box<dyn ReadWrite>,
    ) -> Result<Box<dyn ReadWrite>, ureq::Error> {
        let validated = self.0.connect(name, stream)?;
        Ok(Box::new(match &self.1 {
            Some(policy) => Guarded::staged(validated, policy.clone()),
            None => Guarded::new(validated),
        }))
    }
}
#[cfg(test)]
pub(super) struct LoopbackTls;
#[cfg(test)]
impl TlsConnector for LoopbackTls {
    fn connect(
        &self,
        _: &str,
        stream: Box<dyn ReadWrite>,
    ) -> Result<Box<dyn ReadWrite>, ureq::Error> {
        assert!(
            stream
                .socket()
                .unwrap()
                .peer_addr()
                .unwrap()
                .ip()
                .is_loopback()
        );
        // Deliberate private unit-test transport substitution, not a production TLS override.
        Ok(Box::new(Guarded::new(stream)))
    }
}
#[cfg(test)]
pub(super) struct LoopbackStagingTls(StagePolicy);
#[cfg(test)]
impl LoopbackStagingTls {
    pub(super) fn new(
        ceiling: usize,
        traffic: Arc<super::staging::Traffic>,
        line: Option<Arc<super::wire::RequestLine>>,
    ) -> Self {
        Self(StagePolicy {
            ceiling,
            traffic,
            line,
        })
    }
}
#[cfg(test)]
impl TlsConnector for LoopbackStagingTls {
    fn connect(
        &self,
        _: &str,
        stream: Box<dyn ReadWrite>,
    ) -> Result<Box<dyn ReadWrite>, ureq::Error> {
        assert!(
            stream
                .socket()
                .unwrap()
                .peer_addr()
                .unwrap()
                .ip()
                .is_loopback()
        );
        Ok(Box::new(Guarded::staged(stream, self.0.clone())))
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Headers,
    Length(usize),
    Eof,
    ChunkLine,
    ChunkData(usize),
    ChunkCr,
    ChunkLf,
    Trailers,
    Done,
}
struct Guarded {
    inner: Box<dyn ReadWrite>,
    phase: Phase,
    line: [u8; 8192],
    line_len: usize,
    metadata: usize,
    fields: usize,
    status: bool,
    length: Option<usize>,
    chunked: bool,
    encoding: bool,
    content_type: bool,
    body: usize,
    framing: usize,
    denied: bool,
    ceiling: usize,
    traffic: Option<Arc<super::staging::Traffic>>,
    writer: Option<super::wire::Writer>,
}
impl fmt::Debug for Guarded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BoundedPayloadHttp")
    }
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "payload HTTP limit or framing")
}
impl Guarded {
    fn new(inner: Box<dyn ReadWrite>) -> Self {
        Self {
            inner,
            phase: Phase::Headers,
            line: [0; 8192],
            line_len: 0,
            metadata: 0,
            fields: 0,
            status: false,
            length: None,
            chunked: false,
            encoding: false,
            content_type: false,
            body: 0,
            framing: 0,
            denied: false,
            ceiling: super::MAX_BODY,
            traffic: None,
            writer: None,
        }
    }
    fn staged(inner: Box<dyn ReadWrite>, policy: StagePolicy) -> Self {
        let mut guarded = Self::new(inner);
        guarded.ceiling = policy.ceiling;
        guarded.traffic = Some(policy.traffic);
        guarded.writer = policy.line.map(super::wire::Writer::new);
        guarded
    }
    fn consume(&mut self, b: u8) -> io::Result<()> {
        match self.phase {
            Phase::Headers | Phase::Trailers => {
                self.metadata += 1;
                if self.metadata > 32 * 1024 {
                    return Err(invalid());
                }
                if self.line_byte(b)? {
                    self.header_line()?;
                    self.line_len = 0;
                }
            }
            Phase::Length(n) => {
                self.body_byte()?;
                self.phase = if n == 1 {
                    Phase::Done
                } else {
                    Phase::Length(n - 1)
                };
            }
            Phase::Eof => self.body_byte()?,
            Phase::ChunkLine => {
                self.frame_byte()?;
                if self.line_byte(b)? {
                    let line = std::str::from_utf8(&self.line[..self.line_len - 2])
                        .map_err(|_| invalid())?;
                    let size = line.split(';').next().ok_or_else(invalid)?;
                    if size.is_empty()
                        || size.len() > 16
                        || !size.bytes().all(|c| c.is_ascii_hexdigit())
                    {
                        return Err(invalid());
                    }
                    let n = usize::from_str_radix(size, 16).map_err(|_| invalid())?;
                    if n > self.ceiling - self.body {
                        return Err(invalid());
                    }
                    self.phase = if n == 0 {
                        Phase::Trailers
                    } else {
                        Phase::ChunkData(n)
                    };
                    self.line_len = 0;
                }
            }
            Phase::ChunkData(n) => {
                self.body_byte()?;
                self.phase = if n == 1 {
                    Phase::ChunkCr
                } else {
                    Phase::ChunkData(n - 1)
                };
            }
            Phase::ChunkCr => {
                self.frame_byte()?;
                if b != b'\r' {
                    return Err(invalid());
                }
                self.phase = Phase::ChunkLf;
            }
            Phase::ChunkLf => {
                self.frame_byte()?;
                if b != b'\n' {
                    return Err(invalid());
                }
                self.phase = Phase::ChunkLine;
            }
            Phase::Done => return Err(invalid()),
        }
        Ok(())
    }
    fn body_byte(&mut self) -> io::Result<()> {
        self.body += 1;
        if self.body > self.ceiling {
            Err(invalid())
        } else {
            Ok(())
        }
    }
    fn frame_byte(&mut self) -> io::Result<()> {
        self.framing += 1;
        if self.framing > 16 * 1024 * 1024 {
            Err(invalid())
        } else {
            Ok(())
        }
    }
    fn line_byte(&mut self, b: u8) -> io::Result<bool> {
        if self.line_len == self.line.len() {
            return Err(invalid());
        }
        if self.line_len > 0 && self.line[self.line_len - 1] == b'\r' && b != b'\n' {
            return Err(invalid());
        }
        self.line[self.line_len] = b;
        self.line_len += 1;
        if b == b'\n' {
            if self.line_len < 2 || self.line[self.line_len - 2] != b'\r' {
                return Err(invalid());
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn header_line(&mut self) -> io::Result<()> {
        let line = &self.line[..self.line_len - 2];
        if !self.status {
            if !(line.starts_with(b"HTTP/1.1 200 ") || line.starts_with(b"HTTP/1.0 200 ")) {
                return Err(invalid());
            }
            self.status = true;
            return Ok(());
        }
        if line.is_empty() {
            if matches!(self.phase, Phase::Trailers) {
                self.phase = Phase::Done;
                return Ok(());
            }
            if self.length.is_some() && self.chunked {
                return Err(invalid());
            }
            self.phase = if self.chunked {
                Phase::ChunkLine
            } else {
                match self.length {
                    Some(0) => Phase::Done,
                    Some(n) => Phase::Length(n),
                    None => Phase::Eof,
                }
            };
            return Ok(());
        }
        self.fields += 1;
        if self.fields > 100 {
            return Err(invalid());
        }
        let split = line.iter().position(|c| *c == b':').ok_or_else(invalid)?;
        let name = &line[..split];
        if name.is_empty()
            || name
                .iter()
                .any(|b| !b.is_ascii_alphanumeric() && !b"!#$%&'*+-.^_`|~".contains(b))
        {
            return Err(invalid());
        }
        let value = std::str::from_utf8(&line[split + 1..])
            .map_err(|_| invalid())?
            .trim_matches([' ', '\t']);
        if value.bytes().any(|b| b < 32 && b != b'\t' || b == 127) {
            return Err(invalid());
        }
        if matches!(self.phase, Phase::Trailers) {
            if name.eq_ignore_ascii_case(b"content-length")
                || name.eq_ignore_ascii_case(b"transfer-encoding")
                || name.eq_ignore_ascii_case(b"content-encoding")
            {
                return Err(invalid());
            }
        } else if name.eq_ignore_ascii_case(b"content-length") {
            if self.length.is_some()
                || value.is_empty()
                || !value.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(invalid());
            }
            let n = value.parse::<usize>().map_err(|_| invalid())?;
            if n > self.ceiling {
                return Err(invalid());
            }
            self.length = Some(n);
        } else if name.eq_ignore_ascii_case(b"transfer-encoding") {
            if self.chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(invalid());
            }
            self.chunked = true;
        } else if name.eq_ignore_ascii_case(b"content-encoding") {
            if self.encoding || value != "identity" {
                return Err(invalid());
            }
            self.encoding = true;
        } else if name.eq_ignore_ascii_case(b"content-type") {
            if self.content_type {
                return Err(invalid());
            }
            self.content_type = true;
        }
        Ok(())
    }
}
impl Read for Guarded {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.writer.as_ref().is_some_and(|w| !w.complete()) {
            return Err(invalid());
        }
        if let Some(traffic) = &self.traffic {
            traffic
                .check()
                .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "payload retired"))?;
        }
        if self.denied {
            return Err(invalid());
        }
        if out.is_empty() {
            return Ok(0);
        }
        #[cfg(test)]
        if let Some(traffic) = &self.traffic {
            traffic
                .reading
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let result = self.inner.read(out);
        #[cfg(test)]
        if let Some(traffic) = &self.traffic {
            traffic
                .reading
                .store(false, std::sync::atomic::Ordering::Release);
        }
        let n = result?;
        if let Some(traffic) = &self.traffic {
            if traffic.consume_plaintext(n).is_err() {
                self.denied = true;
                return Err(invalid());
            }
            traffic
                .check()
                .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "payload retired"))?;
        }
        if n == 0 && !matches!(self.phase, Phase::Done | Phase::Eof) {
            self.denied = true;
            return Err(invalid());
        }
        let before = self.body;
        for &b in &out[..n] {
            if self.consume(b).is_err() {
                self.denied = true;
                break;
            }
        }
        if let Some(traffic) = &self.traffic {
            if traffic.consume_body(self.body - before).is_err() {
                self.denied = true;
                return Err(invalid());
            }
        }
        if self.denied {
            return Err(invalid());
        }
        Ok(n)
    }
}
impl Write for Guarded {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(traffic) = &self.traffic {
            traffic
                .check()
                .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "payload retired"))?;
        }
        match &mut self.writer {
            Some(writer) => writer.write(self.inner.as_mut(), bytes),
            None => self.inner.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.writer.as_ref().is_some_and(|w| !w.complete()) {
            return Ok(());
        }
        self.inner.flush()
    }
}
impl ReadWrite for Guarded {
    fn socket(&self) -> Option<&TcpStream> {
        self.inner.socket()
    }
}

#[cfg(test)]
mod tests;
