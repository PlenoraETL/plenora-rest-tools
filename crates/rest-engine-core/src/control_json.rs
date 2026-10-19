//! Shared JSON preflight for control documents, before typed deserialization.

use std::{collections::BTreeSet, fmt};

use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};

/// Redaction-safe failure while parsing a control document.
///
/// Errors contain only static text, never keys or values from the document.
/// The normal serde JSON recursion limit and complete-document check apply.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseError {
    /// Static diagnostic without document data.
    pub message: &'static str,
    /// One-based parser line.
    pub line: usize,
    /// One-based parser column.
    pub column: usize,
}

/// Rejects duplicate members recursively, including inside arrays, without retaining values.
pub fn validate(bytes: &[u8]) -> Result<(), ParseError> {
    let mut duplicate = false;
    let mut parser = serde_json::Deserializer::from_slice(bytes);
    UniqueJson(&mut duplicate)
        .deserialize(&mut parser)
        .and_then(|()| parser.end())
        .map_err(|error| ParseError {
            message: if duplicate {
                "control document contains duplicate JSON members"
            } else {
                "control document is not valid JSON"
            },
            line: error.line(),
            column: error.column(),
        })
}

struct UniqueJson<'a>(&'a mut bool);

impl<'de> DeserializeSeed<'de> for UniqueJson<'_> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for UniqueJson<'_> {
    type Value = ();

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON with unique object members")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                *self.0 = true;
                return Err(serde::de::Error::custom("duplicate JSON member"));
            }
            map.next_value_seed(UniqueJson(self.0))?;
        }
        Ok(())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<(), A::Error> {
        while sequence.next_element_seed(UniqueJson(self.0))?.is_some() {}
        Ok(())
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn recursive_unique_members() {
        for input in [
            r#"{"secret":1,"secret":2}"#,
            r#"{"a":[{"secret":1,"secr\u0065t":2}]}"#,
        ] {
            assert_eq!(
                validate(input.as_bytes()).map_err(|error| error.message),
                Err("control document contains duplicate JSON members")
            );
        }
        assert!(validate(br#"{"a":[{"x":null},{"x":true}],"b":[1,-2,1.5,"s"]}"#).is_ok());
        for input in ["{} {}", "{", "[", "1e999"] {
            assert_eq!(
                validate(input.as_bytes()).map_err(|error| error.message),
                Err("control document is not valid JSON")
            );
        }
    }
}
