//! Checked raw ZIP/ZIP64 index and local/member ranges before allocating an index or inflater.
use super::{Check, Ledger, SelectionError};
use std::ops::Range;
const INVALID: SelectionError = SelectionError::Invalid;
const META: usize = 16 * 1024 * 1024;

pub(super) struct Entry<'a> {
    pub(super) name: &'a str,
    pub(super) directory: bool,
    pub(super) method: u16,
    pub(super) crc: u32,
    pub(super) expanded: u64,
    pub(super) data: Range<usize>,
    local: Range<usize>,
}
fn range(bytes: &[u8], offset: usize, length: usize) -> Result<&[u8], SelectionError> {
    bytes
        .get(offset..offset.checked_add(length).ok_or(INVALID)?)
        .ok_or(INVALID)
}
fn u16at(bytes: &[u8], offset: usize) -> Result<u16, SelectionError> {
    Ok(u16::from_le_bytes(
        range(bytes, offset, 2)?.try_into().map_err(|_| INVALID)?,
    ))
}
fn u32at(bytes: &[u8], offset: usize) -> Result<u32, SelectionError> {
    Ok(u32::from_le_bytes(
        range(bytes, offset, 4)?.try_into().map_err(|_| INVALID)?,
    ))
}
fn u64at(bytes: &[u8], offset: usize) -> Result<u64, SelectionError> {
    Ok(u64::from_le_bytes(
        range(bytes, offset, 8)?.try_into().map_err(|_| INVALID)?,
    ))
}
fn usize64(n: u64) -> Result<usize, SelectionError> {
    usize::try_from(n).map_err(|_| INVALID)
}
struct Directory {
    start: usize,
    end: usize,
    count: usize,
    metadata: usize,
}
fn directory(bytes: &[u8], check: &Check<'_>) -> Result<Directory, SelectionError> {
    let low = bytes.len().saturating_sub(65535 + 22);
    let mut end = None;
    for offset in (low..bytes.len().saturating_sub(21)).rev() {
        if offset % 65536 == 0 {
            check.live()?;
        }
        if u32at(bytes, offset)? == 0x06054b50
            && offset.checked_add(22 + u16at(bytes, offset + 20)? as usize) == Some(bytes.len())
        {
            end = Some(offset);
            break;
        }
    }
    let end = end.ok_or(INVALID)?;
    if u16at(bytes, end + 4)? != 0
        || u16at(bytes, end + 6)? != 0
        || u16at(bytes, end + 8)? != u16at(bytes, end + 10)?
    {
        return Err(INVALID);
    }
    let count = u16at(bytes, end + 10)?;
    let size = u32at(bytes, end + 12)?;
    let offset = u32at(bytes, end + 16)?;
    let (count, size, offset, tail) = if count == u16::MAX || size == u32::MAX || offset == u32::MAX
    {
        let locator = end.checked_sub(20).ok_or(INVALID)?;
        if u32at(bytes, locator)? != 0x07064b50
            || u32at(bytes, locator + 4)? != 0
            || u32at(bytes, locator + 16)? != 1
        {
            return Err(INVALID);
        }
        let record = usize64(u64at(bytes, locator + 8)?)?;
        if u32at(bytes, record)? != 0x06064b50 {
            return Err(INVALID);
        }
        let record_size = usize64(u64at(bytes, record + 4)?)?;
        if !(44..=META).contains(&record_size)
            || u16at(bytes, record + 14)? != 45
            || record
                .checked_add(12)
                .and_then(|n| n.checked_add(record_size))
                != Some(locator)
            || u32at(bytes, record + 16)? != 0
            || u32at(bytes, record + 20)? != 0
            || u64at(bytes, record + 24)? != u64at(bytes, record + 32)?
        {
            return Err(INVALID);
        }
        let count64 = u64at(bytes, record + 32)?;
        let size64 = u64at(bytes, record + 40)?;
        let offset64 = u64at(bytes, record + 48)?;
        if (count != u16::MAX && u64::from(count) != count64)
            || (size != u32::MAX && u64::from(size) != size64)
            || (offset != u32::MAX && u64::from(offset) != offset64)
        {
            return Err(INVALID);
        }
        (
            usize64(count64)?,
            usize64(size64)?,
            usize64(offset64)?,
            record,
        )
    } else {
        (usize::from(count), size as usize, offset as usize, end)
    };
    if count == 0
        || count > 10000
        || count.checked_mul(47).is_none_or(|n| n > size)
        || offset.checked_add(size) != Some(tail)
    {
        return Err(INVALID);
    }
    let metadata = size
        .checked_add(bytes.len().checked_sub(tail).ok_or(INVALID)?)
        .filter(|n| *n <= META)
        .ok_or(INVALID)?;
    range(bytes, offset, size)?;
    Ok(Directory {
        start: offset,
        end: tail,
        count,
        metadata,
    })
}

