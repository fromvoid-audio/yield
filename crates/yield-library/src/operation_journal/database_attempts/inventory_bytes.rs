//! Bound byte ownership while retaining the original complete JSON validation order.
use serde::Deserializer;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::database_claims::MAX_INVENTORY_BYTES;

#[cfg(test)]
thread_local! {
    pub(crate) static OWNED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static MAX_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
    struct Bytes;
    impl<'de> Visitor<'de> for Bytes {
        type Value = Vec<u8>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a sequence of bytes")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Vec<u8>, A::Error> {
            let mut bytes = Vec::new();
            // Validate every original scalar, including after overflow. A later driver/type
            // error must still beat the subsequent canonical and inventory-shape checks.
            while let Some(byte) = sequence.next_element::<u8>()? {
                if bytes.len() <= MAX_INVENTORY_BYTES {
                    if bytes.len() == bytes.capacity() {
                        let next = bytes
                            .capacity()
                            .max(8)
                            .saturating_mul(2)
                            .min(MAX_INVENTORY_BYTES + 1);
                        bytes.reserve_exact(next - bytes.len());
                    }
                    bytes.push(byte);
                    #[cfg(test)]
                    {
                        OWNED_BYTES.with(|n| n.set(n.get() + 1));
                        MAX_CAPACITY.with(|n| n.set(n.get().max(bytes.capacity())));
                    }
                }
            }
            // The extra byte is only evidence of an invalid oversized shape. It can never
            // pass the original encode() bound or yield a successfully decoded start.
            Ok(bytes)
        }
    }
    deserializer.deserialize_seq(Bytes)
}

/// Compare an oversized inventory without retaining its original byte sequence.
/// The complete typed decoder already validated all elements as u8. The canonical
/// start's preceding fields are closed UUIDs, so this first marker is its inventory.
pub(super) fn canonical_overflow(
    input: &[u8],
    canonical: &[u8],
    cap: usize,
) -> super::Result<bool> {
    const MARKER: &[u8] = b"\"inventory\":[";
    let Some(start) = canonical
        .windows(MARKER.len())
        .position(|p| p == MARKER)
        .map(|offset| offset + MARKER.len())
    else {
        return Ok(false);
    };
    let Some(canonical_end) = canonical[start..]
        .iter()
        .position(|b| *b == b']')
        .map(|n| start + n)
    else {
        return Ok(false);
    };
    // The original capped canonical serializer runs before its byte equality test.
    // Count normalized u8 tokens, independently of input whitespace/spelling/order.
    let length = canonical_inventory_length(input)?;
    super::require(
        canonical.len() - (canonical_end - start) + length <= cap,
        "attempt encoding bound",
    )?;
    if !input.starts_with(&canonical[..start]) {
        return Ok(false);
    }
    let Some(end) = input[start..]
        .iter()
        .position(|b| *b == b']')
        .map(|n| start + n)
    else {
        return Ok(false);
    };
    if input[end..] != canonical[canonical_end..] {
        return Ok(false);
    }
    // Canonical u8 decimal spelling: no whitespace, signs, exponents or leading zeros.
    // Complete serde validation already rejected non-u8 values and malformed separators.
    let mut count = 0usize;
    for token in input[start..end].split(|b| *b == b',') {
        if token.is_empty()
            || token.len() > 3
            || !token.iter().all(u8::is_ascii_digit)
            || (token.len() > 1 && token[0] == b'0')
        {
            return Ok(false);
        }
        count += 1;
    }
    Ok(count > MAX_INVENTORY_BYTES)
}

fn canonical_inventory_length(input: &[u8]) -> super::Result<usize> {
    struct Length;
    impl<'de> DeserializeSeed<'de> for Length {
        type Value = usize;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<usize, D::Error> {
            d.deserialize_seq(self)
        }
    }
    impl<'de> Visitor<'de> for Length {
        type Value = usize;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("byte sequence")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<usize, A::Error> {
            let mut length = 0usize;
            let mut first = true;
            while let Some(byte) = a.next_element::<u8>()? {
                length += usize::from(!first)
                    + if byte < 10 {
                        1
                    } else if byte < 100 {
                        2
                    } else {
                        3
                    };
                first = false;
            }
            Ok(length)
        }
    }
    struct Envelope;
    impl<'de> Visitor<'de> for Envelope {
        type Value = usize;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("validated attempt")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<usize, A::Error> {
            let mut length = 0;
            // The first complete typed parse already rejected unknown/duplicate fields.
            // Only closed short field names can reach this scalar-only second pass.
            while let Some(key) = a.next_key::<String>()? {
                if key == "inventory" {
                    length = a.next_value_seed(Length)?;
                } else {
                    a.next_value::<IgnoredAny>()?;
                }
            }
            Ok(length)
        }
    }
    let mut d = serde_json::Deserializer::from_slice(input);
    d.deserialize_map(Envelope)
        .map_err(|_| super::database_claims::DatabaseClaimError("unsupported attempt image"))
}

#[cfg(test)]
mod tests;
