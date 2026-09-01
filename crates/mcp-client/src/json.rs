//! Strict JSON helpers shared by state and configuration writers.
//!
//! `serde_json::Value` normally accepts duplicate object keys and keeps the
//! last value. That is unsafe before a read/modify/write transaction because
//! serialization would silently discard every earlier duplicate.

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::fmt;

#[derive(Debug)]
struct UniqueJsonValue(Value);

impl<'de> Deserialize<'de> for UniqueJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct UniqueJsonVisitor;

        impl<'de> Visitor<'de> for UniqueJsonVisitor {
            type Value = UniqueJsonValue;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJsonValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJsonValue(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJsonValue(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let number = serde_json::Number::from_f64(value)
                    .ok_or_else(|| E::custom("JSON number is not finite"))?;
                Ok(UniqueJsonValue(Value::Number(number)))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJsonValue(Value::String(value.to_string())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJsonValue(Value::String(value)))
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJsonValue(Value::Null))
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJsonValue(Value::Null))
            }

            fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
            where
                D: Deserializer<'de>,
            {
                UniqueJsonValue::deserialize(deserializer)
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(UniqueJsonValue(value)) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJsonValue(Value::Array(values)))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom(format!("duplicate object key '{key}'")));
                    }
                    let UniqueJsonValue(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(UniqueJsonValue(Value::Object(values)))
            }
        }

        deserializer.deserialize_any(UniqueJsonVisitor)
    }
}

/// Parse strict JSON while rejecting duplicate object keys at every depth.
pub fn parse_value_without_duplicate_keys(content: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str::<UniqueJsonValue>(content).map(|value| value.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicates_at_every_depth() {
        assert!(parse_value_without_duplicate_keys(r#"{"a":1,"a":2}"#).is_err());
        assert!(
            parse_value_without_duplicate_keys(r#"{"outer":{"event":[],"event":[1]}}"#).is_err()
        );
        assert!(parse_value_without_duplicate_keys(r#"{"a":1,"outer":{"event":[]}}"#).is_ok());
    }
}
