//! Closed typed audit decoding, without internally tagged Content buffering.
use super::SourceRegistryAuditObservation;
use serde::{Deserialize, Deserializer, de};
use std::fmt;

#[derive(Deserialize)]
#[serde(variant_identifier, rename_all = "snake_case")]
enum Status {
    Unavailable,
    Captured,
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "snake_case")]
enum Field {
    Status,
    Revision,
    DirectoryGeneration,
}

const _: () = {
    assert!(std::mem::size_of::<Status>() <= 1);
    assert!(std::mem::size_of::<Field>() <= 1);
    assert!(std::mem::size_of::<Option<u64>>() <= 16);
    assert!(std::mem::size_of::<SourceRegistryAuditObservation>() <= 32);
};

struct AuditVisitor;
impl<'de> de::Visitor<'de> for AuditVisitor {
    type Value = SourceRegistryAuditObservation;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a closed tagged source audit observation")
    }
    fn visit_map<M: de::MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
        let mut status = None;
        let mut revision = None;
        let mut generation = None;
        while let Some(field) = map.next_key::<Field>()? {
            match field {
                Field::Status => {
                    if status.is_some() {
                        return Err(de::Error::duplicate_field("status"));
                    }
                    status = Some(map.next_value::<Status>()?);
                }
                Field::Revision => {
                    if revision.is_some() {
                        return Err(de::Error::duplicate_field("revision"));
                    }
                    revision = Some(map.next_value::<u64>()?);
                }
                Field::DirectoryGeneration => {
                    if generation.is_some() {
                        return Err(de::Error::duplicate_field("directory_generation"));
                    }
                    generation = Some(map.next_value::<u64>()?);
                }
            }
        }
        match status.ok_or_else(|| de::Error::missing_field("status"))? {
            Status::Unavailable => {
                if revision.is_some() || generation.is_some() {
                    return Err(de::Error::custom("unavailable audit has captured fields"));
                }
                Ok(SourceRegistryAuditObservation::Unavailable {})
            }
            Status::Captured => Ok(SourceRegistryAuditObservation::Captured {
                revision: revision.ok_or_else(|| de::Error::missing_field("revision"))?,
                directory_generation: generation
                    .ok_or_else(|| de::Error::missing_field("directory_generation"))?,
            }),
        }
    }
    fn visit_seq<S: de::SeqAccess<'de>>(self, mut sequence: S) -> Result<Self::Value, S::Error> {
        // Pinned serde's old TaggedContentVisitor accepts a tag followed by the
        // struct fields. Preserve that historical representation without a tree.
        let status = sequence
            .next_element::<Status>()?
            .ok_or_else(|| de::Error::missing_field("status"))?;
        let value = match status {
            Status::Unavailable => SourceRegistryAuditObservation::Unavailable {},
            Status::Captured => SourceRegistryAuditObservation::Captured {
                revision: sequence
                    .next_element::<u64>()?
                    .ok_or_else(|| de::Error::missing_field("revision"))?,
                directory_generation: sequence
                    .next_element::<u64>()?
                    .ok_or_else(|| de::Error::missing_field("directory_generation"))?,
            },
        };
        // The rejecting seed detects an extra item before traversing its value.
        sequence.next_element::<NoMore>()?;
        Ok(value)
    }
}
enum NoMore {}
impl<'de> Deserialize<'de> for NoMore {
    fn deserialize<D: Deserializer<'de>>(_: D) -> Result<Self, D::Error> {
        Err(de::Error::custom("unexpected extra audit sequence item"))
    }
}
impl<'de> Deserialize<'de> for SourceRegistryAuditObservation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(AuditVisitor)
    }
}
