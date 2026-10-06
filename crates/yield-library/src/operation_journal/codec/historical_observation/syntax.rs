//! The original serde number/string/depth rules, without owning any JSON graph.
use serde::Deserializer;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

pub(super) fn validate(bytes: &[u8]) -> Result<(), serde_json::Error> {
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    Discard.deserialize(&mut parser)?;
    parser.end()
}

struct Discard;
impl<'de> DeserializeSeed<'de> for Discard {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, parser: D) -> Result<(), D::Error> {
        parser.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Discard {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a finite JSON value")
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<(), E> {
        serde_json::Number::from_f64(value)
            .map(|_| ())
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_borrowed_str<E>(self, _: &'de str) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while sequence.next_element_seed(Discard)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        while map.next_key_seed(Key)?.is_some() {
            map.next_value_seed(Discard)?;
        }
        Ok(())
    }
}
struct Key;
impl<'de> DeserializeSeed<'de> for Key {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, parser: D) -> Result<(), D::Error> {
        parser.deserialize_str(Discard)
    }
}
