//! Key grammar (SPEC-01 §2): `^[a-z][a-z0-9_]{0,63}$` — lowercase, starts with
//! a letter, snake_case, ≤ 64 chars. Keys are identifiers referenced by writes;
//! they are immutable after creation (rename = deprecate + create). Display
//! names are free UTF-8 ≤ 256 bytes and are not identifiers.

use std::fmt;

/// Maximum key length (SPEC-01 §2).
pub const KEY_MAX_LEN: usize = 64;
/// Maximum display-name bytes (SPEC-01 §2).
pub const DISPLAY_NAME_MAX_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyError {
    Empty,
    TooLong { len: usize },
    InvalidStart,
    InvalidChar { pos: usize, ch: char },
    DisplayNameTooLong { len: usize },
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Empty => write!(f, "key must not be empty"),
            KeyError::TooLong { len } => write!(f, "key length {len} exceeds cap {KEY_MAX_LEN}"),
            KeyError::InvalidStart => write!(f, "key must start with a lowercase letter"),
            KeyError::InvalidChar { pos, ch } => {
                write!(
                    f,
                    "invalid character {ch:?} at position {pos} (lowercase alnum/underscore only)"
                )
            }
            KeyError::DisplayNameTooLong { len } => {
                write!(
                    f,
                    "display name is {len} bytes; cap is {DISPLAY_NAME_MAX_BYTES}"
                )
            }
        }
    }
}

impl std::error::Error for KeyError {}

/// A validated catalog key. Construction is the grammar check (SPEC-01 §2);
/// there is no way to obtain a `Key` that violates the grammar.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(Box<str>);

impl Key {
    pub fn parse(s: impl AsRef<str>) -> Result<Self, KeyError> {
        let s = s.as_ref();
        if s.is_empty() {
            return Err(KeyError::Empty);
        }
        if s.len() > KEY_MAX_LEN {
            return Err(KeyError::TooLong { len: s.len() });
        }
        let bytes = s.as_bytes();
        if !bytes[0].is_ascii_lowercase() {
            return Err(KeyError::InvalidStart);
        }
        for (pos, ch) in s.char_indices().skip(1) {
            let ok = ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_';
            if !ok {
                return Err(KeyError::InvalidChar { pos, ch });
            }
        }
        Ok(Key(s.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validate a display name (free UTF-8, ≤ 256 bytes, not an identifier —
/// it MAY change freely, so this is a pure check, not a newtype).
pub fn validate_display_name(name: &str) -> Result<(), KeyError> {
    let len = name.len();
    if len > DISPLAY_NAME_MAX_BYTES {
        return Err(KeyError::DisplayNameTooLong { len });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grammar_accepts_canonical_keys() {
        for good in [
            "a",
            "task",
            "task_2",
            "state_model_x",
            &"a".repeat(KEY_MAX_LEN),
        ] {
            assert!(
                Key::parse(good).is_ok(),
                "`{}` must be valid",
                &good[..12.min(good.len())]
            );
        }
    }

    #[test]
    fn grammar_rejects_violations() {
        assert!(matches!(Key::parse(""), Err(KeyError::Empty)));
        assert!(matches!(Key::parse("9bad"), Err(KeyError::InvalidStart)));
        assert!(matches!(Key::parse("Bad"), Err(KeyError::InvalidStart)));
        assert!(matches!(
            Key::parse("a-b"),
            Err(KeyError::InvalidChar { pos: 1, ch: '-' })
        ));
        assert!(matches!(Key::parse("_a"), Err(KeyError::InvalidStart)));
        assert!(matches!(
            Key::parse("a b"),
            Err(KeyError::InvalidChar { pos: 1, ch: ' ' })
        ));
        let long = "a".repeat(KEY_MAX_LEN + 1);
        assert!(matches!(
            Key::parse(long),
            Err(KeyError::TooLong { len: 65 })
        ));
    }

    #[test]
    fn display_name_cap() {
        assert!(validate_display_name("Process Task").is_ok());
        assert!(matches!(
            validate_display_name(&"x".repeat(DISPLAY_NAME_MAX_BYTES + 1)),
            Err(KeyError::DisplayNameTooLong { len: 257 })
        ));
    }
}
