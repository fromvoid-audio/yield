use super::*;
use std::io::Cursor;

#[derive(Debug)]
struct Bytes(Cursor<Vec<u8>>);
impl Read for Bytes {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.0.read(out)
    }
}
impl Write for Bytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl ReadWrite for Bytes {
    fn socket(&self) -> Option<&TcpStream> {
        None
    }
}
fn run(bytes: Vec<u8>) -> io::Result<u64> {
    let mut reader = Guarded::new(Box::new(Bytes(Cursor::new(bytes))));
    io::copy(&mut reader, &mut io::sink())
}
fn prefix(headers: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{headers}\r\n").into_bytes()
}

#[test]
fn complete_length_eof_and_chunked_trailer_responses_pass_before_allocating_parser() {
    for input in [b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        b"HTTP/1.0 200 OK\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2;note=ok\r\n{}\r\n0\r\nX-End: yes\r\n\r\n".to_vec()] {
        assert!(run(input).is_ok());
    }
}

#[test]
fn metadata_line_aggregate_field_and_unknown_body_limits_are_enforced_before_forwarding() {
    let oversized_line = prefix(&format!("X-Large: {}\r\n", "x".repeat(8192)));
    let aggregate = prefix(&format!(
        "{}Content-Length: 0\r\n",
        format!("X-Large: {}\r\n", "x".repeat(7000)).repeat(5)
    ));
    let fields = prefix(&format!(
        "{}Content-Length: 0\r\n",
        "X-Small: x\r\n".repeat(100)
    ));
    let mut body = prefix("");
    body.extend(std::iter::repeat_n(b'x', super::super::MAX_BODY + 1));
    for input in [oversized_line, aggregate, fields, body] {
        assert!(run(input).is_err());
    }
}

#[test]
fn actual_chunk_extensions_and_combined_trailers_cannot_bypass_metadata_framing_budgets() {
    let mut extension = prefix("Transfer-Encoding: chunked\r\n");
    extension.extend_from_slice(format!("1;{}\r\nx\r\n0\r\n\r\n", "x".repeat(8192)).as_bytes());
    assert!(run(extension).is_err());
    let mut frames = prefix("Transfer-Encoding: chunked\r\n");
    let frame = format!("1;{}\r\nx\r\n", "x".repeat(64));
    // Actual framing exceeds 16MiB while decoded body remains below 512KiB.
    for _ in 0..(16 * 1024 * 1024 / (frame.len() - 1) + 1) {
        frames.extend_from_slice(frame.as_bytes());
    }
    frames.extend_from_slice(b"0\r\n\r\n");
    assert!(run(frames).is_err());
    let mut trailers = prefix(&format!(
        "{}Transfer-Encoding: chunked\r\n",
        "X-Head: x\r\n".repeat(60)
    ));
    trailers.extend_from_slice(format!("0\r\n{}\r\n", "X-Tail: x\r\n".repeat(40)).as_bytes());
    assert!(run(trailers).is_err());
}

#[test]
fn incomplete_conflicting_framing_encoding_controls_and_extra_response_bytes_fail_closed() {
    for input in [
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nTransfer-Encoding: chunked\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\n\r\n{}".to_vec(),
        b"HTTP/1.1 200 OK\n\n{}".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n".to_vec(),
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\nContent-Length: 0\r\n\r\n"
            .to_vec(),
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}extra".to_vec(),
        b"HTTP/1.1 301 Redirect\r\nLocation: https://evil.example\r\n\r\n".to_vec(),
    ] {
        assert!(run(input).is_err());
    }
}