struct Extras<'a> {
    zip64: Option<&'a [u8]>,
}
fn extras<'a>(bytes: &'a [u8], name: &[u8]) -> Result<Extras<'a>, SelectionError> {
    let mut offset = 0;
    let mut zip64 = None;
    let mut unicode = false;
    while offset < bytes.len() {
        let tag = u16at(bytes, offset)?;
        let length = u16at(bytes, offset + 2)? as usize;
        let data = range(bytes, offset + 4, length)?;
        match tag {
            1 => {
                if zip64.replace(data).is_some() {
                    return Err(INVALID);
                }
            }
            0x7075 => {
                if unicode
                    || data.len() < 5
                    || data[0] != 1
                    || u32at(data, 1)? != crc32fast::hash(name)
                    || &data[5..] != name
                {
                    return Err(INVALID);
                }
                unicode = true;
            }
            0x9901 => return Err(INVALID),
            _ => {}
        }
        offset = offset.checked_add(4 + length).ok_or(INVALID)?;
    }
    Ok(Extras { zip64 })
}
fn take64(extra: &mut &[u8]) -> Result<u64, SelectionError> {
    let n = u64at(extra, 0)?;
    *extra = extra.get(8..).ok_or(INVALID)?;
    Ok(n)
}
fn central64(
    expanded: u32,
    compressed: u32,
    local: u32,
    disk: u16,
    extra: Option<&[u8]>,
) -> Result<(u64, u64, usize), SelectionError> {
    let required =
        expanded == u32::MAX || compressed == u32::MAX || local == u32::MAX || disk == u16::MAX;
    let mut extra = extra.unwrap_or(&[]);
    if required && extra.is_empty() {
        return Err(INVALID);
    }
    let expanded = if expanded == u32::MAX {
        take64(&mut extra)?
    } else {
        u64::from(expanded)
    };
    let compressed = if compressed == u32::MAX {
        take64(&mut extra)?
    } else {
        u64::from(compressed)
    };
    let local = if local == u32::MAX {
        take64(&mut extra)?
    } else {
        u64::from(local)
    };
    let disk = if disk == u16::MAX {
        let n = u32at(extra, 0)?;
        extra = extra.get(4..).ok_or(INVALID)?;
        n
    } else {
        u32::from(disk)
    };
    if disk != 0 || !extra.is_empty() {
        return Err(INVALID);
    }
    Ok((expanded, compressed, usize64(local)?))
}

