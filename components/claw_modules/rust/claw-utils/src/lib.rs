//! Shared helpers for the claw Rust crates: log-safe text truncation and the
//! prefixed-id newtype macro ([`define_prefixed_id`]).

use core::fmt;

use thiserror::Error;

const LOG_SNIPPET_LEN: usize = 96;

/// Log-safe view of text: at most [`LOG_SNIPPET_LEN`] bytes on a char boundary, plus `"..."` when truncated.
pub struct TruncatedText<T>(T);

impl<T: AsRef<str>> TruncatedText<T> {
    pub fn new(text: T) -> Self {
        Self(text)
    }
}

impl<T: AsRef<str>> fmt::Display for TruncatedText<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = self.0.as_ref();
        let mut end = text.len().min(LOG_SNIPPET_LEN);
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        write!(f, "{}", &text[..end])?;
        if text.len() > LOG_SNIPPET_LEN {
            write!(f, "...")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum IdParseError {
    #[error("empty id string")]
    Empty,
    #[error("invalid {kind} id: {value}")]
    Invalid { kind: &'static str, value: String },
}

pub fn parse_prefixed_id(
    value: &str,
    prefix: &str,
    kind: &'static str,
) -> Result<usize, IdParseError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(IdParseError::Empty);
    }

    let rest = trimmed
        .strip_prefix(prefix)
        .ok_or_else(|| IdParseError::Invalid {
            kind,
            value: value.to_string(),
        })?;

    rest.parse::<usize>().map_err(|_| IdParseError::Invalid {
        kind,
        value: value.to_string(),
    })
}

/// Define a strongly typed wire-prefixed id (`session-1`, `task-2`, ...).
#[macro_export]
macro_rules! define_prefixed_id {
    ($name:ident, $prefix:literal, $kind:literal) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub struct $name(pub usize);

        impl $name {
            pub const fn new(id: usize) -> Self {
                Self(id)
            }

            pub fn to_wire(&self) -> String {
                format!(concat!($prefix, "{}"), self.0)
            }

            pub fn from_wire(value: &str) -> Result<Self, $crate::IdParseError> {
                $crate::parse_prefixed_id(value, $prefix, $kind).map(Self)
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_wire(value)
            }
        }

        impl From<usize> for $name {
            fn from(value: usize) -> Self {
                Self(value)
            }
        }

        impl ::serde::Serialize for $name {
            fn serialize<S: ::serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.to_wire())
            }
        }

        impl<'de> ::serde::Deserialize<'de> for $name {
            fn deserialize<D: ::serde::Deserializer<'de>>(
                deserializer: D,
            ) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::from_wire(&value).map_err(::serde::de::Error::custom)
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_short_text_unchanged() {
        assert_eq!(TruncatedText::new("hi").to_string(), "hi");
    }

    #[test]
    fn display_long_text_truncates_with_suffix() {
        let long = "x".repeat(LOG_SNIPPET_LEN + 10);
        let rendered = TruncatedText::new(&long).to_string();
        assert_eq!(rendered.len(), LOG_SNIPPET_LEN + 3);
        assert!(rendered.ends_with("..."));
    }

    #[test]
    fn display_respects_char_boundary() {
        let text = "é".repeat(50);
        let rendered = TruncatedText::new(text).to_string();
        assert!(rendered.is_char_boundary(rendered.len()));
    }
}
