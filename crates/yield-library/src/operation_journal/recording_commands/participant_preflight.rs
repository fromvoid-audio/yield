//! Count existing participant-array limits without owning participant/value graphs.
//! This is only a conservative resource preflight, never schema validation.
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;

use super::super::participants::MAX_PARTICIPANTS;

#[derive(Default)]
struct State {
    excess: bool,
    schema17: bool,
    recording: bool,
}
#[derive(Clone, Copy)]
enum Kind {
    Envelope,
    Intent,
    Args,
    Proposal,
    Set,
    Entries,
}
#[derive(Deserialize)]
#[serde(field_identifier)]
enum Key {
    #[serde(rename = "schema_version")]
    Schema,
    #[serde(rename = "universal_intent")]
    Intent,
    #[serde(rename = "universal_command_args")]
    Args,
    #[serde(rename = "Recording")]
    Recording,
    #[serde(rename = "participants")]
    Participants,
    #[serde(rename = "descriptors")]
    Descriptors,
    #[serde(rename = "checkpoints")]
    Checkpoints,
    #[serde(other)]
    Other,
}
const _: () = {
    assert!(std::mem::size_of::<Key>() <= 1);
    assert!(std::mem::size_of::<Kind>() <= 1);
    assert!(std::mem::size_of::<State>() <= 3);
    assert!(std::mem::size_of::<Scan<'static>>() <= 16);
};

pub(super) fn proposal_exceeds(bytes: &[u8]) -> bool {
    bytes.len() <= super::MAX_RECORDING_VALUE_BYTES
        && scan(bytes, Kind::Proposal).is_some_and(|state| state.excess)
}
pub(crate) fn prestart_exceeds(bytes: &[u8]) -> bool {
    // The actual Store snapshot has already imposed this same 64 KiB row cap.
    // Larger private callers fall back to the unchanged bounded decoder.
    bytes.len() <= 64 * 1024
        && scan(bytes, Kind::Envelope)
            .is_some_and(|state| state.schema17 && state.recording && state.excess)
}
fn scan(bytes: &[u8], kind: Kind) -> Option<State> {
    let mut state = State::default();
    // Scratch and any scanner error drop here, before the caller's original decoder.
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    Scan {
        state: &mut state,
        kind,
    }
    .deserialize(&mut decoder)
    .ok()?;
    decoder.end().ok()?;
    Some(state)
}
struct Scan<'a> {
    state: &'a mut State,
    kind: Kind,
}
impl<'de> DeserializeSeed<'de> for Scan<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<(), D::Error> {
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Scan<'_> {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("participant preflight container")
    }
    // Do not copy a potentially long wrong-shaped string into a scanner diagnostic.
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Err(E::custom("unsupported participant preflight shape"))
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<(), M::Error> {
        if matches!(self.kind, Kind::Entries) {
            return Err(de::Error::custom("unsupported participant preflight shape"));
        }
        let mut args_count = 0u8;
        let mut recording = false;
        while let Some(key) = map.next_key::<Key>()? {
            if matches!(self.kind, Kind::Envelope) && matches!(key, Key::Schema) {
                // Integer schema tags have the same representation as Value::as_u64.
                self.state.schema17 = map.next_value_seed(SchemaTag)? == 17;
                continue;
            }
            let next = match (self.kind, &key) {
                (Kind::Envelope, Key::Intent) => Some(Kind::Intent),
                (Kind::Envelope, Key::Args) => Some(Kind::Args),
                (Kind::Intent | Kind::Proposal, Key::Participants) => Some(Kind::Set),
                (Kind::Set, Key::Descriptors | Key::Checkpoints) => Some(Kind::Entries),
                (Kind::Args, Key::Recording) => Some(Kind::Proposal),
                _ => None,
            };
            if matches!(self.kind, Kind::Args) {
                args_count = args_count.saturating_add(1).min(2);
                recording |= matches!(key, Key::Recording);
            }
            if let Some(kind) = next {
                map.next_value_seed(Scan {
                    state: self.state,
                    kind,
                })?;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        if matches!(self.kind, Kind::Args) {
            self.state.recording = args_count == 1 && recording;
        }
        Ok(())
    }
    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<(), S::Error> {
        if matches!(self.kind, Kind::Entries) {
            let mut count = 0usize;
            while seq.next_element::<IgnoredAny>()?.is_some() {
                count = count.saturating_add(1).min(MAX_PARTICIPANTS + 1);
            }
            self.state.excess |= count > MAX_PARTICIPANTS;
            return Ok(());
        }
        if !matches!(self.kind, Kind::Proposal | Kind::Set) {
            return Err(de::Error::custom("unsupported participant preflight shape"));
        }
        let mut index = 0usize;
        loop {
            let next = match (self.kind, index) {
                (Kind::Proposal, 4) => Some(Kind::Set),
                (Kind::Set, 0 | 1) => Some(Kind::Entries),
                _ => None,
            };
            let present = if let Some(kind) = next {
                seq.next_element_seed(Scan {
                    state: self.state,
                    kind,
                })?
                .is_some()
            } else {
                seq.next_element::<IgnoredAny>()?.is_some()
            };
            if !present {
                break;
            }
            index = index.saturating_add(1);
        }
        Ok(())
    }
}

struct SchemaTag;
impl<'de> DeserializeSeed<'de> for SchemaTag {
    type Value = u64;
    fn deserialize<D: Deserializer<'de>>(self, decoder: D) -> Result<u64, D::Error> {
        decoder.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for SchemaTag {
    type Value = u64;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("integer schema tag")
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<u64, E> {
        Ok(value)
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<u64, E> {
        Err(E::custom("unsupported participant preflight schema tag"))
    }
}
