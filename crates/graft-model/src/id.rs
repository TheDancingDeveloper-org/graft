//! Object ids: BLAKE3 over the canonical encoding, separated by kind
//! (`docs/design.md` §4.1).

use std::fmt;

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
}

/// A 32-byte content address.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    /// Hashes `payload` as an object of `kind`:
    /// `BLAKE3::derive_key("graft 1 " ‖ kind, payload)`.
    pub fn hash(_kind: ObjectKind, _payload: &[u8]) -> Self {
        todo!("G1.1: BLAKE3 derive_key hashing")
    }

    /// The 64-character lowercase hex form, without a kind letter.
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Parses exactly 64 lowercase hex characters.
    pub fn from_hex(_s: &str) -> Result<Self, IdError> {
        todo!("G1.1: strict hex parsing")
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
    /// Not 64 (or 65 with a kind letter) characters.
    Length(usize),
    /// A character outside `[0-9a-f]`.
    NotLowerHex,
    /// The kind letter does not match the expected kind.
    WrongKind {
        /// The expected kind.
        expected: ObjectKind,
    },
}

macro_rules! typed_id {
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub ObjectId);

        impl $name {
            /// The kind this id names.
            pub const KIND: ObjectKind = $kind;

            /// Parses the text form `<letter><64 hex>`.
            pub fn parse(_s: &str) -> Result<Self, IdError> {
                todo!("G1.1: typed id parsing")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}{}", Self::KIND.letter(), self.0.to_hex())
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
