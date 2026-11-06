//! Bounded, read-only access to a separately run Carabiner Link bridge.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpStream};
use std::time::{Duration, Instant};

const PORT: u16 = 17_000;
const CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const IO_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_STATUS_LINE: usize = 4096;
const STATUS_QUERY: &[u8] = b"status\n";

/// Validated tempo and peer count from one Carabiner status message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkStatus {
    /// Number of other peers visible to the bridge.
    pub peers: u32,
    /// Finite, positive session tempo.
    pub tempo_bpm: f64,
}

/// Failure at the local Carabiner transport or framing boundary.
#[derive(Debug)]
pub enum LinkStatusError {
    /// A socket operation failed.
    Io(io::Error),
    /// The bridge closed its connection.
    Disconnected,
    /// A complete message could not be interpreted as Carabiner status.
    InvalidStatus,
    /// The message exceeded the maximum line length.
    OversizedStatus,
}

impl fmt::Display for LinkStatusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "Carabiner socket error: {error}"),
            Self::Disconnected => formatter.write_str("Carabiner disconnected"),
            Self::InvalidStatus => formatter.write_str("Invalid Carabiner status"),
            Self::OversizedStatus => {
                formatter.write_str("Carabiner status exceeded the line limit")
            }
        }
    }
}

impl std::error::Error for LinkStatusError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for LinkStatusError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A local Carabiner socket for use on a background worker.
pub struct CarabinerConnection {
    stream: TcpStream,
    pending: Vec<u8>,
}

impl CarabinerConnection {
    /// Connect to the default Carabiner endpoint on IPv4 loopback.
    pub fn connect() -> Result<Self, LinkStatusError> {
        Self::connect_to_loopback(PORT)
    }

    fn connect_to_loopback(port: u16) -> Result<Self, LinkStatusError> {
        let endpoint = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port));
        let stream = TcpStream::connect_timeout(&endpoint, CONNECT_TIMEOUT)?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Ok(Self {
            stream,
            pending: Vec::with_capacity(MAX_STATUS_LINE + 1),
        })
    }

    /// Request the current tempo and peer count without changing session state.
    pub fn request_status(&mut self) -> Result<(), LinkStatusError> {
        self.stream.write_all(STATUS_QUERY)?;
        Ok(())
    }

    /// Receive one status. A read deadline returns `None` and retains a partial line.
    ///
    /// Carabiner can send unsolicited updates. Continue polling on the same connection
    /// to drain them even between explicit requests.
    pub fn read_status(&mut self) -> Result<Option<LinkStatus>, LinkStatusError> {
        let deadline = Instant::now() + IO_TIMEOUT;
        let mut chunk = [0; 1024];
        loop {
            if let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
                let status = parse_status(&self.pending[..newline])?;
                self.pending.drain(..=newline);
                return Ok(Some(status));
            }
            let available = MAX_STATUS_LINE + 1 - self.pending.len();
            if available == 0 {
                return Err(LinkStatusError::OversizedStatus);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            self.stream.set_read_timeout(Some(remaining))?;
            let request = available.min(chunk.len());
            match self.stream.read(&mut chunk[..request]) {
                Ok(0) => return Err(LinkStatusError::Disconnected),
                Ok(read) => self.pending.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

fn parse_status(line: &[u8]) -> Result<LinkStatus, LinkStatusError> {
    let line = std::str::from_utf8(line).map_err(|_| LinkStatusError::InvalidStatus)?;
    let fields = line
        .trim()
        .strip_prefix("status ")
        .and_then(|line| line.trim().strip_prefix('{'))
        .and_then(|line| line.strip_suffix('}'))
        .ok_or(LinkStatusError::InvalidStatus)?;
    let mut peers = None;
    let mut tempo_bpm = None;
    let mut fields = fields.split_whitespace();
    while let Some(key) = fields.next() {
        let value = fields.next().ok_or(LinkStatusError::InvalidStatus)?;
        match key {
            ":peers" if peers.is_none() => {
                peers = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| LinkStatusError::InvalidStatus)?,
                );
            }
            ":bpm" if tempo_bpm.is_none() => {
                tempo_bpm = Some(
                    value
                        .parse::<f64>()
                        .ok()
                        .filter(|tempo| tempo.is_finite() && *tempo > 0.0)
                        .ok_or(LinkStatusError::InvalidStatus)?,
                );
            }
            ":start" if value.parse::<i64>().is_ok() => {}
            ":beat" if value.parse::<f64>().is_ok_and(f64::is_finite) => {}
            ":playing" if matches!(value, "true" | "false") => {}
            _ => return Err(LinkStatusError::InvalidStatus),
        }
    }
    Ok(LinkStatus {
        peers: peers.ok_or(LinkStatusError::InvalidStatus)?,
        tempo_bpm: tempo_bpm.ok_or(LinkStatusError::InvalidStatus)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn status_parser_rejects_invalid_tempo_peer_count_and_framing() {
        for invalid in [
            "status { :peers 0 :bpm NaN }",
            "status { :peers 0 :bpm 0 }",
            "status { :peers 0 :bpm 1e999 }",
            "status { :peers -1 :bpm 120 }",
            "status { :peers 4294967296 :bpm 120 }",
            "status { :peers 0 :peers 1 :bpm 120 }",
            "status { :peers 0 :bpm 120 :bpm 130 }",
            "status { :bpm 120 }",
            "status { :peers 0 }",
            "status { :peers 0 :bpm 120",
            "error invalid command",
        ] {
            assert!(
                matches!(
                    parse_status(invalid.as_bytes()),
                    Err(LinkStatusError::InvalidStatus)
                ),
                "{invalid}"
            );
        }
    }

    #[test]
    fn local_socket_preserves_partial_and_queued_status_across_reads() {
        let (mut client, mut bridge) = local_bridge();
        client.request_status().unwrap();
        let mut request = [0; STATUS_QUERY.len()];
        bridge.read_exact(&mut request).unwrap();
        assert_eq!(&request, b"status\n");

        bridge.write_all(b"status { :peers 0 :bp").unwrap();
        assert_eq!(client.read_status().unwrap(), None);
        bridge
            .write_all(b"m 120.0 :start 1000 :beat -2.5 }\nstatus { :peers 2 :bpm 133.25 :playing true }\n")
            .unwrap();
        assert_eq!(
            client.read_status().unwrap(),
            Some(LinkStatus {
                peers: 0,
                tempo_bpm: 120.0
            })
        );
        assert_eq!(
            client.read_status().unwrap(),
            Some(LinkStatus {
                peers: 2,
                tempo_bpm: 133.25
            })
        );
        drop(bridge);
        assert!(matches!(
            client.read_status(),
            Err(LinkStatusError::Disconnected)
        ));
    }

    #[test]
    fn local_socket_rejects_an_oversized_unterminated_line() {
        let (mut client, mut bridge) = local_bridge();
        bridge.write_all(&[b'x'; MAX_STATUS_LINE + 1]).unwrap();
        assert!(matches!(
            client.read_status(),
            Err(LinkStatusError::OversizedStatus)
        ));
    }

    fn local_bridge() -> (CarabinerConnection, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client =
            CarabinerConnection::connect_to_loopback(listener.local_addr().unwrap().port())
                .unwrap();
        let (bridge, _) = listener.accept().unwrap();
        bridge.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        bridge.set_write_timeout(Some(IO_TIMEOUT)).unwrap();
        (client, bridge)
    }
}
