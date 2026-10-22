//! Object ids: BLAKE3 over the canonical encoding, separated by kind
//! (`docs/design.md` §4.1).
//!
//! Text forms accepted, and nothing else:
//!
//! - [`ObjectId::from_hex`]: exactly 64 lowercase hex characters;
//! - typed ids ([`BlobId::parse`] and friends): the kind letter (`b`, `t`,
//!   `c`) followed by exactly 64 lowercase hex characters;
//! - [`IdPrefix::parse`]: the kind letter followed by 12 to 64 lowercase hex
//!   characters. Prefixes are for lookup only and are never stored.
//!
//! Upper case, whitespace, `0x` prefixes and any other length are rejected.

use std::fmt;
use std::str::FromStr;

/// The kind of a content-addressed value. Part of the hash domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    /// File bytes.
    Blob,
    /// One directory's entries.
    Tree,
    /// A changeset.
    Changeset,
}

impl ObjectKind {
    /// The one-letter prefix of the text form (`b`, `t`, `c`).
    pub fn letter(self) -> char {
        match self {
            ObjectKind::Blob => 'b',
            ObjectKind::Tree => 't',
            ObjectKind::Changeset => 'c',
        }
    }

    /// The kind named by a text-form letter.
    pub fn from_letter(letter: char) -> Option<Self> {
        match letter {
            'b' => Some(ObjectKind::Blob),
            't' => Some(ObjectKind::Tree),
            'c' => Some(ObjectKind::Changeset),
            _ => None,
        }
    }

    /// The BLAKE3 key-derivation context: `"graft 1 " ‖ kind`.
    pub fn hash_context(self) -> &'static str {
        match self {
            ObjectKind::Blob => "graft 1 blob",
            ObjectKind::Tree => "graft 1 tree",
            ObjectKind::Changeset => "graft 1 changeset",
        }
    }
}

/// The number of hex characters in a full id.
pub const HEX_LEN: usize = 64;

/// The shortest prefix [`IdPrefix::parse`] accepts, in hex characters.
pub const MIN_PREFIX_HEX: usize = 12;

/// A 32-byte content address.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    /// Hashes `payload` as an object of `kind`:
    /// `BLAKE3::derive_key("graft 1 " ‖ kind, payload)`.
    pub fn hash(kind: ObjectKind, payload: &[u8]) -> Self {
        ObjectId(blake3::derive_key(kind.hash_context(), payload))
    }

    /// The 64-character lowercase hex form, without a kind letter.
    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(HEX_LEN);
        for b in self.0 {
            s.push(DIGITS[(b >> 4) as usize] as char);
            s.push(DIGITS[(b & 0xf) as usize] as char);
        }
        s
    }

    /// Parses exactly 64 lowercase hex characters.
    pub fn from_hex(s: &str) -> Result<Self, IdError> {
        let bytes = s.as_bytes();
        if bytes.len() != HEX_LEN {
            return Err(IdError::Length(bytes.len()));
        }
        let mut out = [0u8; 32];
        // The length check above leaves no remainder.
        let (pairs, _rest) = bytes.as_chunks::<2>();
        for (i, pair) in pairs.iter().enumerate() {
            out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Ok(ObjectId(out))
    }
}

fn nibble(c: u8) -> Result<u8, IdError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        _ => Err(IdError::NotLowerHex),
    }
}

/// Splits the kind letter off a text form and checks it.
fn split_kind(s: &str, expected: ObjectKind) -> Result<&str, IdError> {
    match s.chars().next() {
        None => Err(IdError::Length(0)),
        Some(c) if c == expected.letter() => Ok(&s[1..]),
        Some(_) => Err(IdError::WrongKind { expected }),
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", &self.to_hex()[..12])
    }
}

/// Why an id failed to parse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdError {
    /// Not 64 (or 65 with a kind letter) characters; for a prefix, outside
    /// 12 to 64 hex characters. Holds the length seen.
    Length(usize),
    /// A character outside `[0-9a-f]`.
    NotLowerHex,
    /// The kind letter does not match the expected kind.
    WrongKind {
        /// The expected kind.
        expected: ObjectKind,
    },
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::Length(n) => write!(f, "id has the wrong length ({n})"),
            IdError::NotLowerHex => f.write_str("id is not lowercase hex"),
            IdError::WrongKind { expected } => {
                write!(f, "id does not start with '{}'", expected.letter())
            }
        }
    }
}

impl std::error::Error for IdError {}

macro_rules! typed_id {
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub ObjectId);

        impl $name {
            /// The kind this id names.
            pub const KIND: ObjectKind = $kind;

            /// Hashes a canonical payload of this kind.
            pub fn hash(payload: &[u8]) -> Self {
                $name(ObjectId::hash(Self::KIND, payload))
            }

            /// Parses the text form `<letter><64 lowercase hex>`.
            pub fn parse(s: &str) -> Result<Self, IdError> {
                if s.len() != HEX_LEN + 1 {
                    return Err(IdError::Length(s.len()));
                }
                ObjectId::from_hex(split_kind(s, Self::KIND)?).map($name)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", Self::KIND.letter(), self.0.to_hex())
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, IdError> {
                Self::parse(s)
            }
        }

        impl From<$name> for ObjectId {
            fn from(id: $name) -> ObjectId {
                id.0
            }
        }
    };
}

typed_id!(
    /// The id of a blob.
    BlobId,
    ObjectKind::Blob
);
typed_id!(
    /// The id of a tree.
    TreeId,
    ObjectKind::Tree
);
typed_id!(
    /// The id of a changeset.
    ChangesetId,
    ObjectKind::Changeset
);

/// A short id typed by a person or agent: a kind letter and 12 to 64
/// lowercase hex characters. Resolving it (and rejecting an ambiguous one) is
/// the store's job; prefixes are never stored.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct IdPrefix {
    kind: ObjectKind,
    hex: String,
}

impl IdPrefix {
    /// Parses `<letter><12..=64 lowercase hex>`.
    pub fn parse(s: &str) -> Result<Self, IdError> {
        let kind = s
            .chars()
            .next()
            .and_then(ObjectKind::from_letter)
            .ok_or(IdError::NotLowerHex)?;
        let hex = &s[1..];
        if !(MIN_PREFIX_HEX..=HEX_LEN).contains(&hex.len()) {
            return Err(IdError::Length(hex.len()));
        }
        for c in hex.bytes() {
            nibble(c)?;
        }
        Ok(IdPrefix {
            kind,
            hex: hex.to_owned(),
        })
    }

    /// The kind the prefix names.
    pub fn kind(&self) -> ObjectKind {
        self.kind
    }

    /// The hex digits, without the kind letter.
    pub fn hex(&self) -> &str {
        &self.hex
    }

    /// Whether `id` starts with this prefix.
    pub fn matches(&self, id: &ObjectId) -> bool {
        id.to_hex().starts_with(&self.hex)
    }
}

impl fmt::Display for IdPrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.kind.letter(), self.hex)
    }
}
