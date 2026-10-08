//! Exact canonical base64 transport without owning a binary or base64 buffer.
use super::*;
use std::{
    fmt,
    io::{self, Write},
};

struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or(io::ErrorKind::OutOfMemory)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(super) fn serialize<S: serde::Serializer>(
    data: &Data,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let mut count = Counter(16);
    serde_json::to_writer(&mut count, data).map_err(serde::ser::Error::custom)?;
    if count.0 > MAX_LEGACY_ADOPTION_INPUT_BYTES {
        return Err(serde::ser::Error::custom(LegacyAdoptionInputError::Wire));
    }
    serializer.collect_str(&Transport(data))
}
struct Transport<'a>(&'a Data);
impl fmt::Display for Transport<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut writer = Base64 {
            formatter,
            pending: [0; 3],
            used: 0,
        };
        writer.write_all(MAGIC).map_err(|_| fmt::Error)?;
        writer
            .write_all(&LEGACY_ADOPTION_INPUT_VERSION.to_le_bytes())
            .map_err(|_| fmt::Error)?;
        serde_json::to_writer(&mut writer, self.0).map_err(|_| fmt::Error)?;
        writer.finish().map_err(|_| fmt::Error)
    }
}
struct Base64<'a, 'b> {
    formatter: &'a mut fmt::Formatter<'b>,
    pending: [u8; 3],
    used: usize,
}
impl Base64<'_, '_> {
    fn emit(&mut self) -> io::Result<()> {
        const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let [a, b, c] = self.pending;
        let encoded = [
            ABC[(a >> 2) as usize],
            ABC[(((a & 3) << 4) | (b >> 4)) as usize],
            if self.used > 1 {
                ABC[(((b & 15) << 2) | (c >> 6)) as usize]
            } else {
                b'='
            },
            if self.used > 2 {
                ABC[(c & 63) as usize]
            } else {
                b'='
            },
        ];
        self.formatter
            .write_str(std::str::from_utf8(&encoded).map_err(|_| io::ErrorKind::InvalidData)?)
            .map_err(|_| io::ErrorKind::WriteZero)?;
        self.pending = [0; 3];
        self.used = 0;
        Ok(())
    }
    fn finish(&mut self) -> io::Result<()> {
        if self.used != 0 {
            self.emit()?;
        }
        Ok(())
    }
}
impl Write for Base64<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &byte in bytes {
            self.pending[self.used] = byte;
            self.used += 1;
            if self.used == 3 {
                self.emit()?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
