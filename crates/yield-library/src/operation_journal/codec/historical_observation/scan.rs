//! Scalar inventory of validated JSON; offsets never authorize schema interpretation.
use super::keys::{self, Key};

#[derive(Default)]
pub(super) struct Inventory {
    pub(super) keys: usize,
    pub(super) retained_keys: usize,
    pub(super) nodes: usize,
    pub(super) objects: usize,
    pub(super) arrays: usize,
    pub(super) array_entries: usize,
    pub(super) string_bytes: usize,
    pub(super) payload: Option<std::ops::Range<usize>>,
    pub(super) version: Option<u32>,
}
pub(super) fn scan(bytes: &[u8], key: impl FnMut(Key)) -> Inventory {
    scan_with_payload(bytes, key, false, false)
}

pub(super) fn scan_full(bytes: &[u8]) -> Inventory {
    scan_with_payload(bytes, |_| {}, true, false)
}

pub(super) fn scan_without_inventory(bytes: &[u8]) -> Inventory {
    scan_with_payload(bytes, |_| {}, true, true)
}

fn scan_with_payload(
    bytes: &[u8],
    mut key: impl FnMut(Key),
    retain_payload: bool,
    skip_inventory: bool,
) -> Inventory {
    let mut scan = Scanner {
        bytes,
        offset: 0,
        inventory: Inventory::default(),
        key: &mut key,
        retain_payload,
        skip_inventory,
    };
    scan.value(true, true);
    scan.inventory
}
struct Scanner<'a, F> {
    bytes: &'a [u8],
    offset: usize,
    inventory: Inventory,
    key: &'a mut F,
    retain_payload: bool,
    skip_inventory: bool,
}
impl<F: FnMut(Key)> Scanner<'_, F> {
    fn whitespace(&mut self) {
        while self
            .bytes
            .get(self.offset)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.offset += 1;
        }
    }
    fn string(&mut self) -> std::ops::Range<usize> {
        self.offset += 1;
        let start = self.offset;
        while self.bytes[self.offset] != b'"' {
            if self.bytes[self.offset] == b'\\' {
                self.offset += 1;
            }
            self.offset += 1;
        }
        let end = self.offset;
        self.offset += 1;
        start..end
    }
    fn value(&mut self, root: bool, retained: bool) {
        self.whitespace();
        if retained {
            self.inventory.nodes += 1;
        }
        match self.bytes[self.offset] {
            b'{' => {
                let object = self.offset as u32;
                if retained {
                    self.inventory.objects += 1;
                }
                self.offset += 1;
                self.whitespace();
                if self.bytes[self.offset] == b'}' {
                    self.offset += 1;
                    return;
                }
                loop {
                    let range = self.string();
                    let key = Key {
                        object,
                        start: range.start as u32,
                        end: range.end as u32,
                    };
                    self.inventory.keys += 1;
                    (self.key)(key);
                    if retained {
                        self.inventory.string_bytes += range.len();
                        self.inventory.retained_keys += 1;
                    }
                    self.whitespace();
                    self.offset += 1;
                    self.whitespace();
                    let start = self.offset;
                    let payload = root && keys::matches(&key, self.bytes, b"payload");
                    let inventory = root
                        && self.skip_inventory
                        && keys::matches(&key, self.bytes, b"inventory");
                    self.value(
                        false,
                        retained && (!payload || self.retain_payload) && !inventory,
                    );
                    if payload {
                        self.inventory.payload = Some(start..self.offset);
                    }
                    if root && keys::matches(&key, self.bytes, b"schema_version") {
                        self.inventory.version =
                            std::str::from_utf8(&self.bytes[start..self.offset])
                                .ok()
                                .and_then(|v| v.parse().ok());
                    }
                    self.whitespace();
                    if self.bytes[self.offset] == b'}' {
                        self.offset += 1;
                        break;
                    }
                    self.offset += 1;
                    self.whitespace();
                }
            }
            b'[' => {
                if retained {
                    self.inventory.arrays += 1;
                }
                self.offset += 1;
                self.whitespace();
                if self.bytes[self.offset] == b']' {
                    self.offset += 1;
                    return;
                }
                loop {
                    if retained {
                        self.inventory.array_entries += 1;
                    }
                    self.value(false, retained);
                    self.whitespace();
                    if self.bytes[self.offset] == b']' {
                        self.offset += 1;
                        break;
                    }
                    self.offset += 1;
                }
            }
            b'"' => {
                let range = self.string();
                if retained {
                    self.inventory.string_bytes += range.len();
                }
            }
            _ => {
                while self
                    .bytes
                    .get(self.offset)
                    .is_some_and(|b| !matches!(b, b',' | b'}' | b']') && !b.is_ascii_whitespace())
                {
                    self.offset += 1;
                }
            }
        }
    }
}
