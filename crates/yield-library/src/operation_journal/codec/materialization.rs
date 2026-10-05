//! Quotas for owned JSON tree materialization, separate from full heap accounting.
use super::DecodeError;
use serde::Deserializer;
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use thiserror::Error;

/// Structural quotas, cumulative across the entire initial JSON tree.
/// Root depth is zero. Owned string bytes include decoded keys and string values.
/// These are not allocator-byte, parser-scratch or later typed-decoding bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JsonMaterializationLimits {
    /// Maximum value nodes, including root and all containers/scalars.
    pub maximum_nodes: usize,
    /// Maximum entries across all arrays.
    pub maximum_array_entries: usize,
    /// Maximum entries across all objects (including a rejected duplicate key).
    pub maximum_object_entries: usize,
    /// Maximum decoded bytes owned by key/value strings.
    pub maximum_owned_string_bytes: usize,
    /// Maximum nested value depth, with root at zero.
    pub maximum_depth: usize,
}

/// The exhausted materialization resource, without stored payload details.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonResourceKind {
    /// Value node quota.
    Nodes,
    /// Array entry quota.
    ArrayEntries,
    /// Object entry quota.
    ObjectEntries,
    /// Owned decoded key/value bytes.
    OwnedStringBytes,
    /// Nested value depth.
    Depth,
}

