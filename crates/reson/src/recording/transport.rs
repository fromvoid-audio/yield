//! Bounded serializer byte plumbing; requests confer no journal or physical authority.
use super::{RecordingLimits, RecordingSettlements, RecordingSinkBinding};
use std::io::{self, Seek, SeekFrom, Write};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver, RecvTimeoutError, SyncSender},
};
use std::time::Duration;

/// Largest immutable write retained by this transport.
pub const RECORDING_TRANSPORT_MAX_WRITE_BYTES: usize = 16 * 1024;
/// Serializer acknowledgement deadline. Capture callbacks never wait here.
pub const RECORDING_TRANSPORT_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Factory for an original sink, bounded byte receiver and its genuine settlement route.
pub struct RecordingTransport;
impl RecordingTransport {
    /// Construct byte plumbing within exact validated format/frame limits; startup must
    /// preserve those limits. This grants no effect permission.
    /// The receiving owner must run before startup, which synchronously emits the initial
    /// WAV header. It must separately admit and retain each exact request before acknowledging it.
    pub fn bind(
        limits: RecordingLimits,
    ) -> (
        RecordingSinkBinding,
        RecordingTransportReceiver,
        RecordingSettlements,
    ) {
        let (tx, rx) = mpsc::sync_channel(1);
        let sink = Sink {
            tx,
            position: 0,
            limit: limits.max_bytes(),
            failed: false,
            timeout: RECORDING_TRANSPORT_ACK_TIMEOUT,
        };
        let (binding, settlements) = RecordingSinkBinding::bind_with_limits(Box::new(sink), limits);
        (binding, RecordingTransportReceiver { rx }, settlements)
    }
}

/// Single receiver of the original serializer's immutable byte requests; never cloneable.
pub struct RecordingTransportReceiver {
    rx: Receiver<RecordingTransportRequest>,
}
impl RecordingTransportReceiver {
    /// Receive on an owner thread, independently of synchronous producer Stop/join.
    pub fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<RecordingTransportRequest, RecvTimeoutError> {
        self.rx.recv_timeout(timeout)
    }
}

/// Exact byte operation requested by the original serializer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingTransportRequestKind {
    /// Retain/write the entire immutable batch at its observed offset.
    Write,
    /// Complete the separately admitted flush; byte acknowledgement alone is insufficient.
    Flush,
}

/// One immutable operation and its one-shot acknowledgement. Dropping it denies progress.
/// Observed offsets/bytes are not chunk grants, publication evidence or settlement.
pub struct RecordingTransportRequest {
    kind: RecordingTransportRequestKind,
    offset: u64,
    bytes: Box<[u8]>,
    reply: SyncSender<bool>,
    waiting: Arc<AtomicBool>,
}
impl RecordingTransportRequest {
    /// Requested operation, without authority to execute it.
    pub fn kind(&self) -> RecordingTransportRequestKind {
        self.kind
    }
    /// Exact serializer offset. Flush observes the current cursor without changing it.
    pub fn offset(&self) -> u64 {
        self.offset
    }
    /// Immutable exact write bytes; empty for flush, never larger than 16 KiB.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    /// Acknowledge this complete request only after the actual owner accepts its outcome.
    /// A timed-out request cannot restore the failed serializer. No settlement is inferred.
    pub fn acknowledge(self) -> io::Result<()> {
        self.respond(true)
    }
    /// Refuse this request; the serializer permanently fails without fallback I/O.
    pub fn deny(self) -> io::Result<()> {
        self.respond(false)
    }
    fn respond(self, accepted: bool) -> io::Result<()> {
        if !self.waiting.swap(false, Ordering::AcqRel) {
            return Err(io::Error::other(
                "recording transport request no longer waiting",
            ));
        }
        self.reply
            .try_send(accepted)
            .map_err(|_| io::Error::other("recording transport acknowledgement unavailable"))
    }
}

struct Sink {
    tx: SyncSender<RecordingTransportRequest>,
    position: u64,
    limit: u64,
    failed: bool,
    timeout: Duration,
}
impl Sink {
    fn check(&self) -> io::Result<()> {
        if self.failed {
            Err(io::Error::other(
                "recording transport permanently unconfirmed",
            ))
        } else {
            Ok(())
        }
    }
    fn request(&mut self, kind: RecordingTransportRequestKind, bytes: &[u8]) -> io::Result<()> {
        self.check()?;
        let (reply, rx) = mpsc::sync_channel(1);
        let waiting = Arc::new(AtomicBool::new(true));
        let request = RecordingTransportRequest {
            kind,
            offset: self.position,
            bytes: bytes.into(),
            reply,
            waiting: waiting.clone(),
        };
        // There is at most one original outstanding request: this serializer waits for its
        // complete acknowledgement before constructing another. Timeout never permits retry.
        let result = self
            .tx
            .try_send(request)
            .map_err(|_| io::Error::other("recording transport queue unavailable"))
            .and_then(|()| match rx.recv_timeout(self.timeout) {
                Ok(true) => Ok(()),
                Ok(false) => Err(io::Error::other("recording transport request denied")),
                Err(_) => Err(io::Error::other(
                    "recording transport acknowledgement unconfirmed",
                )),
            });
        waiting.store(false, Ordering::Release);
        if result.is_err() {
            self.failed = true;
        }
        result
    }
    fn invalid(&mut self) -> io::Error {
        self.failed = true;
        io::Error::other("recording transport cursor/allocation bound exceeded")
    }
}
impl Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.check()?;
        if bytes.is_empty() {
            return Ok(0);
        }
        let size = bytes.len().min(RECORDING_TRANSPORT_MAX_WRITE_BYTES);
        let Some(end) = self
            .position
            .checked_add(size as u64)
            .filter(|end| *end <= self.limit)
        else {
            return Err(self.invalid());
        };
        self.request(RecordingTransportRequestKind::Write, &bytes[..size])?;
        self.position = end;
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.request(RecordingTransportRequestKind::Flush, &[])
    }
}
impl Seek for Sink {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let candidate = match from {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(_) => return Err(self.invalid()),
        };
        let Some(offset) = u64::try_from(candidate)
            .ok()
            .filter(|offset| *offset <= self.limit)
        else {
            return Err(self.invalid());
        };
        self.position = offset;
        Ok(offset)
    }
}

#[cfg(test)]
mod tests;
