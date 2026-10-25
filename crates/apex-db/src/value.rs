//! Cell values returned by queries, and how they are shown in a results grid.

use std::fmt;

use serde::{Deserialize, Serialize};

/// One cell in a [`QueryResult`](crate::QueryResult).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum Value {
    /// SQL `NULL`.
    Null,
    /// Boolean (drivers with a native boolean type).
    Bool(bool),
    /// 64-bit signed integer.
    Integer(i64),
    /// 64-bit float.
    Real(f64),
    /// UTF-8 text. Invalid UTF-8 from the database is replaced with U+FFFD.
    Text(String),
    /// Raw bytes.
    Blob(Vec<u8>),
}

impl Value {
    /// `true` for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }

    /// Single-line text for a grid cell: line breaks and tabs become spaces and
    /// the result is cut to at most `max_chars` characters, ending in `…` when cut.
    ///
    /// `max_chars == 0` means no length limit.
    pub fn display_cell(&self, max_chars: usize) -> String {
        let full = self.to_string();
        let single_line: String = full
            .chars()
            .map(|c| {
                if matches!(c, '\n' | '\r' | '\t') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        if max_chars == 0 || single_line.chars().count() <= max_chars {
            return single_line;
        }
        let mut cut: String = single_line
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect();
        cut.push('…');
        cut
    }
}

/// Formats the value the way the results grid shows it:
/// `NULL`, `true`/`false`, numbers, raw text, and blobs as `<N bytes>`.
/// Floats always show a decimal point (`1.0`, not `1`).
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("NULL"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Integer(i) => write!(f, "{i}"),
            Self::Real(r) => write!(f, "{r:?}"),
            Self::Text(s) => f.write_str(s),
            Self::Blob(b) if b.len() == 1 => f.write_str("<1 byte>"),
            Self::Blob(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}

impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Integer(v)
    }
}

impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Real(v)
    }
}

impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Text(v.to_owned())
    }
}

impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}

impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Self::Blob(v)
    }
}

impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        v.map_or(Self::Null, Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display() {
        assert_eq!(Value::Null.to_string(), "NULL");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Integer(-42).to_string(), "-42");
        assert_eq!(Value::Real(1.0).to_string(), "1.0");
        assert_eq!(Value::Real(2.5).to_string(), "2.5");
        assert_eq!(Value::from("héllo").to_string(), "héllo");
        assert_eq!(Value::Blob(vec![]).to_string(), "<0 bytes>");
        assert_eq!(Value::Blob(vec![7]).to_string(), "<1 byte>");
        assert_eq!(Value::Blob(vec![0; 16]).to_string(), "<16 bytes>");
        assert_eq!(Value::from(None::<i64>), Value::Null);
    }

    #[test]
    fn display_cell_flattens_and_truncates() {
        let v = Value::from("line one\nline\ttwo");
        assert_eq!(v.display_cell(0), "line one line two");
        assert_eq!(v.display_cell(100), "line one line two");
        assert_eq!(v.display_cell(5), "line…");
        assert_eq!(Value::from("ééééé").display_cell(3), "éé…");
        assert_eq!(Value::from("abc").display_cell(3), "abc");
        assert_eq!(Value::from("abc").display_cell(1), "…");
    }

    #[test]
    fn serde_round_trip() {
        let values = vec![
            Value::Null,
            Value::Integer(3),
            Value::Real(0.5),
            Value::from("x"),
            Value::Blob(vec![1, 2]),
        ];
        let json = serde_json::to_string(&values).expect("serialize");
        let back: Vec<Value> = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, values);
    }
}