/// Resource refusal is separate from existing JSON/schema errors.
#[derive(Debug, Error)]
pub enum BoundedDecodeError {
    /// The initial tree cannot fit the explicit caller quota.
    #[error("JSON materialization {resource:?} exceeds maximum {maximum}")]
    LimitExceeded {
        /// Exhausted resource.
        resource: JsonResourceKind,
        /// Requested maximum.
        maximum: usize,
    },
    /// Existing strict record error, preserving its schema/error category.
    #[error(transparent)]
    Decode(#[from] DecodeError),
}

#[derive(Debug)]
pub(super) struct ParseFailure {
    pub(super) error: serde_json::Error,
    pub(super) limit: Option<(JsonResourceKind, usize)>,
}
#[derive(Default)]
struct State {
    limits: Option<JsonMaterializationLimits>,
    used: [usize; 4],
    exhausted: Option<(JsonResourceKind, usize)>,
    #[cfg(test)]
    created: [usize; 4],
}
impl State {
    fn charge<E: de::Error>(&mut self, resource: JsonResourceKind, amount: usize) -> Result<(), E> {
        let (index, maximum) = match (resource, self.limits) {
            (_, None) => return Ok(()),
            (JsonResourceKind::Nodes, Some(l)) => (0, l.maximum_nodes),
            (JsonResourceKind::ArrayEntries, Some(l)) => (1, l.maximum_array_entries),
            (JsonResourceKind::ObjectEntries, Some(l)) => (2, l.maximum_object_entries),
            (JsonResourceKind::OwnedStringBytes, Some(l)) => (3, l.maximum_owned_string_bytes),
            (JsonResourceKind::Depth, _) => unreachable!(),
        };
        let Some(next) = self.used[index]
            .checked_add(amount)
            .filter(|n| *n <= maximum)
        else {
            self.exhausted = Some((resource, maximum));
            return Err(E::custom("JSON materialization quota exceeded"));
        };
        self.used[index] = next;
        Ok(())
    }
    fn enter<E: de::Error>(&mut self, depth: usize) -> Result<(), E> {
        if let Some(limits) = self.limits {
            if depth > limits.maximum_depth {
                self.exhausted = Some((JsonResourceKind::Depth, limits.maximum_depth));
                return Err(E::custom("JSON materialization depth exceeded"));
            }
        }
        self.charge(JsonResourceKind::Nodes, 1)
    }
    fn value(&mut self, value: Value) -> Value {
        #[cfg(test)]
        {
            self.created[0] += 1;
        }
        value
    }
    fn string<E: de::Error>(&mut self, value: &str) -> Result<String, E> {
        self.charge(JsonResourceKind::OwnedStringBytes, value.len())?;
        let owned = value.to_owned();
        #[cfg(test)]
        {
            self.created[3] += owned.len();
        }
        Ok(owned)
    }
}

pub(super) fn parse(
    bytes: &[u8],
    limits: Option<JsonMaterializationLimits>,
) -> Result<Value, ParseFailure> {
    let mut state = State {
        limits,
        ..State::default()
    };
    parse_in_state(bytes, &mut state)
}
fn parse_in_state(bytes: &[u8], state: &mut State) -> Result<Value, ParseFailure> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let result = ValueSeed { state, depth: 0 }
        .deserialize(&mut deserializer)
        .and_then(|value| {
            deserializer.end()?;
            Ok(value)
        });
    result.map_err(|error| ParseFailure {
        error,
        limit: state.exhausted,
    })
}
struct ValueSeed<'a> {
    state: &'a mut State,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for ValueSeed<'_> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        self.state.enter(self.depth)?;
        deserializer.deserialize_any(ValueVisitor {
            state: self.state,
            depth: self.depth,
        })
    }
}
struct ValueVisitor<'a> {
    state: &'a mut State,
    depth: usize,
}
impl<'de> Visitor<'de> for ValueVisitor<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON value without duplicate object keys")
    }
    fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
        Ok(self.state.value(Value::Bool(v)))
    }
    fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
        Ok(self.state.value(Value::Number(v.into())))
    }
    fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
        Ok(self.state.value(Value::Number(v.into())))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        let value = serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("JSON number is not finite"))?;
        Ok(self.state.value(value))
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        let owned = self.state.string(v)?;
        Ok(self.state.value(Value::String(owned)))
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<Value, E> {
        self.visit_str(v)
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        self.state
            .charge(JsonResourceKind::OwnedStringBytes, v.len())?;
        #[cfg(test)]
        {
            self.state.created[3] += v.len();
        }
        Ok(self.state.value(Value::String(v)))
    }
    fn visit_none<E>(self) -> Result<Value, E> {
        Ok(self.state.value(Value::Null))
    }
    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(self.state.value(Value::Null))
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        ValueSeed {
            state: self.state,
            depth: self.depth,
        }
        .deserialize(d)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(ArrayEntrySeed {
            state: self.state,
            depth: self.depth + 1,
        })? {
            values.push(value);
            #[cfg(test)]
            {
                self.state.created[1] += 1;
            }
        }
        Ok(self.state.value(Value::Array(values)))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(key) = map.next_key_seed(KeySeed { state: self.state })? {
            if object.contains_key(&key) {
                return Err(de::Error::custom(format!(
                    "duplicate JSON object field `{key}`"
                )));
            }
            let value = map.next_value_seed(ValueSeed {
                state: self.state,
                depth: self.depth + 1,
            })?;
            object.insert(key, value);
            #[cfg(test)]
            {
                self.state.created[2] += 1;
            }
        }
        Ok(self.state.value(Value::Object(object)))
    }
}
struct ArrayEntrySeed<'a> {
    state: &'a mut State,
    depth: usize,
}
impl<'de> DeserializeSeed<'de> for ArrayEntrySeed<'_> {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        self.state.charge(JsonResourceKind::ArrayEntries, 1)?;
        ValueSeed {
            state: self.state,
            depth: self.depth,
        }
        .deserialize(d)
    }
}
struct KeySeed<'a> {
    state: &'a mut State,
}
impl<'de> DeserializeSeed<'de> for KeySeed<'_> {
    type Value = String;
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<String, D::Error> {
        self.state.charge(JsonResourceKind::ObjectEntries, 1)?;
        d.deserialize_string(KeyVisitor(self.state))
    }
}
struct KeyVisitor<'a>(&'a mut State);
impl<'de> Visitor<'de> for KeyVisitor<'_> {
    type Value = String;
    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a JSON object key")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<String, E> {
        self.0.string(v)
    }
    fn visit_borrowed_str<E: de::Error>(self, v: &'de str) -> Result<String, E> {
        self.visit_str(v)
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<String, E> {
        self.0.charge(JsonResourceKind::OwnedStringBytes, v.len())?;
        #[cfg(test)]
        {
            self.0.created[3] += v.len();
        }
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn generous() -> JsonMaterializationLimits {
        JsonMaterializationLimits {
            maximum_nodes: 100_000,
            maximum_array_entries: 100_000,
            maximum_object_entries: 100_000,
            maximum_owned_string_bytes: 1_000_000,
            maximum_depth: 64,
        }
    }
    #[test]
    fn actual_wide_array_node_and_entry_refusal_precedes_next_materialization() {
        let bytes = format!("[{}]", vec!["null"; 10_000].join(","));
        for (resource, nodes, entries, expected_values, expected_entries) in [
            (JsonResourceKind::Nodes, 17, 100_000, 16, 16),
            (JsonResourceKind::ArrayEntries, 100_000, 5, 5, 5),
        ] {
            let mut state = State {
                limits: Some(JsonMaterializationLimits {
                    maximum_nodes: nodes,
                    maximum_array_entries: entries,
                    ..generous()
                }),
                ..State::default()
            };
            let failure = parse_in_state(bytes.as_bytes(), &mut state).unwrap_err();
            assert_eq!(failure.limit.unwrap().0, resource);
            assert_eq!(state.created[0], expected_values);
            assert_eq!(state.created[1], expected_entries);
            assert_eq!(state.created[3], 0);
        }
    }
    #[test]
    fn actual_object_quota_stops_before_next_key_and_value_are_owned() {
        let bytes = br#"{"a":null,"b":null,"c":null}"#;
        let mut state = State {
            limits: Some(JsonMaterializationLimits {
                maximum_object_entries: 2,
                ..generous()
            }),
            ..State::default()
        };
        assert_eq!(
            parse_in_state(bytes, &mut state).unwrap_err().limit,
            Some((JsonResourceKind::ObjectEntries, 2))
        );
        assert_eq!(state.created, [2, 0, 2, 2]);
    }
    #[test]
    fn decoded_escaped_keys_and_values_charge_utf8_before_owned_copy() {
        let bytes = br#"{"\u0061":"\u20ac","b":"x"}"#;
        let mut state = State {
            limits: Some(JsonMaterializationLimits {
                maximum_owned_string_bytes: 3,
                ..generous()
            }),
            ..State::default()
        };
        assert_eq!(
            parse_in_state(bytes, &mut state).unwrap_err().limit,
            Some((JsonResourceKind::OwnedStringBytes, 3))
        );
        assert_eq!(state.created[3], 1); // First key only; the three-byte value was refused.
        assert_eq!(state.created[0], 0);
        let limits = JsonMaterializationLimits {
            maximum_owned_string_bytes: 6,
            ..generous()
        };
        assert_eq!(
            parse(bytes, Some(limits)).unwrap(),
            serde_json::json!({"a":"€", "b":"x"})
        );
    }
    #[test]
    fn root_zero_depth_limit_refuses_nested_value_before_it_is_built() {
        let mut state = State {
            limits: Some(JsonMaterializationLimits {
                maximum_depth: 0,
                ..generous()
            }),
            ..State::default()
        };
        assert_eq!(
            parse_in_state(b"[null]", &mut state).unwrap_err().limit,
            Some((JsonResourceKind::Depth, 0))
        );
        assert_eq!(state.created, [0; 4]);
        assert_eq!(
            parse(
                b"null",
                Some(JsonMaterializationLimits {
                    maximum_depth: 0,
                    ..generous()
                })
            )
            .unwrap(),
            Value::Null
        );
        let failure = parse(
            b"null",
            Some(JsonMaterializationLimits {
                maximum_nodes: 0,
                ..generous()
            }),
        )
        .unwrap_err();
        assert_eq!(failure.limit, Some((JsonResourceKind::Nodes, 0)));
    }
    #[test]
    fn duplicate_escaped_duplicate_trailing_and_malformed_remain_strict() {
        for bytes in [
            br#"{"a":1,"a":2}"#.as_slice(),
            br#"{"a":1,"\u0061":2}"#.as_slice(),
            b"[] []",
            b"[",
            b"{bad}",
        ] {
            let plain = parse(bytes, None).unwrap_err();
            let bounded = parse(bytes, Some(generous())).unwrap_err();
            assert_eq!(bounded.limit, None);
            assert_eq!(bounded.error.to_string(), plain.error.to_string());
        }
    }
}