pub(super) fn index<'a>(
    bytes: &'a [u8],
    check: &Check<'_>,
    ledger: &mut Ledger,
) -> Result<Vec<Entry<'a>>, SelectionError> {
    let directory = directory(bytes, check)?;
    let capacity = directory
        .count
        .checked_mul(std::mem::size_of::<Entry<'_>>())
        .ok_or(INVALID)?;
    ledger.charge(capacity)?;
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(directory.count)
        .map_err(|_| INVALID)?;
    ledger.charge(
        entries
            .capacity()
            .checked_mul(std::mem::size_of::<Entry<'_>>())
            .ok_or(INVALID)?
            .checked_sub(capacity)
            .ok_or(INVALID)?,
    )?;
    let mut cursor = directory.start;
    let mut metadata = directory.metadata;
    for _ in 0..directory.count {
        check.live()?;
        let head = range(bytes, cursor, 46)?;
        if u32at(head, 0)? != 0x02014b50 {
            return Err(INVALID);
        }
        let flags = u16at(head, 8)?;
        let method = u16at(head, 10)?;
        let needed = u16at(head, 6)?;
        if !(10..=45).contains(&needed)
            || (method == 8 && needed < 20)
            || flags & !0x080e != 0
            || !matches!(method, 0 | 8)
            || (method == 0 && flags & 6 != 0)
        {
            return Err(INVALID);
        }
        let name_len = u16at(head, 28)? as usize;
        let extra_len = u16at(head, 30)? as usize;
        let comment_len = u16at(head, 32)? as usize;
        if name_len == 0 || name_len > 1024 {
            return Err(INVALID);
        }
        let name_bytes = range(bytes, cursor + 46, name_len)?;
        let name = std::str::from_utf8(name_bytes).map_err(|_| INVALID)?;
        let extra = extras(range(bytes, cursor + 46 + name_len, extra_len)?, name_bytes)?;
        if (u32at(head, 24)? == u32::MAX
            || u32at(head, 20)? == u32::MAX
            || u32at(head, 42)? == u32::MAX)
            && needed < 45
        {
            return Err(INVALID);
        }
        let (expanded, compressed, local) = central64(
            u32at(head, 24)?,
            u32at(head, 20)?,
            u32at(head, 42)?,
            u16at(head, 34)?,
            extra.zip64,
        )?;
        if expanded > 512 * 1024 * 1024
            || compressed > 1024 * 1024 * 1024
            || (expanded > 0
                && (compressed == 0 || expanded > compressed.checked_mul(200).ok_or(INVALID)?))
            || (method == 0 && expanded != compressed)
        {
            return Err(INVALID);
        }
        let directory_entry = name.ends_with('/');
        let attributes = u32at(head, 38)?;
        let kind = (attributes >> 16) & 0xf000;
        if !matches!(kind, 0 | 0x4000 | 0x8000)
            || (kind == 0x4000 && !directory_entry)
            || (kind == 0x8000 && directory_entry)
            || (attributes & 0x10 != 0 && !directory_entry)
            || (directory_entry && (expanded != 0 || u32at(head, 16)? != 0))
        {
            return Err(INVALID);
        }
        let (data, local_end, local_metadata) = local_member(
            bytes,
            local,
            directory.start,
            name_bytes,
            head,
            expanded,
            compressed,
        )?;
        metadata = metadata
            .checked_add(local_metadata)
            .filter(|n| *n <= META)
            .ok_or(INVALID)?;
        entries.push(Entry {
            name,
            directory: directory_entry,
            method,
            crc: u32at(head, 16)?,
            expanded,
            data,
            local: local..local_end,
        });
        cursor = cursor
            .checked_add(46 + name_len + extra_len + comment_len)
            .filter(|n| *n <= directory.end)
            .ok_or(INVALID)?;
    }
    if cursor != directory.end {
        return Err(INVALID);
    }
    entries.sort_unstable_by_key(|e| e.local.start);
    let mut next = 0;
    for entry in &entries {
        check.live()?;
        // Every byte of the local-member area is accounted for once. Hidden/unindexed locals,
        // overlapping ranges, trailing data and self-extracting prefixes cannot evade validation.
        if entry.local.start != next {
            return Err(INVALID);
        }
        next = entry.local.end;
    }
    if next != directory.start {
        return Err(INVALID);
    }
    Ok(entries)
}

fn local_member(
    bytes: &[u8],
    local: usize,
    central: usize,
    name: &[u8],
    central_head: &[u8],
    expanded: u64,
    compressed: u64,
) -> Result<(Range<usize>, usize, usize), SelectionError> {
    let flags = u16at(central_head, 8)?;
    let method = u16at(central_head, 10)?;
    let crc = u32at(central_head, 16)?;
    let head = range(bytes, local, 30)?;
    if u32at(head, 0)? != 0x04034b50
        || u16at(head, 4)? != u16at(central_head, 6)?
        || u16at(head, 6)? != flags
        || u16at(head, 8)? != method
        || u16at(head, 10)? != u16at(central_head, 12)?
        || u16at(head, 12)? != u16at(central_head, 14)?
    {
        return Err(INVALID);
    }
    let name_len = u16at(head, 26)? as usize;
    let extra_len = u16at(head, 28)? as usize;
    if range(bytes, local + 30, name_len)? != name {
        return Err(INVALID);
    }
    let extra = extras(range(bytes, local + 30 + name_len, extra_len)?, name)?;
    let raw_expanded = u32at(head, 22)?;
    let raw_compressed = u32at(head, 18)?;
    let zip64 = raw_expanded == u32::MAX || raw_compressed == u32::MAX;
    if zip64 && u16at(head, 4)? < 45 {
        return Err(INVALID);
    }
    let mut extra = extra.zip64.unwrap_or(&[]);
    let local_expanded = if raw_expanded == u32::MAX {
        take64(&mut extra)?
    } else {
        u64::from(raw_expanded)
    };
    let local_compressed = if raw_compressed == u32::MAX {
        take64(&mut extra)?
    } else {
        u64::from(raw_compressed)
    };
    if !extra.is_empty() {
        return Err(INVALID);
    }
    let descriptor = flags & 8 != 0;
    if (!descriptor
        && (u32at(head, 14)? != crc
            || local_expanded != expanded
            || local_compressed != compressed))
        || (descriptor
            && ((u32at(head, 14)? != 0 && u32at(head, 14)? != crc)
                || (local_expanded != 0 && local_expanded != expanded)
                || (local_compressed != 0 && local_compressed != compressed)))
    {
        return Err(INVALID);
    }
    let start = local
        .checked_add(30 + name_len + extra_len)
        .ok_or(INVALID)?;
    let end = start
        .checked_add(usize64(compressed)?)
        .filter(|n| *n <= central)
        .ok_or(INVALID)?;
    let mut finish = end;
    let mut descriptor_len = 0;
    if descriptor {
        let candidate = |skip: usize| -> Result<usize, SelectionError> {
            let at = end.checked_add(skip).ok_or(INVALID)?;
            if u32at(bytes, at)? != crc {
                return Err(INVALID);
            }
            let (actual_compressed, actual_expanded, size) = if zip64 {
                (u64at(bytes, at + 4)?, u64at(bytes, at + 12)?, 20)
            } else {
                (
                    u64::from(u32at(bytes, at + 4)?),
                    u64::from(u32at(bytes, at + 8)?),
                    12,
                )
            };
            if actual_compressed != compressed || actual_expanded != expanded {
                return Err(INVALID);
            }
            at.checked_add(size)
                .filter(|n| *n <= central)
                .ok_or(INVALID)
        };
        // The optional signature can equal an unsigned descriptor's genuine CRC. Validate
        // complete fields rather than interpreting that first word as authority on its own.
        finish = if u32at(bytes, end)? == 0x08074b50 {
            candidate(4).or_else(|_| candidate(0))?
        } else {
            candidate(0)?
        };
        descriptor_len = finish - end;
    }
    Ok((
        start..end,
        finish,
        30 + name_len + extra_len + descriptor_len,
    ))
}
