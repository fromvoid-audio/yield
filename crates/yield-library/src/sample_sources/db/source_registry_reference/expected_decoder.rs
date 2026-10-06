//! Closed historical prior-row metadata, without internally tagged Content buffering.
use super::{SourceRegistryReferenceExpected, SourceRegistryReferenceTransition};
use serde::{Deserialize, Deserializer, de};
use std::fmt;

#[derive(Deserialize)]
#[serde(variant_identifier, rename_all = "snake_case")]
enum Status {
    Absent,
    Present,
}
#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum Field {
    Status,
    State,
    RowIdentity,
}
const _: () = {
    assert!(std::mem::size_of::<Status>() <= 1);
    assert!(std::mem::size_of::<Field>() <= 1);
    assert!(std::mem::size_of::<Option<SourceRegistryReferenceTransition>>() <= 1);
    assert!(std::mem::size_of::<Option<[u8; 32]>>() <= 33);
    assert!(std::mem::size_of::<SourceRegistryReferenceExpected>() <= 34);
};
struct ExpectedVisitor;
impl<'de> de::Visitor<'de> for ExpectedVisitor {
    type Value = SourceRegistryReferenceExpected;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("closed tagged source prior-row metadata")
    }
    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut status = None;
        let mut state = None;
        let mut row_identity = None;
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Status => {
                    if status.is_some() {
                        return Err(de::Error::duplicate_field("status"));
                    }
                    status = Some(map.next_value::<Status>()?);
                }
                Field::State => {
                    if state.is_some() {
                        return Err(de::Error::duplicate_field("state"));
                    }
                    state = Some(map.next_value::<SourceRegistryReferenceTransition>()?);
                }
                Field::RowIdentity => {
                    if row_identity.is_some() {
                        return Err(de::Error::duplicate_field("row_identity"));
                    }
                    row_identity = Some(map.next_value::<[u8; 32]>()?);
                }
            }
        }
        match status.ok_or_else(|| de::Error::missing_field("status"))? {
            Status::Absent => {
                if state.is_some() || row_identity.is_some() {
                    return Err(de::Error::custom("absent prior row has present fields"));
                }
                Ok(SourceRegistryReferenceExpected::Absent {})
            }
            Status::Present => Ok(SourceRegistryReferenceExpected::Present {
                state: state.ok_or_else(|| de::Error::missing_field("state"))?,
                row_identity: row_identity
                    .ok_or_else(|| de::Error::missing_field("row_identity"))?,
            }),
        }
    }
    fn visit_seq<S: de::SeqAccess<'de>>(self, mut seq: S) -> Result<Self::Value, S::Error> {
        // Original TaggedContentVisitor permits tag-first struct sequences.
        let status = seq
            .next_element::<Status>()?
            .ok_or_else(|| de::Error::missing_field("status"))?;
        let value = match status {
            Status::Absent => SourceRegistryReferenceExpected::Absent {},
            Status::Present => SourceRegistryReferenceExpected::Present {
                state: seq
                    .next_element::<SourceRegistryReferenceTransition>()?
                    .ok_or_else(|| de::Error::missing_field("state"))?,
                row_identity: seq
                    .next_element::<[u8; 32]>()?
                    .ok_or_else(|| de::Error::missing_field("row_identity"))?,
            },
        };
        seq.next_element::<NoMore>()?;
        Ok(value)
    }
}
enum NoMore {}
impl<'de> Deserialize<'de> for NoMore {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(de::Error::custom(
            "unexpected extra prior-row sequence item",
        ))
    }
}
impl<'de> Deserialize<'de> for SourceRegistryReferenceExpected {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(ExpectedVisitor)
    }
}
