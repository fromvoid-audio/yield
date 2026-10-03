//! Keep token URLs out of ureq's URL/prelude debug logs. Only the validated stream sees them.
use std::io::{self, Write};
const MAX_LINE: usize = 20 * 1024 + 32;

pub(super) struct RequestLine {
    expected: String,
    actual: String,
}
impl RequestLine {
    pub(super) fn new(url: &url::Url) -> Result<(url::Url, Self), super::SelectionError> {
        let mut redacted = url.clone();
        let pairs = redacted
            .query_pairs()
            .filter(|(k, _)| k != "download_token")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect::<Vec<_>>();
        if pairs.is_empty() {
            redacted.set_query(None);
        } else {
            redacted.query_pairs_mut().clear().extend_pairs(pairs);
        }
        let line = |url: &url::Url| -> Result<String, super::SelectionError> {
            let target = &url[url::Position::BeforePath..url::Position::AfterQuery];
            let length = target
                .len()
                .checked_add(15)
                .filter(|n| *n <= MAX_LINE)
                .ok_or(super::SelectionError::Invalid)?;
            let mut line = String::new();
            line.try_reserve_exact(length)
                .map_err(|_| super::SelectionError::Invalid)?;
            line.push_str("GET ");
            line.push_str(target);
            line.push_str(" HTTP/1.1\r\n");
            Ok(line)
        };
        let expected = line(&redacted)?;
        let actual = line(url)?;
        Ok((redacted, Self { expected, actual }))
    }
}
pub(super) struct Writer {
    line: std::sync::Arc<RequestLine>,
    prefix: [u8; MAX_LINE],
    used: usize,
    complete: bool,
    failed: bool,
}
impl Writer {
    pub(super) fn new(line: std::sync::Arc<RequestLine>) -> Self {
        Self {
            line,
            prefix: [0; MAX_LINE],
            used: 0,
            complete: false,
            failed: false,
        }
    }
    pub(super) fn complete(&self) -> bool {
        self.complete && !self.failed
    }
    pub(super) fn write(&mut self, inner: &mut dyn Write, bytes: &[u8]) -> io::Result<usize> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "private HTTP request line mismatch",
            )
        };
        if self.failed {
            return Err(invalid());
        }
        if self.complete {
            return inner.write(bytes);
        }
        let count = bytes
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(bytes.len());
        let end = match self
            .used
            .checked_add(count)
            .filter(|n| *n <= self.line.expected.len())
        {
            Some(end) => end,
            None => {
                self.failed = true;
                return Err(invalid());
            }
        };
        if self.line.expected.as_bytes().get(self.used..end) != Some(&bytes[..count]) {
            self.failed = true;
            return Err(invalid());
        }
        self.prefix[self.used..end].copy_from_slice(&bytes[..count]);
        self.used = end;
        if self.used == self.line.expected.len() {
            if self.prefix[..self.used] != *self.line.expected.as_bytes() {
                self.failed = true;
                return Err(invalid());
            }
            if let Err(error) = inner.write_all(self.line.actual.as_bytes()) {
                self.failed = true;
                return Err(error);
            }
            self.complete = true;
            let written = inner.write(&bytes[count..])?;
            Ok(count + written)
        } else {
            Ok(count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn partial_line_rewrite_is_exact_once_and_agent_input_never_contains_gate_token() {
        let url = url::Url::parse(
            "https://portalsurfer.org/yield/a.zip?x=1&download_token=private%2B%2Ftoken",
        )
        .unwrap();
        let (redacted, line) = RequestLine::new(&url).unwrap();
        assert_eq!(
            redacted.as_str(),
            "https://portalsurfer.org/yield/a.zip?x=1"
        );
        assert!(!line.expected.contains("private") && !line.expected.contains("download_token"));
        let expected = line.expected.clone();
        let actual = line.actual.clone();
        let mut writer = Writer::new(std::sync::Arc::new(line));
        let mut output = Vec::new();
        for byte in expected.bytes() {
            assert_eq!(writer.write(&mut output, &[byte]).unwrap(), 1);
        }
        writer
            .write(&mut output, b"Host: portalsurfer.org\r\n\r\n")
            .unwrap();
        assert!(writer.complete());
        assert_eq!(
            output,
            [actual.as_bytes(), b"Host: portalsurfer.org\r\n\r\n"].concat()
        );
        let (_, line) = RequestLine::new(&url).unwrap();
        let mut writer = Writer::new(std::sync::Arc::new(line));
        let mut output = Vec::new();
        assert!(
            writer
                .write(&mut output, b"GET /other HTTP/1.1\r\n")
                .is_err()
        );
        assert!(output.is_empty());
    }
}
