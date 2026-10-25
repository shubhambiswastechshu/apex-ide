//! Validated plugin identifiers.

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A validated, reverse-DNS style plugin identifier such as `apex.hello` or
/// `com.example.sample-theme`.
///
/// Rules:
/// * at least two segments separated by `.`;
/// * every segment starts with a lowercase ASCII letter and continues with
///   lowercase ASCII letters, digits, `-` or `_`;
/// * at most [`PluginId::MAX_LEN`] bytes in total.
///
/// Ids are case-sensitive and compared byte-wise, so the lowercase rule keeps
/// them unambiguous on case-insensitive file systems.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PluginId(String);

/// Error returned when a string is not a valid [`PluginId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid plugin id `{id}`: {reason}")]
pub struct InvalidPluginId {
    /// The rejected input.
    pub id: String,
    /// Human-readable explanation of the rule that was broken.
    pub reason: &'static str,
}

impl PluginId {
    /// Maximum length of an id, in bytes.
    pub const MAX_LEN: usize = 128;

    /// Validates `id` and wraps it.
    pub fn new(id: impl Into<String>) -> Result<Self, InvalidPluginId> {
        let id = id.into();
        match validate(&id) {
            Ok(()) => Ok(Self(id)),
            Err(reason) => Err(InvalidPluginId { id, reason }),
        }
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn validate(id: &str) -> Result<(), &'static str> {
    if id.is_empty() {
        return Err("must not be empty");
    }
    if id.len() > PluginId::MAX_LEN {
        return Err("must be at most 128 bytes long");
    }
    let mut segments = 0usize;
    for segment in id.split('.') {
        segments += 1;
        let mut chars = segment.chars();
        match chars.next() {
            None => return Err("segments separated by `.` must not be empty"),
            Some(c) if c.is_ascii_lowercase() => {}
            Some(_) => return Err("each segment must start with a lowercase ASCII letter"),
        }
        if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') {
            return Err("segments may only contain lowercase ASCII letters, digits, `-` and `_`");
        }
    }
    if segments < 2 {
        return Err(
            "must be reverse-DNS style with at least two segments, e.g. `com.example.plugin`",
        );
    }
    Ok(())
}

impl fmt::Display for PluginId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for PluginId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for PluginId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl FromStr for PluginId {
    type Err = InvalidPluginId;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for PluginId {
    type Error = InvalidPluginId;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for PluginId {
    type Error = InvalidPluginId;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PluginId> for String {
    fn from(id: PluginId) -> Self {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reverse_dns_ids() {
        for id in [
            "apex.hello",
            "com.example.sample-theme",
            "io.github.user_1.tool",
            "a.b",
        ] {
            assert_eq!(PluginId::new(id).map(|p| p.to_string()), Ok(id.to_owned()));
        }
    }

    #[test]
    fn rejects_malformed_ids() {
        let too_long = format!("a.{}", "b".repeat(PluginId::MAX_LEN));
        for id in [
            "",
            "hello",
            "Com.Example",
            "com.Example",
            ".apex",
            "apex.",
            "apex..x",
            "apex.1abc",
            "apex.-x",
            "apex.he llo",
            "apex/hello",
            "apex.héllo",
            too_long.as_str(),
        ] {
            assert!(PluginId::new(id).is_err(), "`{id}` should be rejected");
        }
    }

    #[test]
    fn serde_round_trip_validates() {
        #[derive(Debug, Serialize, Deserialize)]
        struct Wrapper {
            id: PluginId,
        }
        let parsed: Wrapper = toml::from_str("id = \"apex.hello\"").unwrap();
        assert_eq!(parsed.id.as_str(), "apex.hello");
        assert_eq!(
            toml::to_string(&parsed).unwrap().trim(),
            "id = \"apex.hello\""
        );
        assert!(toml::from_str::<Wrapper>("id = \"Nope\"").is_err());
    }
}
