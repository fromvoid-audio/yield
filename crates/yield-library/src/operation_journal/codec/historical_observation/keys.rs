//! Fixed scalar offsets into already validated JSON; decoded-key equality owns no strings.
use std::cmp::Ordering;

#[derive(Clone, Copy)]
pub(super) struct Key {
    pub(super) object: u32,
    pub(super) start: u32,
    pub(super) end: u32,
}
impl Key {
    fn bytes<'a>(&self, bytes: &'a [u8]) -> Decoded<'a> {
        Decoded {
            raw: &bytes[self.start as usize..self.end as usize],
            pending: [0; 4],
            used: 0,
            count: 0,
        }
    }
}
pub(super) fn compare(left: &Key, right: &Key, bytes: &[u8]) -> Ordering {
    left.object
        .cmp(&right.object)
        .then_with(|| left.bytes(bytes).cmp(right.bytes(bytes)))
}
pub(super) fn matches(key: &Key, bytes: &[u8], name: &[u8]) -> bool {
    key.bytes(bytes).eq(name.iter().copied())
}
struct Decoded<'a> {
    raw: &'a [u8],
    pending: [u8; 4],
    used: usize,
    count: usize,
}
impl Iterator for Decoded<'_> {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        if self.used < self.count {
            let byte = self.pending[self.used];
            self.used += 1;
            return Some(byte);
        }
        let (&byte, tail) = self.raw.split_first()?;
        self.raw = tail;
        if byte != b'\\' {
            return Some(byte);
        }
        let (&escaped, tail) = self.raw.split_first()?;
        self.raw = tail;
        Some(match escaped {
            b'b' => 8,
            b'f' => 12,
            b'n' => 10,
            b'r' => 13,
            b't' => 9,
            b'u' => {
                let mut scalar = self.hex();
                if (0xd800..=0xdbff).contains(&scalar) {
                    self.raw = &self.raw[2..];
                    scalar = 0x10000 + ((scalar - 0xd800) << 10) + (self.hex() - 0xdc00);
                }
                let text = char::from_u32(scalar)
                    .expect("validated JSON scalar")
                    .encode_utf8(&mut self.pending);
                self.count = text.len();
                self.used = 1;
                self.pending[0]
            }
            other => other,
        })
    }
}
impl Decoded<'_> {
    fn hex(&mut self) -> u32 {
        let value = self.raw[..4].iter().fold(0, |value, byte| {
            value * 16
                + match byte {
                    b'0'..=b'9' => (byte - b'0') as u32,
                    b'a'..=b'f' => (byte - b'a' + 10) as u32,
                    _ => (byte - b'A' + 10) as u32,
                }
        });
        self.raw = &self.raw[4..];
        value
    }
}
