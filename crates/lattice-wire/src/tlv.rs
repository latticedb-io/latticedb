//! TLV payloads (SPEC-02 §2): `[u16 tag][u32 len][value]` sequences, LE,
//! self-describing — any language binds without a code generator.
//!
//! Tags per §7–§10; `request_id` is tag 0x0001, mandatory first TLV in
//! every request, echoed in every reply (WPT-2.3).

use std::collections::BTreeMap;
use std::fmt;

/// LE array from a statically verified slice (loud on mismatch — invariant).
fn le_bytes<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

/// Mandatory correlation tag (SPEC-02 §2).
pub const TAG_REQUEST_ID: u16 = 0x0001;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TlvError {
    Truncated {
        need: usize,
        have: usize,
    },
    /// Value length ≠ the tag's declared shape.
    BadLength {
        tag: u16,
        expected: usize,
        got: usize,
    },
    DuplicateTag {
        tag: u16,
    },
    MissingRequest,
}

impl fmt::Display for TlvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlvError::Truncated { need, have } => {
                write!(f, "TLV truncated: need {need}, have {have}")
            }
            TlvError::BadLength { tag, expected, got } => {
                write!(f, "TLV tag {tag:#06x}: length {got}, expected {expected}")
            }
            TlvError::DuplicateTag { tag } => write!(f, "duplicate TLV tag {tag:#06x}"),
            TlvError::MissingRequest => {
                write!(f, "request_id (tag 0x0001) is mandatory in every request")
            }
        }
    }
}

impl std::error::Error for TlvError {}

/// A decoded TLV sequence: tag → value (BTreeMap = deterministic order).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TlvMap {
    values: BTreeMap<u16, Vec<u8>>,
}

impl TlvMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, tag: u16, value: Vec<u8>) {
        self.values.insert(tag, value);
    }

    pub fn put_u64(&mut self, tag: u16, v: u64) {
        self.put(tag, v.to_le_bytes().to_vec());
    }

    pub fn put_str(&mut self, tag: u16, s: &str) {
        self.put(tag, s.as_bytes().to_vec());
    }

    pub fn request_id(&mut self, id: u64) {
        self.put_u64(TAG_REQUEST_ID, id);
    }

    pub fn get(&self, tag: u16) -> Option<&[u8]> {
        self.values.get(&tag).map(Vec::as_slice)
    }

    pub fn get_u64(&self, tag: u16) -> Option<u64> {
        let v = self.get(tag)?;
        if v.len() != 8 {
            return None;
        }
        Some(u64::from_le_bytes(le_bytes(v)))
    }

    pub fn get_str(&self, tag: u16) -> Option<&str> {
        std::str::from_utf8(self.get(tag)?).ok()
    }

    pub fn request_id_value(&self) -> Option<u64> {
        self.get_u64(TAG_REQUEST_ID)
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Consume into the raw tag→value map (command layer's tag lookup).
    pub fn into_owned(self) -> BTreeMap<u16, Vec<u8>> {
        self.values
    }

    /// Encode: `[u16 tag][u32 len][value]` per TLV, tag order = BTreeMap
    /// (deterministic).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (tag, value) in &self.values {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&(value.len() as u32).to_le_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    /// Decode a TLV payload; duplicate tags rejected (self-describing
    /// sequences are sets, not multisets).
    pub fn decode(data: &[u8]) -> Result<Self, TlvError> {
        let mut map = Self::default();
        let mut pos = 0usize;
        while pos < data.len() {
            if pos + 6 > data.len() {
                return Err(TlvError::Truncated {
                    need: pos + 6,
                    have: data.len(),
                });
            }
            let tag = u16::from_le_bytes(le_bytes(&data[pos..pos + 2]));
            let len = u32::from_le_bytes(le_bytes(&data[pos + 2..pos + 6])) as usize;
            pos += 6;
            if pos + len > data.len() {
                return Err(TlvError::Truncated {
                    need: pos + len,
                    have: data.len(),
                });
            }
            let value = data[pos..pos + len].to_vec();
            pos += len;
            if map.values.insert(tag, value).is_some() {
                return Err(TlvError::DuplicateTag { tag });
            }
        }
        Ok(map)
    }

    /// Requests MUST carry request_id (tag 0x0001) — decode + validate.
    pub fn decode_request(data: &[u8]) -> Result<Self, TlvError> {
        let map = Self::decode(data)?;
        if map.request_id_value().is_none() {
            return Err(TlvError::MissingRequest);
        }
        Ok(map)
    }
}
